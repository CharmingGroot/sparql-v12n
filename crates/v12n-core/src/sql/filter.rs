// SPDX-License-Identifier: Apache-2.0
//! SQL for `FILTER` expressions: the SPARQL operator mapping (SPARQL 1.1
//! §17.3) over the datatypes a row's terms have, with `NULL` for errors.

use oxrdf::Term;

use super::{
    ColumnType, Condition, Filter, FilterValue, Renderer, TermDatatype, TermMatch, TermSqlValue,
    XSD_STRING,
};
use crate::Error;
use crate::mapping::TermType;
use crate::solution::canonical_double;
use crate::sparql::{CompareOp, TermTest};
use crate::xsd::{self, Num, Number, Value, XSD};

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
}

/// The value of `numeric` columns that is not an `xsd:decimal`: `NaN` and
/// the infinities have no decimal lexical form, so their literals are
/// ill-typed.
fn decimal_guard(column: &str) -> String {
    format!("{column}::text NOT IN ('NaN', 'Infinity', '-Infinity')")
}

impl<'a> Renderer<'a> {
    pub(super) fn filter(&mut self, filter: &Filter, mode: Mode) -> Result<(), Error> {
        match filter {
            Filter::Constant(Some(true)) => self.sql.push_str("true"),
            Filter::Constant(Some(false)) => self.sql.push_str("false"),
            Filter::Constant(None) => self.sql.push_str("NULL::bool"),
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
                        self.same_strings(&l, &r)?
                    }
                }
            }
            Filter::Regex { text, pattern } => self.regex(text, pattern)?,
            Filter::Test(test, value) => {
                let Some(term_type) = self.operand(value).term_type else {
                    self.sql.push_str("NULL::bool");
                    return Ok(());
                };
                let holds = match test {
                    TermTest::IsIri => term_type == TermType::Iri,
                    TermTest::IsBlank => term_type == TermType::BlankNode,
                    TermTest::IsLiteral => term_type == TermType::Literal,
                };
                self.sql.push_str(if holds { "true" } else { "false" });
            }
            Filter::Ebv(value) => self.ebv(value)?,
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
        };
        let error = Operand {
            kind: Kind::Error,
            term_type: None,
            ..operand.clone()
        };
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
            // The results of STR and LANG are simple literals.
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
            self.sql.push_str("NULL::bool");
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
                self.sql.push_str(&format!(" COLLATE \"C\" {} ", op.sql()));
                self.text(&r)
            }
            (Kind::Boolean, Kind::Boolean) => {
                self.typed_value(&l, "bool")?;
                self.sql.push_str(&format!(" {} ", op.sql()));
                self.typed_value(&r, "bool")
            }
            (Kind::DateTime { zoned: a }, Kind::DateTime { zoned: b }) if a == b => {
                let ty = if *a { "timestamptz" } else { "timestamp" };
                self.guarded(
                    mode,
                    &[&l, &r],
                    op,
                    |o| o.infinite,
                    |c| format!("isfinite({c})"),
                    |this| {
                        this.typed_value(&l, ty)?;
                        this.sql.push_str(&format!(" {} ", op.sql()));
                        this.typed_value(&r, ty)
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
                (CompareOp::Equal, ..) => "false",
                _ => "NULL::bool",
            };
            self.sql.push_str(sql);
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
                self.sql.push_str("NULL::bool");
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
                self.sql.push_str("NULL::bool");
                return Ok(());
            }
            (Kind::Lang(a), Kind::Lang(b)) => a == b,
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
        if !matches!(operand.kind, Kind::String | Kind::Lang(_) | Kind::Unknown) {
            self.sql.push_str("NULL::bool");
            return Ok(());
        }
        if self.options.non_utf8 {
            return Err(unsupported(
                "REGEX in a database whose encoding is not UTF-8, where PostgreSQL's \
                 regular expressions do not see Unicode characters",
            ));
        }
        self.sql.push('(');
        self.text(&operand)?;
        self.sql.push_str(" COLLATE \"C\" ~ ");
        self.expr(&super::SqlExpr::Param(pattern.into()));
        self.sql.push_str("::text)");
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
                let column = super::column_sql(table, name);
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
                    .map(|c| decimal_guard(c))
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
        for column in &float_columns {
            self.sql
                .push_str(&format!(" AND {column} <> 'NaN'::float8"));
        }
        if !exact_decimal {
            for column in &decimal_columns {
                self.sql
                    .push_str(&format!(" AND {}", decimal_guard(column)));
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
                    " WHEN {}::text = {}::text THEN true",
                    decimal_columns[0], decimal_columns[1]
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
            self.expr(&super::SqlExpr::Param(value.into()));
            self.sql.push_str(&format!("::{}", ty.sql_type()));
            return Ok(());
        }
        let (table, name) = operand
            .column
            .expect("a numeric operand is a column or a constant");
        self.sql.push_str(&super::column_sql(table, name));
        if class < ty {
            self.sql.push_str(&format!("::{}", ty.sql_type()));
        }
        Ok(())
    }

    /// A boolean or dateTime operand as a value of SQL type `ty`.
    pub(super) fn typed_value(&mut self, operand: &Operand<'_>, ty: &str) -> Result<(), Error> {
        let value = match &operand.constant {
            Some(Value::Boolean(b)) => b.to_string(),
            Some(Value::DateTime(d)) => d.canonical.clone(),
            _ => {
                let (table, name) = operand
                    .column
                    .expect("a boolean or dateTime operand is a column or a constant");
                self.sql.push_str(&super::column_sql(table, name));
                return Ok(());
            }
        };
        self.expr(&super::SqlExpr::Param(value.into()));
        self.sql.push_str(&format!("::{ty}"));
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
            .map(|(table, name)| guard(&super::column_sql(table, name)))
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
        match &operand.text {
            Text::Constant(text) => {
                if text.contains('\0') {
                    return Err(unsupported("strings with a NUL character in FILTER"));
                }
                self.expr(&super::SqlExpr::Param(text.as_str().into()));
                self.sql.push_str("::text");
            }
            Text::Term(term) => {
                // A template is a concatenation, which binds looser than
                // the COLLATE and the comparison around it.
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
                    "CASE WHEN isfinite({}) THEN ",
                    super::column_sql(column.0, column.1)
                ));
                self.expr(&super::SqlExpr::Param(datatype.as_str().into()));
                self.sql.push_str("::text ELSE ");
                self.expr(&super::SqlExpr::Param(XSD_STRING.into()));
                self.sql.push_str("::text END");
            }
            Text::Unknown => self.sql.push_str("NULL::text"),
        }
        Ok(())
    }

    /// The effective boolean value (SPARQL 1.1 §17.2.2).
    fn ebv(&mut self, value: &FilterValue) -> Result<(), Error> {
        let operand = self.operand(value);
        if operand.kind == Kind::Error {
            self.sql.push_str("NULL::bool");
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
        let column = operand
            .column
            .map(|(table, name)| super::column_sql(table, name));
        match (&operand.kind, column) {
            (Kind::Boolean, Some(c)) => self.sql.push_str(&c),
            (Kind::Numeric(Num::Integer), Some(c)) => self.sql.push_str(&format!("{c} <> 0")),
            (Kind::Numeric(Num::Decimal), Some(c)) => self.sql.push_str(&format!(
                "CASE WHEN {} THEN {c} <> 0 ELSE false END",
                decimal_guard(&c)
            )),
            (Kind::Numeric(Num::Double), Some(c)) => self
                .sql
                .push_str(&format!("({c} <> 0 AND {c} <> 'NaN'::float8)")),
            (Kind::String | Kind::Lang(_) | Kind::Unknown, _) => {
                self.sql.push('(');
                self.text(&operand)?;
                self.sql.push_str(" <> '')");
            }
            (Kind::Unsupported(reason), _) => return Err(unsupported(reason)),
            _ => self.sql.push_str("NULL::bool"),
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
        Some("double") => {
            Kind::Unsupported(format!("comparing values of {} columns by value", ty.name))
        }
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
            term_key: match sql {
                "float8" | "float4" => Some(TermKey::Text),
                "bpchar" => Some(TermKey::OctetLength),
                _ => None,
            },
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
            .to_sql_with(&RenderOptions {
                column_types: Some(&types),
                non_utf8,
                ..RenderOptions::default()
            })?
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
    fn render(patterns: &str, filter: &str) -> Result<String, Error> {
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
        let iri = x("link", "STR(?x) < \"b\"");
        assert!(
            iri.starts_with("($1 || CASE WHEN t0.\"i4\"::text IS NULL"),
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
                    .to_sql_with(&RenderOptions {
                        column_types: Some(&types),
                        ..RenderOptions::default()
                    })
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
}
