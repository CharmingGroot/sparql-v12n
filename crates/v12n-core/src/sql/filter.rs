// SPDX-License-Identifier: Apache-2.0
//! SQL for `FILTER` expressions: the SPARQL operator mapping (SPARQL 1.1
//! §17.3) over the datatypes a row's terms have, with `NULL` for errors.

use oxrdf::Term;

use super::{
    ColumnType, Condition, Filter, FilterValue, Renderer, TermDatatype, TermMatch, TermSqlValue,
    ValueCases, ValueKind, XSD_STRING,
};
use crate::Error;
use crate::dialect::{Dialect, SqlType};
use crate::mapping::TermType;
use crate::solution::canonical_double;
use crate::sparql::{
    ArithOp, Cast, CompareOp, DateTimeFunction, NumericFunction, StrTest, TermTest,
};
use crate::xsd::{self, Num, Number, Value, XSD};

mod cast;
mod double;

const RDF_LANG_STRING: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#langString";

/// How much of a filter's result matters where it is rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    /// Only whether it is true: an error and false both drop the row, so
    /// either may render as the other. Directly under `WHERE` or `ON`, and
    /// under `AND` and `OR` there.
    Positive,
    /// Error, false and true must all be told apart, as under `NOT`.
    Exact,
}

/// What an operand is for the operator mapping (SPARQL 1.1 §17.3).
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Kind {
    Iri,
    BlankNode,
    /// A simple literal or `xsd:string`.
    String,
    /// A literal with this language tag, in lower case.
    Lang(String),
    Numeric(Num),
    Boolean,
    DateTime {
        zoned: bool,
    },
    /// A literal of a datatype outside the operator mapping.
    Other(String),
    /// A constant with a numeric, boolean or dateTime datatype and a
    /// lexical form not valid for it.
    IllTyped(String),
    /// A literal from a column of natural datatype whose SQL type is not
    /// known, when rendering without column types.
    Unknown,
    /// Comparing it is not supported, for this reason.
    Unsupported(String),
    /// A type error, such as `STR` of a blank node.
    Error,
}

impl Kind {
    fn is_literal(&self) -> bool {
        !matches!(self, Self::Iri | Self::BlankNode | Self::Error)
    }
}

/// How SQL writes an operand's string: the IRI, a blank node's label, or
/// a literal's lexical form.
#[derive(Debug, Clone)]
pub(super) enum Text<'v> {
    /// The term map's term (R2RML §11.2 for IRIs).
    Term(&'v super::TermSql),
    Constant(String),
    /// The datatype of a column's natural literals, which is `xsd:string`
    /// for its infinite values (see the decoder).
    NaturalDatatype {
        column: (usize, &'v str),
        datatype: String,
    },
    /// The string of a function on strings (SPARQL 1.1 §17.4.3), computed
    /// in SQL.
    Computed(&'v FilterValue),
    /// Not known without the column types.
    Unknown,
}

/// An operand of a comparison, with what its rendering needs.
#[derive(Clone)]
pub(super) struct Operand<'v> {
    pub(super) value: &'v FilterValue,
    pub(super) kind: Kind,
    /// `None` for a type error.
    pub(super) term_type: Option<TermType>,
    pub(super) text: Text<'v>,
    /// The column of a literal whose value is the column's own value.
    pub(super) column: Option<(usize, &'v str)>,
    /// A constant literal's value.
    pub(super) constant: Option<Value>,
    /// A `date`, `timestamp` or `timestamptz` column, whose infinite
    /// values are plain literals (`"infinity"`, see the decoder).
    pub(super) infinite: bool,
    /// A `timestamp` or `timestamptz` column that a function of SPARQL 1.1
    /// §17.4.5 reads: the value is an error where the column is infinite,
    /// since that is a plain literal and not an `xsd:dateTime`.
    pub(super) finite: Option<(usize, &'v str)>,
}

/// Whether the value is a number computed in SQL, by arithmetic, a
/// numeric function, `STRLEN`, a component of a dateTime or a cast to a
/// numeric datatype.
fn is_arith(value: &FilterValue) -> bool {
    matches!(
        value,
        FilterValue::Arith(..)
            | FilterValue::Negate(_)
            | FilterValue::Numeric(..)
            | FilterValue::StrLen(_)
    ) || matches!(value, FilterValue::DateTime(function, _) if function.is_numeric())
        || matches!(value, FilterValue::Cast(cast, _) if *cast != Cast::Boolean)
}

const XSD_DATE_TIME: &str = "http://www.w3.org/2001/XMLSchema#dateTime";
const XSD_DAY_TIME_DURATION: &str = "http://www.w3.org/2001/XMLSchema#dayTimeDuration";

/// What a string function sees in an argument, which must be a string
/// literal (SPARQL 1.1 §17.4.3.1.1): `String` or `Lang`, an error for any
/// other term, or the reason the argument is not supported.
fn string_argument(operand: &Operand<'_>) -> Kind {
    match &operand.kind {
        Kind::Unsupported(reason) => Kind::Unsupported(reason.clone()),
        _ if operand.infinite => Kind::Unsupported(
            "functions on strings of a date or timestamp column: its infinite values are plain \
             literals"
                .to_owned(),
        ),
        // Without column types, as if the column held strings.
        Kind::String | Kind::Unknown => Kind::String,
        Kind::Lang(tag) => Kind::Lang(tag.clone()),
        _ => Kind::Error,
    }
}

/// Whether two string arguments are compatible (SPARQL 1.1 §17.4.3.1.2):
/// both without a language tag, both with the same one, or the first with
/// one and the second without.
fn compatible(text: &Kind, search: &Kind) -> bool {
    match (text, search) {
        (Kind::String | Kind::Lang(_), Kind::String) => true,
        (Kind::Lang(a), Kind::Lang(b)) => a == b,
        _ => false,
    }
}

/// `text` as a `LIKE` pattern that matches only itself, `\` being
/// PostgreSQL's default escape character.
fn like_escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The value of `numeric` columns that is not an `xsd:decimal`: `NaN` and
/// the infinities have no decimal lexical form, so their literals are
/// ill-typed.
fn decimal_guard(dialect: &dyn Dialect, column: &str) -> String {
    format!(
        "{} NOT IN ('NaN', 'Infinity', '-Infinity')",
        dialect.cast(column, &SqlType::Text)
    )
}

impl<'a> Renderer<'a> {
    pub(super) fn filter(&mut self, filter: &Filter, mode: Mode) -> Result<(), Error> {
        match filter {
            Filter::Constant(Some(true)) => self.sql.push_str("true"),
            Filter::Constant(Some(false)) => self.sql.push_str("false"),
            Filter::Constant(None) => self.typed_null(&SqlType::Boolean),
            Filter::And(a, b) | Filter::Or(a, b) => {
                let op = if matches!(filter, Filter::And(..)) {
                    " AND "
                } else {
                    " OR "
                };
                self.sql.push('(');
                self.filter(a, mode)?;
                self.sql.push_str(op);
                self.filter(b, mode)?;
                self.sql.push(')');
            }
            Filter::Not(a) => {
                self.sql.push_str("NOT (");
                self.filter(a, Mode::Exact)?;
                self.sql.push(')');
            }
            Filter::Cases(cases) => {
                self.sql.push_str("CASE");
                for (guards, filter) in cases {
                    self.sql.push_str(" WHEN ");
                    if guards.is_empty() {
                        self.sql.push_str("true");
                    } else {
                        let guards: Vec<&Condition> = guards.iter().collect();
                        self.conditions(&guards)?;
                    }
                    self.sql.push_str(" THEN ");
                    self.filter(filter, mode)?;
                }
                self.sql.push_str(" END");
            }
            Filter::Any(conditions) => {
                if conditions.is_empty() {
                    self.sql.push_str("false");
                    return Ok(());
                }
                self.sql.push('(');
                for (i, condition) in conditions.iter().enumerate() {
                    if i > 0 {
                        self.sql.push_str(" OR ");
                    }
                    self.conditions(&[condition])?;
                }
                self.sql.push(')');
            }
            Filter::Compare {
                op,
                left,
                right,
                same_term,
            } => self.compare(*op, left, right, same_term, mode)?,
            Filter::SameTerm {
                left,
                right,
                same_term,
            } => {
                // As in a triple pattern: a database adapter keys the
                // comparisons that `=` alone would get wrong, such as a
                // zero constant against `-0` (`TermKey`).
                match same_term {
                    Some(same_term) => self.term_match(same_term, mode)?,
                    None => {
                        let (l, r) = (self.operand(left), self.operand(right));
                        if [left, right]
                            .iter()
                            .any(|v| matches!(v, FilterValue::Cast(..)))
                        {
                            return Err(unsupported(
                                "sameTerm of a cast, whose lexical form SQL does not compute yet",
                            ));
                        }
                        if is_arith(left) || is_arith(right) {
                            return Err(unsupported(
                                "sameTerm of an arithmetic result, whose lexical form SQL does \
                                 not compute yet",
                            ));
                        }
                        if [left, right].contains(&&FilterValue::Now) {
                            return Err(unsupported(
                                "sameTerm of NOW, whose lexical form SQL does not compute yet",
                            ));
                        }
                        self.same_strings(&l, &r)?
                    }
                }
            }
            Filter::Regex { text, pattern } => self.regex(text, pattern)?,
            Filter::StrTest { test, text, search } => self.str_test(*test, text, search)?,
            Filter::Test(test, value) => {
                let operand = self.operand(value);
                if let Kind::Unsupported(reason) = &operand.kind {
                    return Err(unsupported(reason));
                }
                let Some(term_type) = operand.term_type else {
                    self.typed_null(&SqlType::Boolean);
                    return Ok(());
                };
                let holds = match test {
                    TermTest::IsIri => term_type == TermType::Iri,
                    TermTest::IsBlank => term_type == TermType::BlankNode,
                    TermTest::IsLiteral => term_type == TermType::Literal,
                };
                let holds = if holds { "true" } else { "false" };
                // A cast is an error where the lexical form is not valid.
                if let FilterValue::Cast(..) = value {
                    self.sql.push_str("CASE WHEN ");
                    self.no_error(value)?;
                    self.sql.push_str(&format!(" THEN {holds} END"));
                    return Ok(());
                }
                match operand.finite {
                    Some((table, name)) => self.sql.push_str(&format!(
                        "CASE WHEN {} THEN {holds} END",
                        self.dialect.is_finite(&self.column(table, name))
                    )),
                    None => self.sql.push_str(holds),
                }
            }
            Filter::Ebv(value) => self.ebv(value)?,
            Filter::NoError(value) => self.no_error(value)?,
        }
        Ok(())
    }

    /// Whether `value` is not an error. Only arithmetic and the functions
    /// of dateTimes are errors that depend on the row, such as a division
    /// by zero or an infinite timestamp.
    fn no_error(&mut self, value: &FilterValue) -> Result<(), Error> {
        let operand = self.operand(value);
        match operand.kind {
            Kind::Unsupported(reason) => return Err(unsupported(&reason)),
            Kind::Error => self.sql.push_str("false"),
            Kind::Numeric(class) if is_arith(value) => {
                self.sql.push('(');
                self.class_value(value, class)?;
                self.sql.push_str(" IS NOT NULL)");
            }
            Kind::Boolean if matches!(value, FilterValue::Cast(..)) => {
                self.sql.push('(');
                self.boolean_value(value)?;
                self.sql.push_str(" IS NOT NULL)");
            }
            _ => match operand.finite {
                Some((table, name)) => self
                    .sql
                    .push_str(&self.dialect.is_finite(&self.column(table, name))),
                None => self.sql.push_str("true"),
            },
        }
        Ok(())
    }

    /// The operand as the operator mapping sees it.
    pub(super) fn operand<'v>(&self, value: &'v FilterValue) -> Operand<'v> {
        let mut operand = Operand {
            value,
            kind: Kind::Unknown,
            term_type: Some(TermType::Literal),
            text: Text::Unknown,
            column: None,
            constant: None,
            infinite: false,
            finite: None,
        };
        match value {
            FilterValue::Constant(Term::NamedNode(iri)) => {
                operand.kind = Kind::Iri;
                operand.term_type = Some(TermType::Iri);
                operand.text = Text::Constant(iri.as_str().to_owned());
            }
            FilterValue::Constant(Term::BlankNode(_)) => {
                operand.kind = Kind::BlankNode;
                operand.term_type = Some(TermType::BlankNode);
            }
            FilterValue::Constant(Term::Literal(literal)) => {
                operand.text = Text::Constant(literal.value().to_owned());
                match literal.language() {
                    Some(language) => operand.kind = Kind::Lang(language.to_ascii_lowercase()),
                    None => {
                        let datatype = literal.datatype().as_str();
                        let value = xsd::value(literal.value(), datatype);
                        operand.kind = kind_of(&value, datatype);
                        operand.constant = Some(value);
                    }
                }
            }
            // Quoted triples exist only with oxrdf's `rdf-12` feature.
            #[allow(unreachable_patterns)]
            FilterValue::Constant(_) => {
                operand.kind = Kind::Unsupported("quoted triples in FILTER".to_owned())
            }
            FilterValue::Str(inner) | FilterValue::Lang(inner) | FilterValue::Datatype(inner) => {
                return self.derived(value, self.operand(inner));
            }
            FilterValue::Arith(op, a, b) => {
                return self.arith(value, Some(*op), &[self.operand(a), self.operand(b)]);
            }
            FilterValue::Negate(a) | FilterValue::Numeric(_, a) => {
                return self.arith(value, None, &[self.operand(a)]);
            }
            FilterValue::DateTime(function, a) => {
                return self.date_time(value, *function, self.operand(a));
            }
            FilterValue::Cast(cast, a) => return self.cast(value, *cast, self.operand(a)),
            FilterValue::Now => {
                let now = self.now();
                operand.kind = Kind::DateTime { zoned: true };
                operand.text = Text::Constant(now.canonical.clone());
                operand.constant = Some(Value::DateTime(now));
            }
            FilterValue::StrLen(a) => {
                return match string_argument(&self.operand(a)) {
                    Kind::String | Kind::Lang(_) => Operand {
                        kind: Kind::Numeric(Num::Integer),
                        ..operand
                    },
                    kind => self.string_error(value, kind),
                };
            }
            FilterValue::Substr { text: a, .. }
            | FilterValue::UCase(a)
            | FilterValue::LCase(a)
            | FilterValue::Replace { text: a, .. } => {
                return match string_argument(&self.operand(a)) {
                    kind @ (Kind::String | Kind::Lang(_)) => Operand {
                        kind,
                        text: Text::Computed(value),
                        ..operand
                    },
                    kind => self.string_error(value, kind),
                };
            }
            // A simple literal (SPARQL 1.1 §17.4.3.11).
            FilterValue::EncodeForUri(a) => {
                return match string_argument(&self.operand(a)) {
                    Kind::String | Kind::Lang(_) => Operand {
                        kind: Kind::String,
                        text: Text::Computed(value),
                        ..operand
                    },
                    kind => self.string_error(value, kind),
                };
            }
            // The kind of the first argument; the cases of the value give
            // the simple literal "" where the second is not in the first.
            FilterValue::StrBefore(a, b) | FilterValue::StrAfter(a, b) => {
                let (text, search) = (
                    string_argument(&self.operand(a)),
                    string_argument(&self.operand(b)),
                );
                for kind in [&text, &search] {
                    if let Kind::Unsupported(_) = kind {
                        return self.string_error(value, kind.clone());
                    }
                }
                if !compatible(&text, &search) {
                    return self.string_error(value, Kind::Error);
                }
                return Operand {
                    kind: text,
                    text: Text::Computed(value),
                    ..operand
                };
            }
            // A language tag where all arguments have the same one; a
            // simple literal otherwise (SPARQL 1.1 §17.4.3.12).
            FilterValue::Concat(args) => {
                let mut kinds = Vec::with_capacity(args.len());
                for arg in args {
                    match string_argument(&self.operand(arg)) {
                        kind @ (Kind::String | Kind::Lang(_)) => kinds.push(kind),
                        kind => return self.string_error(value, kind),
                    }
                }
                operand.kind = match kinds.split_first() {
                    Some((first @ Kind::Lang(_), rest)) if rest.iter().all(|k| k == first) => {
                        first.clone()
                    }
                    _ => Kind::String,
                };
                operand.text = if args.is_empty() {
                    Text::Constant(String::new())
                } else {
                    Text::Computed(value)
                };
            }
            FilterValue::Term(term) => {
                operand.term_type = Some(term.term_type);
                operand.text = Text::Term(&term.term);
                let column = match &term.term.value {
                    TermSqlValue::Column { table, name } => Some((*table, name.as_str())),
                    _ => None,
                };
                let ty = column.and_then(|(table, name)| self.column_type(table, name));
                if let Some(ty) = ty {
                    operand.infinite =
                        matches!(ty.name.as_str(), "date" | "timestamp" | "timestamptz");
                }
                operand.kind = match (term.term_type, &term.language, &term.term.datatype) {
                    (TermType::Iri, ..) => Kind::Iri,
                    (TermType::BlankNode, ..) => Kind::BlankNode,
                    (TermType::Literal, Some(language), _) => {
                        Kind::Lang(language.to_ascii_lowercase())
                    }
                    (TermType::Literal, None, Some(TermDatatype::Natural)) => match ty {
                        Some(ty) => natural_kind(ty),
                        None => Kind::Unknown,
                    },
                    (TermType::Literal, None, Some(TermDatatype::Iri(datatype))) => {
                        match (column, ty) {
                            // R2RML §10.2: a column with the natural datatype of
                            // its SQL type as rr:datatype gives the same literal.
                            (Some(_), Some(ty))
                                if ty.natural_datatype.as_deref().unwrap_or(XSD_STRING)
                                    == datatype =>
                            {
                                natural_kind(ty)
                            }
                            // Without column types, as if it were.
                            (Some(_), None) if mapped(datatype) => assumed_kind(datatype),
                            _ if datatype == XSD_STRING => Kind::String,
                            _ if mapped(datatype) => Kind::Unsupported(format!(
                                "comparing literals with rr:datatype <{datatype}> built from \
                                 values of another type, whose lexical forms SQL cannot read"
                            )),
                            _ => Kind::Other(datatype.clone()),
                        }
                    }
                    (TermType::Literal, None, None) => Kind::Unknown,
                };
                if matches!(
                    operand.kind,
                    Kind::Numeric(_) | Kind::Boolean | Kind::DateTime { .. } | Kind::Unknown
                ) {
                    operand.column = column;
                }
            }
        }
        operand
    }

    /// A function on strings whose argument is `kind`, which is not a
    /// string literal.
    fn string_error<'v>(&self, value: &'v FilterValue, kind: Kind) -> Operand<'v> {
        Operand {
            value,
            kind: match kind {
                Kind::Unsupported(reason) => Kind::Unsupported(reason),
                _ => Kind::Error,
            },
            term_type: None,
            text: Text::Unknown,
            column: None,
            constant: None,
            infinite: false,
            finite: None,
        }
    }

    /// `STR`, `LANG` or `DATATYPE` (SPARQL 1.1 §17.4.2) of `inner`.
    fn derived<'v>(&self, value: &'v FilterValue, inner: Operand<'v>) -> Operand<'v> {
        let mut operand = Operand {
            value,
            kind: Kind::String,
            term_type: Some(TermType::Literal),
            text: Text::Unknown,
            column: None,
            constant: None,
            infinite: false,
            finite: None,
        };
        let error = Operand {
            kind: Kind::Error,
            term_type: None,
            ..operand.clone()
        };
        if let FilterValue::Cast(..) = inner.value
            && inner.kind != Kind::Error
        {
            return Operand {
                kind: match inner.kind {
                    Kind::Unsupported(reason) => Kind::Unsupported(reason),
                    _ => Kind::Unsupported("STR, LANG and DATATYPE of a cast".to_owned()),
                },
                ..operand
            };
        }
        if is_arith(inner.value) && inner.kind != Kind::Error {
            return Operand {
                kind: Kind::Unsupported(
                    "STR, LANG and DATATYPE of an arithmetic result".to_owned(),
                ),
                ..operand
            };
        }
        operand.finite = inner.finite;
        let literal = inner.term_type == Some(TermType::Literal);
        match value {
            FilterValue::Str(_) if literal || inner.term_type == Some(TermType::Iri) => {
                operand.text = inner.text;
            }
            FilterValue::Lang(_) if literal => {
                operand.text = Text::Constant(match &inner.kind {
                    Kind::Lang(tag) => tag.clone(),
                    _ => String::new(),
                });
            }
            FilterValue::Datatype(_) if literal => {
                operand.kind = Kind::Iri;
                operand.term_type = Some(TermType::Iri);
                operand.text = self.datatype(&inner);
            }
            _ => return error,
        }
        operand
    }

    /// `function` (SPARQL 1.1 §17.4.5) of `inner`, which must be an
    /// `xsd:dateTime` from a column or `NOW`; constants are computed
    /// before rendering. The engine writes `timestamptz` values in UTC
    /// with `Z`, so their components are those in UTC, their `TZ` is `Z`
    /// and their `TIMEZONE` is `PT0S`. A `timestamp` value has no time
    /// zone: its `TZ` is empty and its `TIMEZONE` an error.
    fn date_time<'v>(
        &self,
        value: &'v FilterValue,
        function: DateTimeFunction,
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
        let zoned = match (&inner.kind, inner.value) {
            (Kind::Unsupported(reason), _) => {
                operand.kind = Kind::Unsupported(reason.clone());
                return operand;
            }
            (Kind::DateTime { zoned }, FilterValue::Term(_) | FilterValue::Now) => *zoned,
            // Without column types, as if the column were a timestamp.
            (Kind::Unknown, FilterValue::Term(_)) => false,
            (Kind::Other(datatype), _) if xsd::is_date_or_time(datatype) => {
                operand.kind = Kind::Unsupported(xsd::DATE_OR_TIME_ARGUMENT.to_owned());
                return operand;
            }
            _ => return operand,
        };
        if inner.infinite {
            operand.finite = inner.column;
        }
        operand.term_type = Some(TermType::Literal);
        operand.kind = match function {
            DateTimeFunction::Seconds => Kind::Numeric(Num::Decimal),
            DateTimeFunction::Tz => {
                operand.text = Text::Constant(if zoned { "Z" } else { "" }.to_owned());
                Kind::String
            }
            DateTimeFunction::Timezone if zoned => {
                operand.text = Text::Constant("PT0S".to_owned());
                Kind::Other(XSD_DAY_TIME_DURATION.to_owned())
            }
            DateTimeFunction::Timezone => {
                return Operand {
                    kind: Kind::Error,
                    term_type: None,
                    finite: None,
                    ..operand
                };
            }
            _ => Kind::Numeric(Num::Integer),
        };
        operand
    }

    /// A component of a dateTime (SPARQL 1.1 §17.4.5) as `numeric`. A
    /// `timestamptz` is read in UTC, as the engine writes it.
    fn date_time_number(
        &mut self,
        function: DateTimeFunction,
        value: &FilterValue,
    ) -> Result<(), Error> {
        let inner = self.operand(value);
        let zoned = match inner.kind {
            Kind::DateTime { zoned } => zoned,
            Kind::Unknown => false,
            _ => {
                self.typed_null(&SqlType::Numeric);
                return Ok(());
            }
        };
        let start = self.sql.len();
        if zoned {
            self.sql.push('(');
        }
        self.typed_value(&inner, &SqlType::TimestampTz)?;
        if zoned {
            self.sql.push_str(" AT TIME ZONE 'UTC')");
        }
        let x = self.sql.split_off(start);
        let field = match function {
            DateTimeFunction::Year => "year",
            DateTimeFunction::Month => "month",
            DateTimeFunction::Day => "day",
            DateTimeFunction::Hours => "hour",
            DateTimeFunction::Minutes => "minute",
            DateTimeFunction::Seconds => "second",
            DateTimeFunction::Timezone | DateTimeFunction::Tz => {
                unreachable!("TIMEZONE and TZ are not numbers")
            }
        };
        // PostgreSQL 13 extracts a float8, which is exact for these
        // components; 14 and later extract a numeric.
        let mut sql = self
            .dialect
            .cast(&format!("extract({field} FROM {x})"), &SqlType::Numeric);
        // PostgreSQL calls the year before 1 the year -1, which the engine
        // writes as 0000 (XML Schema 1.1 Part 2 §3.3.7).
        if function == DateTimeFunction::Year {
            let first = self
                .dialect
                .cast("'0001-01-01T00:00:00'", &SqlType::Timestamp);
            sql = format!("CASE WHEN {x} < {first} THEN {sql} + 1 ELSE {sql} END");
        }
        if inner.infinite
            && let Some((table, name)) = inner.column
        {
            sql = format!(
                "CASE WHEN {} THEN {sql} END",
                self.dialect.is_finite(&self.column(table, name))
            );
        }
        self.sql.push_str(&sql);
        Ok(())
    }

    /// The result of `op` over `operands`, or of negation without `op`:
    /// a number whose class follows XPath's type promotion (an integer
    /// divided by an integer is a decimal, op:numeric-divide; anything
    /// with a double is a double), and a type error when an operand is not
    /// a number (SPARQL 1.1 §17.3).
    fn arith<'v>(
        &self,
        value: &'v FilterValue,
        op: Option<ArithOp>,
        operands: &[Operand<'v>],
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
        let mut class = Num::Integer;
        for o in operands {
            match &o.kind {
                Kind::Numeric(c) => class = class.max(*c),
                // Without column types, as if the column held integers,
                // which shows the SQL of the common case.
                Kind::Unknown => {}
                Kind::Unsupported(reason) => {
                    operand.kind = Kind::Unsupported(reason.clone());
                    return operand;
                }
                _ => return operand,
            }
        }
        if op == Some(ArithOp::Divide) && class < Num::Double {
            class = Num::Decimal;
        }
        operand.kind = Kind::Numeric(class);
        operand.term_type = Some(TermType::Literal);
        operand
    }

    /// An arithmetic expression of integers and decimals, or one of its
    /// operands, as `numeric`.
    /// Integers are computed in `numeric` too, since `xsd:integer` has no
    /// bound and `int8` would overflow. A division by zero is an error
    /// (op:numeric-divide), so its divisor is `NULLIF(…, 0)`; an operand
    /// that is an error is `NULL`, and so is the result.
    fn numeric_value(&mut self, value: &FilterValue) -> Result<(), Error> {
        match value {
            FilterValue::Arith(op, a, b) => {
                self.sql.push('(');
                self.numeric_value(a)?;
                self.sql.push_str(&format!(" {} ", op.sql()));
                if *op == ArithOp::Divide {
                    self.sql.push_str("NULLIF(");
                    self.numeric_value(b)?;
                    self.sql.push_str(", 0)");
                } else {
                    self.numeric_value(b)?;
                }
                self.sql.push(')');
            }
            FilterValue::Negate(a) => {
                self.sql.push_str("(-");
                self.numeric_value(a)?;
                self.sql.push(')');
            }
            FilterValue::Numeric(function, a) => {
                // An integer is its own ceiling, floor and rounding
                // (fn:ceiling, fn:floor, fn:round on xs:integer).
                let integer = matches!(
                    self.operand(a).kind,
                    Kind::Numeric(Num::Integer) | Kind::Unknown
                );
                match function {
                    NumericFunction::Abs => self.sql.push_str("abs("),
                    _ if integer => return self.numeric_value(a),
                    NumericFunction::Ceil => self.sql.push_str("ceil("),
                    NumericFunction::Floor | NumericFunction::Round => self.sql.push_str("floor("),
                }
                self.numeric_value(a)?;
                // fn:round takes the integer nearest to the decimal and, of
                // two, the one towards positive infinity, which `numeric`
                // computes exactly as floor(x + 0.5). PostgreSQL's
                // round(numeric) rounds halves away from zero instead.
                if *function == NumericFunction::Round {
                    self.sql.push_str(" + 0.5");
                }
                self.sql.push(')');
            }
            FilterValue::DateTime(function, a) if self.operand(value).kind != Kind::Error => {
                self.date_time_number(*function, a)?;
            }
            FilterValue::Cast(cast, a) => match self.operand(value).kind {
                Kind::Unsupported(reason) => return Err(unsupported(&reason)),
                Kind::Numeric(_) => self.cast_number(*cast, a)?,
                _ => self.typed_null(&SqlType::Numeric),
            },
            // Characters are code points in a UTF-8 database.
            FilterValue::StrLen(a) => {
                let operand = self.operand(a);
                if !matches!(string_argument(&operand), Kind::String | Kind::Lang(_)) {
                    self.typed_null(&SqlType::Numeric);
                    return Ok(());
                }
                self.utf8_strings()?;
                let start = self.sql.len();
                self.text(&operand)?;
                let text = self.sql.split_off(start);
                let length = self.dialect.char_length(&text);
                self.sql
                    .push_str(&self.dialect.cast(&length, &SqlType::Numeric));
            }
            _ => {
                let operand = self.operand(value);
                if let Some(Value::Numeric(n)) = &operand.constant {
                    let decimal = n
                        .decimal
                        .clone()
                        .expect("integer and decimal constants have a decimal value");
                    self.cast_rendered(&SqlType::Numeric, |this| {
                        this.expr(&super::SqlExpr::Param(decimal.into()))
                    });
                    return Ok(());
                }
                let Some((table, name)) = operand.column else {
                    // A type error: an operand that is not a number.
                    self.typed_null(&SqlType::Numeric);
                    return Ok(());
                };
                let column = self.column(table, name);
                match operand.kind {
                    // NaN and the infinities of a numeric column are not
                    // xsd:decimal values: an error.
                    Kind::Numeric(Num::Decimal) => self.sql.push_str(&format!(
                        "CASE WHEN {} THEN {column} END",
                        decimal_guard(self.dialect, &column)
                    )),
                    _ => self
                        .sql
                        .push_str(&self.dialect.cast(&column, &SqlType::Numeric)),
                }
            }
        }
        Ok(())
    }

    /// A computed value as a number of class `kind` where it is one, and
    /// `NULL` where it is another class or an error, which leaves the
    /// variable unbound (SPARQL 1.1 §18.5, Extend). The cases render as
    /// `CASE`, so the first whose guards hold gives the value.
    pub(super) fn value(&mut self, cases: &ValueCases, kind: ValueKind) -> Result<(), Error> {
        let class = match kind {
            ValueKind::Integer => Num::Integer,
            ValueKind::Decimal => Num::Decimal,
            ValueKind::Double => Num::Double,
        };
        let ty = if class == Num::Double {
            SqlType::Double
        } else {
            SqlType::Numeric
        };
        let mut of_class = Vec::with_capacity(cases.len());
        for (_, value) in cases {
            of_class.push(match self.operand(value).kind {
                Kind::Numeric(c) => c == class,
                Kind::Unsupported(reason) => return Err(unsupported(&reason)),
                _ => false,
            });
        }
        if !of_class.contains(&true) {
            self.typed_null(&ty);
            return Ok(());
        }
        if let [(guards, value)] = cases.as_slice()
            && guards.is_empty()
        {
            return self.class_value(value, class);
        }
        self.sql.push_str("CASE");
        for ((guards, value), of_class) in cases.iter().zip(of_class) {
            self.sql.push_str(" WHEN ");
            if guards.is_empty() {
                self.sql.push_str("true");
            } else {
                let guards: Vec<&Condition> = guards.iter().collect();
                self.conditions(&guards)?;
            }
            self.sql.push_str(" THEN ");
            if of_class {
                self.class_value(value, class)?;
            } else {
                self.typed_null(&ty);
            }
        }
        self.sql.push_str(" END");
        Ok(())
    }

    /// A computed number of class `class`, as `float8` for a double and
    /// as `numeric` otherwise.
    fn class_value(&mut self, value: &FilterValue, class: Num) -> Result<(), Error> {
        if class == Num::Double {
            self.double_value(value)
        } else {
            self.numeric_value(value)
        }
    }

    /// The datatype IRI of a literal operand.
    pub(super) fn datatype<'v>(&self, literal: &Operand<'v>) -> Text<'v> {
        if let Kind::Lang(_) = literal.kind {
            return Text::Constant(RDF_LANG_STRING.to_owned());
        }
        match literal.value {
            FilterValue::Constant(Term::Literal(l)) => {
                Text::Constant(l.datatype().as_str().to_owned())
            }
            FilterValue::Term(term) => match (&term.term.datatype, &term.term.value) {
                (Some(TermDatatype::Iri(datatype)), _) => Text::Constant(datatype.clone()),
                (Some(TermDatatype::Natural), TermSqlValue::Column { table, name }) => {
                    match self.column_type(*table, name) {
                        Some(ty) => {
                            let datatype = ty
                                .natural_datatype
                                .clone()
                                .unwrap_or_else(|| XSD_STRING.to_owned());
                            if literal.infinite {
                                Text::NaturalDatatype {
                                    column: (*table, name.as_str()),
                                    datatype,
                                }
                            } else {
                                Text::Constant(datatype)
                            }
                        }
                        None => Text::Unknown,
                    }
                }
                _ => Text::Unknown,
            },
            FilterValue::Now => Text::Constant(XSD_DATE_TIME.to_owned()),
            FilterValue::DateTime(DateTimeFunction::Timezone, _) => {
                Text::Constant(XSD_DAY_TIME_DURATION.to_owned())
            }
            // The results of STR, LANG and TZ are simple literals.
            _ => Text::Constant(XSD_STRING.to_owned()),
        }
    }

    fn compare(
        &mut self,
        op: CompareOp,
        left: &FilterValue,
        right: &FilterValue,
        same_term: &Option<TermMatch>,
        mode: Mode,
    ) -> Result<(), Error> {
        let (mut l, mut r) = (self.operand(left), self.operand(right));
        if l.kind == Kind::Error || r.kind == Kind::Error {
            self.typed_null(&SqlType::Boolean);
            return Ok(());
        }
        // Without column types: as if the column had the other side's
        // datatype, which shows the SQL of the common case.
        match (&l.kind, &r.kind) {
            (Kind::Unknown, Kind::Unknown) => {
                l.kind = Kind::String;
                r.kind = Kind::String;
            }
            (Kind::Unknown, other) => l.kind = assumed(other),
            (other, Kind::Unknown) => r.kind = assumed(other),
            _ => {}
        }
        for side in [&l, &r] {
            if let Kind::Unsupported(reason) = &side.kind {
                return Err(unsupported(reason));
            }
        }
        // A number compared with another kind of literal is a type error
        // (SPARQL 1.1 §17.3); an arithmetic result has no lexical form to
        // compare as a term.
        if (is_arith(l.value) || is_arith(r.value))
            && !matches!((&l.kind, &r.kind), (Kind::Numeric(_), Kind::Numeric(_)))
            && l.kind.is_literal()
            && r.kind.is_literal()
        {
            self.typed_null(&SqlType::Boolean);
            return Ok(());
        }
        match (&l.kind, &r.kind) {
            (Kind::Numeric(_), Kind::Numeric(_)) => self.numeric(op, &l, &r, mode),
            (Kind::String, Kind::String) if op == CompareOp::Equal => {
                self.same_term_or_strings(same_term, &l, &r, mode)
            }
            (Kind::String, Kind::String) => {
                if self.options.non_utf8 {
                    return Err(unsupported(
                        "ordering strings in a database whose encoding is not UTF-8, where \
                         byte order is not code point order",
                    ));
                }
                self.text(&l)?;
                self.sql.push_str(self.dialect.code_point_collation());
                self.sql.push_str(&format!(" {} ", op.sql()));
                self.text(&r)
            }
            (Kind::Boolean, Kind::Boolean) => {
                self.typed_value(&l, &SqlType::Boolean)?;
                self.sql.push_str(&format!(" {} ", op.sql()));
                self.typed_value(&r, &SqlType::Boolean)
            }
            (Kind::DateTime { zoned: a }, Kind::DateTime { zoned: b }) if a == b => {
                let ty = if *a {
                    SqlType::TimestampTz
                } else {
                    SqlType::Timestamp
                };
                let dialect = self.dialect;
                self.guarded(
                    mode,
                    &[&l, &r],
                    op,
                    |o| o.infinite,
                    |c| dialect.is_finite(c),
                    |this| {
                        this.typed_value(&l, &ty)?;
                        this.sql.push_str(&format!(" {} ", op.sql()));
                        this.typed_value(&r, &ty)
                    },
                )
            }
            // XPath compares a dateTime with a time zone and one without
            // through the implicit time zone of the dynamic context, which
            // SPARQL leaves to the implementation (XPath F&O §10.4).
            (Kind::DateTime { .. }, Kind::DateTime { .. }) => Err(unsupported(
                "comparing an xsd:dateTime with a time zone and one without, whose order \
                 depends on an implicit time zone",
            )),
            _ => self.term_compare(op, &l, &r, same_term, mode),
        }
    }

    /// Operands outside the numeric, string, boolean and dateTime rows of
    /// the operator mapping: `=` is RDFterm-equal, which is a type error
    /// for two literals that are not the same term (SPARQL 1.1 §17.4.1.7),
    /// and the other operators are type errors.
    fn term_compare(
        &mut self,
        op: CompareOp,
        l: &Operand<'_>,
        r: &Operand<'_>,
        same_term: &Option<TermMatch>,
        mode: Mode,
    ) -> Result<(), Error> {
        if (l.infinite && r.kind == Kind::String) || (r.infinite && l.kind == Kind::String) {
            return Err(unsupported(
                "comparing a date or timestamp column with a string: its infinite values are \
                 plain literals",
            ));
        }
        let (a, b) = (&l.kind, &r.kind);
        if !a.is_literal() || !b.is_literal() {
            let sql = match (op, a, b) {
                (CompareOp::Equal, Kind::Iri, Kind::Iri)
                | (CompareOp::Equal, Kind::BlankNode, Kind::BlankNode) => {
                    return self.same_term_or_strings(same_term, l, r, mode);
                }
                (CompareOp::Equal, ..) => "false".to_owned(),
                _ => self.dialect.cast("NULL", &SqlType::Boolean),
            };
            self.sql.push_str(&sql);
            return Ok(());
        }
        let ill_typed = matches!(a, Kind::IllTyped(_)) || matches!(b, Kind::IllTyped(_));
        let same_type = match (a, b) {
            (Kind::Lang(_), Kind::Lang(_)) => true,
            (Kind::Other(x), Kind::Other(y)) => x == y,
            _ => false,
        };
        match op {
            // True for the same term, an error otherwise. Engines that
            // compare the values of more datatypes (an extension SPARQL
            // allows, §17.3.1) turn some of these errors into false, which
            // is told apart only under negation: accepted where it is not.
            CompareOp::Equal if ill_typed => self.only_same_term(same_term, l, r, mode),
            CompareOp::Equal
                if same_type && mode == Mode::Positive && matches!(a, Kind::Lang(_)) =>
            {
                self.same_term_or_strings(same_term, l, r, mode)
            }
            _ if same_type => Err(unsupported(&format!(
                "comparing two {} literals with {}{}: SPARQL 1.1 only finds the same RDF term \
                 equal and makes anything else an error, while engines that compare their \
                 values differ",
                match a {
                    Kind::Other(datatype) => format!("<{datatype}>"),
                    _ => "language-tagged".to_owned(),
                },
                op.sql(),
                if mode == Mode::Exact && op == CompareOp::Equal {
                    " under negation"
                } else {
                    ""
                }
            ))),
            _ => {
                self.typed_null(&SqlType::Boolean);
                Ok(())
            }
        }
    }

    /// RDFterm-equal of two literals: true for the same term, an error
    /// otherwise.
    fn only_same_term(
        &mut self,
        same_term: &Option<TermMatch>,
        l: &Operand<'_>,
        r: &Operand<'_>,
        mode: Mode,
    ) -> Result<(), Error> {
        match mode {
            Mode::Positive => self.same_term_or_strings(same_term, l, r, mode),
            Mode::Exact => {
                self.sql.push_str("CASE WHEN ");
                self.same_term_or_strings(same_term, l, r, Mode::Positive)?;
                self.sql.push_str(" THEN true END");
                Ok(())
            }
        }
    }

    fn same_term_or_strings(
        &mut self,
        same_term: &Option<TermMatch>,
        l: &Operand<'_>,
        r: &Operand<'_>,
        mode: Mode,
    ) -> Result<(), Error> {
        match same_term {
            Some(same_term) => self.term_match(same_term, mode),
            None => self.same_strings(l, r),
        }
    }

    /// Whether two operands are the same RDF term, by kind and string:
    /// for the results of `STR`, `LANG` and `DATATYPE`, which are simple
    /// literals and IRIs. Never an error.
    fn same_strings(&mut self, l: &Operand<'_>, r: &Operand<'_>) -> Result<(), Error> {
        let same_kind = match (&l.kind, &r.kind) {
            (Kind::Error, _) | (_, Kind::Error) => {
                self.typed_null(&SqlType::Boolean);
                return Ok(());
            }
            (Kind::Lang(a), Kind::Lang(b)) | (Kind::Other(a), Kind::Other(b)) => a == b,
            (Kind::Iri, Kind::Iri)
            | (Kind::BlankNode, Kind::BlankNode)
            | (Kind::String | Kind::Unknown, Kind::String | Kind::Unknown) => true,
            _ => false,
        };
        if !same_kind {
            self.sql.push_str("false");
            return Ok(());
        }
        self.sql.push('(');
        self.text(l)?;
        self.sql.push_str(" = ");
        self.text(r)?;
        self.sql.push(')');
        Ok(())
    }

    /// `REGEX` over a string literal (SPARQL 1.1 §17.4.3.14); anything else
    /// is a type error.
    fn regex(&mut self, text: &FilterValue, pattern: &str) -> Result<(), Error> {
        let operand = self.operand(text);
        if let Kind::Unsupported(reason) = &operand.kind {
            return Err(unsupported(reason));
        }
        if !matches!(operand.kind, Kind::String | Kind::Lang(_) | Kind::Unknown) {
            self.typed_null(&SqlType::Boolean);
            return Ok(());
        }
        if self.options.non_utf8 {
            return Err(unsupported(
                "REGEX in a database whose encoding is not UTF-8, where PostgreSQL's \
                 regular expressions do not see Unicode characters",
            ));
        }
        self.sql.push('(');
        let start = self.sql.len();
        self.text(&operand)?;
        self.sql.push_str(self.dialect.code_point_collation());
        let text = self.sql.split_off(start);
        self.cast_rendered(&SqlType::Text, |this| {
            this.expr(&super::SqlExpr::Param(pattern.into()))
        });
        let pattern = self.sql.split_off(start);
        self.sql
            .push_str(&self.dialect.regex_match(&text, &pattern));
        self.sql.push(')');
        Ok(())
    }

    /// Whether two terms are the same RDF term, never an error.
    fn term_match(&mut self, same_term: &TermMatch, mode: Mode) -> Result<(), Error> {
        match same_term {
            TermMatch::Never => self.sql.push_str("false"),
            TermMatch::When(conditions) if conditions.is_empty() => self.sql.push_str("true"),
            TermMatch::When(conditions) => {
                let conditions: Vec<&Condition> = conditions.iter().collect();
                // A constant that no value can match is bound to NULL.
                let exact = mode == Mode::Exact;
                self.sql.push_str(if exact { "coalesce((" } else { "(" });
                self.conditions(&conditions)?;
                self.sql.push_str(if exact { "), false)" } else { ")" });
            }
        }
        Ok(())
    }

    /// op:numeric-equal, op:numeric-less-than and op:numeric-greater-than
    /// with XPath's type promotion: integer to decimal to double. A
    /// constant is converted to the column's type where that gives the
    /// same result, so an index on the column can be used; otherwise the
    /// column is cast to the promoted type.
    fn numeric(
        &mut self,
        op: CompareOp,
        l: &Operand<'_>,
        r: &Operand<'_>,
        mode: Mode,
    ) -> Result<(), Error> {
        let number = |o: &Operand<'_>| match &o.constant {
            Some(Value::Numeric(n)) => Some(n.clone()),
            _ => None,
        };
        let class = |o: &Operand<'_>| match o.kind {
            Kind::Numeric(class) => class,
            _ => unreachable!("numeric operands"),
        };
        let (ln, rn) = (number(l), number(r));
        // Every comparison with NaN is false (XPath op:numeric-equal and
        // op:numeric-less-than), while PostgreSQL orders NaN last.
        if [&ln, &rn].iter().any(|n| {
            n.as_ref()
                .is_some_and(|n| n.class == Num::Double && n.double.is_nan())
        }) {
            self.sql.push_str("false");
            return Ok(());
        }
        let promoted = class(l).max(class(r));
        if promoted == Num::Double && (is_arith(l.value) || is_arith(r.value)) {
            return self.double_compare(op, l, r);
        }
        let fits = |n: &Option<Number>| n.as_ref().is_none_or(|n| n.int8().is_some());
        let ty = match (&ln, &rn) {
            // An integer column and an integral constant compare as
            // integers, which is exact.
            (None, Some(_)) | (Some(_), None)
                if promoted <= Num::Decimal
                    && class(if ln.is_none() { l } else { r }) == Num::Integer
                    && fits(&ln)
                    && fits(&rn) =>
            {
                Num::Integer
            }
            _ if promoted == Num::Integer && !(fits(&ln) && fits(&rn)) => Num::Decimal,
            _ => promoted,
        };
        let constant_other = |o: &Operand<'_>| o.constant.is_some();
        let mut decimal_columns = Vec::new();
        let mut float_columns = Vec::new();
        for (side, other) in [(l, r), (r, l)] {
            if let Some((table, name)) = side.column {
                let column = self.column(table, name);
                match class(side) {
                    Num::Decimal
                        if !(op == CompareOp::Equal
                            && mode == Mode::Positive
                            && constant_other(other)) =>
                    {
                        decimal_columns.push(column)
                    }
                    Num::Double if !(op == CompareOp::Equal && constant_other(other)) => {
                        float_columns.push(column)
                    }
                    _ => {}
                }
            }
        }
        let exact_decimal = mode == Mode::Exact && !decimal_columns.is_empty();
        if exact_decimal {
            self.sql.push_str("CASE WHEN ");
            self.sql.push_str(
                &decimal_columns
                    .iter()
                    .map(|c| decimal_guard(self.dialect, c))
                    .collect::<Vec<_>>()
                    .join(" AND "),
            );
            self.sql.push_str(" THEN ");
        }
        let guarded = !float_columns.is_empty() || (!exact_decimal && !decimal_columns.is_empty());
        if guarded {
            self.sql.push('(');
        }
        self.number(l, ln.as_ref(), class(l), ty)?;
        self.sql.push_str(&format!(" {} ", op.sql()));
        self.number(r, rn.as_ref(), class(r), ty)?;
        let nan = self.dialect.cast("'NaN'", &SqlType::Double);
        for column in &float_columns {
            self.sql.push_str(&format!(" AND {column} <> {nan}"));
        }
        if !exact_decimal {
            for column in &decimal_columns {
                self.sql
                    .push_str(&format!(" AND {}", decimal_guard(self.dialect, column)));
            }
        }
        if guarded {
            self.sql.push(')');
        }
        if exact_decimal {
            // Two NaN or infinite values are the same ill-typed literal,
            // which RDFterm-equal finds equal.
            if op == CompareOp::Equal && decimal_columns.len() == 2 {
                self.sql.push_str(&format!(
                    " WHEN {} = {} THEN true",
                    self.dialect.cast(&decimal_columns[0], &SqlType::Text),
                    self.dialect.cast(&decimal_columns[1], &SqlType::Text)
                ));
            }
            self.sql.push_str(" END");
        }
        Ok(())
    }

    /// A numeric operand as a value of type `ty`.
    pub(super) fn number(
        &mut self,
        operand: &Operand<'_>,
        number: Option<&Number>,
        class: Num,
        ty: Num,
    ) -> Result<(), Error> {
        if is_arith(operand.value) {
            // Compared as `numeric`, which PostgreSQL compares exactly with
            // the other side's type.
            return self.numeric_value(operand.value);
        }
        if let Some(number) = number {
            let value = match ty {
                Num::Integer => number
                    .int8()
                    .expect("integer comparisons only with int8 constants")
                    .to_string(),
                Num::Decimal => number
                    .decimal
                    .clone()
                    .expect("decimal comparisons only with integer and decimal constants"),
                Num::Double => canonical_double(number.double),
            };
            self.cast_rendered(&ty.sql_type(), |this| {
                this.expr(&super::SqlExpr::Param(value.into()))
            });
            return Ok(());
        }
        let (table, name) = operand
            .column
            .expect("a numeric operand is a column or a constant");
        let column = self.column(table, name);
        if class == Num::Decimal && ty == Num::Double {
            self.sql.push_str(&self.decimal_column_as_double(&column));
        } else if class < ty {
            self.sql
                .push_str(&self.dialect.cast(&column, &ty.sql_type()));
        } else {
            self.sql.push_str(&column);
        }
        Ok(())
    }

    /// A boolean or dateTime operand as a value of SQL type `ty`.
    pub(super) fn typed_value(&mut self, operand: &Operand<'_>, ty: &SqlType) -> Result<(), Error> {
        if let FilterValue::Cast(..) = operand.value {
            return self.boolean_value(operand.value);
        }
        let value = match &operand.constant {
            Some(Value::Boolean(b)) => b.to_string(),
            Some(Value::DateTime(d)) => d.canonical.clone(),
            _ => {
                let (table, name) = operand
                    .column
                    .expect("a boolean or dateTime operand is a column or a constant");
                self.sql.push_str(&self.column(table, name));
                return Ok(());
            }
        };
        self.cast_rendered(ty, |this| this.expr(&super::SqlExpr::Param(value.into())));
        Ok(())
    }

    /// `comparison`, false (or, under `mode` Exact, an error) where a
    /// column of `operands` for which `special` holds fails `guard`. A
    /// comparison by `=` with a constant needs no guard in a positive
    /// context: the special values never equal a constant.
    fn guarded(
        &mut self,
        mode: Mode,
        operands: &[&Operand<'_>],
        op: CompareOp,
        special: impl Fn(&Operand<'_>) -> bool,
        guard: impl Fn(&str) -> String,
        comparison: impl FnOnce(&mut Self) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let has_constant = operands.iter().any(|o| o.constant.is_some());
        let guards: Vec<String> = operands
            .iter()
            .filter(|o| special(o))
            .filter_map(|o| o.column)
            .map(|(table, name)| guard(&self.column(table, name)))
            .collect();
        if guards.is_empty() || (mode == Mode::Positive && op == CompareOp::Equal && has_constant) {
            return comparison(self);
        }
        match mode {
            Mode::Positive => {
                self.sql.push('(');
                comparison(self)?;
                self.sql
                    .push_str(&format!(" AND {})", guards.join(" AND ")));
            }
            Mode::Exact => {
                self.sql
                    .push_str(&format!("CASE WHEN {} THEN ", guards.join(" AND ")));
                comparison(self)?;
                self.sql.push_str(" END");
            }
        }
        Ok(())
    }

    /// The operand's string as text: an IRI, a blank node's label or a
    /// literal's lexical form.
    pub(super) fn text(&mut self, operand: &Operand<'_>) -> Result<(), Error> {
        if let Some((table, name)) = operand.finite {
            self.sql.push_str(&format!(
                "CASE WHEN {} THEN ",
                self.dialect.is_finite(&self.column(table, name))
            ));
            self.text(&Operand {
                finite: None,
                ..operand.clone()
            })?;
            self.sql.push_str(" END");
            return Ok(());
        }
        match &operand.text {
            Text::Constant(text) => {
                if text.contains('\0') {
                    return Err(unsupported("strings with a NUL character in FILTER"));
                }
                self.cast_rendered(&SqlType::Text, |this| {
                    this.expr(&super::SqlExpr::Param(text.as_str().into()))
                });
            }
            Text::Term(term) => {
                // A template is a concatenation, which binds looser than
                // the collation and the comparison around it.
                let types = self.options.column_types;
                self.sql.push('(');
                if term.base_iri.is_some() {
                    self.term_side(term, types, false)?;
                } else {
                    self.term_string(term, types, true)?;
                }
                self.sql.push(')');
            }
            Text::NaturalDatatype { column, datatype } => {
                self.sql.push_str(&format!(
                    "CASE WHEN {} THEN ",
                    self.dialect.is_finite(&self.column(column.0, column.1))
                ));
                self.cast_rendered(&SqlType::Text, |this| {
                    this.expr(&super::SqlExpr::Param(datatype.as_str().into()))
                });
                self.sql.push_str(" ELSE ");
                self.cast_rendered(&SqlType::Text, |this| {
                    this.expr(&super::SqlExpr::Param(XSD_STRING.into()))
                });
                self.sql.push_str(" END");
            }
            Text::Computed(value) => self.computed_text(value)?,
            Text::Unknown => self.typed_null(&SqlType::Text),
        }
        Ok(())
    }

    /// The string of a function on strings, under the code point
    /// collation, so that comparing, searching and concatenating it do not
    /// depend on the collations of the columns it is computed from.
    fn computed_text(&mut self, value: &FilterValue) -> Result<(), Error> {
        self.utf8_strings()?;
        self.sql.push('(');
        match value {
            FilterValue::Substr {
                text,
                start,
                length,
            } => {
                let at = self.sql.len();
                self.argument_text(text)?;
                let text = self.sql.split_off(at);
                let mut int4 = |n: i32| {
                    self.cast_rendered(&SqlType::Integer, |this| {
                        this.expr(&super::SqlExpr::Param(n.to_string().into()))
                    });
                    self.sql.split_off(at)
                };
                let start = int4(*start);
                let length = length.map(&mut int4);
                self.sql
                    .push_str(&self.dialect.substring(&text, &start, length.as_deref()));
            }
            FilterValue::UCase(a) | FilterValue::LCase(a) => {
                let start = self.sql.len();
                self.argument_text(a)?;
                let text = self.sql.split_off(start);
                let upper = matches!(value, FilterValue::UCase(_));
                self.sql.push_str(&self.dialect.case_map(&text, upper));
            }
            FilterValue::Concat(args) => {
                self.sql.push('(');
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        self.sql.push_str(" || ");
                    }
                    self.argument_text(arg)?;
                }
                self.sql.push(')');
            }
            FilterValue::EncodeForUri(a) => {
                let start = self.sql.len();
                self.sql.push('(');
                self.argument_text(a)?;
                self.sql.push_str(self.dialect.code_point_collation());
                self.sql.push(')');
                let text = self.sql.split_off(start);
                self.sql.push_str(&self.dialect.encode_for_uri(&text));
            }
            FilterValue::Replace {
                text,
                pattern,
                replacement,
            } => {
                let start = self.sql.len();
                self.argument_text(text)?;
                self.sql.push_str(self.dialect.code_point_collation());
                let text = self.sql.split_off(start);
                let mut param = |value: &str| {
                    self.cast_rendered(&SqlType::Text, |this| {
                        this.expr(&super::SqlExpr::Param(value.into()))
                    });
                    self.sql.split_off(start)
                };
                let (pattern, replacement) = (param(pattern), param(replacement));
                self.sql
                    .push_str(&self.dialect.regex_replace(&text, &pattern, &replacement));
            }
            FilterValue::StrBefore(a, b) | FilterValue::StrAfter(a, b) => {
                let collation = self.dialect.code_point_collation();
                let start = self.sql.len();
                self.argument_text(a)?;
                self.sql.push_str(collation);
                let text = self.sql.split_off(start);
                self.argument_text(b)?;
                self.sql.push_str(collation);
                let search = self.sql.split_off(start);
                let position = self.dialect.position(&text, &search);
                let part = match value {
                    FilterValue::StrBefore(..) => {
                        self.dialect
                            .substring(&text, "1", Some(&format!("{position} - 1")))
                    }
                    _ => self.dialect.substring(
                        &text,
                        &format!("{position} + {}", self.dialect.char_length(&search)),
                        None,
                    ),
                };
                self.sql
                    .push_str(&format!("CASE WHEN {position} > 0 THEN {part} ELSE '' END"));
            }
            other => unreachable!("not a computed string: {other:?}"),
        }
        self.sql.push_str(self.dialect.code_point_collation());
        self.sql.push(')');
        Ok(())
    }

    /// The lexical form of a string argument.
    fn argument_text(&mut self, value: &FilterValue) -> Result<(), Error> {
        let operand = self.operand(value);
        self.text(&operand)
    }

    /// Functions on strings count, compare and map characters, which
    /// PostgreSQL does as code points only in a UTF-8 database.
    fn utf8_strings(&self) -> Result<(), Error> {
        if self.options.non_utf8 {
            return Err(unsupported(
                "functions on strings in a database whose encoding is not UTF-8, where \
                 PostgreSQL's characters are not Unicode code points",
            ));
        }
        Ok(())
    }

    /// `STRSTARTS`, `STRENDS` or `CONTAINS` (SPARQL 1.1 §17.4.3.6 to
    /// §17.4.3.8): an error unless the arguments are compatible strings.
    /// A constant to search for is a `LIKE` pattern, which an index on the
    /// column under the code point collation can serve for `STRSTARTS`.
    fn str_test(
        &mut self,
        test: StrTest,
        text: &FilterValue,
        search: &FilterValue,
    ) -> Result<(), Error> {
        let (l, r) = (self.operand(text), self.operand(search));
        let (a, b) = (string_argument(&l), string_argument(&r));
        for kind in [&a, &b] {
            if let Kind::Unsupported(reason) = kind {
                return Err(unsupported(reason));
            }
        }
        if !compatible(&a, &b) {
            self.typed_null(&SqlType::Boolean);
            return Ok(());
        }
        self.utf8_strings()?;
        let collation = self.dialect.code_point_collation();
        self.sql.push('(');
        let start = self.sql.len();
        self.text(&l)?;
        self.sql.push_str(collation);
        let text = self.sql.split_off(start);
        if let Text::Constant(constant) = &r.text {
            if constant.contains('\0') {
                return Err(unsupported("strings with a NUL character in FILTER"));
            }
            let escaped = like_escaped(constant);
            let pattern = match test {
                StrTest::Starts => format!("{escaped}%"),
                StrTest::Ends => format!("%{escaped}"),
                StrTest::Contains => format!("%{escaped}%"),
            };
            self.cast_rendered(&SqlType::Text, |this| {
                this.expr(&super::SqlExpr::Param(pattern.into()))
            });
            let pattern = self.sql.split_off(start);
            self.sql.push_str(&self.dialect.like(&text, &pattern));
        } else {
            self.text(&r)?;
            self.sql.push_str(collation);
            let search = self.sql.split_off(start);
            self.sql
                .push_str(&self.dialect.str_test(test, &text, &search));
        }
        self.sql.push(')');
        Ok(())
    }

    /// The effective boolean value (SPARQL 1.1 §17.2.2).
    fn ebv(&mut self, value: &FilterValue) -> Result<(), Error> {
        let operand = self.operand(value);
        if operand.kind == Kind::Error {
            self.typed_null(&SqlType::Boolean);
            return Ok(());
        }
        if let FilterValue::Cast(Cast::Boolean, _) = value {
            if let Kind::Unsupported(reason) = &operand.kind {
                return Err(unsupported(reason));
            }
            return self.boolean_value(value);
        }
        if is_arith(value) {
            if let Kind::Unsupported(reason) = &operand.kind {
                return Err(unsupported(reason));
            }
            if operand.kind == Kind::Numeric(Num::Double) {
                return self.double_ebv(value);
            }
            // A number is true unless it is zero (SPARQL 1.1 §17.2.2).
            self.sql.push('(');
            self.numeric_value(value)?;
            self.sql.push_str(" <> 0)");
            return Ok(());
        }
        if let Some(constant) = &operand.constant {
            let lexical = match value {
                FilterValue::Constant(Term::Literal(l)) => l.value(),
                _ => "",
            };
            let ebv = match constant {
                Value::Boolean(b) => Some(*b),
                Value::Numeric(n) => Some(n.double != 0.0 && !n.double.is_nan()),
                Value::String => Some(!lexical.is_empty()),
                Value::IllTyped => match &operand.kind {
                    Kind::IllTyped(datatype) if datatype == &format!("{XSD}dateTime") => None,
                    _ => Some(false),
                },
                Value::Unsupported(reason) => return Err(unsupported(reason)),
                Value::DateTime(_) | Value::Other => None,
            };
            return self.filter(&Filter::Constant(ebv), Mode::Exact);
        }
        if let FilterValue::Constant(Term::Literal(literal)) = value {
            // A literal with a language tag.
            let ebv = !literal.value().is_empty();
            return self.filter(&Filter::Constant(Some(ebv)), Mode::Exact);
        }
        if operand.infinite {
            return Err(unsupported(
                "the effective boolean value of a date or timestamp column: its infinite \
                 values are plain literals",
            ));
        }
        let column = operand.column.map(|(table, name)| self.column(table, name));
        match (&operand.kind, column) {
            (Kind::Boolean, Some(c)) => self.sql.push_str(&c),
            (Kind::Numeric(Num::Integer), Some(c)) => self.sql.push_str(&format!("{c} <> 0")),
            (Kind::Numeric(Num::Decimal), Some(c)) => self.sql.push_str(&format!(
                "CASE WHEN {} THEN {c} <> 0 ELSE false END",
                decimal_guard(self.dialect, &c)
            )),
            (Kind::Numeric(Num::Double), Some(c)) => self.sql.push_str(&format!(
                "({c} <> 0 AND {c} <> {})",
                self.dialect.cast("'NaN'", &SqlType::Double)
            )),
            (Kind::String | Kind::Lang(_) | Kind::Unknown, _) => {
                self.sql.push('(');
                self.text(&operand)?;
                self.sql.push_str(" <> '')");
            }
            (Kind::Unsupported(reason), _) => return Err(unsupported(reason)),
            _ => self.typed_null(&SqlType::Boolean),
        }
        Ok(())
    }
}

/// The kind of a column of natural datatype (R2RML §10.2).
fn natural_kind(ty: &ColumnType) -> Kind {
    let Some(datatype) = ty.natural_datatype.as_deref() else {
        return Kind::String;
    };
    match datatype.strip_prefix(XSD) {
        Some("integer") => Kind::Numeric(Num::Integer),
        Some("decimal") => Kind::Numeric(Num::Decimal),
        Some("double") if ty.name == "float8" => Kind::Numeric(Num::Double),
        // The literal's value is the double of the value's shortest
        // decimal form, not the value widened to a double.
        Some("double") => Kind::Unsupported(format!(
            "comparing or computing with values of {} columns, whose xsd:double literals have \
             the value of their shortest decimal form",
            ty.name
        )),
        Some("boolean") => Kind::Boolean,
        Some("dateTime") => Kind::DateTime {
            zoned: ty.name == "timestamptz",
        },
        _ => Kind::Other(datatype.to_owned()),
    }
}

fn kind_of(value: &Value, datatype: &str) -> Kind {
    match value {
        Value::String => Kind::String,
        Value::Numeric(n) => Kind::Numeric(n.class),
        Value::Boolean(_) => Kind::Boolean,
        Value::DateTime(d) => Kind::DateTime { zoned: d.zoned },
        Value::Other => Kind::Other(datatype.to_owned()),
        Value::IllTyped => Kind::IllTyped(datatype.to_owned()),
        Value::Unsupported(reason) => Kind::Unsupported(reason.clone()),
    }
}

/// Datatypes with rows in the operator mapping, besides `xsd:string`.
fn mapped(datatype: &str) -> bool {
    !matches!(xsd::value("", datatype), Value::Other | Value::String)
}

/// The kind of a column with a mapped `rr:datatype` when its SQL type is
/// not known: as if the datatype were the column's natural one.
fn assumed_kind(datatype: &str) -> Kind {
    match datatype.strip_prefix(XSD) {
        Some("integer") => Kind::Numeric(Num::Integer),
        Some("decimal") => Kind::Numeric(Num::Decimal),
        Some("double") => Kind::Numeric(Num::Double),
        Some("boolean") => Kind::Boolean,
        Some("dateTime") => Kind::DateTime { zoned: false },
        _ => Kind::Unsupported(format!("comparing literals of datatype <{datatype}>")),
    }
}

/// The kind a column of unknown type is shown as, next to `other`: an
/// integer next to a number.
fn assumed(other: &Kind) -> Kind {
    match other {
        Kind::Numeric(_) => Kind::Numeric(Num::Integer),
        Kind::Boolean | Kind::DateTime { .. } => other.clone(),
        _ => Kind::String,
    }
}

pub(super) fn unsupported(reason: &str) -> Error {
    Error::UnsupportedFeature(reason.to_owned())
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::{ColumnTypes, Comparison, Lexical, RenderOptions, SqlPlan};
    use super::*;
    use crate::dialect::PostgresDialect;
    use crate::mapping::{LogicalTable, Mapping, MappingIndex};
    use crate::sql::TermKey;
    use crate::{sparql, unfold};

    /// One column per SQL type under test, each with its own predicate.
    const COLUMNS: [(&str, &str); 12] = [
        ("i4", "int4"),
        ("i8", "int8"),
        ("dec", "numeric"),
        ("f8", "float8"),
        ("f4", "float4"),
        ("txt", "text"),
        ("bpc", "bpchar"),
        ("b", "bool"),
        ("ts", "timestamp"),
        ("tz", "timestamptz"),
        ("d", "date"),
        ("u", "uuid"),
    ];

    pub(in crate::sql) fn mapping() -> String {
        let objects: String = COLUMNS
            .iter()
            .map(|(c, _)| {
                format!(
                    "  rr:predicateObjectMap [ rr:predicate ex:{c} ; rr:objectMap [ rr:column \"{c}\" ] ] ;\n"
                )
            })
            .collect();
        format!(
            "@prefix rr: <http://www.w3.org/ns/r2rml#> .\n@prefix ex: <http://example.org/> .\n\
             @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\
             <#T> rr:logicalTable [ rr:tableName \"t\" ] ;\n{objects}\
               rr:predicateObjectMap [ rr:predicate ex:intText ; rr:objectMap [ rr:column \"txt\" ; rr:datatype xsd:integer ] ] ;\n\
               rr:predicateObjectMap [ rr:predicate ex:intInt ; rr:objectMap [ rr:column \"i4\" ; rr:datatype xsd:integer ] ] ;\n\
               rr:predicateObjectMap [ rr:predicate ex:label ; rr:objectMap [ rr:column \"txt\" ; rr:language \"EN\" ] ] ;\n\
               rr:predicateObjectMap [ rr:predicate ex:link ; rr:objectMap [ rr:template \"http://example.org/r/{{i4}}\" ] ] ;\n\
               rr:predicateObjectMap [ rr:predicate ex:textLink ; rr:objectMap [ rr:template \"http://example.org/t/{{txt}}\" ] ] ;\n\
               rr:predicateObjectMap [ rr:predicate ex:pairLink ; rr:objectMap [ rr:template \"http://example.org/p/{{i8}}/{{txt}}/{{u}}\" ] ] ;\n\
               rr:subjectMap [ rr:template \"http://example.org/r/{{i4}}\" ] .\n"
        )
    }

    pub(in crate::sql) fn column_type(sql: &str) -> ColumnType {
        let xsd = |local: &str| Some(format!("{XSD}{local}"));
        let (group, natural, comparison, lexical) = match sql {
            "int4" | "int8" => (
                "integer",
                xsd("integer"),
                Comparison::Value,
                Some(Lexical::Text),
            ),
            "numeric" => (
                "numeric",
                xsd("decimal"),
                Comparison::Value,
                Some(Lexical::Decimal),
            ),
            "float8" | "float4" => ("double", xsd("double"), Comparison::Text, None),
            "text" => ("text", None, Comparison::Value, Some(Lexical::Text)),
            "bpchar" => ("bpchar", None, Comparison::Value, None),
            "bool" => (
                "boolean",
                xsd("boolean"),
                Comparison::Value,
                Some(Lexical::Boolean),
            ),
            "timestamp" | "timestamptz" => (sql, xsd("dateTime"), Comparison::Value, None),
            "date" => ("date", xsd("date"), Comparison::Value, None),
            "uuid" => ("uuid", None, Comparison::Value, Some(Lexical::Text)),
            other => panic!("{other}"),
        };
        ColumnType {
            name: sql.to_owned(),
            sql_type: format!("\"pg_catalog\".\"{sql}\""),
            union_class: group.to_owned(),
            natural_datatype: natural,
            group: group.to_owned(),
            comparison,
            lexical,
            lexical_unreserved: matches!(group, "integer" | "numeric" | "boolean" | "uuid"),
            term_key: match sql {
                "float8" | "float4" => Some(TermKey::Text),
                "bpchar" => Some(TermKey::OctetLength),
                _ => None,
            },
            term_normal: None,
        }
    }

    pub(in crate::sql) fn plan(query: &str) -> Result<SqlPlan, Error> {
        let mapping = Mapping::from_turtle(&mapping()).unwrap();
        let index = MappingIndex::new(&mapping);
        let query = sparql::parse(&format!(
            "PREFIX ex: <http://example.org/>\nPREFIX xsd: <http://www.w3.org/2001/XMLSchema#>\n{query}"
        ))?;
        unfold::unfold(&query, &mapping, &index)
    }

    /// The SQL types of the columns of [`COLUMNS`].
    pub(in crate::sql) fn types() -> ColumnTypes {
        COLUMNS
            .iter()
            .map(|(c, ty)| {
                (
                    (LogicalTable::TableName("t".into()), (*c).to_owned()),
                    column_type(ty),
                )
            })
            .collect()
    }

    fn sql_of(query: &str, non_utf8: bool) -> Result<String, Error> {
        let types = types();
        Ok(plan(query)?
            .to_sql_with(
                &PostgresDialect,
                &RenderOptions {
                    column_types: Some(&types),
                    non_utf8,
                    ..RenderOptions::default()
                },
            )?
            .sql)
    }

    fn render_with(query: &str, non_utf8: bool) -> Result<String, Error> {
        let sql = sql_of(query, non_utf8)?;
        // The filter is the first condition of the WHERE clause.
        let filter = sql.split("\nWHERE ").nth(1).unwrap_or_default();
        Ok(filter
            .split(" AND t0.\"i4\" IS NOT NULL")
            .next()
            .unwrap_or_default()
            .to_owned())
    }

    /// The SQL of `FILTER(filter)` over `?s ex:<p> ?x` (and `?s ex:<q> ?y`).
    pub(super) fn render(patterns: &str, filter: &str) -> Result<String, Error> {
        render_with(
            &format!("SELECT ?s WHERE {{ {patterns} FILTER({filter}) }}"),
            false,
        )
    }

    fn x(predicate: &str, filter: &str) -> String {
        render(&format!("?s ex:{predicate} ?x"), filter).unwrap()
    }

    fn rejected(patterns: &str, filter: &str) -> String {
        match render(patterns, filter) {
            Err(Error::UnsupportedFeature(reason)) => reason,
            other => panic!("{filter}: {other:?}"),
        }
    }

    #[test]
    fn arithmetic_is_computed_in_numeric_and_compared_exactly() {
        // Integers are added in numeric, so int8 cannot overflow; the
        // comparison with the integer constant stays exact.
        assert_eq!(
            x("i4", "?x + 1 > 5"),
            "(t0.\"i4\"::numeric + $1::numeric) > $2::int8"
        );
        assert_eq!(
            x("i8", "?x * ?x >= 9223372036854775807"),
            "(t0.\"i8\"::numeric * t0.\"i8\"::numeric) >= $1::int8"
        );
        // An integer divided by an integer is a decimal, and a division
        // by zero is an error (op:numeric-divide).
        assert_eq!(
            x("i4", "?x / 2 = 1.5"),
            "(t0.\"i4\"::numeric / NULLIF($1::numeric, 0)) = $2::numeric"
        );
        assert_eq!(x("i4", "-?x < 0"), "(-t0.\"i4\"::numeric) < $1::int8");
        assert_eq!(
            x("i4", "+?x = 3"),
            "(t0.\"i4\"::numeric - $1::numeric) = $2::int8"
        );
    }

    #[test]
    fn arithmetic_over_a_numeric_column_treats_nan_as_an_error() {
        assert_eq!(
            x("dec", "?x - 1 > 0"),
            "(CASE WHEN t0.\"dec\"::text NOT IN ('NaN', 'Infinity', '-Infinity') THEN \
             t0.\"dec\" END - $1::numeric) > $2::numeric"
        );
    }

    #[test]
    fn arithmetic_over_anything_but_numbers_is_a_type_error() {
        assert_eq!(x("txt", "?x + 1 > 0"), "NULL::bool");
        assert_eq!(x("i4", "?x + \"1\" = 2"), "NULL::bool");
        assert_eq!(x("i4", "?x + 1 = \"2\""), "NULL::bool");
        assert_eq!(x("b", "-?x = 0"), "NULL::bool");
        // A literal and an IRI are never the same term.
        assert_eq!(x("i4", "?x + 1 = <http://example.org/r/2>"), "false");
    }

    #[test]
    fn arithmetic_has_an_effective_boolean_value() {
        assert_eq!(
            x("i4", "?x - 3"),
            "((t0.\"i4\"::numeric - $1::numeric) <> 0)"
        );
        assert_eq!(x("txt", "?x - 3"), "NULL::bool");
    }

    #[test]
    fn arithmetic_that_sql_cannot_answer_exactly_is_rejected() {
        assert!(rejected("?s ex:f4 ?x", "?x + 1 > 0").contains("float4"));
        assert!(rejected("?s ex:f4 ?x", "?x * 1e0 > 0").contains("float4"));
        assert!(rejected("?s ex:i4 ?x", "STR(?x + 1) = \"2\"").contains("STR, LANG"));
        assert!(rejected("?s ex:f8 ?x", "STR(?x + 1) = \"2\"").contains("STR, LANG"));
        assert!(rejected("?s ex:i4 ?x", "sameTerm(?x + 1, 2)").contains("sameTerm"));
    }

    #[test]
    fn double_arithmetic_is_computed_once_and_excludes_nan_from_comparisons() {
        let sql = x("f8", "?x * 2 > 1");
        assert!(
            sql.starts_with(
                "(SELECT (x > $2::float8 AND x <> 'NaN'::float8) FROM (SELECT CASE WHEN CASE \
                 WHEN abs(t0.\"f8\") BETWEEN '1e0'::float8 AND"
            ),
            "{sql}"
        );
        assert!(
            sql.ends_with("ELSE t0.\"f8\" * $1::float8 END AS x) AS d)"),
            "{sql}"
        );
        // Integers convert to float8 exactly rounded, and are never NaN.
        let sql = x("i4", "?x / 2e0 = 1");
        assert!(
            sql.starts_with(
                "(SELECT (x = $2::float8 AND x <> 'NaN'::float8) FROM (SELECT CASE WHEN \
                 $1::float8 = 0 THEN"
            ),
            "{sql}"
        );
        assert!(
            sql.ends_with("ELSE t0.\"i4\"::float8 / $1::float8 END AS x) AS d)"),
            "{sql}"
        );
        // Integer arithmetic stays exact in numeric, then becomes a double.
        let sql = x("i4", "(?x + 1) * 1e0 > 2");
        assert!(
            sql.contains("FROM (SELECT (t0.\"i4\"::numeric + $1::numeric) AS x) AS d)"),
            "{sql}"
        );
        assert!(
            x("f8", "?x * 2").starts_with("(SELECT (x <> 0 AND x <> 'NaN'::float8) FROM"),
            "effective boolean value"
        );
        // Unary plus subtracts zero, which keeps a -0.
        assert!(x("f8", "+?x = 0").contains("ELSE t0.\"f8\" - $1::float8 END"));
        assert_eq!(x("f8", "isLiteral(?x * 2)"), "true");
        assert_eq!(x("txt", "?x * 1e0 > 0"), "NULL::bool");
    }

    fn params(query: &str) -> Vec<String> {
        let types = types();
        plan(query)
            .unwrap()
            .to_sql_with(
                &PostgresDialect,
                &RenderOptions {
                    column_types: Some(&types),
                    ..RenderOptions::default()
                },
            )
            .unwrap()
            .params
    }

    #[test]
    fn double_arithmetic_of_constants_is_computed_before_binding() {
        // 1e0 / 0 is INF in IEEE 754; Rust's f64 computes it.
        assert_eq!(x("i4", "?x < 1e0 / 0"), "(t0.\"i4\"::float8 < $1::float8)");
        assert_eq!(
            params("SELECT ?s WHERE { ?s ex:i4 ?x FILTER(?x < 1e0 / 0 - -1) }")[0],
            "INF"
        );
        // NaN is never ordered.
        assert_eq!(
            x("i4", "?x < 0e0 / 0"),
            "(t0.\"i4\"::float8 < $1::float8 AND $1::float8 <> 'NaN'::float8)"
        );
        // Arithmetic on integers and decimals stays exact in SQL.
        assert!(x("i4", "?x < (1 + 2) * 1e0").contains("($1::numeric + $2::numeric) AS x"));
    }

    #[test]
    fn decimals_promote_to_doubles_with_infinities_and_ill_typed_values() {
        let sql = x("dec", "?x < 1e0");
        assert!(
            sql.starts_with(
                "(CASE WHEN NOT (t0.\"dec\"::text NOT IN ('NaN', 'Infinity', '-Infinity')) \
                 THEN NULL::float8 WHEN abs(t0.\"dec\") >= 2::numeric ^ 1024 - 2::numeric ^ \
                 970 THEN"
            ),
            "{sql}"
        );
        assert!(sql.ends_with("ELSE t0.\"dec\"::float8 END < $1::float8 AND t0.\"dec\"::text NOT IN ('NaN', 'Infinity', '-Infinity'))"), "{sql}");
    }

    #[test]
    fn integers_compare_with_the_constant_converted_to_keep_the_index() {
        assert_eq!(x("i4", "?x > 5"), "t0.\"i4\" > $1::int8");
        // An integral decimal is the same integer; a fraction is compared
        // as a decimal, which casts the column.
        assert_eq!(x("i4", "?x = 5.0"), "t0.\"i4\" = $1::int8");
        assert_eq!(x("i4", "?x < 5.5"), "t0.\"i4\"::numeric < $1::numeric");
        assert_eq!(
            x("i8", "?x < 99999999999999999999"),
            "t0.\"i8\"::numeric < $1::numeric"
        );
        // A double constant promotes the integer to a double (XPath).
        assert_eq!(x("i4", "?x < 1e1"), "t0.\"i4\"::float8 < $1::float8");
        assert_eq!(x("i4", "?x <= -3"), "t0.\"i4\" <= $1::int8");
        assert_eq!(x("i4", "5 >= ?x"), "$1::int8 >= t0.\"i4\"");
    }

    #[test]
    fn decimal_columns_exclude_nan_and_infinities_which_are_not_decimals() {
        assert_eq!(x("dec", "?x = 1.5"), "t0.\"dec\" = $1::numeric");
        assert_eq!(
            x("dec", "?x > 1"),
            "(t0.\"dec\" > $1::numeric AND t0.\"dec\"::text NOT IN ('NaN', 'Infinity', '-Infinity'))"
        );
        // Under negation an ill-typed value is an error, not false.
        assert_eq!(
            x("dec", "!(?x = 1.5)"),
            "NOT (CASE WHEN t0.\"dec\"::text NOT IN ('NaN', 'Infinity', '-Infinity') \
             THEN t0.\"dec\" = $1::numeric END)"
        );
        let sql = sql_of(
            "SELECT ?s WHERE { ?s ex:dec ?x ; ex:dec ?y FILTER(!(?x = ?y)) }",
            false,
        )
        .unwrap();
        // Two equal ill-typed values are the same term, hence equal.
        assert!(
            sql.contains(
                "NOT (CASE WHEN t0.\"dec\"::text NOT IN ('NaN', 'Infinity', '-Infinity') \
                 AND t1.\"dec\"::text NOT IN ('NaN', 'Infinity', '-Infinity') \
                 THEN t0.\"dec\" = t1.\"dec\" WHEN t0.\"dec\"::text = t1.\"dec\"::text THEN true END)"
            ),
            "{sql}"
        );
    }

    #[test]
    fn doubles_exclude_nan_which_postgresql_orders_last() {
        assert_eq!(x("f8", "?x = 0"), "t0.\"f8\" = $1::float8");
        assert_eq!(
            x("f8", "?x > 0"),
            "(t0.\"f8\" > $1::float8 AND t0.\"f8\" <> 'NaN'::float8)"
        );
        assert_eq!(x("i4", "?x = \"NaN\"^^xsd:double"), "false");
        assert!(rejected("?s ex:f4 ?x", "?x > 0").contains("float4"));
    }

    #[test]
    fn strings_order_by_code_point() {
        assert_eq!(x("txt", "?x = \"a\""), "(t0.\"txt\" = $1)");
        assert_eq!(
            x("txt", "?x < \"a\""),
            "(t0.\"txt\"::text) COLLATE \"C\" < $1::text"
        );
        assert_eq!(
            x("u", "?x > \"a\""),
            "(t0.\"u\"::text) COLLATE \"C\" > $1::text"
        );
        assert!(rejected("?s ex:bpc ?x", "?x > \"a\"").contains("bpchar"));
        assert!(matches!(
            render_with("SELECT ?s WHERE { ?s ex:txt ?x FILTER(?x < \"a\") }", true),
            Err(Error::UnsupportedFeature(reason)) if reason.contains("UTF-8")
        ));
        // A string constant matched as a term binds to NULL where no value
        // can match it, so under negation it is coalesced to false.
        assert_eq!(
            x("txt", "!(?x = \"a\")"),
            "NOT (coalesce((t0.\"txt\" = $1), false))"
        );
    }

    #[test]
    fn date_times_compare_within_one_kind_of_time_zone() {
        assert_eq!(
            x("ts", "?x < \"2024-01-01T00:00:00\"^^xsd:dateTime"),
            "(t0.\"ts\" < $1::timestamp AND isfinite(t0.\"ts\"))"
        );
        assert_eq!(
            x("tz", "?x = \"2024-01-01T09:00:00+09:00\"^^xsd:dateTime"),
            "t0.\"tz\" = $1::timestamptz"
        );
        assert_eq!(
            x("ts", "!(?x = \"2024-01-01T00:00:00\"^^xsd:dateTime)"),
            "NOT (CASE WHEN isfinite(t0.\"ts\") THEN t0.\"ts\" = $1::timestamp END)"
        );
        assert!(
            rejected("?s ex:tz ?x", "?x < \"2024-01-01T00:00:00\"^^xsd:dateTime")
                .contains("implicit time zone")
        );
        assert!(rejected("?s ex:ts ?x", "?x = \"infinity\"").contains("infinite"));
        assert!(rejected("?s ex:ts ?x", "?x").contains("infinite"));
    }

    #[test]
    fn type_errors_are_null_and_different_kinds_of_terms_are_not_equal() {
        assert_eq!(x("txt", "?x = 5"), "NULL::bool");
        assert_eq!(x("txt", "?x < 5"), "NULL::bool");
        assert_eq!(x("b", "?x < 5"), "NULL::bool");
        assert_eq!(x("i4", "?x = <http://example.org/a>"), "false");
        assert_eq!(x("i4", "?x < <http://example.org/a>"), "NULL::bool");
        assert_eq!(x("b", "?x = true"), "t0.\"b\" = $1::bool");
        assert_eq!(x("b", "?x"), "t0.\"b\"");
        // RDFterm-equal of IRIs keeps template decomposition.
        assert_eq!(
            x("link", "?x = <http://example.org/r/7>"),
            "(t0.\"i4\" = $1)"
        );
        assert_eq!(x("link", "?x < <http://example.org/r/7>"), "NULL::bool");
    }

    #[test]
    fn rendering_without_column_types_reports_an_unsupported_comparison() {
        let plan = plan("SELECT ?s WHERE { ?s ex:label ?x FILTER(?x != \"a\"@en) }").unwrap();
        match plan.to_sql() {
            Err(Error::UnsupportedFeature(reason)) => assert!(reason.contains("under negation")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn literals_outside_the_operator_mapping_are_only_equal_as_terms() {
        assert_eq!(x("label", "?x = \"a\"@en"), "(t0.\"txt\" = $1)");
        assert!(rejected("?s ex:label ?x", "?x != \"a\"@en").contains("under negation"));
        assert!(rejected("?s ex:label ?x", "?x < \"a\"@en").contains("language-tagged"));
        assert!(rejected("?s ex:d ?x", "?x = \"2024-01-01\"^^xsd:date").contains("#date"));
        // Different datatypes: a type error.
        assert_eq!(x("d", "?x = \"a\"@en"), "NULL::bool");
        // An ill-typed constant is only equal to the same term.
        assert_eq!(
            x("i4", "!(?x = \"abc\"^^xsd:integer)"),
            "NOT (CASE WHEN (t0.\"i4\" = $1) THEN true END)"
        );
        // rr:datatype equal to the column's natural one is its literal;
        // another one cannot be read in SQL.
        assert_eq!(x("intInt", "?x > 5"), "t0.\"i4\" > $1::int8");
        assert!(rejected("?s ex:intText ?x", "?x > 5").contains("rr:datatype"));
        assert!(rejected("?s ex:i4 ?x", "?x > \"5\"^^xsd:int").contains("xsd:int"));
    }

    #[test]
    fn effective_boolean_values() {
        assert_eq!(x("i4", "?x"), "t0.\"i4\" <> 0");
        assert_eq!(
            x("dec", "?x"),
            "CASE WHEN t0.\"dec\"::text NOT IN ('NaN', 'Infinity', '-Infinity') \
             THEN t0.\"dec\" <> 0 ELSE false END"
        );
        assert_eq!(
            x("f8", "?x"),
            "(t0.\"f8\" <> 0 AND t0.\"f8\" <> 'NaN'::float8)"
        );
        assert_eq!(x("txt", "?x"), "((t0.\"txt\"::text) <> '')");
        assert_eq!(x("label", "?x"), "((t0.\"txt\"::text) <> '')");
        assert_eq!(x("link", "?x"), "NULL::bool");
        assert_eq!(x("i4", "\"\" || \"x\""), "(false OR true)");
        assert_eq!(x("i4", "0.0 || \"abc\"^^xsd:integer"), "(false OR false)");
        assert_eq!(x("i4", "<http://example.org/a>"), "NULL::bool");
    }

    #[test]
    fn same_term_and_term_tests() {
        assert_eq!(x("i4", "sameTerm(?x, 5)"), "(t0.\"i4\" = $1)");
        assert_eq!(x("i4", "sameTerm(?x, \"5\")"), "(t0.\"i4\" = $1)");
        // The adapter adds the sign of a zero to the comparison.
        assert_eq!(x("f8", "sameTerm(?x, 0e0)"), "(t0.\"f8\" = $1)");
        assert_eq!(
            x("link", "isIRI(?x) && !isBlank(?x) && !isLiteral(?x)"),
            "((true AND NOT (false)) AND NOT (false))"
        );
    }

    #[test]
    fn unbound_variables_are_errors_except_for_bound() {
        assert_eq!(x("i4", "?nothing = 1"), "NULL::bool");
        assert_eq!(
            x("i4", "!BOUND(?nothing) && BOUND(?x)"),
            "(NOT (false) AND true)"
        );
    }

    #[test]
    fn str_lang_and_datatype_are_strings_and_iris() {
        assert_eq!(
            x("txt", "STR(?x) = \"a\""),
            "((t0.\"txt\"::text) = $1::text)"
        );
        // An integer's lexical form needs no IRI-safe encoding.
        assert_eq!(
            x("link", "STR(?x) < \"b\""),
            "($1 || t0.\"i4\"::text) COLLATE \"C\" < $2::text"
        );
        let iri = x("textLink", "STR(?x) < \"b\"");
        assert!(
            iri.starts_with("($1 || CASE WHEN t0.\"txt\"::text ~ '^["),
            "{iri}"
        );
        assert!(iri.ends_with(" END) COLLATE \"C\" < $2::text"), "{iri}");
        // LANG gives the tag, lower-cased as in the results, or "".
        assert_eq!(x("label", "LANG(?x) = \"en\""), "($1::text = $2::text)");
        assert_eq!(x("i4", "LANG(?x)"), "($1::text <> '')");
        assert_eq!(
            x("i4", "DATATYPE(?x) = xsd:integer"),
            "($1::text = $2::text)"
        );
        // Infinite timestamps are plain literals.
        assert_eq!(
            x("ts", "DATATYPE(?x) = xsd:dateTime"),
            "(CASE WHEN isfinite(t0.\"ts\") THEN $1::text ELSE $2::text END = $3::text)"
        );
        // Type errors: STR of a blank node does not exist here, LANG and
        // DATATYPE of an IRI do.
        assert_eq!(x("link", "LANG(?x) = \"\""), "NULL::bool");
        assert_eq!(x("link", "isLiteral(DATATYPE(?x))"), "NULL::bool");
        assert_eq!(
            x("i4", "isIRI(DATATYPE(?x)) && isLiteral(STR(?x))"),
            "(true AND true)"
        );
        assert_eq!(x("i4", "sameTerm(STR(?x), ?x)"), "false");
        assert_eq!(
            x("txt", "sameTerm(STR(?x), ?x)"),
            "((t0.\"txt\"::text) = (t0.\"txt\"::text))"
        );
        // A string of a float column has no exact SQL.
        assert!(rejected("?s ex:f8 ?x", "STR(?x) = \"1\"").contains("float8"));
    }

    #[test]
    fn regex_matches_string_literals_under_the_c_collation() {
        let (sql, params) = {
            let types: ColumnTypes = COLUMNS
                .iter()
                .map(|(c, ty)| {
                    (
                        (LogicalTable::TableName("t".into()), (*c).to_owned()),
                        column_type(ty),
                    )
                })
                .collect();
            let rendered =
                plan("SELECT ?s WHERE { ?s ex:label ?x FILTER(REGEX(?x, \"^a.c\", \"i\")) }")
                    .unwrap()
                    .to_sql_with(
                        &PostgresDialect,
                        &RenderOptions {
                            column_types: Some(&types),
                            ..RenderOptions::default()
                        },
                    )
                    .unwrap();
            (rendered.sql, rendered.params)
        };
        assert!(
            sql.contains("((t0.\"txt\"::text) COLLATE \"C\" ~ $1::text)"),
            "{sql}"
        );
        assert_eq!(params, ["^[Aa][^\\u000A\\u000D][Cc]"]);
        assert_eq!(x("i4", "REGEX(?x, \"1\")"), "NULL::bool");
        assert_eq!(x("link", "REGEX(?x, \"1\")"), "NULL::bool");
        assert_eq!(x("txt", "REGEX(?x, \"(\")"), "NULL::bool");
        assert_eq!(x("txt", "REGEX(?x, \"a\"@en)"), "NULL::bool");
        assert!(rejected("?s ex:txt ?x", "REGEX(?x, \"\\\\d\")").contains("\\d"));
        assert!(matches!(
            render_with("SELECT ?s WHERE { ?s ex:txt ?x FILTER(REGEX(?x, \"a\")) }", true),
            Err(Error::UnsupportedFeature(reason)) if reason.contains("UTF-8")
        ));
    }

    #[test]
    fn in_is_a_disjunction_of_equalities_on_the_column() {
        // Each comparison keeps the column as it is, so that an index on
        // the column can be used.
        assert_eq!(
            x("i4", "?x IN (1, 2, 3)"),
            "((t0.\"i4\" = $1::int8 OR t0.\"i4\" = $2::int8) OR t0.\"i4\" = $3::int8)"
        );
        assert_eq!(
            x(
                "link",
                "?x IN (<http://example.org/r/1>, <http://example.org/r/2>)"
            ),
            "((t0.\"i4\" = $1) OR (t0.\"i4\" = $2))"
        );
        assert_eq!(
            x("i4", "?x NOT IN (1, 2)"),
            "NOT ((t0.\"i4\" = $1::int8 OR t0.\"i4\" = $2::int8))"
        );
        // An empty list (SPARQL 1.1 §17.4.1.9, §17.4.1.10).
        assert_eq!(x("i4", "?x IN ()"), "false");
        assert_eq!(x("i4", "?x NOT IN ()"), "NOT (false)");
        // A string compared with a number is an error, which only a true
        // comparison overrules, so "b" NOT IN ("a", 1) is an error.
        assert_eq!(
            x("txt", "?x IN (\"a\", 1)"),
            "((t0.\"txt\" = $1) OR NULL::bool)"
        );
        assert_eq!(
            x("txt", "?x NOT IN (\"a\", 1)"),
            "NOT ((coalesce((t0.\"txt\" = $1), false) OR NULL::bool))"
        );
        assert_eq!(x("i4", "?nothing IN (1)"), "NULL::bool");
        assert!(rejected("?s ex:label ?x", "?x NOT IN (\"a\"@en)").contains("under negation"));
    }

    #[test]
    fn if_takes_the_branch_its_condition_chooses() {
        // Where the condition is an error, neither branch is taken
        // (SPARQL 1.1 §17.4.1.2).
        assert_eq!(
            x("i4", "IF(?x > 2, ?x, 0) > 1"),
            "CASE WHEN t0.\"i4\" > $1::int8 THEN t0.\"i4\" > $2::int8 \
             WHEN NOT (t0.\"i4\" > $3::int8) THEN $4::int8 > $5::int8 END"
        );
        assert_eq!(
            x("i4", "IF(?x, \"\", \"b\")"),
            "CASE WHEN t0.\"i4\" <> 0 THEN false WHEN NOT (t0.\"i4\" <> 0) THEN true END"
        );
        // A condition known without the rows takes one branch, and the
        // other is not evaluated.
        assert_eq!(x("i4", "IF(BOUND(?x), ?x, 0) > 1"), "t0.\"i4\" > $1::int8");
        assert_eq!(
            x("txt", "IF(!BOUND(?x), ?x + 1, 2) = 2"),
            "$1::int8 = $2::int8"
        );
        assert_eq!(x("i4", "IF(?nothing, 1, 0) = 1"), "NULL::bool");
    }

    #[test]
    fn coalesce_takes_the_first_argument_that_is_not_an_error() {
        // A division by zero is an error, which COALESCE skips (SPARQL 1.1
        // §17.4.1.3).
        let skipped = "CASE WHEN ((t0.\"i4\"::numeric / NULLIF($1::numeric, 0)) IS NOT NULL) \
                       THEN (t0.\"i4\"::numeric / NULLIF($2::numeric, 0)) = $3::numeric \
                       WHEN true THEN $4::int8 = $5::int8 END";
        assert_eq!(x("i4", "COALESCE(?x / 0, 5) = 5"), skipped);
        assert_eq!(
            x("i4", "!(COALESCE(?x / 0, 5) = 5)"),
            format!("NOT ({skipped})")
        );
        // An unbound variable is an error; a bound one never is.
        assert_eq!(
            x("i4", "COALESCE(?nothing, ?x) = 5"),
            "t0.\"i4\" = $1::int8"
        );
        // Arithmetic over a string, and LANG of an IRI, are errors in
        // every row.
        assert_eq!(
            x("txt", "COALESCE(?x + 1, 7) = 7"),
            "CASE WHEN false THEN NULL::bool WHEN true THEN $1::int8 = $2::int8 END"
        );
        assert!(
            x("link", "COALESCE(LANG(?x), \"none\") = \"none\"")
                .starts_with("CASE WHEN false THEN NULL::bool WHEN true THEN ")
        );
        assert_eq!(
            x("txt", "COALESCE(STR(?x), 1) = \"a\""),
            "CASE WHEN true THEN ((t0.\"txt\"::text) = $1::text) WHEN true THEN NULL::bool END"
        );
        assert_eq!(x("i4", "COALESCE() = 1"), "NULL::bool");
        // A double is not an error unless an operand is one.
        let sql = x("f8", "COALESCE(?x * 2, 0) > 1");
        assert!(sql.starts_with("CASE WHEN (CASE WHEN "), "{sql}");
        assert!(
            sql.ends_with(
                "ELSE t0.\"f8\" * $2::float8 END AS x) AS d) WHEN true THEN $4::int8 > $5::int8 END"
            ),
            "{sql}"
        );
        assert!(rejected("?s ex:f4 ?x", "COALESCE(?x + 1, 0) > 0").contains("float4"));
    }

    #[test]
    fn numeric_functions_of_integers_and_decimals_are_computed_in_numeric() {
        // An integer is its own rounding, ceiling and floor, and its
        // absolute value is an integer, compared as one.
        assert_eq!(x("i4", "ROUND(?x) = 2"), "t0.\"i4\"::numeric = $1::int8");
        assert_eq!(x("i4", "CEIL(?x) > 2"), "t0.\"i4\"::numeric > $1::int8");
        assert_eq!(x("i4", "FLOOR(?x) = ?x"), "t0.\"i4\"::numeric = t0.\"i4\"");
        assert_eq!(x("i8", "ABS(?x) = 2"), "abs(t0.\"i8\"::numeric) = $1::int8");
        // A decimal rounds halves towards positive infinity (fn:round),
        // which PostgreSQL's round(numeric) does not.
        assert_eq!(
            x("i4", "ROUND(?x / 2) = 1"),
            "floor((t0.\"i4\"::numeric / NULLIF($1::numeric, 0)) + 0.5) = $2::numeric"
        );
        let decimal = "CASE WHEN t0.\"dec\"::text NOT IN ('NaN', 'Infinity', '-Infinity') THEN \
                       t0.\"dec\" END";
        assert_eq!(
            x("dec", "CEIL(?x) = 3"),
            format!("ceil({decimal}) = $1::numeric")
        );
        assert_eq!(
            x("dec", "FLOOR(?x) < 3"),
            format!("floor({decimal}) < $1::numeric")
        );
        assert_eq!(
            x("dec", "ABS(?x) > 1"),
            format!("abs({decimal}) > $1::numeric")
        );
        assert_eq!(x("i4", "ROUND(?x)"), "(t0.\"i4\"::numeric <> 0)");
        assert_eq!(x("i4", "isLiteral(ABS(?x))"), "true");
        // A division by zero is an error, which COALESCE skips.
        assert_eq!(
            x("i4", "COALESCE(ROUND(?x / 0), 1) = 1"),
            "CASE WHEN (floor((t0.\"i4\"::numeric / NULLIF($1::numeric, 0)) + 0.5) IS NOT NULL) \
             THEN floor((t0.\"i4\"::numeric / NULLIF($2::numeric, 0)) + 0.5) = $3::numeric \
             WHEN true THEN $4::int8 = $5::int8 END"
        );
    }

    #[test]
    fn numeric_functions_of_anything_but_numbers_are_type_errors() {
        assert_eq!(x("txt", "ROUND(?x) = 3"), "NULL::bool");
        assert_eq!(x("txt", "!(ABS(?x) > 0)"), "NOT (NULL::bool)");
        assert_eq!(x("link", "CEIL(?x) = 1"), "NULL::bool");
        assert_eq!(x("i4", "FLOOR(\"1\") = ?x"), "NULL::bool");
        // A number is not a string.
        assert_eq!(x("i4", "ROUND(?x) = \"a\""), "NULL::bool");
        assert!(rejected("?s ex:f4 ?x", "ABS(?x) > 0").contains("float4"));
        assert!(rejected("?s ex:i4 ?x", "STR(ROUND(?x)) = \"1\"").contains("STR, LANG"));
    }

    #[test]
    fn numeric_functions_of_doubles_keep_nan_infinities_and_signed_zeros() {
        // abs, ceil and floor of a float8 cannot fail and are cheap to
        // repeat.
        assert_eq!(
            x("f8", "ABS(?x) = 3"),
            "(abs(t0.\"f8\") = $1::float8 AND abs(t0.\"f8\") <> 'NaN'::float8)"
        );
        assert_eq!(
            x("f8", "FLOOR(?x) < 0"),
            "(floor(t0.\"f8\") < $1::float8 AND floor(t0.\"f8\") <> 'NaN'::float8)"
        );
        assert_eq!(
            x("f8", "CEIL(?x) < 0"),
            "(ceil(t0.\"f8\") < $1::float8 AND ceil(t0.\"f8\") <> 'NaN'::float8)"
        );
        // ROUND is computed once, and rounds halves towards positive
        // infinity, where round(float8) rounds them to even.
        assert_eq!(
            x("f8", "ROUND(?x) = 3"),
            "(SELECT (x = $1::float8 AND x <> 'NaN'::float8) FROM (SELECT CASE WHEN NOT \
             (abs(t0.\"f8\") < '4.503599627370496e15'::float8) THEN t0.\"f8\" WHEN t0.\"f8\" < 0 \
             AND t0.\"f8\" >= '-5e-1'::float8 THEN '-0e0'::float8 WHEN t0.\"f8\" - \
             floor(t0.\"f8\") >= '5e-1'::float8 THEN floor(t0.\"f8\") + 1 ELSE floor(t0.\"f8\") \
             END AS x) AS d)"
        );
        // An integer argument is rounded in numeric, then promoted.
        let sql = x("i4", "ROUND(?x / 2) < 1e0");
        assert!(
            sql.contains(
                "FROM (SELECT floor((t0.\"i4\"::numeric / NULLIF($1::numeric, 0)) + 0.5) AS x) \
                 AS d)"
            ),
            "{sql}"
        );
    }

    #[test]
    fn numeric_functions_of_double_constants_are_computed_before_binding() {
        for (constant, rounded) in [
            ("2.5e0", "3.0E0"),
            ("-2.5e0", "-2.0E0"),
            ("0.5e0", "1.0E0"),
            ("-0.5e0", "-0.0E0"),
            ("-0.2e0", "-0.0E0"),
            ("0.49999999999999994e0", "0.0E0"),
            ("4503599627370497e0", "4.503599627370497E15"),
            ("\"NaN\"^^xsd:double", "NaN"),
            ("\"-INF\"^^xsd:double", "-INF"),
        ] {
            assert_eq!(
                params(&format!(
                    "SELECT ?s WHERE {{ ?s ex:i4 ?x FILTER(?x < ROUND({constant})) }}"
                ))[0],
                rounded,
                "{constant}"
            );
        }
        assert_eq!(
            params("SELECT ?s WHERE { ?s ex:i4 ?x FILTER(?x < CEIL(-0.5e0)) }")[0],
            "-0.0E0"
        );
        assert_eq!(
            params("SELECT ?s WHERE { ?s ex:i4 ?x FILTER(?x < ABS(-0e0)) }")[0],
            "0.0E0"
        );
        assert_eq!(
            params("SELECT ?s WHERE { ?s ex:i4 ?x FILTER(?x < FLOOR(-1.5e0)) }")[0],
            "-2.0E0"
        );
    }

    /// The parameters of `FILTER(filter)` over `?s ex:txt ?x`.
    fn filter_params(filter: &str) -> Vec<String> {
        params(&format!(
            "SELECT ?s WHERE {{ ?s ex:txt ?x FILTER({filter}) }}"
        ))
    }

    #[test]
    fn strlen_counts_the_characters_of_string_literals() {
        let length = "char_length((t0.\"txt\"::text))::numeric";
        assert_eq!(x("txt", "STRLEN(?x) > 3"), format!("{length} > $1::int8"));
        assert_eq!(x("label", "STRLEN(?x) > 3"), format!("{length} > $1::int8"));
        // Computed in numeric, so that a quotient is a decimal.
        assert_eq!(
            x("txt", "STRLEN(?x) / 2 > 3"),
            format!("({length} / NULLIF($1::numeric, 0)) > $2::numeric")
        );
        assert_eq!(x("txt", "STRLEN(?x)"), format!("({length} <> 0)"));
        // Anything but a string literal is a type error (SPARQL 1.1
        // §17.4.3.1.1), also a number's lexical form.
        assert_eq!(x("i4", "STRLEN(?x) > 3"), "NULL::bool");
        assert_eq!(x("link", "STRLEN(?x) > 3"), "NULL::bool");
        assert_eq!(x("i4", "STRLEN(STRLEN(?x)) > 3"), "NULL::bool");
        assert_eq!(
            x("link", "STRLEN(STR(?x)) = 3"),
            "char_length(($1 || t0.\"i4\"::text))::numeric = $2::int8"
        );
        assert_eq!(
            x("txt", "COALESCE(STRLEN(?nothing), 0) = 0"),
            "$1::int8 = $2::int8"
        );
    }

    #[test]
    fn substr_takes_the_positions_of_fn_substring() {
        assert_eq!(
            x("txt", "SUBSTR(?x, 2) = \"bc\""),
            "((substr((t0.\"txt\"::text), $1::int4) COLLATE \"C\") = $2::text)"
        );
        assert_eq!(
            x("label", "SUBSTR(?x, 2, 1) = \"b\"@en"),
            "((substr((t0.\"txt\"::text), $1::int4, $2::int4) COLLATE \"C\") = $3::text)"
        );
        // The positions p with start <= p < start + length, from 1.
        for (args, expected) in [
            ("2", ["2", ""]),
            ("0, 3", ["1", "2"]),
            ("-5, 7", ["1", "1"]),
            ("5, -3", ["5", "0"]),
            ("2, 0", ["2", "0"]),
            ("99999999999999999999999", ["2147483647", ""]),
            ("1, 99999999999999999999999", ["1", "2147483646"]),
            (
                "-99999999999999999999999, 99999999999999999999999",
                ["1", "0"],
            ),
            (
                "-99999999999999999999999, 100000000000000000000002",
                ["1", "2"],
            ),
            (
                "170141183460469231731687303715884105727, 170141183460469231731687303715884105727",
                ["2147483647", "0"],
            ),
            (
                "-170141183460469231731687303715884105727, 170141183460469231731687303715884105727",
                ["1", "0"],
            ),
            ("3000000000, 1", ["2147483647", "0"]),
            ("\"7\"^^xsd:integer, 2", ["7", "2"]),
        ] {
            let mut expected: Vec<String> = expected
                .iter()
                .filter(|p| !p.is_empty())
                .map(|p| (*p).to_owned())
                .collect();
            expected.push("x".to_owned());
            assert_eq!(
                filter_params(&format!("SUBSTR(?x, {args}) = \"x\"")),
                expected,
                "{args}"
            );
        }
        // The arguments are xsd:integer, anything else a type error.
        assert_eq!(x("txt", "SUBSTR(?x, \"1\") = \"a\""), "NULL::bool");
        assert_eq!(
            x("txt", "SUBSTR(?x, 1, <http://example.org/a>) = \"a\""),
            "NULL::bool"
        );
        assert_eq!(
            x("txt", "SUBSTR(?x, \"one\"^^xsd:integer) = \"a\""),
            "NULL::bool"
        );
        assert_eq!(x("i4", "SUBSTR(?x, 1) = \"1\""), "NULL::bool");
        assert!(rejected("?s ex:txt ?x", "SUBSTR(?x, 1.5) = \"a\"").contains("xsd:integer"));
        assert!(rejected("?s ex:txt ?x", "SUBSTR(?x, 1, 2e0) = \"a\"").contains("round"));
        assert!(rejected("?s ex:txt ?x", "SUBSTR(?x, \"1\"^^xsd:int) = \"a\"").contains("#int>"));
        assert!(
            rejected(
                "?s ex:txt ?x",
                "SUBSTR(?x, 1701411834604692317316873037158841057270)"
            )
            .contains("128-bit")
        );
    }

    #[test]
    fn case_mappings_do_not_depend_on_the_database_locale() {
        assert_eq!(
            x("txt", "UCASE(?x) = \"AB\""),
            "((upper((t0.\"txt\"::text) COLLATE \"und-x-icu\") COLLATE \"C\") = $1::text)"
        );
        assert_eq!(
            x("txt", "LCASE(?x) < \"b\""),
            "(lower((t0.\"txt\"::text) COLLATE \"und-x-icu\") COLLATE \"C\") COLLATE \"C\" < \
             $1::text"
        );
        // The result keeps the language tag (SPARQL 1.1 §17.4.3.4).
        assert_eq!(
            x("label", "UCASE(?x) = \"AB\"@en"),
            "((upper((t0.\"txt\"::text) COLLATE \"und-x-icu\") COLLATE \"C\") = $1::text)"
        );
        assert_eq!(x("label", "UCASE(?x) = \"AB\""), "NULL::bool");
        assert_eq!(
            x("label", "LANG(LCASE(?x)) = \"en\""),
            "($1::text = $2::text)"
        );
        assert_eq!(
            x(
                "label",
                "DATATYPE(LCASE(?x)) = <http://www.w3.org/1999/02/22-rdf-syntax-ns#langString>"
            ),
            "($1::text = $2::text)"
        );
        assert_eq!(x("txt", "isLiteral(UCASE(?x))"), "true");
        assert_eq!(x("i4", "isLiteral(UCASE(?x))"), "NULL::bool");
        assert_eq!(x("i4", "UCASE(?x) = \"1\""), "NULL::bool");
        assert_eq!(x("link", "LCASE(?x) = \"a\""), "NULL::bool");
        // Under negation, a type error stays an error.
        assert_eq!(x("i4", "!(UCASE(?x) = \"1\")"), "NOT (NULL::bool)");
        assert!(rejected("?s ex:label ?x", "!(UCASE(?x) = \"A\"@en)").contains("under negation"));
        assert_eq!(
            x("txt", "UCASE(?x)"),
            "((upper((t0.\"txt\"::text) COLLATE \"und-x-icu\") COLLATE \"C\") <> '')"
        );
    }

    #[test]
    fn string_tests_need_compatible_arguments() {
        let like = "((t0.\"txt\"::text) COLLATE \"C\" LIKE $1::text)";
        assert_eq!(x("txt", "STRSTARTS(?x, \"a\")"), like);
        assert_eq!(x("label", "STRSTARTS(?x, \"a\")"), like);
        assert_eq!(x("label", "STRSTARTS(?x, \"a\"@EN)"), like);
        assert_eq!(x("label", "STRSTARTS(?x, \"a\"^^xsd:string)"), like);
        // SPARQL 1.1 §17.4.3.1.2: a language tag on the second argument
        // only with the same one on the first.
        assert_eq!(x("label", "STRSTARTS(?x, \"a\"@fr)"), "NULL::bool");
        assert_eq!(x("txt", "STRENDS(?x, \"a\"@en)"), "NULL::bool");
        assert_eq!(x("label", "CONTAINS(\"abc\", ?x)"), "NULL::bool");
        assert_eq!(x("i4", "CONTAINS(?x, \"1\")"), "NULL::bool");
        assert_eq!(x("txt", "CONTAINS(?x, 1)"), "NULL::bool");
        assert_eq!(x("link", "STRSTARTS(?x, \"http\")"), "NULL::bool");
        assert_eq!(x("txt", "!CONTAINS(?x, 1)"), "NOT (NULL::bool)");
        assert_eq!(
            x("link", "STRSTARTS(STR(?x), \"http\")"),
            "(($1 || t0.\"i4\"::text) COLLATE \"C\" LIKE $2::text)"
        );
        // The wildcards and the escape character of a constant stand for
        // themselves.
        for (filter, pattern) in [
            ("STRSTARTS(?x, \"a%_\\\\b\")", "a\\%\\_\\\\b%"),
            ("STRENDS(?x, \"%\")", "%\\%"),
            ("CONTAINS(?x, \"_\")", "%\\_%"),
            ("CONTAINS(?x, \"\")", "%%"),
            ("STRSTARTS(?x, \"\u{e9}\u{1F600}\")", "\u{e9}\u{1F600}%"),
        ] {
            assert_eq!(filter_params(filter), [pattern], "{filter}");
        }
        // A computed string to search for.
        assert_eq!(
            x("txt", "STRSTARTS(?x, ?x)"),
            "(starts_with((t0.\"txt\"::text) COLLATE \"C\", (t0.\"txt\"::text) COLLATE \"C\"))"
        );
        assert_eq!(
            x("txt", "STRENDS(?x, ?x)"),
            "(right((t0.\"txt\"::text) COLLATE \"C\", char_length((t0.\"txt\"::text) COLLATE \
             \"C\")) = (t0.\"txt\"::text) COLLATE \"C\")"
        );
        assert_eq!(
            x("txt", "CONTAINS(UCASE(?x), STR(?x))"),
            "(strpos((upper((t0.\"txt\"::text) COLLATE \"und-x-icu\") COLLATE \"C\") COLLATE \
             \"C\", (t0.\"txt\"::text) COLLATE \"C\") > 0)"
        );
        assert!(rejected("?s ex:txt ?x", "CONTAINS(?x, \"a\\u0000\")").contains("NUL character"));
    }

    #[test]
    fn concat_keeps_a_language_tag_that_all_arguments_have() {
        assert_eq!(
            x("txt", "CONCAT(?x, \"-\", ?x) = \"a-a\""),
            "((((t0.\"txt\"::text) || $1::text || (t0.\"txt\"::text)) COLLATE \"C\") = \
             $2::text)"
        );
        // SPARQL 1.1 §17.4.3.12.
        assert_eq!(
            x("label", "CONCAT(?x, \"a\"@en) = \"a\"@en"),
            "((((t0.\"txt\"::text) || $1::text) COLLATE \"C\") = $2::text)"
        );
        assert_eq!(x("label", "CONCAT(?x, \"a\") = \"a\"@en"), "NULL::bool");
        assert_eq!(x("label", "CONCAT(?x, \"a\"@fr) = \"a\"@en"), "NULL::bool");
        assert_eq!(
            x("label", "LANG(CONCAT(?x, \"a\")) = \"\""),
            "($1::text = $2::text)"
        );
        assert_eq!(x("txt", "CONCAT() = \"\""), "($1::text = $2::text)");
        assert_eq!(x("txt", "CONCAT(?x, 1) = \"a1\""), "NULL::bool");
        assert_eq!(x("txt", "CONCAT(?x, ?nothing) = \"a\""), "NULL::bool");
    }

    #[test]
    fn functions_on_strings_that_sql_cannot_answer_exactly_are_rejected() {
        for filter in [
            "STRLEN(?x) > 1",
            "UCASE(?x) = \"A\"",
            "STRSTARTS(?x, \"a\")",
            "CONCAT(?x, \"a\") = \"aa\"",
        ] {
            assert!(
                matches!(
                    render_with(&format!("SELECT ?s WHERE {{ ?s ex:txt ?x FILTER({filter}) }}"), true),
                    Err(Error::UnsupportedFeature(reason)) if reason.contains("UTF-8")
                ),
                "{filter}"
            );
            // Infinite timestamps are plain literals.
            assert!(
                rejected("?s ex:ts ?x", filter).contains("infinite"),
                "{filter}"
            );
        }
        assert!(rejected("?s ex:bpc ?x", "UCASE(?x) = \"A\"").contains("bpchar"));
    }

    #[test]
    fn date_time_functions_read_timestamptz_in_utc() {
        // A timestamp has no time zone; a timestamptz is written in UTC
        // with `Z`, so its components are those in UTC.
        assert_eq!(
            x("ts", "HOURS(?x) = 3"),
            "CASE WHEN isfinite(t0.\"ts\") THEN extract(hour FROM t0.\"ts\")::numeric END = $1::int8"
        );
        assert_eq!(
            x("tz", "MINUTES(?x) = 3"),
            "CASE WHEN isfinite(t0.\"tz\") THEN extract(minute FROM (t0.\"tz\" AT TIME ZONE \
             'UTC'))::numeric END = $1::int8"
        );
        // The year before 1 is 0000 in the engine's lexical forms.
        assert_eq!(
            x("ts", "YEAR(?x) = 0"),
            "CASE WHEN isfinite(t0.\"ts\") THEN CASE WHEN t0.\"ts\" < \
             '0001-01-01T00:00:00'::timestamp THEN extract(year FROM t0.\"ts\")::numeric + 1 \
             ELSE extract(year FROM t0.\"ts\")::numeric END END = $1::int8"
        );
        // SECONDS is a decimal with the fraction.
        assert_eq!(
            x("ts", "SECONDS(?x) = 1.5"),
            "CASE WHEN isfinite(t0.\"ts\") THEN extract(second FROM t0.\"ts\")::numeric END = \
             $1::numeric"
        );
        assert!(x("ts", "SECONDS(?x) < 1e0").contains("extract(second FROM t0.\"ts\")"));
        assert_eq!(
            x("tz", "MONTH(?x) + DAY(?x) > 2"),
            "(CASE WHEN isfinite(t0.\"tz\") THEN extract(month FROM (t0.\"tz\" AT TIME ZONE \
             'UTC'))::numeric END + CASE WHEN isfinite(t0.\"tz\") THEN extract(day FROM \
             (t0.\"tz\" AT TIME ZONE 'UTC'))::numeric END) > $1::int8"
        );
        // TZ is the zone the engine writes, TIMEZONE its duration.
        assert_eq!(
            x("tz", "TZ(?x) = \"Z\""),
            "(CASE WHEN isfinite(t0.\"tz\") THEN $1::text END = $2::text)"
        );
        assert_eq!(
            params("SELECT ?s WHERE { ?s ex:tz ?x FILTER(TZ(?x) = \"Z\") }"),
            ["Z", "Z"]
        );
        assert_eq!(
            params("SELECT ?s WHERE { ?s ex:ts ?x FILTER(TZ(?x) = \"\") }"),
            ["", ""]
        );
        assert_eq!(
            params("SELECT ?s WHERE { ?s ex:tz ?x FILTER(STR(TIMEZONE(?x)) = \"PT0S\") }"),
            ["PT0S", "PT0S"]
        );
        assert_eq!(
            params(
                "SELECT ?s WHERE { ?s ex:tz ?x \
                 FILTER(DATATYPE(TIMEZONE(?x)) = xsd:dayTimeDuration) }"
            )[0],
            format!("{XSD}dayTimeDuration")
        );
        assert_eq!(
            x(
                "tz",
                "sameTerm(TIMEZONE(?x), \"PT0S\"^^xsd:dayTimeDuration)"
            ),
            "(CASE WHEN isfinite(t0.\"tz\") THEN $1::text END = $2::text)"
        );
        assert_eq!(
            x("tz", "TZ(?x)"),
            "(CASE WHEN isfinite(t0.\"tz\") THEN $1::text END <> '')"
        );
    }

    #[test]
    fn date_time_functions_of_infinite_values_are_errors() {
        assert_eq!(
            x("tz", "isLiteral(TIMEZONE(?x))"),
            "CASE WHEN isfinite(t0.\"tz\") THEN true END"
        );
        assert_eq!(
            x("ts", "!isLiteral(YEAR(?x))"),
            "NOT (CASE WHEN isfinite(t0.\"ts\") THEN true END)"
        );
        assert_eq!(
            x("ts", "COALESCE(TZ(?x), \"none\") = \"none\""),
            "CASE WHEN isfinite(t0.\"ts\") THEN (CASE WHEN isfinite(t0.\"ts\") THEN $1::text END \
             = $2::text) WHEN true THEN true END"
        );
        let sql = x("ts", "COALESCE(DAY(?x), 0) = 0");
        assert!(
            sql.starts_with("CASE WHEN (CASE WHEN isfinite(t0.\"ts\") THEN extract(day"),
            "{sql}"
        );
    }

    #[test]
    fn date_time_functions_of_anything_but_date_times_are_type_errors() {
        for (column, filter) in [
            ("txt", "YEAR(?x) = 1"),
            ("i4", "HOURS(?x) = 1"),
            ("b", "TZ(?x) = \"\""),
            ("link", "SECONDS(?x) > 0"),
            ("dec", "STR(TIMEZONE(?x)) = \"PT0S\""),
            // A timestamp has no time zone.
            ("ts", "STR(TIMEZONE(?x)) = \"PT0S\""),
            ("ts", "isLiteral(TIMEZONE(?x))"),
            // A number is not a string.
            ("tz", "YEAR(?x) = \"2024\""),
            ("tz", "TZ(?x) = 0"),
            ("i4", "YEAR(STR(NOW())) = 1"),
        ] {
            assert_eq!(x(column, filter), "NULL::bool", "{column}: {filter}");
        }
        assert_eq!(x("ts", "!(TZ(YEAR(?x)) = \"\")"), "NOT (NULL::bool)");
        assert_eq!(x("tz", "TIMEZONE(?x)"), "NULL::bool");
        assert!(rejected("?s ex:tz ?x", "STR(YEAR(?x)) = \"1\"").contains("STR, LANG"));
        assert!(rejected("?s ex:tz ?x", "sameTerm(YEAR(?x), 1)").contains("sameTerm"));
        assert!(
            rejected(
                "?s ex:tz ?x",
                "TIMEZONE(?x) = \"PT0S\"^^xsd:dayTimeDuration"
            )
            .contains("dayTimeDuration")
        );
    }

    #[test]
    fn date_time_functions_of_dates_and_times_are_rejected() {
        for (patterns, filter) in [
            ("?s ex:d ?x", "YEAR(?x) = 2024"),
            ("?s ex:d ?x", "TZ(?x) = \"\""),
            ("?s ex:i4 ?x", "MONTH(\"2024-01-01\"^^xsd:date) = ?x"),
            ("?s ex:i4 ?x", "HOURS(\"10:00:00\"^^xsd:time) = ?x"),
        ] {
            assert!(
                rejected(patterns, filter).contains("xsd:date and xsd:time"),
                "{filter}"
            );
        }
        assert!(
            rejected(
                "?s ex:i4 ?x",
                "YEAR(\"10000-01-01T00:00:00\"^^xsd:dateTime) = ?x"
            )
            .contains("after year 9999")
        );
    }

    #[test]
    fn date_time_functions_of_constants_are_computed_in_their_time_zone() {
        let one = |function: &str, constant: &str| {
            params(&format!(
                "SELECT ?s WHERE {{ ?s ex:i4 ?x \
                 FILTER(?x = {function}(\"{constant}\"^^xsd:dateTime)) }}"
            ))
        };
        let zoned = "2024-01-01T02:30:07.250-05:30";
        assert_eq!(one("YEAR", zoned), ["2024"]);
        assert_eq!(one("MONTH", zoned), ["1"]);
        assert_eq!(one("DAY", zoned), ["1"]);
        assert_eq!(one("HOURS", zoned), ["2"]);
        assert_eq!(one("MINUTES", zoned), ["30"]);
        assert_eq!(one("SECONDS", zoned), ["7.25"]);
        // 24:00:00 is the first instant of the next day.
        let end = "1999-12-31T24:00:00";
        assert_eq!(one("YEAR", end), ["2000"]);
        assert_eq!(one("MONTH", end), ["1"]);
        assert_eq!(one("HOURS", end), ["0"]);
        assert_eq!(one("SECONDS", end), ["0"]);
        let text = |function: &str, constant: &str| {
            params(&format!(
                "SELECT ?s WHERE {{ ?s ex:txt ?x \
                 FILTER(?x = STR({function}(\"{constant}\"^^xsd:dateTime))) }}"
            ))
        };
        assert_eq!(text("TZ", zoned), ["-05:30"]);
        assert_eq!(text("TIMEZONE", zoned), ["-PT5H30M"]);
        assert_eq!(text("TZ", "2024-01-01T00:00:00Z"), ["Z"]);
        assert_eq!(text("TIMEZONE", "2024-01-01T00:00:00Z"), ["PT0S"]);
        assert_eq!(text("TZ", "2024-01-01T00:00:00+00:00"), ["+00:00"]);
        assert_eq!(text("TIMEZONE", "2024-01-01T00:00:00+14:00"), ["PT14H"]);
        assert_eq!(text("TZ", end), [""]);
        // Without a time zone, TIMEZONE is an error.
        assert_eq!(
            x(
                "txt",
                "?x = STR(TIMEZONE(\"2024-01-01T00:00:00\"^^xsd:dateTime))"
            ),
            "NULL::bool"
        );
        // Ill-typed literals and other terms are errors.
        for constant in [
            "\"2024-13-01T00:00:00\"^^xsd:dateTime",
            "\"2024-01-01T00:00:00\"",
            "<http://example.org/t>",
            "1",
        ] {
            assert_eq!(
                x("i4", &format!("?x = YEAR({constant})")),
                "NULL::bool",
                "{constant}"
            );
        }
    }

    #[test]
    fn now_is_one_utc_parameter_per_rendering() {
        assert_eq!(
            x("tz", "?x < NOW()"),
            "(t0.\"tz\" < $1::timestamptz AND isfinite(t0.\"tz\"))"
        );
        let values = params(
            "SELECT ?s WHERE { ?s ex:tz ?x FILTER(?x < NOW() && ?x <= NOW() \
             && STR(NOW()) != \"\") }",
        );
        assert_eq!(values.len(), 4, "{values:?}");
        assert!(values[..3].iter().all(|v| v == &values[0]), "{values:?}");
        assert!(
            matches!(
                xsd::value(&values[0], &format!("{XSD}dateTime")),
                Value::DateTime(xsd::DateTime { zoned: true, .. })
            ),
            "{values:?}"
        );
        assert!(values[0].ends_with('Z'), "{values:?}");
        assert_eq!(
            x("i4", "YEAR(NOW()) > ?x"),
            "CASE WHEN ($1::timestamptz AT TIME ZONE 'UTC') < '0001-01-01T00:00:00'::timestamp \
             THEN extract(year FROM ($1::timestamptz AT TIME ZONE 'UTC'))::numeric + 1 ELSE \
             extract(year FROM ($1::timestamptz AT TIME ZONE 'UTC'))::numeric END > t0.\"i4\""
        );
        assert_eq!(
            params("SELECT ?s WHERE { ?s ex:txt ?x FILTER(TZ(NOW()) = ?x) }"),
            ["Z"]
        );
        assert_eq!(
            params("SELECT ?s WHERE { ?s ex:txt ?x FILTER(STR(TIMEZONE(NOW())) = ?x) }"),
            ["PT0S"]
        );
        assert_eq!(
            params("SELECT ?s WHERE { ?s ex:txt ?x FILTER(STR(DATATYPE(NOW())) = ?x) }"),
            [format!("{XSD}dateTime")]
        );
        // The effective boolean value of a dateTime is an error.
        assert_eq!(x("i4", "NOW()"), "NULL::bool");
        assert!(rejected("?s ex:ts ?x", "?x < NOW()").contains("implicit time zone"));
        assert!(rejected("?s ex:tz ?x", "sameTerm(?x, NOW())").contains("sameTerm of NOW"));
    }

    #[test]
    fn strbefore_and_strafter_drop_the_language_tag_without_a_match() {
        let position = "strpos((t0.\"txt\"::text) COLLATE \"C\", $1::text COLLATE \"C\")";
        assert_eq!(
            x("txt", "STRBEFORE(?x, \"b\") = \"a\""),
            format!(
                "((CASE WHEN {position} > 0 THEN substr((t0.\"txt\"::text) COLLATE \"C\", 1, \
                 {position} - 1) ELSE '' END COLLATE \"C\") = $2::text)"
            )
        );
        assert_eq!(
            x("txt", "STRAFTER(?x, \"b\") = \"c\""),
            format!(
                "((CASE WHEN {position} > 0 THEN substr((t0.\"txt\"::text) COLLATE \"C\", \
                 {position} + char_length($1::text COLLATE \"C\")) ELSE '' END COLLATE \"C\") = \
                 $2::text)"
            )
        );
        // SPARQL 1.1 §17.4.3.9: the tag of the first argument where the
        // second is in it, and the simple literal "" where it is not.
        let tagged = x("label", "STRAFTER(?x, \"b\") = \"\"");
        assert!(
            tagged.starts_with(
                "CASE WHEN ((t0.\"txt\"::text) COLLATE \"C\" LIKE $1::text) THEN NULL::bool \
                 WHEN NOT (((t0.\"txt\"::text) COLLATE \"C\" LIKE $2::text)) THEN ($3::text = \
                 $4::text) END"
            ),
            "{tagged}"
        );
        let tagged = x("label", "STRBEFORE(?x, \"b\") = \"a\"@en");
        assert!(
            tagged
                .starts_with("CASE WHEN ((t0.\"txt\"::text) COLLATE \"C\" LIKE $1::text) THEN (("),
            "{tagged}"
        );
        assert!(tagged.ends_with(" THEN NULL::bool END"), "{tagged}");
        assert_eq!(
            x("label", "LANG(STRBEFORE(?x, \"b\")) = \"en\""),
            "CASE WHEN ((t0.\"txt\"::text) COLLATE \"C\" LIKE $1::text) THEN ($2::text = $3::text) \
             WHEN NOT (((t0.\"txt\"::text) COLLATE \"C\" LIKE $4::text)) THEN ($5::text = $6::text) END"
        );
        // Arguments that are not compatible strings are an error in every
        // case.
        assert_eq!(
            x("label", "STRAFTER(?x, \"b\"@fr) = \"\""),
            "CASE WHEN NULL::bool THEN NULL::bool WHEN NOT (NULL::bool) THEN ($1::text = $2::text) END"
        );
        assert_eq!(x("txt", "STRBEFORE(?x, \"b\"@en) = \"\""), "NULL::bool");
        assert_eq!(x("i4", "STRAFTER(?x, \"1\") = \"\""), "NULL::bool");
        assert_eq!(x("txt", "STRAFTER(?x, 1) = \"\""), "NULL::bool");
        assert!(rejected("?s ex:ts ?x", "STRAFTER(?x, \"1\") = \"\"").contains("infinite"));
    }

    #[test]
    fn encode_for_uri_is_a_simple_literal() {
        assert_eq!(
            x("txt", "ENCODE_FOR_URI(?x) = \"a%20b\""),
            "((CASE WHEN ((t0.\"txt\"::text) COLLATE \"C\") ~ '^[-.0-9A-Z_a-z~]*$' THEN \
             ((t0.\"txt\"::text) COLLATE \"C\") WHEN ((t0.\"txt\"::text) COLLATE \"C\") IS NOT \
             NULL THEN (SELECT coalesce(string_agg(CASE WHEN c ~ '^[-.0-9A-Z_a-z~]$' THEN c ELSE \
             regexp_replace(upper(encode(convert_to(c, 'UTF8'), 'hex')), '(..)', '%\\1', 'g') END, \
             '' ORDER BY n), '') FROM generate_series(1, char_length(((t0.\"txt\"::text) COLLATE \
             \"C\"))) AS n, substr(((t0.\"txt\"::text) COLLATE \"C\"), n, 1) AS c) END COLLATE \
             \"C\") = $1::text)"
        );
        // SPARQL 1.1 §17.4.3.11: also of a language-tagged literal.
        assert!(x("label", "ENCODE_FOR_URI(?x) = \"a\"").ends_with(" = $1::text)"));
        assert_eq!(x("label", "ENCODE_FOR_URI(?x) = \"a\"@en"), "NULL::bool");
        assert_eq!(x("link", "ENCODE_FOR_URI(?x) = \"a\""), "NULL::bool");
    }

    #[test]
    fn replace_uses_the_translated_pattern_and_replacement() {
        assert_eq!(
            x("txt", "REPLACE(?x, \"a+\", \"[$0]\") = \"[a]\""),
            "((regexp_replace((t0.\"txt\"::text) COLLATE \"C\", $1::text, $2::text, 'g') \
             COLLATE \"C\") = $3::text)"
        );
        assert_eq!(
            filter_params("REPLACE(?x, \"(b)\\\\.\", \"$1\\\\$\", \"i\") = \"x\""),
            ["([Bb])\\u002E", "\\1$", "x"]
        );
        // The result keeps the language tag of the first argument.
        assert!(x("label", "REPLACE(?x, \"a\", \"b\") = \"b\"@en").starts_with("((regexp_replace"));
        assert_eq!(
            x("label", "REPLACE(?x, \"a\", \"b\") = \"b\""),
            "NULL::bool"
        );
        // Errors: a pattern that matches the empty string, an invalid
        // pattern, replacement or flag, arguments other than simple
        // literals.
        for filter in [
            "REPLACE(?x, \"a*\", \"b\") = \"b\"",
            "REPLACE(?x, \"(\", \"b\") = \"b\"",
            "REPLACE(?x, \"a\", \"$\") = \"b\"",
            "REPLACE(?x, \"a\", \"b\", \"g\") = \"b\"",
            "REPLACE(?x, \"a\", \"b\"@en) = \"b\"",
            "REPLACE(?x, \"a\"@en, \"b\") = \"b\"",
            "REPLACE(?x, \"a\", 1) = \"b\"",
        ] {
            assert_eq!(x("txt", filter), "NULL::bool", "{filter}");
        }
        assert_eq!(x("i4", "REPLACE(?x, \"1\", \"2\") = \"2\""), "NULL::bool");
        assert!(
            rejected("?s ex:txt ?x", "REPLACE(?x, \"a|b\", \"c\") = \"c\"")
                .contains("alternatives")
        );
        assert!(rejected("?s ex:txt ?x", "REPLACE(?x, \"\\\\d\", \"c\") = \"c\"").contains("\\d"));
        assert!(
            rejected("?s ex:txt ?x", "REPLACE(?x, \"a\", \"b\", \"q\") = \"b\"").contains("q flag")
        );
    }

    #[test]
    fn casts_of_strings_match_the_lexical_space_before_reading() {
        // The lexical form is matched before PostgreSQL reads it, so an
        // invalid one is an error and not a failed statement.
        let string = "(SELECT CASE WHEN char_length(s) <= 1000 AND s ~ $1::text THEN s::numeric \
                      END FROM (SELECT (t0.\"txt\"::text) COLLATE \"C\" AS s) AS c)";
        assert_eq!(
            x("txt", "xsd:integer(?x) = 1"),
            format!("{string} = $2::int8")
        );
        assert_eq!(
            x("txt", "xsd:decimal(?x) > 1"),
            format!("{string} > $2::numeric")
        );
        assert_eq!(
            filter_params("xsd:integer(?x) = 1")[0],
            "^[\\t\\n\\r ]*[+-]?[0-9]+[\\t\\n\\r ]*$"
        );
        assert_eq!(
            filter_params("xsd:decimal(?x) = 1")[0],
            "^[\\t\\n\\r ]*[+-]?([0-9]+([.][0-9]*)?|[.][0-9]+)[\\t\\n\\r ]*$"
        );
        assert_eq!(
            x("txt", "xsd:boolean(?x)"),
            "(SELECT CASE WHEN s ~ $1::text THEN true WHEN s ~ $2::text THEN false END FROM \
             (SELECT (t0.\"txt\"::text) COLLATE \"C\" AS s) AS c)"
        );
        // A double is read as numeric and rounded as decimals are, so an
        // overflow is an infinity; a zero keeps its sign.
        let double = x("txt", "xsd:double(?x) = 1");
        assert!(
            double
                .contains("WHEN d = 0 THEN CASE WHEN n THEN '-0e0'::float8 ELSE '0e0'::float8 END")
                && double.contains("FROM (SELECT s::numeric AS d) AS r)"),
            "{double}"
        );
        // Unlike a comparison, a type test sees the error.
        assert_eq!(
            x("txt", "isLiteral(xsd:integer(?x))"),
            format!("CASE WHEN ({string} IS NOT NULL) THEN true END")
        );
        assert_eq!(
            x("txt", "COALESCE(xsd:boolean(?x), false)"),
            "CASE WHEN ((SELECT CASE WHEN s ~ $1::text THEN true WHEN s ~ $2::text THEN false END \
             FROM (SELECT (t0.\"txt\"::text) COLLATE \"C\" AS s) AS c) IS NOT NULL) THEN (SELECT \
             CASE WHEN s ~ $3::text THEN true WHEN s ~ $4::text THEN false END FROM (SELECT \
             (t0.\"txt\"::text) COLLATE \"C\" AS s) AS c) WHEN true THEN false END"
        );
    }

    #[test]
    fn casts_of_numbers_and_booleans_follow_xpath() {
        // A decimal loses its fraction; a double too, exactly at every
        // magnitude, and NaN and the infinities are errors.
        assert_eq!(
            x("dec", "xsd:integer(?x) = 1"),
            "trunc(CASE WHEN t0.\"dec\"::text NOT IN ('NaN', 'Infinity', '-Infinity') THEN \
             t0.\"dec\" END) = $1::int8"
        );
        assert_eq!(
            x("f8", "xsd:integer(?x) = 1"),
            "CASE WHEN t0.\"f8\" >= '-9.223372036854776e18'::float8 AND t0.\"f8\" < \
             '9.223372036854776e18'::float8 THEN trunc(t0.\"f8\")::int8::numeric WHEN \
             abs(t0.\"f8\") <= '1.7976931348623157e308'::float8 THEN (SELECT trunc((t0.\"f8\" / \
             2::float8 ^ e)::int8::numeric * 2::numeric ^ e) FROM (SELECT \
             (floor(ln(abs(t0.\"f8\")) / ln(2::float8)) - 57)::int4 AS e) AS p) END = $1::int8"
        );
        assert_eq!(
            x("i4", "xsd:double(?x) = 1"),
            "(t0.\"i4\"::float8 = $1::float8)"
        );
        assert_eq!(
            x("i4", "xsd:decimal(?x) = 1"),
            "t0.\"i4\"::numeric = $1::numeric"
        );
        assert_eq!(x("i4", "xsd:boolean(?x)"), "(t0.\"i4\"::numeric <> 0)");
        assert_eq!(
            x("f8", "xsd:boolean(?x)"),
            "(t0.\"f8\" <> 0 AND t0.\"f8\" <> 'NaN'::float8)"
        );
        assert_eq!(
            x("b", "xsd:integer(?x) = 1"),
            "CASE t0.\"b\" WHEN true THEN 1::numeric WHEN false THEN 0::numeric END = $1::int8"
        );
        assert_eq!(x("b", "xsd:boolean(?x) = ?x"), "t0.\"b\" = t0.\"b\"");
        // Constants are cast before the query is sent.
        assert_eq!(
            x("i4", "xsd:integer(\" +01 \") = ?x"),
            "$1::int8 = t0.\"i4\""
        );
        assert_eq!(
            params("SELECT ?s WHERE { ?s ex:i4 ?x FILTER(xsd:integer(\" +01 \") = ?x) }"),
            ["1"]
        );
        assert_eq!(x("i4", "xsd:integer(\"1.0\") = ?x"), "NULL::bool");
    }

    #[test]
    fn casts_the_table_does_not_allow_are_errors() {
        for (predicate, filter) in [
            ("ts", "xsd:integer(?x) = 1"),
            ("tz", "xsd:boolean(?x)"),
            ("d", "xsd:double(?x) = 1"),
            ("label", "xsd:integer(?x) = 1"),
            ("link", "xsd:decimal(?x) = 1"),
            ("i4", "xsd:integer(ex:a) = ?x"),
            ("i4", "xsd:integer(\"1\"@en) = ?x"),
            (
                "i4",
                "xsd:double(\"2024-01-01T00:00:00\"^^xsd:dateTime) = ?x",
            ),
        ] {
            assert_eq!(x(predicate, filter), "NULL::bool", "{filter}");
        }
        assert_eq!(x("ts", "!(xsd:integer(?x) = 1)"), "NOT (NULL::bool)");
    }

    #[test]
    fn casts_whose_results_engines_compute_differently_are_rejected() {
        for (patterns, filter, reason) in [
            (
                "?s ex:f8 ?x",
                "xsd:decimal(?x) = 1",
                "xsd:decimal of an xsd:double",
            ),
            (
                "?s ex:f8 ?x",
                "xsd:decimal(1e0) = 1",
                "xsd:decimal of an xsd:double",
            ),
            ("?s ex:f4 ?x", "xsd:integer(?x) = 1", "float4"),
            ("?s ex:txt ?x", "xsd:integer(\"1\"^^xsd:int) = 1", "xsd:int"),
            (
                "?s ex:txt ?x",
                "STR(xsd:integer(?x)) = \"1\"",
                "STR, LANG and DATATYPE of a cast",
            ),
            (
                "?s ex:txt ?x",
                "sameTerm(xsd:boolean(?x), true)",
                "sameTerm of a cast",
            ),
        ] {
            let message = rejected(patterns, filter);
            assert!(message.contains(reason), "{filter}: {message}");
        }
    }
}
