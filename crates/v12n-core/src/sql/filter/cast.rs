// SPDX-License-Identifier: Apache-2.0
//! SQL for the XPath constructor functions (SPARQL 1.1 §17.5) of values
//! computed from a row. A string is matched against the lexical space of
//! the target datatype before PostgreSQL reads it, so a lexical form that
//! is not valid is an error (`NULL`) and never a failed statement.

use super::super::{FilterValue, Renderer, SqlExpr};
use super::{Kind, Operand, Text};
use crate::Error;
use crate::dialect::SqlType;
use crate::mapping::TermType;
use crate::sparql::Cast;
use crate::xsd::{self, MAX_CAST_LENGTH, Num};

/// XML Schema 1.1 Part 2 lexical spaces, with the whitespace that the
/// facet `collapse` strips around them (XPath F&O 1.0 §17.1.1).
const INTEGER: &str = "^[\\t\\n\\r ]*[+-]?[0-9]+[\\t\\n\\r ]*$";
const DECIMAL: &str = "^[\\t\\n\\r ]*[+-]?([0-9]+([.][0-9]*)?|[.][0-9]+)[\\t\\n\\r ]*$";
pub(super) const DOUBLE: &str = "^[\\t\\n\\r ]*([+-]?([0-9]+([.][0-9]*)?|[.][0-9]+)([eE][+-]?[0-9]+)?\
                                 |[+-]?INF|NaN)[\\t\\n\\r ]*$";
const TRUE: &str = "^[\\t\\n\\r ]*(true|1)[\\t\\n\\r ]*$";
const FALSE: &str = "^[\\t\\n\\r ]*(false|0)[\\t\\n\\r ]*$";

/// What a cast sees in its argument (the rows of the table of SPARQL 1.1
/// §17.5).
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Source {
    Number(Num),
    Boolean,
    /// A simple literal or an `xsd:string`.
    String,
    /// A term that casts to none of the supported datatypes, such as an
    /// IRI, an `xsd:dateTime` or a literal with a language tag.
    Error,
    Unsupported(String),
}

pub(super) fn source(operand: &Operand<'_>) -> Source {
    match &operand.kind {
        Kind::Unsupported(reason) => Source::Unsupported(reason.clone()),
        Kind::Numeric(class) => Source::Number(*class),
        Kind::Boolean => Source::Boolean,
        // Without column types, as if the column held strings.
        Kind::String | Kind::Unknown => Source::String,
        _ => Source::Error,
    }
}

impl Renderer<'_> {
    /// `cast` of `inner`: a literal of the target datatype, an error for a
    /// source the table does not allow, and unsupported for `xsd:decimal`
    /// of a double.
    pub(super) fn cast<'v>(
        &self,
        value: &'v FilterValue,
        cast: Cast,
        inner: Operand<'v>,
    ) -> Operand<'v> {
        let mut operand = Operand {
            value,
            kind: Kind::Error,
            term_type: None,
            text: Text::Unknown,
            column: None,
            constant: None,
            infinite: false,
            finite: None,
        };
        match (source(&inner), cast) {
            (Source::Unsupported(reason), _) => {
                operand.kind = Kind::Unsupported(reason);
                return operand;
            }
            (Source::Error, _) => return operand,
            (Source::Number(Num::Double), Cast::Decimal) => {
                operand.kind = Kind::Unsupported(xsd::DOUBLE_TO_DECIMAL.to_owned());
                return operand;
            }
            _ => {}
        }
        operand.term_type = Some(TermType::Literal);
        operand.kind = match cast {
            Cast::Boolean => Kind::Boolean,
            Cast::Integer => Kind::Numeric(Num::Integer),
            Cast::Decimal => Kind::Numeric(Num::Decimal),
            Cast::Double => Kind::Numeric(Num::Double),
        };
        operand
    }

    /// A cast to `xsd:integer` or `xsd:decimal` of `a` as `numeric`. A
    /// decimal or a double loses its fraction (XPath F&O 1.0 §17.1.3.4),
    /// and a boolean is 1 or 0.
    pub(super) fn cast_number(&mut self, cast: Cast, a: &FilterValue) -> Result<(), Error> {
        match source(&self.operand(a)) {
            Source::Number(Num::Double) => self.integer_of_double(a)?,
            Source::Number(Num::Decimal) if cast == Cast::Integer => {
                self.sql.push_str("trunc(");
                self.numeric_value(a)?;
                self.sql.push(')');
            }
            Source::Number(_) => self.numeric_value(a)?,
            Source::Boolean => {
                let (one, zero) = (
                    self.dialect.cast("1", &SqlType::Numeric),
                    self.dialect.cast("0", &SqlType::Numeric),
                );
                self.sql.push_str("CASE ");
                self.boolean_value(a)?;
                self.sql
                    .push_str(&format!(" WHEN true THEN {one} WHEN false THEN {zero} END"));
            }
            Source::String => {
                let pattern = if cast == Cast::Integer {
                    INTEGER
                } else {
                    DECIMAL
                };
                let valid = self.matches("s", pattern);
                let numeric = self.dialect.cast("s", &SqlType::Numeric);
                let length = self.dialect.char_length("s");
                self.with_string(
                    a,
                    &format!(
                        "CASE WHEN {length} <= {MAX_CAST_LENGTH} AND {valid} THEN {numeric} END"
                    ),
                )?;
            }
            Source::Error => self.typed_null(&SqlType::Numeric),
            Source::Unsupported(reason) => return Err(super::unsupported(&reason)),
        }
        Ok(())
    }

    /// A boolean or a cast to `xsd:boolean`, as `bool`.
    pub(super) fn boolean_value(&mut self, value: &FilterValue) -> Result<(), Error> {
        match value {
            FilterValue::Cast(Cast::Boolean, a) => self.cast_boolean(a),
            _ => {
                let operand = self.operand(value);
                if operand.kind != Kind::Boolean {
                    self.typed_null(&SqlType::Boolean);
                    return Ok(());
                }
                self.typed_value(&operand, &SqlType::Boolean)
            }
        }
    }

    /// A cast to `xsd:boolean` of `a`: false for a zero or NaN, true for
    /// other numbers (XPath F&O 1.0 §17.1.6), and `true`, `1`, `false` and
    /// `0` for strings.
    fn cast_boolean(&mut self, a: &FilterValue) -> Result<(), Error> {
        match source(&self.operand(a)) {
            Source::Number(Num::Double) => self.double_ebv(a)?,
            Source::Number(_) => {
                self.sql.push('(');
                self.numeric_value(a)?;
                self.sql.push_str(" <> 0)");
            }
            Source::Boolean => self.boolean_value(a)?,
            Source::String => {
                let (t, f) = (self.matches("s", TRUE), self.matches("s", FALSE));
                self.with_string(
                    a,
                    &format!("CASE WHEN {t} THEN true WHEN {f} THEN false END"),
                )?;
            }
            Source::Error => self.typed_null(&SqlType::Boolean),
            Source::Unsupported(reason) => return Err(super::unsupported(&reason)),
        }
        Ok(())
    }

    /// `body` over `s`, the lexical form of the string `a` under the code
    /// point collation, computed once in a scalar subquery.
    pub(super) fn with_string(&mut self, a: &FilterValue, body: &str) -> Result<(), Error> {
        let operand = self.operand(a);
        let start = self.sql.len();
        self.text(&operand)?;
        let text = self.sql.split_off(start);
        self.sql.push_str(&format!(
            "(SELECT {body} FROM (SELECT {text}{} AS s) AS c)",
            self.dialect.code_point_collation()
        ));
        Ok(())
    }

    /// Whether `text` matches the regular expression `pattern`, sent as a
    /// parameter.
    pub(super) fn matches(&mut self, text: &str, pattern: &str) -> String {
        let start = self.sql.len();
        self.cast_rendered(&SqlType::Text, |this| {
            this.expr(&SqlExpr::Param(pattern.into()))
        });
        let pattern = self.sql.split_off(start);
        self.dialect.regex_match(text, &pattern)
    }
}
