// SPDX-License-Identifier: Apache-2.0
//! Quoting of identifiers and table names, and the IRI-safe version of text.

use crate::mapping::LogicalTable;
use crate::mapping::template::UCSCHAR;

/// A logical table for error messages.
pub(super) fn describe_table(table: &LogicalTable) -> String {
    match table {
        LogicalTable::TableName(t) => format!("table {t}"),
        LogicalTable::SqlQuery(_) => "an rr:sqlQuery".to_owned(),
    }
}

/// R2RML §7.3 IRI-safe version of a text expression: every character
/// outside `iunreserved` becomes `%XX` per UTF-8 byte, upper case, as the
/// engine encodes template values when it builds IRIs. NULL stays NULL.
///
/// A value made only of `iunreserved` characters is its own IRI-safe
/// version, so the whole value is matched against the same class first;
/// only the other values pay for the subquery that encodes them character
/// by character.
pub fn iri_safe_sql(text: &str) -> String {
    let mut class = String::from("[-.0-9A-Z_a-z~");
    for &(from, to) in UCSCHAR {
        let escape = |c: u32| {
            if c <= 0xFFFF {
                format!("\\u{c:04X}")
            } else {
                format!("\\U{c:08X}")
            }
        };
        class.push_str(&format!("{}-{}", escape(from), escape(to)));
    }
    class.push(']');
    format!(
        "CASE WHEN {text} ~ '^{class}*$' THEN {text} \
         WHEN {text} IS NOT NULL THEN (SELECT coalesce(string_agg(\
         CASE WHEN c ~ '^{class}$' THEN c \
         ELSE regexp_replace(upper(encode(convert_to(c, 'UTF8'), 'hex')), '(..)', '%\\1', 'g') END, \
         '' ORDER BY n), '') \
         FROM generate_series(1, char_length({text})) AS n, substr({text}, n, 1) AS c) END"
    )
}

pub(super) fn logical_table(table: &LogicalTable) -> String {
    match table {
        LogicalTable::TableName(name) => quote_table_name(name),
        LogicalTable::SqlQuery(query) => {
            format!("({})", query.trim().trim_end_matches(';').trim_end())
        }
    }
}

/// Whether a column name from the mapping is a regular (undelimited)
/// identifier that PostgreSQL may resolve to a different name.
///
/// R2RML §5 makes column names SQL identifiers: delimited ones are taken
/// as written, regular ones are case-insensitive. PostgreSQL folds a
/// regular identifier (PostgreSQL docs §4.1.1: a letter, `_` or a
/// non-ASCII character, then also digits and `$`) to lower case, with
/// encoding-dependent rules for non-ASCII letters, and truncates it to
/// `NAMEDATALEN - 1` bytes. Quoting such a name as written would refer to
/// a different column, so its effective name is asked from the database
/// ([`column_name_probe`]). A name of lower-case ASCII letters, digits,
/// `_` and `$` of at most 63 bytes folds to itself and needs no lookup.
/// Anything else that is not a regular identifier keeps being quoted as
/// written.
pub fn folded_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let regular = (first.is_ascii_alphabetic() || first == '_' || !first.is_ascii())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$') || !c.is_ascii());
    regular
        && (name.len() > 63
            || name
                .chars()
                .any(|c| c.is_ascii_uppercase() || !c.is_ascii()))
}

/// Quotes an identifier from the mapping. An identifier that is already
/// a valid delimited identifier (`"Order Id"`) is kept as written, so
/// R2RML mappings can use either form; anything else is wrapped in double
/// quotes with embedded quotes doubled, which makes it impossible to
/// break out of the identifier.
pub fn quote_identifier(name: &str) -> String {
    if is_delimited(name) {
        name.to_owned()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// The name [`quote_identifier`] refers to, as the database stores it.
pub fn identifier_name(name: &str) -> String {
    if is_delimited(name) {
        name[1..name.len() - 1].replace("\"\"", "\"")
    } else {
        name.to_owned()
    }
}

fn is_delimited(name: &str) -> bool {
    name.len() >= 2
        && name.starts_with('"')
        && name.ends_with('"')
        && !name[1..name.len() - 1].replace("\"\"", "").contains('"')
}

/// Quotes each part of a possibly schema-qualified `rr:tableName`.
pub fn quote_table_name(name: &str) -> String {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    for (i, c) in name.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '.' if !quoted => {
                parts.push(&name[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&name[start..]);
    parts
        .into_iter()
        .map(quote_identifier)
        .collect::<Vec<_>>()
        .join(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folded_identifier_is_a_regular_identifier_that_folding_may_change() {
        for name in ["StudentId", "ID", "SPORTCOUNT", "_X", "a$B", "Ä", "nameé"] {
            assert!(folded_identifier(name), "{name}");
        }
        let long = "a".repeat(64);
        assert!(folded_identifier(&long));
        // Folding leaves these as they are, or they are not regular
        // identifiers and keep being quoted as written.
        for name in [
            "studentid",
            "a_b$1",
            &"a".repeat(63),
            "\"Name\"",
            "Order Id",
            "1A",
            "$A",
            "a-B",
            "a\"B",
            "",
        ] {
            assert!(!folded_identifier(name), "{name}");
        }
    }

    #[test]
    fn quote_identifier_plain_name_is_wrapped() {
        assert_eq!(quote_identifier("customers"), r#""customers""#);
        assert_eq!(quote_identifier("Order Id"), r#""Order Id""#);
    }

    #[test]
    fn quote_identifier_delimited_name_is_kept() {
        assert_eq!(quote_identifier(r#""Order Id""#), r#""Order Id""#);
        assert_eq!(quote_identifier(r#""a""b""#), r#""a""b""#);
    }

    #[test]
    fn quote_identifier_stray_quotes_are_escaped() {
        assert_eq!(quote_identifier(r#"a"b"#), r#""a""b""#);
        assert_eq!(
            quote_identifier(r#""x" ; DROP TABLE t --""#),
            r#""""x"" ; DROP TABLE t --""""#
        );
    }

    #[test]
    fn quote_table_name_quotes_each_part() {
        assert_eq!(quote_table_name("shop.customers"), r#""shop"."customers""#);
        assert_eq!(quote_table_name(r#""my.schema".t"#), r#""my.schema"."t""#);
    }

    #[test]
    fn iri_safe_sql_keeps_nulls_and_uses_the_iunreserved_ranges() {
        let sql = iri_safe_sql("x");
        // A value of `iunreserved` characters only is kept as it is; NULL
        // matches neither case and stays NULL.
        assert!(
            sql.starts_with(r"CASE WHEN x ~ '^[-.0-9A-Z_a-z~\u00A0-\uD7FF"),
            "{sql}"
        );
        assert!(
            sql.contains(r"\U000E1000-\U000EFFFD]*$' THEN x WHEN x IS NOT NULL THEN (SELECT"),
            "{sql}"
        );
        assert!(sql.contains(r"\U000E1000-\U000EFFFD]$' THEN c"), "{sql}");
        assert!(sql.ends_with(" END"), "{sql}");
        assert!(!sql.contains("ELSE NULL"), "{sql}");
    }

    #[test]
    fn identifier_name_unquotes_delimited_names() {
        assert_eq!(identifier_name("id"), "id");
        assert_eq!(identifier_name("\"Id\""), "Id");
        assert_eq!(identifier_name("\"a\"\"b\""), "a\"b");
        assert_eq!(identifier_name("a\"b"), "a\"b");
    }
}
