// SPDX-License-Identifier: Apache-2.0
//! The SQL that differs between databases, behind [`Dialect`].

mod postgres;

pub use postgres::PostgresDialect;

/// A type that rendered SQL converts a value to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SqlType {
    Text,
    Boolean,
    /// 32-bit integer.
    Integer,
    /// 64-bit integer.
    BigInt,
    /// Exact decimal number of any precision.
    Numeric,
    /// 64-bit floating point.
    Double,
    Timestamp,
    /// Timestamp with time zone.
    TimestampTz,
    Date,
    Time,
    /// A type as the database names it, such as
    /// [`ColumnType::sql_type`](crate::sql::ColumnType::sql_type).
    Named(String),
}

/// SQL syntax that differs between databases. Rendering a
/// [`SqlPlan`](crate::sql::SqlPlan) writes everything else itself.
///
/// Arguments and results are SQL expressions. An argument may contain
/// parameters; a result may repeat an argument, so the parameters of a
/// dialect are numbered (`$1`), not positional.
pub trait Dialect {
    /// `expr` converted to `ty`.
    fn cast(&self, expr: &str, ty: &SqlType) -> String;

    /// Written after a text expression, makes comparing and sorting it
    /// compare code points.
    fn code_point_collation(&self) -> &str;

    /// Whether `text` matches the regular expression `pattern`.
    fn regex_match(&self, text: &str, pattern: &str) -> String;

    /// Whether the timestamp or date `expr` is neither infinity.
    fn is_finite(&self, expr: &str) -> String;

    /// The name of the SQL type of `expr`, as text.
    fn type_of(&self, expr: &str) -> String;

    /// The R2RML §7.3 IRI-safe version of the text `text`: every character
    /// outside `iunreserved` becomes `%XX` per UTF-8 byte, upper case.
    /// NULL stays NULL.
    fn iri_safe(&self, text: &str) -> String;

    /// The binary string `expr` as upper-case hexadecimal digits.
    fn hex_lexical(&self, expr: &str) -> String;

    /// The canonical `xsd:decimal` lexical form (XML Schema 1.1
    /// `decimalCanonicalMap`) of the exact number `expr`.
    fn decimal_lexical(&self, expr: &str) -> String;

    /// The `time` value `expr` with `24:00:00` as `00:00:00`, the
    /// canonical `xsd:time` value of both.
    fn time_normal(&self, expr: &str) -> String;
}
