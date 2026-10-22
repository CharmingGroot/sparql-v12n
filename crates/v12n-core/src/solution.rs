// SPDX-License-Identifier: Apache-2.0
//! Result rows to SPARQL solutions: the natural mapping of SQL values
//! (R2RML §10.2) and term generation from projected term maps (R2RML §11).
//! Database adapters only have to read their values into [`SqlValue`].

use std::borrow::Cow;
use std::collections::HashMap;

use oxrdf::{BlankNode, Literal, NamedNode, Term};

use crate::Error;
use crate::mapping::template::iri_safe;
use crate::mapping::{TemplatePart, TermType};
use crate::sql::{ResultValue, SqlPlan};

const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
const XSD_DECIMAL: &str = "http://www.w3.org/2001/XMLSchema#decimal";
const XSD_DOUBLE: &str = "http://www.w3.org/2001/XMLSchema#double";
const XSD_BOOLEAN: &str = "http://www.w3.org/2001/XMLSchema#boolean";
const XSD_DATE: &str = "http://www.w3.org/2001/XMLSchema#date";
const XSD_TIME: &str = "http://www.w3.org/2001/XMLSchema#time";
const XSD_DATE_TIME: &str = "http://www.w3.org/2001/XMLSchema#dateTime";
const XSD_HEX_BINARY: &str = "http://www.w3.org/2001/XMLSchema#hexBinary";

/// A value of one result column, classified by the SQL type families of
/// the R2RML natural mapping (§10.2).
#[derive(Debug, Clone, PartialEq)]
pub enum SqlValue {
    Null,
    /// Character strings, and values of any type without a natural
    /// datatype, cast to a string. They become plain literals.
    String(String),
    /// `SMALLINT`, `INTEGER`, `BIGINT`.
    Integer(i64),
    /// `NUMERIC`, `DECIMAL`: decimal digits with an optional sign and
    /// point, such as `-12.50`.
    Decimal(String),
    /// `REAL`, `DOUBLE PRECISION`, `FLOAT`.
    Double(f64),
    Boolean(bool),
    /// `DATE`, already in `xsd:date` lexical form.
    Date(String),
    /// `TIME`, already in `xsd:time` lexical form.
    Time(String),
    /// `TIMESTAMP`, already in `xsd:dateTime` lexical form.
    DateTime(String),
    /// `BINARY`, `VARBINARY`, `BLOB`.
    Binary(Vec<u8>),
}

impl SqlValue {
    /// The natural RDF datatype; `None` means a plain literal.
    pub fn natural_datatype(&self) -> Option<&'static str> {
        match self {
            Self::Null | Self::String(_) => None,
            Self::Integer(_) => Some(XSD_INTEGER),
            Self::Decimal(_) => Some(XSD_DECIMAL),
            Self::Double(_) => Some(XSD_DOUBLE),
            Self::Boolean(_) => Some(XSD_BOOLEAN),
            Self::Date(_) => Some(XSD_DATE),
            Self::Time(_) => Some(XSD_TIME),
            Self::DateTime(_) => Some(XSD_DATE_TIME),
            Self::Binary(_) => Some(XSD_HEX_BINARY),
        }
    }

    /// The natural RDF lexical form, `None` for `NULL`.
    ///
    /// R2RML §10.2 requires one lexical form per value and recommends the
    /// canonical one, so numbers, booleans and binaries are canonicalized
    /// (XML Schema 1.0 Part 2). Template values and column literals both go
    /// through here, which keeps the two consistent.
    pub fn natural_lexical_form(&self) -> Option<Cow<'_, str>> {
        Some(match self {
            Self::Null => return None,
            Self::String(s) | Self::Date(s) | Self::Time(s) | Self::DateTime(s) => Cow::Borrowed(s),
            Self::Integer(i) => Cow::Owned(i.to_string()),
            Self::Decimal(d) => match canonical_decimal(d) {
                Some(canonical) => Cow::Owned(canonical),
                // NaN and infinities have no xsd:decimal form; keep them visible.
                None => Cow::Borrowed(d),
            },
            Self::Double(f) => Cow::Owned(canonical_double(*f)),
            Self::Boolean(b) => Cow::Borrowed(if *b { "true" } else { "false" }),
            Self::Binary(bytes) => Cow::Owned(bytes.iter().map(|b| format!("{b:02X}")).collect()),
        })
    }
}

/// Canonical `xsd:decimal` form of a decimal string: no leading or trailing
/// zeros, but at least one digit on each side of the point. `None` when
/// `value` is not an `xsd:decimal` lexical form.
pub fn canonical_decimal(value: &str) -> Option<String> {
    let (negative, digits) = match value.as_bytes().first() {
        Some(b'-') => (true, &value[1..]),
        Some(b'+') => (false, &value[1..]),
        _ => (false, value),
    };
    let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
    if (int.is_empty() && frac.is_empty())
        || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let int = match int.trim_start_matches('0') {
        "" => "0",
        int => int,
    };
    let frac = match frac.trim_end_matches('0') {
        "" => "0",
        frac => frac,
    };
    let sign = if negative && (int, frac) != ("0", "0") {
        "-"
    } else {
        ""
    };
    Some(format!("{sign}{int}.{frac}"))
}

/// Canonical `xsd:double` form: a mantissa with one non-zero digit before
/// the point and at least one after it, then `E` and the exponent.
pub fn canonical_double(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value.is_infinite() {
        return if value > 0.0 { "INF" } else { "-INF" }.to_owned();
    }
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0E0"
        } else {
            "0.0E0"
        }
        .to_owned();
    }
    // `{:E}` prints the shortest mantissa that reads back as the same value.
    let formatted = format!("{value:E}");
    let (mantissa, exponent) = formatted
        .split_once('E')
        .expect("exponent formatting has an E");
    if mantissa.contains('.') {
        formatted
    } else {
        format!("{mantissa}.0E{exponent}")
    }
}

/// One solution: a term, or nothing when unbound, per projected variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Solution {
    values: Vec<Option<Term>>,
}

impl Solution {
    pub fn new(values: Vec<Option<Term>>) -> Self {
        Self { values }
    }

    /// Values in the order of [`SolutionBuilder::variables`].
    pub fn values(&self) -> &[Option<Term>] {
        &self.values
    }
}

/// Turns result rows of one [`SqlPlan`] into solutions.
#[derive(Debug, Clone)]
pub struct SolutionBuilder {
    variables: Vec<String>,
    terms: Vec<Option<TermBuilder>>,
}

#[derive(Debug, Clone)]
struct TermBuilder {
    term_type: TermType,
    value: ValueBuilder,
    datatype: Option<NamedNode>,
    language: Option<String>,
}

#[derive(Debug, Clone)]
enum ValueBuilder {
    Constant(Term),
    Column(usize),
    Template(Vec<Part>),
}

#[derive(Debug, Clone)]
enum Part {
    Literal(String),
    Column(usize),
}

impl SolutionBuilder {
    pub fn new(plan: &SqlPlan) -> Self {
        let index: HashMap<&str, usize> = plan
            .columns
            .iter()
            .enumerate()
            .map(|(i, alias)| (alias.as_str(), i))
            .collect();
        let column = |alias: &str| {
            *index
                .get(alias)
                .expect("projected terms only refer to result columns of the plan")
        };
        let terms = plan
            .projection
            .iter()
            .map(|projected| {
                let term = projected.term.as_ref()?;
                let value = match &term.value {
                    ResultValue::Constant(c) => ValueBuilder::Constant(c.clone()),
                    ResultValue::Column(alias) => ValueBuilder::Column(column(alias)),
                    ResultValue::Template(parts) => ValueBuilder::Template(
                        parts
                            .iter()
                            .map(|p| match p {
                                TemplatePart::Literal(text) => Part::Literal(text.clone()),
                                TemplatePart::Column(alias) => Part::Column(column(alias)),
                            })
                            .collect(),
                    ),
                };
                Some(TermBuilder {
                    term_type: term.term_type,
                    value,
                    datatype: term.datatype.as_deref().map(NamedNode::new_unchecked),
                    language: term.language.clone(),
                })
            })
            .collect();
        Self {
            variables: plan.projection.iter().map(|p| p.variable.clone()).collect(),
            terms,
        }
    }

    /// Projected variable names, without `?`.
    pub fn variables(&self) -> &[String] {
        &self.variables
    }

    /// Builds the solution for one row, which holds a value per entry of
    /// [`SqlPlan::columns`].
    pub fn build(&self, row: &[SqlValue]) -> Result<Solution, Error> {
        let values = self
            .terms
            .iter()
            .map(|term| match term {
                Some(term) => term.build(row),
                None => Ok(None),
            })
            .collect::<Result<_, _>>()?;
        Ok(Solution { values })
    }
}

impl TermBuilder {
    /// R2RML §11: `None` when a column it reads is `NULL`.
    fn build(&self, row: &[SqlValue]) -> Result<Option<Term>, Error> {
        let (lexical, natural) = match &self.value {
            ValueBuilder::Constant(term) => return Ok(Some(term.clone())),
            ValueBuilder::Column(i) => {
                let Some(lexical) = row[*i].natural_lexical_form() else {
                    return Ok(None);
                };
                (lexical.into_owned(), row[*i].natural_datatype())
            }
            ValueBuilder::Template(parts) => {
                let mut out = String::new();
                for part in parts {
                    match part {
                        Part::Literal(text) => out.push_str(text),
                        Part::Column(i) => {
                            let Some(value) = row[*i].natural_lexical_form() else {
                                return Ok(None);
                            };
                            // R2RML §7.3: only IRI templates percent-encode values.
                            if self.term_type == TermType::Iri {
                                out.push_str(&iri_safe(&value));
                            } else {
                                out.push_str(&value);
                            }
                        }
                    }
                }
                // R2RML §7.4: a template literal is a plain literal.
                (out, None)
            }
        };
        let term: Term = match self.term_type {
            TermType::Iri => NamedNode::new(&lexical)
                .map_err(|_| {
                    Error::InvalidTerm(format!(
                        "{lexical:?} is not an absolute IRI (R2RML data error)"
                    ))
                })?
                .into(),
            // Labels are derived from the value, so the same value is the
            // same blank node throughout the result (R2RML §11). Hex keeps
            // every value a valid label without collisions.
            TermType::BlankNode => BlankNode::new_unchecked(format!(
                "b{}",
                lexical
                    .bytes()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            ))
            .into(),
            TermType::Literal => match (&self.language, &self.datatype, natural) {
                (Some(language), _, _) => {
                    Literal::new_language_tagged_literal(lexical, language.as_str())
                        .map_err(|e| {
                            Error::InvalidTerm(format!("invalid language tag {language:?}: {e}"))
                        })?
                        .into()
                }
                (None, Some(datatype), _) => {
                    Literal::new_typed_literal(lexical, datatype.clone()).into()
                }
                (None, None, Some(natural)) => {
                    Literal::new_typed_literal(lexical, NamedNode::new_unchecked(natural)).into()
                }
                (None, None, None) => Literal::new_simple_literal(lexical).into(),
            },
        };
        Ok(Some(term))
    }
}

#[cfg(test)]
mod tests;
