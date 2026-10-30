// SPDX-License-Identifier: Apache-2.0
//! PostgreSQL syntax.

use super::{Dialect, SqlType};
use crate::mapping::template::UCSCHAR;

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

    fn is_finite(&self, expr: &str) -> String {
        format!("isfinite({expr})")
    }

    fn type_of(&self, expr: &str) -> String {
        format!("pg_typeof({expr})::text")
    }

    /// A value made only of `iunreserved` characters is its own IRI-safe
    /// version, so the whole value is matched against the same class
    /// first; only the other values pay for the subquery that encodes them
    /// character by character.
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
        format!(
            "CASE WHEN {text} ~ '^{class}*$' THEN {text} \
             WHEN {text} IS NOT NULL THEN (SELECT coalesce(string_agg(\
             CASE WHEN c ~ '^{class}$' THEN c \
             ELSE regexp_replace(upper(encode(convert_to(c, 'UTF8'), 'hex')), '(..)', '%\\1', 'g') END, \
             '' ORDER BY n), '') \
             FROM generate_series(1, char_length({text})) AS n, substr({text}, n, 1) AS c) END"
        )
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
