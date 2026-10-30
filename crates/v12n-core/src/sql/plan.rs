// SPDX-License-Identifier: Apache-2.0
//! The SQL plan (`SqlPlan`) that unfolding builds and rendering reads.

use std::collections::{BTreeSet, HashMap};
use std::ops::Range;

use oxrdf::Term;

use crate::dialect::{Dialect, SqlType};
use crate::mapping::{BaseIri, LogicalTable, TemplatePart, TermType};
use crate::sparql::{ArithOp, CompareOp, TermTest};

pub(super) const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

/// An unfolded query: one `SELECT` per branch, combined with `UNION ALL`
/// (or `UNION` under `DISTINCT`, or when duplicate triples are removed).
///
/// The branches of one arm of the query's `UNION`s (see
/// [`SqlBranch::arm`]) are the triples maps' ways to produce its solutions;
/// the arms' solutions are combined as a bag (SPARQL 1.1 §18.5), so
/// duplicates are only removed within an arm, unless under `DISTINCT`.
///
/// An RDF graph is a set of triples, so a basic graph pattern has one
/// solution per distinct mapping of all its variables (SPARQL 1.1 §18.5),
/// while duplicate rows of a logical table, or two term maps producing the
/// same triple, give one SQL row each. Unless the database's keys prove
/// that no two rows give the same solution, the rendered SQL removes
/// duplicates over the columns of every variable of the pattern, and only
/// then projects the selected ones, which keeps the bag semantics of the
/// projection (see [`SqlPlan::to_sql_with`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlPlan {
    /// Result column aliases, shared by every branch.
    pub columns: Vec<String>,
    /// Aliases of the columns of variables that occur in the pattern but
    /// are not selected. They take part in removing duplicates only.
    pub hidden_columns: Vec<String>,
    /// The entries of `hidden_columns` whose terms depend on the columns'
    /// natural lexical forms only: all but literals of natural datatype
    /// (R2RML §10.2), whose datatype comes from the SQL type. Leaving an
    /// entry out is safe: it can only keep a duplicate.
    pub lexical_hidden_columns: BTreeSet<String>,
    /// The entries of [`SqlBranch::select`] in the order duplicates are
    /// removed over: by first occurrence of their variable in the pattern,
    /// so that an index on the first subject can deliver the rows sorted
    /// for `DISTINCT` and the first rows arrive without reading them all.
    /// Empty means the order of [`SqlBranch::select`].
    pub distinct_order: Vec<usize>,
    /// How to rebuild each projected variable from the result columns.
    pub projection: Vec<ProjectedTerm>,
    /// Groups of entries of `columns` that together hold the values of
    /// one projected column, one SQL type per entry (see
    /// [`with_union_types`](Self::with_union_types)). Each branch fills
    /// one entry of a group and leaves the others `NULL`. The projection
    /// refers to the first entry.
    pub split_columns: Vec<Vec<String>>,
    /// No branches means the query has no answers.
    pub branches: Vec<SqlBranch>,
    pub distinct: bool,
    /// The base IRI that relative IRIs of the results resolve against
    /// (R2RML §11.2), from the mapping.
    pub base_iri: Option<BaseIri>,
    /// The order comparators of `ORDER BY`, whose values each branch gives
    /// in [`SqlBranch::order`]; empty without one.
    pub order: Vec<OrderKey>,
    /// `OFFSET`: solutions skipped after ordering and removing duplicates
    /// (SPARQL 1.1 §18.2.5, Slice).
    pub offset: usize,
    /// `LIMIT`: most solutions returned after `OFFSET`.
    pub limit: Option<usize>,
}

/// One order comparator of `ORDER BY` (SPARQL 1.1 §15.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderKey {
    pub descending: bool,
}

/// The value of an order comparator in the rows of a branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderValue {
    /// A term: the value of the first case whose conditions all hold, or
    /// no value where none does. A variable bound by several `OPTIONAL`
    /// parts has one case per part, as in [`Filter::Cases`].
    Term(Vec<(Vec<Condition>, FilterValue)>),
    /// A boolean expression, rendered as a [`Filter`]: a boolean literal,
    /// or no value where it is an error.
    Boolean(Box<Filter>),
}

/// One `SELECT ... FROM ... WHERE ...` of the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlBranch {
    /// The arm the branch belongs to: the choice of one side of every
    /// `UNION` outside `OPTIONAL`, and of the side of a `UNION` inside an
    /// `OPTIONAL` part that splits the branch. Branches of one arm are
    /// consecutive in [`SqlPlan::branches`].
    pub arm: usize,
    /// Logical tables; the one at position `n` is aliased `tn`.
    pub from: Vec<LogicalTable>,
    /// One expression per entry of [`SqlPlan::columns`], then one per
    /// entry of [`SqlPlan::hidden_columns`].
    pub select: Vec<SqlExpr>,
    /// Conjunction of conditions on every row of the branch. Rendering
    /// puts a condition that links two tables in the `ON` clause of the
    /// later one, the rest in `WHERE`.
    pub conditions: Vec<Condition>,
    /// The `OPTIONAL` parts, as `LEFT JOIN`s, in the order they are
    /// written; one comes before those nested in it.
    pub left_joins: Vec<LeftJoin>,
    /// For each entry of `from`, the columns whose values a solution
    /// fixes: those of terms that distinct column values always render as
    /// distinct terms. When they include a unique key of the table, the
    /// table contributes at most one row to each solution.
    pub fixed_columns: Vec<BTreeSet<String>>,
    /// The value of each of [`SqlPlan::order`] in the rows of the branch.
    pub order: Vec<OrderValue>,
    /// The values of the expressions that `BIND` and `SELECT (expr AS ?v)`
    /// compute, which [`SqlExpr::Value`] entries select: the value of the
    /// first case whose conditions all hold, as in [`OrderValue::Term`].
    pub values: Vec<ValueCases>,
}

/// An expression's value in the rows of a branch, by cases (see
/// [`SqlBranch::values`]).
pub type ValueCases = Vec<(Vec<Condition>, FilterValue)>;

/// The class of number a result column of a computed value holds. The
/// class of an arithmetic result depends on the SQL types of its
/// operands, known only when rendering, so each class has its own column,
/// and terms of different datatypes never compare equal when duplicates
/// are removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    Integer,
    Decimal,
}

/// An `OPTIONAL` part of a branch: `LEFT JOIN (tables) ON conditions`, so
/// the rows of the left side are kept, and extended by the matching rows
/// where there are some (SPARQL 1.1 §18.5, LeftJoin).
///
/// Every column of its tables is `NULL` in a row it does not extend, and
/// a variable bound only by it is then unbound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeftJoin {
    /// The tables of the part, those nested in it included: aliases
    /// `t{tables.start}` and up, all after the tables before it.
    pub tables: Range<usize>,
    /// The index in [`SqlBranch::left_joins`] of the part this one is
    /// nested in; `None` at the top level.
    pub parent: Option<usize>,
    /// Conjunction of the conditions of the part: within it, and with the
    /// left side, whose solutions it must be compatible with (SPARQL 1.1
    /// §18.3). The left side is the tables before it, those of the parent
    /// part on, or all of them at the top level.
    pub conditions: Vec<Condition>,
    /// Set when the part can only match one row of a table of its left
    /// side, which it then reads instead of joining.
    pub same_row: Option<SameRow>,
}

/// How an `OPTIONAL` part reads a row of its left side: its one table of
/// its own, `t{scan}`, can only match the row of `t{row_of}`, pinned by a
/// `NOT NULL` unique key (see
/// [`eliminate_self_joins`](crate::optimize::eliminate_self_joins)).
///
/// Each column of `t{scan}` is then that column of `t{row_of}` where
/// every condition of `guard` holds and `NULL` elsewhere, which is what
/// the `LEFT JOIN` gives, so the part is rendered without one. `guard`
/// reads only tables before the part and holds no parameter, so it can
/// be repeated wherever a column of `t{scan}` is read; every part nested
/// in this one reads a row as well.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SameRow {
    pub scan: usize,
    pub row_of: usize,
    pub guard: Vec<Condition>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlExpr {
    /// Column `name` of table alias `t{table}`.
    Column { table: usize, name: String },
    /// A constant, always sent as a bound parameter.
    Param(Param),
    /// String concatenation of the parts, each cast to text.
    Concat(Vec<SqlExpr>),
    /// The natural lexical form of column `name` of table alias
    /// `t{table}`, computed in SQL.
    Lexical {
        table: usize,
        name: String,
        lexical: Lexical,
    },
    /// `NULL` of an SQL type.
    Null(SqlType),
    /// `NULL` for a column of an `OPTIONAL` part that cannot match in
    /// this branch. Its type comes from the other branches (see
    /// [`SqlPlan::with_union_types`]).
    Unbound,
    /// `true` where the expression is not `NULL`, `NULL` where it is:
    /// whether the `OPTIONAL` part of a column matched.
    Matched(Box<SqlExpr>),
    /// `true`: a constant term bound in every row of the branch.
    True,
    /// The string of an RDF term as text: an IRI (resolved against the base
    /// IRI where it may be relative) or a blank node's value, or a literal's
    /// lexical form, computed in SQL as for [`Condition::SameTerm`]. With a
    /// guard, `NULL` where the guard is `NULL`.
    Term {
        term: TermSql,
        guard: Option<Box<SqlExpr>>,
    },
    /// Entry `value` of [`SqlBranch::values`] as `numeric` where it is a
    /// number of class `kind`, `NULL` where it is another class or an
    /// error.
    Value { value: usize, kind: ValueKind },
}

/// A constant of the query and what it has to match.
///
/// A database adapter checks every parameter against the SQL type the
/// database infers for it before binding, because the database parses
/// text with its own input syntax, which differs from RDF lexical forms:
/// `007` and `+7` are the integer 7 there, `7.0` is an error for an
/// integer, and `99999999999` overflows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    /// The lexical form a value must render as to match (R2RML §10.2,
    /// natural RDF lexical form), or plain text for parts of a string.
    pub value: String,
    /// The datatype IRI of a literal constant compared with a column
    /// whose literal datatype is the natural one of its SQL type, which
    /// is only known from the database. The two must then be equal:
    /// `"7"^^xsd:decimal` is not the term an integer column produces.
    pub datatype: Option<String>,
}

impl From<String> for Param {
    fn from(value: String) -> Self {
        Self {
            value,
            datatype: None,
        }
    }
}

impl From<&str> for Param {
    fn from(value: &str) -> Self {
        value.to_owned().into()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    /// SQL equality: the columns of an `rr:joinCondition` (R2RML §8 joins
    /// with SQL `=`), or a column and a constant, which a database adapter
    /// checks against the column's type before binding (see [`Param`]).
    Eq(SqlExpr, SqlExpr),
    /// SQL equality with one of several expressions, rendered as `IN`: a
    /// column and the constants that each make it produce the same term,
    /// such as an absolute IRI and the relative IRI that resolves to it
    /// (R2RML §11.2). Each constant is checked like one of [`Eq`](Self::Eq).
    EqAny(SqlExpr, Vec<SqlExpr>),
    IsNotNull(SqlExpr),
    IsNull(SqlExpr),
    /// Disjunction of conjunctions: a variable bound by an `OPTIONAL`
    /// part on one side of a join is compatible with any value where it is
    /// unbound, and equal to the other side's term where it is bound
    /// (SPARQL 1.1 §18.3, compatible mappings).
    Or(Vec<Vec<Condition>>),
    /// Both sides produce the same RDF term. Column values take part
    /// through their natural lexical form (R2RML §10.2), which SQL `=`
    /// matches only for some pairs of SQL types, so how this renders
    /// depends on the column types (see [`SqlPlan::to_sql_with`]).
    SameTerm(TermSql, TermSql),
    /// A `FILTER` expression, which holds where its effective boolean
    /// value is true (SPARQL 1.1 §17.2).
    Filter(Filter),
}

/// A `FILTER` expression in SQL's three-valued logic, with `NULL` for a
/// SPARQL error: `||`, `&&` and `!` treat an error exactly as SQL `OR`,
/// `AND` and `NOT` treat `NULL` (SPARQL 1.1 §17.2, Filter Evaluation), and
/// `WHERE` and `ON` keep a row only where the condition is true. Every
/// part that can be an error renders as `NULL` there, never as `false`.
///
/// How a comparison renders depends on the datatypes of its operands
/// (SPARQL 1.1 §17.3, Operator Mapping), which for a column without
/// `rr:datatype` come from its SQL type, so like
/// [`Condition::SameTerm`] it is rendered with the column types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter {
    /// `true`, `false`, or an error (`None`).
    Constant(Option<bool>),
    And(Box<Filter>, Box<Filter>),
    Or(Box<Filter>, Box<Filter>),
    Not(Box<Filter>),
    /// The expression of the first case whose conditions all hold; an
    /// error where none does. A variable bound by several `OPTIONAL`
    /// parts has one case per part, each holding where the part matched,
    /// and an unbound variable is an error (SPARQL 1.1 §17.2).
    Cases(Vec<(Vec<Condition>, Filter)>),
    /// Whether any of the conditions holds, never an error: `BOUND` over
    /// the parts that bind a variable.
    Any(Vec<Condition>),
    /// `=`, `<`, `<=`, `>`, `>=` per SPARQL 1.1 §17.3; `same_term` is when
    /// the operands are the same RDF term, for RDFterm-equal, where both
    /// are term maps' terms or constants (`None` compares their strings).
    Compare {
        op: CompareOp,
        left: FilterValue,
        right: FilterValue,
        same_term: Option<TermMatch>,
    },
    /// `sameTerm` (SPARQL 1.1 §17.4.1.8).
    SameTerm {
        left: FilterValue,
        right: FilterValue,
        same_term: Option<TermMatch>,
    },
    /// `isIRI`, `isBlank`, `isLiteral` (SPARQL 1.1 §17.4.2).
    Test(TermTest, FilterValue),
    /// The effective boolean value of a term (SPARQL 1.1 §17.2.2).
    Ebv(FilterValue),
    /// `REGEX` (SPARQL 1.1 §17.4.3.14) with the XPath pattern and flags
    /// already written as a PostgreSQL pattern with the same matches.
    Regex {
        text: FilterValue,
        pattern: String,
    },
}

/// An RDF term in a `FILTER` expression, bound in every row where it is
/// evaluated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterValue {
    /// A term built by a term map from the columns of a row.
    Term(FilterTerm),
    /// A constant of the query or of the mapping.
    Constant(Term),
    /// `STR`: the lexical form of a literal or the string of an IRI, as a
    /// simple literal (SPARQL 1.1 §17.4.2.5).
    Str(Box<FilterValue>),
    /// `LANG`: a literal's language tag, or `""` (SPARQL 1.1 §17.4.2.6).
    Lang(Box<FilterValue>),
    /// `DATATYPE`: a literal's datatype IRI (SPARQL 1.1 §17.4.2.7).
    Datatype(Box<FilterValue>),
    /// `+`, `-`, `*` or `/` of two numbers (SPARQL 1.1 §17.3), an error
    /// for anything else.
    Arith(ArithOp, Box<FilterValue>, Box<FilterValue>),
    /// The negation of a number (op:numeric-unary-minus).
    Negate(Box<FilterValue>),
}

impl FilterValue {
    /// The term maps' terms the value is built from.
    fn terms(&self) -> Vec<&FilterTerm> {
        match self {
            Self::Term(t) => vec![t],
            Self::Constant(_) => Vec::new(),
            Self::Str(v) | Self::Lang(v) | Self::Datatype(v) | Self::Negate(v) => v.terms(),
            Self::Arith(_, a, b) => {
                let mut terms = a.terms();
                terms.extend(b.terms());
                terms
            }
        }
    }

    fn terms_mut(&mut self) -> Vec<&mut FilterTerm> {
        match self {
            Self::Term(t) => vec![t],
            Self::Constant(_) => Vec::new(),
            Self::Str(v) | Self::Lang(v) | Self::Datatype(v) | Self::Negate(v) => v.terms_mut(),
            Self::Arith(_, a, b) => {
                let mut terms = a.terms_mut();
                terms.extend(b.terms_mut());
                terms
            }
        }
    }
}

/// A term map's term in a `FILTER` expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterTerm {
    pub term_type: TermType,
    /// The term's string, and its datatype for a literal without a
    /// language tag.
    pub term: TermSql,
    pub language: Option<String>,
}

/// When two terms are the same RDF term.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermMatch {
    /// Never, whatever the rows hold.
    Never,
    /// When all conditions hold. A constant in them that no value can
    /// match is bound to `NULL` (see [`Param`]), so they can be unknown
    /// rather than false.
    When(Vec<Condition>),
}

/// A part of a [`Filter`] that refers to columns.
pub(super) enum Part<'f> {
    Condition(&'f Condition),
    Term(&'f TermSql),
}

pub(super) enum PartMut<'f> {
    Condition(&'f mut Condition),
    Term(&'f mut TermSql),
}

impl Filter {
    /// Every condition inside, not recursing into them, and every term
    /// map's term.
    pub(super) fn visit<'f>(&'f self, f: &mut dyn FnMut(Part<'f>)) {
        fn value<'f>(v: &'f FilterValue, f: &mut dyn FnMut(Part<'f>)) {
            for t in v.terms() {
                f(Part::Term(&t.term));
            }
        }
        match self {
            Self::Constant(_) => {}
            Self::And(a, b) | Self::Or(a, b) => {
                a.visit(f);
                b.visit(f);
            }
            Self::Not(a) => a.visit(f),
            Self::Cases(cases) => {
                for (guards, filter) in cases {
                    guards.iter().for_each(|c| f(Part::Condition(c)));
                    filter.visit(f);
                }
            }
            Self::Any(conditions) => conditions.iter().for_each(|c| f(Part::Condition(c))),
            Self::Compare {
                left,
                right,
                same_term,
                ..
            }
            | Self::SameTerm {
                left,
                right,
                same_term,
            } => {
                value(left, f);
                value(right, f);
                if let Some(TermMatch::When(conditions)) = same_term {
                    conditions.iter().for_each(|c| f(Part::Condition(c)));
                }
            }
            Self::Test(_, v) | Self::Ebv(v) | Self::Regex { text: v, .. } => value(v, f),
        }
    }

    pub(super) fn visit_mut(&mut self, f: &mut dyn FnMut(PartMut<'_>)) {
        fn value(v: &mut FilterValue, f: &mut dyn FnMut(PartMut<'_>)) {
            for t in v.terms_mut() {
                f(PartMut::Term(&mut t.term));
            }
        }
        match self {
            Self::Constant(_) => {}
            Self::And(a, b) | Self::Or(a, b) => {
                a.visit_mut(f);
                b.visit_mut(f);
            }
            Self::Not(a) => a.visit_mut(f),
            Self::Cases(cases) => {
                for (guards, filter) in cases {
                    guards.iter_mut().for_each(|c| f(PartMut::Condition(c)));
                    filter.visit_mut(f);
                }
            }
            Self::Any(conditions) => conditions.iter_mut().for_each(|c| f(PartMut::Condition(c))),
            Self::Compare {
                left,
                right,
                same_term,
                ..
            }
            | Self::SameTerm {
                left,
                right,
                same_term,
            } => {
                value(left, f);
                value(right, f);
                if let Some(TermMatch::When(conditions)) = same_term {
                    conditions.iter_mut().for_each(|c| f(PartMut::Condition(c)));
                }
            }
            Self::Test(_, v) | Self::Ebv(v) | Self::Regex { text: v, .. } => value(v, f),
        }
    }

    /// The conditions and terms inside.
    pub(super) fn parts(&self) -> (Vec<&Condition>, Vec<&TermSql>) {
        let (mut conditions, mut terms) = (Vec::new(), Vec::new());
        self.visit(&mut |part| match part {
            Part::Condition(c) => conditions.push(c),
            Part::Term(t) => terms.push(t),
        });
        (conditions, terms)
    }
}

impl OrderValue {
    /// Every condition inside, not recursing into them, and every term
    /// map's term.
    pub(super) fn visit<'f>(&'f self, f: &mut dyn FnMut(Part<'f>)) {
        match self {
            Self::Term(cases) => visit_cases(cases, f),
            Self::Boolean(filter) => filter.visit(f),
        }
    }

    pub(super) fn visit_mut(&mut self, f: &mut dyn FnMut(PartMut<'_>)) {
        match self {
            Self::Term(cases) => visit_cases_mut(cases, f),
            Self::Boolean(filter) => filter.visit_mut(f),
        }
    }
}

/// Every guard of the cases, not recursing into them, and every term map's
/// term of their values.
pub(super) fn visit_cases<'f>(cases: &'f ValueCases, f: &mut dyn FnMut(Part<'f>)) {
    for (guards, value) in cases {
        guards.iter().for_each(|c| f(Part::Condition(c)));
        for t in value.terms() {
            f(Part::Term(&t.term));
        }
    }
}

pub(super) fn visit_cases_mut(cases: &mut ValueCases, f: &mut dyn FnMut(PartMut<'_>)) {
    for (guards, value) in cases {
        guards.iter_mut().for_each(|c| f(PartMut::Condition(c)));
        for t in value.terms_mut() {
            f(PartMut::Term(&mut t.term));
        }
    }
}

/// One side of a [`Condition::SameTerm`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermSql {
    pub value: TermSqlValue,
    /// The datatype of a literal without language tag, when the
    /// comparison may have to check it; `None` when it never needs to,
    /// such as for IRIs and template columns.
    pub datatype: Option<TermDatatype>,
    /// For an IRI whose strings may be relative: the base IRI they resolve
    /// against (R2RML §11.2). The side then compares as the generated IRI,
    /// the string itself when it has a scheme and the base followed by it
    /// otherwise, which no index on the columns serves.
    pub base_iri: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermSqlValue {
    /// The natural lexical form of a column of table alias `t{table}`.
    Column { table: usize, name: String },
    /// A template over table alias `t{table}`. With `iri_safe`, column
    /// values are percent-encoded as in an IRI template (R2RML §7.3).
    Template {
        table: usize,
        parts: Vec<TemplatePart>,
        iri_safe: bool,
    },
    /// The lexical form of a constant.
    Param(Param),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermDatatype {
    /// The natural datatype of the column's SQL type (R2RML §10.2), only
    /// known from the database.
    Natural,
    /// A datatype IRI given by the mapping.
    Iri(String),
}

/// What a database adapter knows about the SQL type of a column that a
/// [`Condition::SameTerm`] reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnType {
    /// The SQL type name, for error messages.
    pub name: String,
    /// The type as SQL text for a cast, schema-qualified and quoted.
    pub sql_type: String,
    /// Columns of one class can share a `UNION` result column: the type
    /// the database resolves for them keeps the natural RDF term of every
    /// value (integer widths do, `real` and `double precision` do not).
    pub union_class: String,
    /// IRI of the type's natural datatype; `None` for `xsd:string`.
    pub natural_datatype: Option<String>,
    /// Two columns of the same group compare as `comparison` says.
    pub group: String,
    pub comparison: Comparison,
    /// How SQL computes the natural lexical form, when it can.
    pub lexical: Option<Lexical>,
    /// Whether that lexical form only ever has characters of RFC 3987
    /// `iunreserved`, which the IRI-safe version keeps as they are (R2RML
    /// §7.3), so an IRI template uses it without encoding it in SQL.
    pub lexical_unreserved: bool,
    /// For a type whose `=` finds some values equal that give different
    /// terms, what tells them apart.
    pub term_key: Option<TermKey>,
    /// For a type whose `=` finds some values different that give the
    /// same term, what maps them to one value.
    pub term_normal: Option<TermNormal>,
}

/// SQL that tells apart values that `=` finds equal but whose natural
/// RDF terms differ (R2RML §10.2), so that comparing terms stays exact
/// while `=` still lets an index find the candidates: `col = $1 AND
/// key(col) = key($1)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermKey {
    /// The text output: floating point, where `-0 = 0` holds but `-0.0E0` and
    /// `0.0E0` are different `xsd:double` terms (XML Schema 1.1
    /// `doubleCanonicalMap`).
    Text,
    /// `octet_length(x)`: `bpchar`, whose `=` ignores trailing spaces,
    /// while its values keep the padding (`'a  '::char(3)`) and so do
    /// their literals.
    OctetLength,
}

impl TermKey {
    pub(super) fn sql(self, dialect: &dyn Dialect, expr: &str) -> String {
        match self {
            Self::Text => dialect.cast(expr, &SqlType::Text),
            Self::OctetLength => format!("octet_length({expr})"),
        }
    }
}

/// SQL that maps values that `=` finds different but whose natural RDF
/// terms are the same (R2RML §10.2) to one value, the one whose term it
/// is, so that comparing, deduplicating and sorting terms in SQL treats
/// them as one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermNormal {
    /// [`Dialect::time_normal`]: `time`, where `24:00:00` and `00:00:00` are
    /// different values but the same `xsd:time` value, whose canonical
    /// form is `00:00:00` (XML Schema 1.1 `timeCanonicalMap`).
    Time,
}

impl TermNormal {
    pub(super) fn sql(self, dialect: &dyn Dialect, expr: &str) -> String {
        match self {
            Self::Time => dialect.time_normal(expr),
        }
    }

    /// The values other than `x` itself that a column must also match to
    /// match the term of a parameter `x` in normal form, for parameters
    /// that [`RenderOptions::normal_params`] lists.
    pub(super) fn others(self) -> &'static str {
        match self {
            Self::Time => "'24:00:00'",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comparison {
    /// `a = b`: within the group, equal values have equal natural lexical
    /// forms and the other way round.
    Value,
    /// `a::text = b::text`: the text output identifies the natural
    /// lexical form within the group, but value equality does not (for
    /// example `-0` and `0`, or `1 day` and `24 hours`).
    Text,
}

/// SQL for a column's natural lexical form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lexical {
    /// The text output.
    Text,
    /// `true` or `false`.
    Boolean,
    /// Upper-case hexadecimal digits.
    Hex,
    /// The canonical `xsd:decimal` form of an exact number
    /// ([`Dialect::decimal_lexical`]).
    Decimal,
}

/// Column types by logical table and column name.
pub type ColumnTypes = HashMap<(LogicalTable, String), ColumnType>;

/// Unique keys of logical tables, each the column names as the database
/// stores them ([`identifier_name`]).
pub type TableKeys = HashMap<LogicalTable, Vec<Vec<String>>>;

/// What a database adapter learned before rendering, see
/// [`SqlPlan::to_sql_with`].
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderOptions<'a> {
    /// Parameters (0-based) whose compared column is cast to text.
    pub text_params: &'a [usize],
    /// Parameters (0-based) whose comparison with a column also compares
    /// this key of both, because a value of the parameter's type that `=`
    /// finds equal to the constant may still give a different term (a
    /// zero `float8` constant against `-0`, see [`TermKey`]).
    pub keyed_params: &'a [(usize, TermKey)],
    /// Parameters (0-based) holding a value whose term other values of
    /// the parameter's type also give, which the comparison with a column
    /// also matches (`00:00:00` against `24:00:00`, see [`TermNormal`]).
    pub normal_params: &'a [(usize, TermNormal)],
    /// Types of the columns that [`SqlPlan::term_columns`] lists.
    pub column_types: Option<&'a ColumnTypes>,
    /// Unique keys of the tables that [`SqlPlan::key_tables`] lists.
    pub keys: Option<&'a TableKeys>,
    /// The database encoding is not UTF-8, so `COLLATE "C"` does not
    /// order strings by code point, which `FILTER` string comparisons need
    /// (SPARQL 1.1 §17.3: the codepoint collation).
    pub non_utf8: bool,
}

/// A projected variable and how to rebuild its RDF term from a result row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedTerm {
    pub variable: String,
    /// The terms the variable can take its value from, tried in order: the
    /// first that is bound in a row is the value. Bound terms of one row
    /// are the same RDF term. Several only when the variable is bound by
    /// more than one `OPTIONAL` part and by nothing else. Empty when the
    /// variable is unbound in every solution.
    pub terms: Vec<ResultTerm>,
}

/// The term map a projected variable takes its values from, with column
/// names replaced by result column aliases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultTerm {
    pub term_type: TermType,
    pub value: ResultValue,
    pub datatype: Option<String>,
    pub language: Option<String>,
    /// For a constant of an `OPTIONAL` part, the result column that is
    /// `NULL` when the part did not match and the term is unbound.
    pub guard: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultValue {
    Constant(Term),
    Column(String),
    /// Template parts; [`TemplatePart::Column`] holds a result column alias.
    Template(Vec<TemplatePart>),
}

impl ResultValue {
    /// Result column aliases this value is built from, in order.
    pub fn columns(&self) -> Vec<&str> {
        match self {
            Self::Constant(_) => Vec::new(),
            Self::Column(alias) => vec![alias.as_str()],
            Self::Template(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    TemplatePart::Column(alias) => Some(alias.as_str()),
                    TemplatePart::Literal(_) => None,
                })
                .collect(),
        }
    }
}

/// PostgreSQL query text and its parameters (`$1` is `params[0]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedSql {
    pub sql: String,
    pub params: Vec<String>,
    /// [`Param::datatype`] of each parameter.
    pub param_datatypes: Vec<Option<String>>,
    /// Index into [`SqlPlan::branches`] of the branch each parameter
    /// belongs to.
    pub param_branches: Vec<usize>,
    /// Whether each parameter is one side of an equality that every row
    /// of its branch satisfies, so that binding it to `NULL` empties the
    /// branch. Not for one of an `OPTIONAL` part or of a disjunction.
    pub param_required: Vec<bool>,
    /// For a parameter that is one of the alternatives of a
    /// [`Condition::EqAny`], an id it shares with the other alternatives.
    pub param_alternatives: Vec<Option<usize>>,
    /// The sort keys of `ORDER BY`, selected after the columns of
    /// [`SqlPlan::columns`] in this order. They are not part of the
    /// results. SQL keeps no order through a subquery, so a query that
    /// reads this one as a subquery sorts again by them
    /// ([`order_by_clause`](Self::order_by_clause)).
    pub order_by: Vec<OrderColumn>,
}

/// A sort key column of a rendered query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderColumn {
    pub alias: String,
    /// Compared by code point ([`Dialect::code_point_collation`]).
    pub collated: bool,
    pub descending: bool,
}

impl SqlBranch {
    /// The conditions of the branch and of its `OPTIONAL` parts.
    pub fn all_conditions(&self) -> impl Iterator<Item = &Condition> {
        self.conditions
            .iter()
            .chain(self.left_joins.iter().flat_map(|j| &j.conditions))
    }
}

impl LeftJoin {
    /// The conditions of the part, then those of [`SameRow::guard`].
    pub(super) fn conditions_mut(&mut self) -> impl Iterator<Item = &mut Condition> {
        let Self {
            conditions,
            same_row,
            ..
        } = self;
        conditions
            .iter_mut()
            .chain(same_row.iter_mut().flat_map(|s| &mut s.guard))
    }
}

impl TermSql {
    /// `(table, column)` pairs the side reads.
    pub fn columns(&self) -> Vec<(usize, &str)> {
        match &self.value {
            TermSqlValue::Column { table, name } => vec![(*table, name.as_str())],
            TermSqlValue::Template { table, parts, .. } => parts
                .iter()
                .filter_map(|p| match p {
                    TemplatePart::Column(name) => Some((*table, name.as_str())),
                    TemplatePart::Literal(_) => None,
                })
                .collect(),
            TermSqlValue::Param(_) => Vec::new(),
        }
    }
}
