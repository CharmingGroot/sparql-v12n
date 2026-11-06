// SPDX-License-Identifier: Apache-2.0
//! PostgreSQL syntax.

use super::{Dialect, SqlType};
use crate::mapping::template::UCSCHAR;
use crate::sparql::{HashFunction, StrTest};

/// PostgreSQL 13 or later.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PostgresDialect;

impl Dialect for PostgresDialect {
    fn cast(&self, expr: &str, ty: &SqlType) -> String {
        let name = match ty {
            SqlType::Text => "text",
            SqlType::Boolean => "bool",
            SqlType::Integer => "int4",
            SqlType::BigInt => "int8",
            SqlType::Numeric => "numeric",
            SqlType::Double => "float8",
            SqlType::Timestamp => "timestamp",
            SqlType::TimestampTz => "timestamptz",
            SqlType::Date => "date",
            SqlType::Time => "time",
            SqlType::Named(name) => name,
        };
        format!("{expr}::{name}")
    }

    fn code_point_collation(&self) -> &str {
        // Code point order in a UTF-8 database only; rendering refuses to
        // order strings in other encodings.
        " COLLATE \"C\""
    }

    fn regex_match(&self, text: &str, pattern: &str) -> String {
        format!("{text} ~ {pattern}")
    }

    /// `char_length` counts characters, which are code points in a UTF-8
    /// database, as `substr` does.
    fn char_length(&self, text: &str) -> String {
        format!("char_length({text})")
    }

    fn substring(&self, text: &str, start: &str, length: Option<&str>) -> String {
        match length {
            Some(length) => format!("substr({text}, {start}, {length})"),
            None => format!("substr({text}, {start})"),
        }
    }

    fn position(&self, text: &str, search: &str) -> String {
        format!("strpos({text}, {search})")
    }

    fn regex_replace(&self, text: &str, pattern: &str, replacement: &str) -> String {
        format!("regexp_replace({text}, {pattern}, {replacement}, 'g')")
    }

    fn encode_for_uri(&self, text: &str) -> String {
        percent_encoded(text, "[-.0-9A-Z_a-z~]")
    }

    /// `convert_to` gives the bytes of the UTF-8 encoding whatever the
    /// encoding of the database. `md5` writes lower-case hexadecimal
    /// digits itself; the SHA-2 functions (11 or later) give the digest as
    /// `bytea`.
    fn hash(&self, function: HashFunction, text: &str) -> String {
        let bytes = format!("convert_to({text}, 'UTF8')");
        match function {
            HashFunction::Md5 => format!("md5({bytes})"),
            HashFunction::Sha256 => format!("encode(sha256({bytes}), 'hex')"),
            HashFunction::Sha384 => format!("encode(sha384({bytes}), 'hex')"),
            HashFunction::Sha512 => format!("encode(sha512({bytes}), 'hex')"),
        }
    }

    /// The root locale of ICU applies Unicode's default case operations,
    /// full mappings (`ß` to `SS`) and the final sigma included, while
    /// `upper` and `lower` under a libc locale map one character at a time
    /// and under `"C"` only ASCII. Needs a server built with ICU.
    fn case_map(&self, text: &str, upper: bool) -> String {
        let function = if upper { "upper" } else { "lower" };
        format!("{function}({text} COLLATE \"und-x-icu\")")
    }

    fn like(&self, text: &str, pattern: &str) -> String {
        format!("{text} LIKE {pattern}")
    }

    fn str_test(&self, test: StrTest, text: &str, search: &str) -> String {
        match test {
            StrTest::Starts => format!("starts_with({text}, {search})"),
            StrTest::Ends => format!("right({text}, char_length({search})) = {search}"),
            StrTest::Contains => format!("strpos({text}, {search}) > 0"),
        }
    }

    fn is_finite(&self, expr: &str) -> String {
        format!("isfinite({expr})")
    }

    /// `float8send` writes the IEEE 754 bits in network byte order, sign
    /// first.
    fn sign_bit(&self, expr: &str) -> String {
        format!("get_byte(float8send({expr}), 0) >= 128")
    }

    fn type_of(&self, expr: &str) -> String {
        format!("pg_typeof({expr})::text")
    }

    fn iri_safe(&self, text: &str) -> String {
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
        percent_encoded(text, &class)
    }

    fn hex_lexical(&self, expr: &str) -> String {
        format!("upper(encode({expr}, 'hex'))")
    }

    /// `numeric` output never uses an exponent, and `trim_scale` (13 or
    /// later) drops exactly the trailing fractional zeros (`3`, `12.5`);
    /// tested against `canonical_decimal` over many values in
    /// v12n-postgres.
    fn decimal_lexical(&self, expr: &str) -> String {
        format!("trim_scale({expr})::text")
    }

    fn time_normal(&self, expr: &str) -> String {
        format!("({expr} + interval '0')")
    }
}

/// `text` with every character outside the bracket expression `class` as
/// `%XX` per UTF-8 byte, upper case. A value made only of characters of
/// the class is its own encoding, so the whole value is matched against
/// the class first; only the other values pay for the subquery that
/// encodes them character by character. NULL stays NULL.
fn percent_encoded(text: &str, class: &str) -> String {
    format!(
        "CASE WHEN {text} ~ '^{class}*$' THEN {text} \
         WHEN {text} IS NOT NULL THEN (SELECT coalesce(string_agg(\
         CASE WHEN c ~ '^{class}$' THEN c \
         ELSE regexp_replace(upper(encode(convert_to(c, 'UTF8'), 'hex')), '(..)', '%\\1', 'g') END, \
         '' ORDER BY n), '') \
         FROM generate_series(1, char_length({text})) AS n, substr({text}, n, 1) AS c) END"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iri_safe_keeps_nulls_and_uses_the_iunreserved_ranges() {
        let sql = PostgresDialect.iri_safe("x");
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
}
