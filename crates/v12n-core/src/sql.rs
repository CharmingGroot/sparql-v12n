// SPDX-License-Identifier: Apache-2.0
//! SQL plan (`SqlPlan`) and PostgreSQL text rendering with bound parameters.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Range;

use oxrdf::Term;

use crate::Error;
use crate::mapping::template::UCSCHAR;
use crate::mapping::{BaseIri, LogicalTable, SCHEME_REGEX, TemplatePart, TermType};
use crate::sparql::{CompareOp, TermTest};

mod filter;

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

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
}

/// One `SELECT ... FROM ... WHERE ...` of the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlBranch {
    /// The arm of the query's `UNION`s the branch belongs to: the choice of
    /// one side of every `UNION` outside `OPTIONAL`. Branches of one arm
    /// are consecutive in [`SqlPlan::branches`].
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
    /// `NULL` of an SQL type, written as [`ColumnType::sql_type`].
    Null(String),
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
}

impl FilterValue {
    /// The term map's term the value is built from, if any.
    fn term(&self) -> Option<&FilterTerm> {
        match self {
            Self::Term(t) => Some(t),
            Self::Constant(_) => None,
            Self::Str(v) | Self::Lang(v) | Self::Datatype(v) => v.term(),
        }
    }

    fn term_mut(&mut self) -> Option<&mut FilterTerm> {
        match self {
            Self::Term(t) => Some(t),
            Self::Constant(_) => None,
            Self::Str(v) | Self::Lang(v) | Self::Datatype(v) => v.term_mut(),
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
enum Part<'f> {
    Condition(&'f Condition),
    Term(&'f TermSql),
}

enum PartMut<'f> {
    Condition(&'f mut Condition),
    Term(&'f mut TermSql),
}

impl Filter {
    /// Every condition inside, not recursing into them, and every term
    /// map's term.
    fn visit<'f>(&'f self, f: &mut dyn FnMut(Part<'f>)) {
        fn value<'f>(v: &'f FilterValue, f: &mut dyn FnMut(Part<'f>)) {
            if let Some(t) = v.term() {
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

    fn visit_mut(&mut self, f: &mut dyn FnMut(PartMut<'_>)) {
        fn value(v: &mut FilterValue, f: &mut dyn FnMut(PartMut<'_>)) {
            if let Some(t) = v.term_mut() {
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
    fn parts(&self) -> (Vec<&Condition>, Vec<&TermSql>) {
        let (mut conditions, mut terms) = (Vec::new(), Vec::new());
        self.visit(&mut |part| match part {
            Part::Condition(c) => conditions.push(c),
            Part::Term(t) => terms.push(t),
        });
        (conditions, terms)
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
    /// The text output, `col::text`.
    Text,
    /// `true` or `false`.
    Boolean,
    /// Upper-case hexadecimal digits.
    Hex,
    /// The canonical `xsd:decimal` form of a PostgreSQL `numeric`,
    /// `trim_scale(col)::text` (PostgreSQL 13 or later).
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
}

impl SqlPlan {
    /// Renders the plan. Parameters are numbered in the order they appear
    /// in the text.
    ///
    /// Every parameter is one side of an equality, so binding it to `NULL`
    /// makes the equality fail. For a [required](RenderedSql::param_required)
    /// one, that empties its branch and leaves the others as they are; in
    /// an `OPTIONAL` part or a disjunction, the part or the alternative
    /// does not match. An alternative of an `IN` list
    /// ([`RenderedSql::param_alternatives`]) only drops itself; binding all
    /// of them to `NULL` fails the `IN`.
    ///
    /// Without column types, a [`Condition::SameTerm`] compares columns
    /// with `=` and template columns by their text output, which is what
    /// the database sees for text and integer columns.
    pub fn to_sql(&self) -> RenderedSql {
        self.to_sql_with(&RenderOptions::default())
            .expect("rendering without column types does not fail")
    }

    /// Like [`to_sql`](Self::to_sql), with what a database adapter knows:
    ///
    /// - The column compared with each parameter in `text_params` is cast
    ///   to text. An adapter uses this for column types whose values it
    ///   does not format itself: the comparison then uses the same text
    ///   the result column gets, instead of the database parsing the
    ///   constant as that type. The cast keeps an index on the column
    ///   from being used.
    /// - With `column_types`, a [`Condition::SameTerm`] is `false` when a
    ///   natural datatype differs from the other side's datatype, compares
    ///   two columns of the same group as [`Comparison`] says, and
    ///   otherwise compares natural lexical forms computed in SQL. A
    ///   column whose lexical form SQL cannot compute is an
    ///   [`Error::UnsupportedFeature`], rather than a comparison that
    ///   fails in the database or gives wrong answers.
    /// - With `keys`, duplicates are not removed when the keys prove that
    ///   there are none (see [`has_duplicates`](Self::has_duplicates)).
    ///
    /// Duplicates are removed with `DISTINCT` (one branch) or `UNION`
    /// over the columns of all variables, in a subquery when some of them
    /// are not selected. This compares column values, so two different
    /// values that a template renders as the same string (see
    /// [`SqlPlan::hidden_columns`]) still count twice.
    pub fn to_sql_with(&self, options: &RenderOptions<'_>) -> Result<RenderedSql, Error> {
        let mut r = Renderer {
            options: *options,
            ..Renderer::default()
        };
        if self.branches.is_empty() {
            r.sql.push_str("SELECT 1 WHERE false");
        }
        // Under `DISTINCT`, all branches form one group whose duplicates
        // are removed; otherwise each arm is one, whose duplicates are
        // removed unless keys prove there are none.
        let groups: Vec<(Range<usize>, bool)> = if self.distinct {
            vec![(0..self.branches.len(), true)]
        } else {
            self.arms()
                .into_iter()
                .map(|arm| {
                    let dedup = arm_has_duplicates(&self.branches[arm.clone()], options.keys);
                    (arm, dedup)
                })
                .collect()
        };
        let dedup = !self.distinct && groups.iter().any(|&(_, dedup)| dedup);
        let mut select = Select {
            aliases: self.columns.clone(),
            order: (0..self.columns.len()).collect(),
            typed_text_from: None,
        };
        if dedup {
            select.aliases.extend(self.hidden_columns.iter().cloned());
            if self.distinct_order.is_empty() {
                select.order = (0..select.aliases.len()).collect();
            } else {
                select.order.clone_from(&self.distinct_order);
            }
            if self.branches.len() > 1 {
                select.typed_text_from = Some(self.columns.len());
            }
        }
        let subquery = select.aliases.len() > self.columns.len()
            || select.order.iter().enumerate().any(|(i, &o)| i != o);
        if subquery {
            r.sql.push_str("SELECT ");
            if self.columns.is_empty() {
                r.sql.push('1');
            }
            let outer: Vec<String> = self
                .columns
                .iter()
                .map(|c| format!("q.{}", quote_identifier(c)))
                .collect();
            r.sql.push_str(&outer.join(", "));
            r.sql.push_str("\nFROM (\n");
        }
        for (g, (range, distinct)) in groups.iter().enumerate() {
            if g > 0 {
                r.sql.push_str("\nUNION ALL\n");
            }
            let single = range.len() == 1;
            // `UNION` and `UNION ALL` apply from left to right, so a group
            // after another is parenthesized.
            let parenthesized = *distinct && !single && groups.len() > 1;
            if parenthesized {
                r.sql.push('(');
            }
            for i in range.clone() {
                if i > range.start {
                    r.sql.push_str(if *distinct {
                        "\nUNION\n"
                    } else {
                        "\nUNION ALL\n"
                    });
                }
                r.branch_index = i;
                r.branch(&self.branches[i], &select, *distinct && single)?;
            }
            if parenthesized {
                r.sql.push(')');
            }
        }
        if subquery {
            r.sql.push_str("\n) q");
        }
        Ok(RenderedSql {
            sql: r.sql,
            params: r.params,
            param_datatypes: r.param_datatypes,
            param_branches: r.param_branches,
            param_required: r.param_required,
            param_alternatives: r.param_alternatives,
        })
    }

    /// The branches of each arm (see [`SqlBranch::arm`]), in order.
    fn arms(&self) -> Vec<Range<usize>> {
        let mut out: Vec<Range<usize>> = Vec::new();
        for (i, branch) in self.branches.iter().enumerate() {
            match out.last_mut() {
                Some(last) if self.branches[last.start].arm == branch.arm => last.end = i + 1,
                _ => out.push(i..i + 1),
            }
        }
        out
    }

    /// Whether two SQL rows may give the same solution of an arm of the
    /// pattern, so that rendering removes duplicates (see
    /// [`SqlBranch::arm`]). Never under `DISTINCT`, which already does.
    /// Otherwise true unless every arm has one branch and `keys` holds,
    /// for every table that branch reads, a unique key among the table's
    /// [`SqlBranch::fixed_columns`]: then each solution fixes one row of
    /// every table, and so one row of the result.
    pub fn has_duplicates(&self, keys: Option<&TableKeys>) -> bool {
        !self.distinct
            && self
                .arms()
                .into_iter()
                .any(|arm| arm_has_duplicates(&self.branches[arm], keys))
    }

    /// The tables whose keys can show that an arm has no duplicates
    /// ([`has_duplicates`](Self::has_duplicates)), each once. Empty when
    /// keys cannot help: under `DISTINCT`, or when no arm has one branch
    /// that reads tables only, without an `rr:sqlQuery`, which has no keys.
    pub fn key_tables(&self) -> Vec<LogicalTable> {
        let mut out: Vec<LogicalTable> = Vec::new();
        if self.distinct {
            return out;
        }
        for arm in self.arms() {
            let [branch] = &self.branches[arm] else {
                continue;
            };
            if branch
                .from
                .iter()
                .any(|t| matches!(t, LogicalTable::SqlQuery(_)))
            {
                continue;
            }
            for table in &branch.from {
                if !out.contains(table) {
                    out.push(table.clone());
                }
            }
        }
        out
    }

    /// The columns [`Condition::SameTerm`]s and [`SqlExpr::Term`]s read,
    /// each once, in order of appearance. Their types go into
    /// [`RenderOptions::column_types`].
    pub fn term_columns(&self) -> Vec<(LogicalTable, String)> {
        fn collect(
            condition: &Condition,
            from: &[LogicalTable],
            out: &mut Vec<(LogicalTable, String)>,
        ) {
            match condition {
                Condition::SameTerm(left, right) => {
                    for (table, name) in left.columns().into_iter().chain(right.columns()) {
                        let key = (from[table].clone(), name.to_owned());
                        if !out.contains(&key) {
                            out.push(key);
                        }
                    }
                }
                Condition::Or(alternatives) => alternatives
                    .iter()
                    .flatten()
                    .for_each(|c| collect(c, from, out)),
                Condition::Filter(filter) => {
                    let (conditions, terms) = filter.parts();
                    for c in conditions {
                        collect(c, from, out);
                    }
                    for (table, name) in terms.iter().flat_map(|t| t.columns()) {
                        let key = (from[table].clone(), name.to_owned());
                        if !out.contains(&key) {
                            out.push(key);
                        }
                    }
                }
                _ => {}
            }
        }
        let mut out: Vec<(LogicalTable, String)> = Vec::new();
        for branch in &self.branches {
            for expr in &branch.select {
                if let SqlExpr::Term { term, .. } = expr {
                    for (table, name) in term.columns() {
                        let key = (branch.from[table].clone(), name.to_owned());
                        if !out.contains(&key) {
                            out.push(key);
                        }
                    }
                }
            }
            for condition in branch.all_conditions() {
                collect(condition, &branch.from, &mut out);
            }
        }
        out
    }

    /// The columns that branches select, each once, in order of
    /// appearance; empty with fewer than two branches. Their types go into
    /// [`with_union_types`](Self::with_union_types).
    pub fn union_columns(&self) -> Vec<(LogicalTable, String)> {
        let mut out: Vec<(LogicalTable, String)> = Vec::new();
        if self.branches.len() < 2 {
            return out;
        }
        for branch in &self.branches {
            for expr in &branch.select {
                if let SqlExpr::Column { table, name } = expr {
                    let key = (branch.from[*table].clone(), name.clone());
                    if !out.contains(&key) {
                        out.push(key);
                    }
                }
            }
        }
        out
    }

    /// The plan with result columns that `UNION` can combine, given the
    /// types of [`union_columns`](Self::union_columns).
    ///
    /// `UNION` matches columns by position and needs one type per result
    /// column, while a projected column may read columns of different SQL
    /// types in different branches. Where it does, and the types are not
    /// of one [`ColumnType::union_class`], each branch keeps its own type
    /// as long as no two of them can produce the same RDF term:
    ///
    /// - A literal of natural datatype (R2RML §10.2) is split into one
    ///   result column per natural datatype ([`SqlPlan::split_columns`]).
    ///   Terms of different datatypes differ, so removing duplicates stays
    ///   exact, and each value is read with its own type.
    /// - Columns whose terms can be the same, any other term or template
    ///   value, or a natural datatype shared by different classes (`text`
    ///   and `uuid` both give plain literals), are selected as their
    ///   natural lexical form, computed in SQL ([`Lexical`]), so equal
    ///   terms compare equal. A type without such SQL is an
    ///   [`Error::UnsupportedFeature`] instead of a guessed form.
    ///
    /// A branch where an `OPTIONAL` part cannot match selects
    /// [`SqlExpr::Unbound`] for its columns, which becomes a `NULL` of the
    /// type of the other branches' columns.
    ///
    /// Hidden columns are compared, never read, by their type and text
    /// output (see [`to_sql_with`](Self::to_sql_with)), which keeps terms
    /// of different types apart; there the lexical forms only replace
    /// columns whose terms can be the same, where SQL computes them.
    pub fn with_union_types(&self, types: &ColumnTypes) -> Result<Self, Error> {
        if self.branches.len() < 2 {
            return Ok(self.clone());
        }
        let mut owners: HashMap<&str, (&str, bool)> = HashMap::new();
        for projected in &self.projection {
            for term in &projected.terms {
                let natural = term.term_type == TermType::Literal
                    && matches!(term.value, ResultValue::Column(_))
                    && term.datatype.is_none()
                    && term.language.is_none();
                for alias in term.value.columns() {
                    owners.insert(alias, (&projected.variable, natural));
                }
            }
        }
        let mut used: HashSet<String> = self
            .columns
            .iter()
            .chain(&self.hidden_columns)
            .cloned()
            .collect();
        let mut plan = self.clone();
        plan.columns.clear();
        for branch in &mut plan.branches {
            branch.select.clear();
        }
        // Entries of the new select lists for each old one.
        let mut entries: Vec<Vec<usize>> = Vec::new();
        for (i, alias) in self.columns.iter().enumerate() {
            let start = plan.columns.len();
            let owner = owners.get(alias.as_str()).copied();
            for (alias, exprs) in self.union_column(i, alias, owner, false, types, &mut used)? {
                plan.columns.push(alias);
                for (branch, expr) in plan.branches.iter_mut().zip(exprs) {
                    branch.select.push(expr);
                }
            }
            if plan.columns.len() - start > 1 {
                plan.split_columns.push(plan.columns[start..].to_vec());
            }
            entries.push((start..plan.columns.len()).collect());
        }
        let width = plan.columns.len();
        for (h, alias) in self.hidden_columns.iter().enumerate() {
            let natural = !self.lexical_hidden_columns.contains(alias);
            let owner = Some((alias.as_str(), natural));
            let i = self.columns.len() + h;
            for (_, exprs) in self.union_column(i, alias, owner, true, types, &mut used)? {
                for (branch, expr) in plan.branches.iter_mut().zip(exprs) {
                    branch.select.push(expr);
                }
            }
        }
        entries.extend((0..self.hidden_columns.len()).map(|h| vec![width + h]));
        plan.distinct_order = self
            .distinct_order
            .iter()
            .flat_map(|&e| entries[e].iter().copied())
            .collect();
        Ok(plan)
    }

    /// The result columns for entry `i` of the select lists, each with
    /// one expression per branch; one column for a `hidden` entry.
    fn union_column(
        &self,
        i: usize,
        alias: &str,
        owner: Option<(&str, bool)>,
        hidden: bool,
        types: &ColumnTypes,
        used: &mut HashSet<String>,
    ) -> Result<Vec<(String, Vec<SqlExpr>)>, Error> {
        let exprs: Vec<&SqlExpr> = self.branches.iter().map(|b| &b.select[i]).collect();
        let unchanged = || vec![(alias.to_owned(), exprs.iter().map(|&e| e.clone()).collect())];
        let Some((variable, natural)) = owner else {
            return Ok(unchanged());
        };
        // `None` for a branch that selects `Unbound`.
        let mut columns: Vec<Option<(usize, &str, &ColumnType)>> = Vec::new();
        for (branch, expr) in self.branches.iter().zip(&exprs) {
            let (table, name) = match expr {
                SqlExpr::Column { table, name } => (*table, name),
                SqlExpr::Unbound => {
                    columns.push(None);
                    continue;
                }
                _ => return Ok(unchanged()),
            };
            let Some(ty) = types.get(&(branch.from[table].clone(), name.clone())) else {
                return Ok(unchanged());
            };
            columns.push(Some((table, name.as_str(), ty)));
        }
        let column = |b: usize| columns[b].expect("only branches with a column are members");
        let classes = |members: &[usize]| {
            let mut out: Vec<&str> = Vec::new();
            for &b in members {
                let class = column(b).2.union_class.as_str();
                if !out.contains(&class) {
                    out.push(class);
                }
            }
            out
        };
        let present: Vec<usize> = (0..columns.len())
            .filter(|&b| columns[b].is_some())
            .collect();
        let Some(&first) = present.first() else {
            return Ok(unchanged());
        };
        if classes(&present).len() < 2 {
            // `UNION` resolves the `NULL`s of two leading branches to text
            // before it sees a third, so they get the type of the others.
            let null = SqlExpr::Null(column(first).2.sql_type.clone());
            let exprs = exprs
                .iter()
                .map(|&e| match e {
                    SqlExpr::Unbound if !hidden => null.clone(),
                    other => other.clone(),
                })
                .collect();
            return Ok(vec![(alias.to_owned(), exprs)]);
        }

        // Branches whose terms can be the same, in order of first branch.
        let mut buckets: Vec<(Option<&str>, Vec<usize>)> = Vec::new();
        for &b in &present {
            let ty = column(b).2;
            let key = if natural {
                Some(ty.natural_datatype.as_deref().unwrap_or(XSD_STRING))
            } else {
                None
            };
            match buckets.iter_mut().find(|(k, _)| *k == key) {
                Some((_, members)) => members.push(b),
                None => buckets.push((key, vec![b])),
            }
        }
        // A lexical form read back as a string makes a plain literal, which
        // is only right for natural datatype xsd:string.
        let lexical_ok = |key: Option<&str>| key.is_none_or(|datatype| datatype == XSD_STRING);
        if hidden {
            let mut out: Vec<SqlExpr> = exprs.iter().map(|&e| e.clone()).collect();
            for (key, members) in &buckets {
                if classes(members).len() < 2 || !lexical_ok(*key) {
                    continue;
                }
                // Without SQL for one of the forms the columns stay as they
                // are, which can only keep a duplicate.
                let Some(lexical) = members
                    .iter()
                    .map(|&b| column(b).2.lexical)
                    .collect::<Option<Vec<Lexical>>>()
                else {
                    continue;
                };
                for (&b, lexical) in members.iter().zip(lexical) {
                    let (table, name, _) = column(b);
                    out[b] = SqlExpr::Lexical {
                        table,
                        name: name.to_owned(),
                        lexical,
                    };
                }
            }
            return Ok(vec![(alias.to_owned(), out)]);
        }
        let mut out = Vec::new();
        for (k, (key, members)) in buckets.iter().enumerate() {
            let typed = classes(members).len() == 1;
            let lexical_ok = lexical_ok(*key);
            let null = if typed {
                SqlExpr::Null(column(members[0]).2.sql_type.clone())
            } else {
                SqlExpr::Null("\"pg_catalog\".\"text\"".to_owned())
            };
            let mut exprs_k = Vec::with_capacity(columns.len());
            for (b, &expr) in exprs.iter().enumerate() {
                exprs_k.push(if !members.contains(&b) {
                    null.clone()
                } else if typed {
                    expr.clone()
                } else {
                    let (table, name, ty) = column(b);
                    match ty.lexical.filter(|_| lexical_ok) {
                        Some(lexical) => SqlExpr::Lexical {
                            table,
                            name: name.to_owned(),
                            lexical,
                        },
                        None => {
                            let names: Vec<&str> =
                                members.iter().map(|&m| column(m).2.name.as_str()).collect();
                            return Err(Error::UnsupportedFeature(format!(
                                "?{variable} reads columns of SQL types {} in different UNION \
                                 branches, whose terms can be the same: comparing them needs the \
                                 natural lexical form of column {name} of {} ({}), which cannot \
                                 be computed in SQL",
                                names.join(", "),
                                describe_table(&self.branches[b].from[table]),
                                ty.name
                            )));
                        }
                    }
                });
            }
            let alias = if k == 0 {
                alias.to_owned()
            } else {
                let fresh = (1..)
                    .map(|n| format!("{alias}_{n}"))
                    .find(|a| !used.contains(a))
                    .expect("an unused alias exists");
                used.insert(fresh.clone());
                fresh
            };
            out.push((alias, exprs_k));
        }
        Ok(out)
    }
}

impl SqlPlan {
    /// The column names that PostgreSQL may resolve differently from how
    /// they are written ([`folded_identifier`]), with their logical tables,
    /// each once, in order of appearance.
    pub fn folded_columns(&self) -> Vec<(LogicalTable, String)> {
        let mut out: Vec<(LogicalTable, String)> = Vec::new();
        let mut plan = self.clone();
        plan.for_each_column(|table, name| {
            if folded_identifier(name) {
                let key = (table.clone(), name.clone());
                if !out.contains(&key) {
                    out.push(key);
                }
            }
        });
        out
    }

    /// The plan with column names replaced by the names the database
    /// resolved them to, written as delimited identifiers so they are
    /// taken as they are. Names missing from `resolved` are kept.
    pub fn with_resolved_columns(
        &self,
        resolved: &HashMap<(LogicalTable, String), String>,
    ) -> Self {
        let mut plan = self.clone();
        plan.for_each_column(|table, name| {
            if let Some(found) = resolved.get(&(table.clone(), name.clone())) {
                *name = quote_identifier(found);
            }
        });
        plan
    }

    fn for_each_column(&mut self, mut f: impl FnMut(&LogicalTable, &mut String)) {
        fn expr(
            e: &mut SqlExpr,
            from: &[LogicalTable],
            f: &mut impl FnMut(&LogicalTable, &mut String),
        ) {
            match e {
                SqlExpr::Column { table, name } | SqlExpr::Lexical { table, name, .. } => {
                    f(&from[*table], name);
                }
                SqlExpr::Param(_) | SqlExpr::Null(_) | SqlExpr::Unbound | SqlExpr::True => {}
                SqlExpr::Concat(parts) => parts.iter_mut().for_each(|p| expr(p, from, f)),
                SqlExpr::Matched(inner) => expr(inner, from, f),
                SqlExpr::Term { term: t, guard } => {
                    term(t, from, f);
                    if let Some(guard) = guard {
                        expr(guard, from, f);
                    }
                }
            }
        }
        fn term(
            t: &mut TermSql,
            from: &[LogicalTable],
            f: &mut impl FnMut(&LogicalTable, &mut String),
        ) {
            match &mut t.value {
                TermSqlValue::Column { table, name } => f(&from[*table], name),
                TermSqlValue::Template { table, parts, .. } => {
                    for part in parts {
                        if let TemplatePart::Column(name) = part {
                            f(&from[*table], name);
                        }
                    }
                }
                TermSqlValue::Param(_) => {}
            }
        }
        for branch in &mut self.branches {
            let from = &branch.from;
            for e in &mut branch.select {
                expr(e, from, &mut f);
            }
            fn condition(
                c: &mut Condition,
                from: &[LogicalTable],
                f: &mut impl FnMut(&LogicalTable, &mut String),
            ) {
                match c {
                    Condition::Eq(left, right) => {
                        expr(left, from, f);
                        expr(right, from, f);
                    }
                    Condition::EqAny(left, right) => {
                        expr(left, from, f);
                        right.iter_mut().for_each(|e| expr(e, from, f));
                    }
                    Condition::IsNotNull(e) | Condition::IsNull(e) => expr(e, from, f),
                    Condition::Or(alternatives) => alternatives
                        .iter_mut()
                        .flatten()
                        .for_each(|c| condition(c, from, f)),
                    Condition::SameTerm(left, right) => {
                        term(left, from, f);
                        term(right, from, f);
                    }
                    Condition::Filter(filter) => filter.visit_mut(&mut |part| match part {
                        PartMut::Condition(c) => condition(c, from, f),
                        PartMut::Term(t) => term(t, from, f),
                    }),
                }
            }
            let conditions = branch
                .conditions
                .iter_mut()
                .chain(branch.left_joins.iter_mut().flat_map(|j| &mut j.conditions));
            for c in conditions {
                condition(c, from, &mut f);
            }
            for (table, fixed) in from.iter().zip(&mut branch.fixed_columns) {
                *fixed = std::mem::take(fixed)
                    .into_iter()
                    .map(|mut name| {
                        f(table, &mut name);
                        name
                    })
                    .collect();
            }
        }
    }
}

impl SqlBranch {
    /// The conditions of the branch and of its `OPTIONAL` parts.
    pub fn all_conditions(&self) -> impl Iterator<Item = &Condition> {
        self.conditions
            .iter()
            .chain(self.left_joins.iter().flat_map(|j| &j.conditions))
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

/// A statement whose result columns have the types of `columns`, to
/// prepare (not run) so the database reports them. Each logical table
/// appears once.
pub fn column_type_probe(columns: &[(LogicalTable, String)]) -> String {
    probe(columns, quote_identifier)
}

/// A statement that refers to each of `columns`, all
/// [`folded_identifier`]s, as written, without quotes, to prepare (not
/// run): the database then resolves each one as it resolves a regular
/// identifier and reports the name it found as the result column's name.
/// Each logical table appears once.
pub fn column_name_probe(columns: &[(LogicalTable, String)]) -> String {
    assert!(
        columns.iter().all(|(_, name)| folded_identifier(name)),
        "only regular identifiers may appear unquoted"
    );
    probe(columns, str::to_owned)
}

fn probe(columns: &[(LogicalTable, String)], name_sql: impl Fn(&str) -> String) -> String {
    let mut tables: Vec<&LogicalTable> = Vec::new();
    let select: Vec<String> = columns
        .iter()
        .map(|(table, name)| {
            let position = tables.iter().position(|t| *t == table).unwrap_or_else(|| {
                tables.push(table);
                tables.len() - 1
            });
            format!("p{position}.{}", name_sql(name))
        })
        .collect();
    let from: Vec<String> = tables
        .iter()
        .enumerate()
        .map(|(i, table)| format!("{} p{i}", logical_table(table)))
        .collect();
    format!(
        "SELECT {}\nFROM {}\nLIMIT 0",
        select.join(", "),
        from.join("\nCROSS JOIN ")
    )
}

/// A table or a nested `OPTIONAL` part of a join tree.
#[derive(Debug, Clone, Copy)]
enum Member {
    Table(usize),
    Part(usize),
}

/// The select list of every branch.
struct Select {
    /// Alias of each entry of [`SqlBranch::select`] that is rendered.
    aliases: Vec<String>,
    /// The entries in the order they are selected.
    order: Vec<usize>,
    /// Entries from here on are only compared, never decoded, and are
    /// selected as their type and text output.
    typed_text_from: Option<usize>,
}

#[derive(Default)]
struct Renderer<'a> {
    sql: String,
    params: Vec<String>,
    param_datatypes: Vec<Option<String>>,
    param_branches: Vec<usize>,
    param_required: Vec<bool>,
    param_alternatives: Vec<Option<usize>>,
    alternatives: usize,
    branch_index: usize,
    /// Whether the conditions being rendered hold for every row of the
    /// branch, rather than in an `OPTIONAL` part or a disjunction.
    required: bool,
    /// Units rendered so far, for their aliases.
    units: usize,
    options: RenderOptions<'a>,
    /// Logical tables of the branch being rendered.
    from: &'a [LogicalTable],
}

impl<'a> Renderer<'a> {
    fn branch(
        &mut self,
        branch: &'a SqlBranch,
        select: &Select,
        distinct: bool,
    ) -> Result<(), Error> {
        self.from = &branch.from;
        // Constants of the select list are strings of terms, never
        // compared, so binding one cannot empty the branch.
        self.required = false;
        self.sql.push_str("SELECT ");
        if distinct {
            self.sql.push_str("DISTINCT ");
        }
        if select.order.is_empty() {
            self.sql.push('1');
        }
        for (i, &entry) in select.order.iter().enumerate() {
            if i > 0 {
                self.sql.push_str(", ");
            }
            if select.typed_text_from.is_some_and(|from| entry >= from) {
                // UNION matches columns by position, and the same variable
                // may read columns of different types in different
                // branches. Equal type and text output imply the same
                // value, hence the same term; the reverse can fail
                // (`numeric` 1.0 and 1.00, or `varchar` and `text`), which
                // only keeps a duplicate.
                let start = self.sql.len();
                self.select_expr(&branch.select[entry])?;
                let column = self.sql.split_off(start);
                self.sql.push_str(&format!(
                    "pg_typeof({column})::text || ' ' || {column}::text"
                ));
            } else {
                self.select_expr(&branch.select[entry])?;
            }
            self.sql.push_str(" AS ");
            self.sql.push_str(&quote_identifier(&select.aliases[entry]));
        }

        self.required = true;
        if branch.from.is_empty() {
            // Every `OPTIONAL` part was left out: one row, of unbound values.
            if !branch.conditions.is_empty() {
                self.sql.push_str("\nWHERE ");
                let conditions: Vec<&Condition> = branch.conditions.iter().collect();
                self.conditions(&conditions)?;
            }
            return Ok(());
        }
        self.sql.push_str("\nFROM ");
        let filters = self.join_tree(branch, None)?;
        if !filters.is_empty() {
            self.sql.push_str("\nWHERE ");
            self.conditions(&filters)?;
        }
        Ok(())
    }

    /// The tables of `scope`, the whole branch or one of its `OPTIONAL`
    /// parts, joined: its own tables with `JOIN`, the parts nested in it
    /// with `LEFT JOIN`, in the order they are written. Returns the
    /// conditions of `scope` that are left for its `WHERE` or `ON` clause.
    ///
    /// A condition that links two tables of the scope goes to the `ON`
    /// clause of the later one, as long as that is a table of the scope
    /// rather than a nested part: a condition on a nested part's columns
    /// (its variables being unbound, or compatible) filters the rows of
    /// `scope` after the left join and must not decide whether the part
    /// matches.
    fn join_tree(
        &mut self,
        branch: &'a SqlBranch,
        scope: Option<usize>,
    ) -> Result<Vec<&'a Condition>, Error> {
        let (range, conditions) = match scope {
            None => (0..branch.from.len(), &branch.conditions),
            Some(j) => (
                branch.left_joins[j].tables.clone(),
                &branch.left_joins[j].conditions,
            ),
        };
        // The members of the scope: its own tables and its nested parts.
        let mut members: Vec<Member> = Vec::new();
        let mut member_of = vec![None; branch.from.len()];
        let mut t = range.start;
        while t < range.end {
            let nested =
                branch.left_joins.iter().enumerate().find(|(_, j)| {
                    j.parent == scope && j.tables.start == t && !j.tables.is_empty()
                });
            let end = match nested {
                Some((j, join)) => {
                    members.push(Member::Part(j));
                    join.tables.end
                }
                None => {
                    members.push(Member::Table(t));
                    t + 1
                }
            };
            for slot in &mut member_of[t..end] {
                *slot = Some(members.len() - 1);
            }
            t = end;
        }

        let mut on: Vec<Vec<&Condition>> = vec![Vec::new(); members.len()];
        let mut rest = Vec::new();
        for condition in conditions {
            let tables = condition_tables(condition);
            let owners: Option<BTreeSet<usize>> =
                tables.iter().map(|&table| member_of[table]).collect();
            match owners
                .as_ref()
                .and_then(|o| o.last().map(|&m| (o.len(), m)))
            {
                Some((n, last)) if n > 1 && matches!(members[last], Member::Table(_)) => {
                    on[last].push(condition)
                }
                _ => rest.push(condition),
            }
        }

        for (k, member) in members.iter().enumerate() {
            if k == 0 && matches!(member, Member::Part(_)) {
                // `{ OPTIONAL { ... } }`: the left side is the empty
                // pattern, whose one solution binds nothing.
                self.sql.push_str(&format!("(SELECT) u{}", self.units));
                self.units += 1;
            }
            match *member {
                Member::Table(i) => {
                    if k > 0 {
                        self.sql.push_str(if on[k].is_empty() {
                            "\nCROSS JOIN "
                        } else {
                            "\nJOIN "
                        });
                    }
                    self.sql.push_str(&logical_table(&branch.from[i]));
                    self.sql.push_str(&format!(" t{i}"));
                    if k > 0 && !on[k].is_empty() {
                        self.sql.push_str(" ON ");
                        self.conditions(&on[k])?;
                    }
                }
                Member::Part(part) => {
                    self.sql.push_str("\nLEFT JOIN ");
                    let required = std::mem::replace(&mut self.required, false);
                    let single = branch.left_joins[part].tables.len() == 1
                        && !branch.left_joins.iter().any(|j| j.parent == Some(part));
                    if !single {
                        self.sql.push('(');
                    }
                    let rest = self.join_tree(branch, Some(part))?;
                    if !single {
                        self.sql.push(')');
                    }
                    self.sql.push_str(" ON ");
                    if rest.is_empty() {
                        self.sql.push_str("true");
                    } else {
                        self.conditions(&rest)?;
                    }
                    self.required = required;
                }
            }
        }
        Ok(rest)
    }

    fn conditions(&mut self, conditions: &[&Condition]) -> Result<(), Error> {
        for (i, condition) in conditions.iter().enumerate() {
            if i > 0 {
                self.sql.push_str(" AND ");
            }
            match condition {
                Condition::Eq(left, right) => {
                    // A column side holds no parameter, so the parameter
                    // on the other side is the next one either way.
                    let as_text = matches!(
                        (left, right),
                        (SqlExpr::Column { .. }, SqlExpr::Param(_))
                            | (SqlExpr::Param(_), SqlExpr::Column { .. })
                    ) && self.options.text_params.contains(&self.params.len());
                    self.expr(left);
                    if as_text && matches!(left, SqlExpr::Column { .. }) {
                        self.sql.push_str("::text");
                    }
                    self.sql.push_str(" = ");
                    self.expr(right);
                    if as_text && matches!(right, SqlExpr::Column { .. }) {
                        self.sql.push_str("::text");
                    }
                }
                Condition::EqAny(left, right) => {
                    let as_text = matches!(left, SqlExpr::Column { .. })
                        && self.options.text_params.contains(&self.params.len());
                    self.expr(left);
                    if as_text {
                        self.sql.push_str("::text");
                    }
                    self.sql.push_str(" IN (");
                    let group = self.alternatives;
                    self.alternatives += 1;
                    for (i, alternative) in right.iter().enumerate() {
                        if i > 0 {
                            self.sql.push_str(", ");
                        }
                        let before = self.params.len();
                        self.expr(alternative);
                        for slot in &mut self.param_alternatives[before..] {
                            *slot = Some(group);
                        }
                    }
                    self.sql.push(')');
                }
                Condition::IsNotNull(expr) => {
                    self.expr(expr);
                    self.sql.push_str(" IS NOT NULL");
                }
                Condition::IsNull(expr) => {
                    self.expr(expr);
                    self.sql.push_str(" IS NULL");
                }
                Condition::Or(alternatives) => {
                    let required = std::mem::replace(&mut self.required, false);
                    self.sql.push('(');
                    for (i, alternative) in alternatives.iter().enumerate() {
                        if i > 0 {
                            self.sql.push_str(" OR ");
                        }
                        let conjunction: Vec<&Condition> = alternative.iter().collect();
                        if conjunction.len() > 1 {
                            self.sql.push('(');
                        }
                        self.conditions(&conjunction)?;
                        if conjunction.len() > 1 {
                            self.sql.push(')');
                        }
                    }
                    self.sql.push(')');
                    self.required = required;
                }
                Condition::SameTerm(left, right) => self.same_term(left, right)?,
                Condition::Filter(filter) => {
                    // A constant of a FILTER binds to NULL only where it is
                    // compared as an RDF term, which fails that comparison
                    // and not the branch.
                    let required = std::mem::replace(&mut self.required, false);
                    self.filter(filter, filter::Mode::Positive)?;
                    self.required = required;
                }
            }
        }
        Ok(())
    }

    fn same_term(&mut self, left: &TermSql, right: &TermSql) -> Result<(), Error> {
        let Some(types) = self.options.column_types else {
            self.term_side(left, None, false)?;
            self.sql.push_str(" = ");
            return self.term_side(right, None, false);
        };
        if !self.datatypes_agree(left, right, types) {
            self.sql.push_str("false");
            return Ok(());
        }
        if let (
            TermSqlValue::Column {
                table: lt,
                name: ln,
            },
            TermSqlValue::Column {
                table: rt,
                name: rn,
            },
        ) = (&left.value, &right.value)
            && left.base_iri.is_none()
            && right.base_iri.is_none()
            && let (Some(a), Some(b)) = (self.column_type(*lt, ln), self.column_type(*rt, rn))
            && a.group == b.group
        {
            let cast = match a.comparison {
                Comparison::Value => "",
                Comparison::Text => "::text",
            };
            self.sql.push_str(&format!(
                "{}{cast} = {}{cast}",
                column_sql(*lt, ln),
                column_sql(*rt, rn)
            ));
            return Ok(());
        }
        self.term_side(left, Some(types), false)?;
        self.sql.push_str(" = ");
        self.term_side(right, Some(types), false)
    }

    /// R2RML §10.2: a literal from a column without `rr:datatype` has its
    /// SQL type's natural datatype, so it can only equal a literal with
    /// that datatype.
    fn datatypes_agree(&self, left: &TermSql, right: &TermSql, types: &ColumnTypes) -> bool {
        let datatype = |side: &TermSql| -> Option<String> {
            match (&side.datatype, &side.value) {
                (Some(TermDatatype::Iri(iri)), _) => Some(iri.clone()),
                (Some(TermDatatype::Natural), TermSqlValue::Column { table, name }) => {
                    let ty = types.get(&(self.from[*table].clone(), name.clone()))?;
                    Some(
                        ty.natural_datatype
                            .as_deref()
                            .unwrap_or(XSD_STRING)
                            .to_owned(),
                    )
                }
                _ => None,
            }
        };
        let natural = |side: &TermSql| side.datatype == Some(TermDatatype::Natural);
        if !natural(left) && !natural(right) {
            return true;
        }
        match (datatype(left), datatype(right)) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        }
    }

    fn column_type(&self, table: usize, name: &str) -> Option<&'a ColumnType> {
        self.options
            .column_types?
            .get(&(self.from[table].clone(), name.to_owned()))
    }

    /// One side of a term comparison as a string expression. Without
    /// `types`, columns are rendered as they are, or by their text output
    /// with `text`, and template columns by their text output.
    ///
    /// With a base IRI, the side is the IRI R2RML §11.2 generates: `CASE
    /// WHEN s ~ '^scheme:' THEN s ELSE $base || s END`, the same test as
    /// [`has_scheme`](crate::mapping::has_scheme), with `s` rendered three
    /// times, since a column is the common case and costs nothing to repeat.
    fn term_side(
        &mut self,
        side: &TermSql,
        types: Option<&ColumnTypes>,
        text: bool,
    ) -> Result<(), Error> {
        let Some(base) = &side.base_iri else {
            return self.term_string(side, types, text);
        };
        self.sql.push_str("CASE WHEN (");
        self.term_string(side, types, true)?;
        self.sql.push_str(&format!(") ~ '{SCHEME_REGEX}' THEN ("));
        self.term_string(side, types, true)?;
        self.sql.push_str(") ELSE ");
        self.expr(&SqlExpr::Param(base.as_str().into()));
        self.sql.push_str(" || (");
        self.term_string(side, types, true)?;
        self.sql.push_str(") END");
        Ok(())
    }

    /// The string of one side; `text` casts a column without `types` to
    /// text, which comparing it with a pattern needs.
    fn term_string(
        &mut self,
        side: &TermSql,
        types: Option<&ColumnTypes>,
        text: bool,
    ) -> Result<(), Error> {
        match &side.value {
            TermSqlValue::Column { table, name } => match types {
                None if text => self
                    .sql
                    .push_str(&format!("{}::text", column_sql(*table, name))),
                None => self.sql.push_str(&column_sql(*table, name)),
                Some(_) => {
                    let lexical = self.lexical(*table, name)?;
                    self.sql.push_str(&lexical);
                }
            },
            TermSqlValue::Template {
                table,
                parts,
                iri_safe,
            } => {
                for (i, part) in parts.iter().enumerate() {
                    if i > 0 {
                        self.sql.push_str(" || ");
                    }
                    match part {
                        TemplatePart::Literal(text) => {
                            self.expr(&SqlExpr::Param(text.as_str().into()))
                        }
                        TemplatePart::Column(name) => {
                            let value = match types {
                                None => format!("{}::text", column_sql(*table, name)),
                                Some(_) => self.lexical(*table, name)?,
                            };
                            self.sql.push_str(&if *iri_safe {
                                iri_safe_sql(&value)
                            } else {
                                value
                            });
                        }
                    }
                }
            }
            TermSqlValue::Param(param) => self.expr(&SqlExpr::Param(param.clone())),
        }
        Ok(())
    }

    /// SQL for the natural lexical form of a column.
    fn lexical(&self, table: usize, name: &str) -> Result<String, Error> {
        let column = column_sql(table, name);
        let Some(ty) = self.column_type(table, name) else {
            return Ok(format!("{column}::text"));
        };
        match ty.lexical {
            Some(lexical) => Ok(lexical_sql(&column, lexical)),
            None => Err(Error::UnsupportedFeature(format!(
                "comparing terms built from column {name} of {} ({}) with terms of a \
                 different SQL type or shape: its natural lexical form cannot be computed in SQL",
                describe_table(&self.from[table]),
                ty.name
            ))),
        }
    }

    /// An entry of a select list: [`SqlExpr::Term`] or any other.
    fn select_expr(&mut self, expr: &SqlExpr) -> Result<(), Error> {
        let SqlExpr::Term { term, guard } = expr else {
            self.expr(expr);
            return Ok(());
        };
        if let Some(guard) = guard {
            self.sql.push_str("CASE WHEN ");
            self.expr(guard);
            self.sql.push_str(" IS NOT NULL THEN ");
        }
        // Text in every branch, so that `UNION` combines them.
        self.sql.push('(');
        let types = self.options.column_types;
        self.term_side(term, types, true)?;
        self.sql.push_str(")::text");
        if guard.is_some() {
            self.sql.push_str(" END");
        }
        Ok(())
    }

    fn expr(&mut self, expr: &SqlExpr) {
        match expr {
            SqlExpr::Column { table, name } => self.sql.push_str(&column_sql(*table, name)),
            SqlExpr::Param(param) => {
                self.params.push(param.value.clone());
                self.param_datatypes.push(param.datatype.clone());
                self.param_branches.push(self.branch_index);
                self.param_required.push(self.required);
                self.param_alternatives.push(None);
                self.sql.push_str(&format!("${}", self.params.len()));
            }
            SqlExpr::Concat(parts) => {
                for (i, part) in parts.iter().enumerate() {
                    if i > 0 {
                        self.sql.push_str(" || ");
                    }
                    self.expr(part);
                    // An untyped parameter next to `||` already resolves to text.
                    if matches!(part, SqlExpr::Column { .. }) {
                        self.sql.push_str("::text");
                    }
                }
            }
            SqlExpr::Lexical {
                table,
                name,
                lexical,
            } => self
                .sql
                .push_str(&lexical_sql(&column_sql(*table, name), *lexical)),
            // Typed, since `UNION` resolves an untyped NULL of two branches
            // to text before it sees the third.
            SqlExpr::Null(ty) => self.sql.push_str(&format!("NULL::{ty}")),
            SqlExpr::Unbound => self.sql.push_str("NULL"),
            SqlExpr::Matched(inner) => {
                self.sql.push_str("CASE WHEN ");
                self.expr(inner);
                self.sql.push_str(" IS NOT NULL THEN true END");
            }
            SqlExpr::True => self.sql.push_str("true"),
            SqlExpr::Term { .. } => unreachable!("only select lists hold terms"),
        }
    }
}

/// Whether the branches of one arm may give the same solution twice: one
/// branch proves it may not when `keys` holds, for every table it reads, a
/// unique key among its [`SqlBranch::fixed_columns`].
fn arm_has_duplicates(branches: &[SqlBranch], keys: Option<&TableKeys>) -> bool {
    let ([branch], Some(keys)) = (branches, keys) else {
        return true;
    };
    !branch
        .from
        .iter()
        .zip(&branch.fixed_columns)
        .all(|(table, fixed)| {
            let fixed: BTreeSet<String> = fixed.iter().map(|c| identifier_name(c)).collect();
            keys.get(table)
                .is_some_and(|keys| keys.iter().any(|key| key.iter().all(|c| fixed.contains(c))))
        })
}

fn column_sql(table: usize, name: &str) -> String {
    format!("t{table}.{}", quote_identifier(name))
}

/// SQL for the natural lexical form of `column`.
fn lexical_sql(column: &str, lexical: Lexical) -> String {
    match lexical {
        Lexical::Text => format!("{column}::text"),
        Lexical::Boolean => {
            format!("CASE {column} WHEN true THEN 'true' WHEN false THEN 'false' END")
        }
        Lexical::Hex => format!("upper(encode({column}, 'hex'))"),
        // `numeric` output never uses an exponent, and trim_scale drops
        // exactly the trailing fractional zeros, which is the XML Schema
        // 1.1 decimalCanonicalMap (`3`, `12.5`); tested against
        // `canonical_decimal` over many values in v12n-postgres.
        Lexical::Decimal => format!("trim_scale({column})::text"),
    }
}

/// A logical table for error messages.
fn describe_table(table: &LogicalTable) -> String {
    match table {
        LogicalTable::TableName(t) => format!("table {t}"),
        LogicalTable::SqlQuery(_) => "an rr:sqlQuery".to_owned(),
    }
}

/// R2RML §7.3 IRI-safe version of a text expression: every character
/// outside `iunreserved` becomes `%XX` per UTF-8 byte, upper case, as the
/// engine encodes template values when it builds IRIs. NULL stays NULL.
pub fn iri_safe_sql(text: &str) -> String {
    let mut class = String::from("^[-.0-9A-Z_a-z~");
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
    class.push_str("]$");
    format!(
        "CASE WHEN {text} IS NULL THEN NULL ELSE (SELECT coalesce(string_agg(\
         CASE WHEN c ~ '{class}' THEN c \
         ELSE regexp_replace(upper(encode(convert_to(c, 'UTF8'), 'hex')), '(..)', '%\\1', 'g') END, \
         '' ORDER BY n), '') \
         FROM generate_series(1, char_length({text})) AS n, substr({text}, n, 1) AS c) END"
    )
}

/// Table aliases a condition refers to, in ascending order.
fn condition_tables(condition: &Condition) -> Vec<usize> {
    fn collect(expr: &SqlExpr, out: &mut BTreeSet<usize>) {
        match expr {
            SqlExpr::Column { table, .. } | SqlExpr::Lexical { table, .. } => {
                out.insert(*table);
            }
            SqlExpr::Param(_) | SqlExpr::Null(_) | SqlExpr::Unbound | SqlExpr::True => {}
            SqlExpr::Concat(parts) => parts.iter().for_each(|p| collect(p, out)),
            SqlExpr::Matched(inner) => collect(inner, out),
            SqlExpr::Term { term, guard } => {
                out.extend(term.columns().into_iter().map(|(table, _)| table));
                if let Some(guard) = guard {
                    collect(guard, out);
                }
            }
        }
    }
    fn tables(condition: &Condition, out: &mut BTreeSet<usize>) {
        match condition {
            Condition::Eq(left, right) => {
                collect(left, out);
                collect(right, out);
            }
            Condition::EqAny(left, right) => {
                collect(left, out);
                right.iter().for_each(|e| collect(e, out));
            }
            Condition::IsNotNull(expr) | Condition::IsNull(expr) => collect(expr, out),
            Condition::Or(alternatives) => {
                alternatives.iter().flatten().for_each(|c| tables(c, out))
            }
            Condition::SameTerm(left, right) => {
                for (table, _) in left.columns().into_iter().chain(right.columns()) {
                    out.insert(table);
                }
            }
            Condition::Filter(filter) => {
                let (conditions, terms) = filter.parts();
                for c in conditions {
                    tables(c, out);
                }
                for t in terms {
                    out.extend(t.columns().into_iter().map(|(table, _)| table));
                }
            }
        }
    }
    let mut out = BTreeSet::new();
    tables(condition, &mut out);
    out.into_iter().collect()
}

fn logical_table(table: &LogicalTable) -> String {
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
    fn column_name_probe_leaves_regular_identifiers_unquoted() {
        let view = LogicalTable::SqlQuery("SELECT 1 AS Id".into());
        let table = LogicalTable::TableName("t".into());
        assert_eq!(
            column_name_probe(&[
                (view.clone(), "Id".into()),
                (table, "Name".into()),
                (view, "LABEL".into()),
            ]),
            "SELECT p0.Id, p1.Name, p0.LABEL\nFROM (SELECT 1 AS Id) p0\nCROSS JOIN \"t\" p1\nLIMIT 0"
        );
    }

    #[test]
    #[should_panic(expected = "only regular identifiers")]
    fn column_name_probe_refuses_other_names() {
        column_name_probe(&[(LogicalTable::TableName("t".into()), "a; DROP".into())]);
    }

    #[test]
    fn resolved_columns_are_rendered_as_delimited_identifiers() {
        let view = LogicalTable::SqlQuery("SELECT 1 AS Id, 2 AS \"V\"".into());
        let template = |table| TermSql {
            value: TermSqlValue::Template {
                table,
                parts: vec![
                    TemplatePart::Literal("r/".into()),
                    TemplatePart::Column("Id".into()),
                ],
                iri_safe: false,
            },
            datatype: None,
            base_iri: None,
        };
        let plan = SqlPlan {
            columns: vec!["a".into(), "b".into()],
            hidden_columns: Vec::new(),
            distinct_order: Vec::new(),
            split_columns: Vec::new(),
            lexical_hidden_columns: Default::default(),
            projection: Vec::new(),
            branches: vec![SqlBranch {
                arm: 0,
                from: vec![view.clone(), view.clone()],
                select: vec![column(0, "Id"), column(1, "\"V\"")],
                conditions: vec![
                    Condition::SameTerm(template(0), template(1)),
                    Condition::Eq(column(0, "ID"), SqlExpr::Param("1".into())),
                ],
                left_joins: Vec::new(),
                fixed_columns: Vec::new(),
            }],
            distinct: false,
            base_iri: None,
        };
        assert_eq!(
            plan.folded_columns(),
            [
                (view.clone(), "Id".to_owned()),
                (view.clone(), "ID".to_owned())
            ]
        );
        let resolved = HashMap::from([
            ((view.clone(), "Id".to_owned()), "id".to_owned()),
            ((view, "ID".to_owned()), "id".to_owned()),
        ]);
        assert_eq!(
            plan.with_resolved_columns(&resolved).to_sql().sql,
            "SELECT DISTINCT t0.\"id\" AS \"a\", t1.\"V\" AS \"b\"\n\
             FROM (SELECT 1 AS Id, 2 AS \"V\") t0\n\
             JOIN (SELECT 1 AS Id, 2 AS \"V\") t1 ON $1 || t0.\"id\"::text = $2 || t1.\"id\"::text\n\
             WHERE t0.\"id\" = $3"
        );
    }

    #[test]
    fn resolved_columns_keep_keys_matching_the_fixed_columns() {
        let table = LogicalTable::TableName("t".into());
        let plan = SqlPlan {
            columns: vec!["s".into()],
            hidden_columns: Vec::new(),
            distinct_order: Vec::new(),
            split_columns: Vec::new(),
            lexical_hidden_columns: Default::default(),
            projection: Vec::new(),
            branches: vec![SqlBranch {
                arm: 0,
                from: vec![table.clone()],
                select: vec![column(0, "ID")],
                conditions: Vec::new(),
                left_joins: Vec::new(),
                fixed_columns: vec![["ID".to_owned()].into()],
            }],
            distinct: false,
            base_iri: None,
        };
        let resolved = HashMap::from([((table.clone(), "ID".to_owned()), "id".to_owned())]);
        let plan = plan.with_resolved_columns(&resolved);
        assert_eq!(
            plan.branches[0].fixed_columns,
            [["\"id\"".to_owned()].into()]
        );
        let keys: TableKeys = [(table, vec![vec!["id".to_owned()]])].into();
        assert!(!plan.has_duplicates(Some(&keys)));
    }

    fn optional_plan(query: &str) -> SqlPlan {
        let mapping = crate::mapping::Mapping::from_turtle(include_str!(
            "../../../examples/shop/mapping.ttl"
        ))
        .unwrap();
        let index = crate::mapping::MappingIndex::new(&mapping);
        let query =
            crate::sparql::parse(&format!("PREFIX ex: <http://example.org/>\n{query}")).unwrap();
        crate::unfold::unfold(&query, &mapping, &index).unwrap()
    }

    #[test]
    fn to_sql_nests_optional_parts_and_keeps_their_conditions_in_on() {
        let plan = optional_plan(
            "SELECT ?c ?o ?a WHERE { ?c ex:tier \"VIP\" \
             OPTIONAL { ?o ex:placedBy ?c OPTIONAL { ?o ex:amount ?a } } }",
        );
        let rendered = plan.to_sql();
        assert_eq!(
            rendered.sql,
            "SELECT DISTINCT t0.\"id\" AS \"c\", t1.\"id\" AS \"o\", t2.\"amount\" AS \"a\"\n\
             FROM \"customers\" t0\n\
             LEFT JOIN (\"orders\" t1\n\
             LEFT JOIN \"orders\" t2 ON t2.\"id\" = t1.\"id\" AND t2.\"amount\" IS NOT NULL) \
             ON t1.\"customer_id\" = t0.\"id\" AND t1.\"id\" IS NOT NULL\n\
             WHERE t0.\"tier\" = $1 AND t0.\"id\" IS NOT NULL"
        );
        assert_eq!(rendered.param_required, [true]);
    }

    /// A constant that cannot match fails its own condition: in an
    /// `OPTIONAL` part, the part does not match, and the branch stays.
    #[test]
    fn to_sql_marks_parameters_of_optional_parts_as_not_required() {
        let plan = optional_plan(
            "SELECT ?c ?o WHERE { ?c ex:tier \"VIP\" OPTIONAL { ?o ex:placedBy ?c ; ex:amount 100 } }",
        );
        let rendered = plan.to_sql();
        assert_eq!(rendered.params, ["100", "VIP"]);
        assert_eq!(rendered.param_required, [false, true]);
    }

    #[test]
    fn to_sql_puts_conditions_on_an_optional_part_after_the_left_join() {
        // ?c may be unbound after the OPTIONAL: the last pattern must not
        // decide whether the part matches.
        let plan = optional_plan(
            "SELECT ?o ?n WHERE { ?o ex:amount ?a OPTIONAL { ?o ex:placedBy ?c } ?c ex:name ?n }",
        );
        assert_eq!(
            plan.to_sql().sql,
            "SELECT q.\"o\", q.\"n\"\nFROM (\n\
             SELECT DISTINCT t0.\"id\" AS \"o\", t0.\"amount\" AS \"a\", t2.\"id\" AS \"c\", \
             t2.\"name\" AS \"n\"\n\
             FROM \"orders\" t0\n\
             LEFT JOIN \"orders\" t1 ON t1.\"id\" = t0.\"id\" AND t1.\"customer_id\" IS NOT NULL\n\
             JOIN \"customers\" t2 ON (t1.\"customer_id\" IS NULL OR t2.\"id\" = t1.\"customer_id\")\n\
             WHERE t0.\"id\" IS NOT NULL AND t0.\"amount\" IS NOT NULL AND t2.\"id\" IS NOT NULL \
             AND t2.\"name\" IS NOT NULL\n\
             ) q"
        );
    }

    #[test]
    fn to_sql_starts_from_one_row_when_the_left_side_is_empty() {
        let plan = optional_plan("SELECT ?c WHERE { OPTIONAL { ?c ex:tier ?t } }");
        assert!(
            plan.to_sql()
                .sql
                .contains("FROM (SELECT) u0\nLEFT JOIN \"customers\" t0 ON "),
            "{}",
            plan.to_sql().sql
        );
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

    fn column(table: usize, name: &str) -> SqlExpr {
        SqlExpr::Column {
            table,
            name: name.to_owned(),
        }
    }

    #[test]
    fn to_sql_places_linking_conditions_in_on_and_numbers_params_in_order() {
        let plan = SqlPlan {
            columns: vec!["x".into()],
            hidden_columns: Vec::new(),
            distinct_order: Vec::new(),
            split_columns: Vec::new(),
            lexical_hidden_columns: Default::default(),
            projection: Vec::new(),
            branches: vec![SqlBranch {
                arm: 0,
                from: vec![
                    LogicalTable::TableName("a".into()),
                    LogicalTable::SqlQuery("SELECT * FROM b;\n".into()),
                    LogicalTable::TableName("c".into()),
                ],
                select: vec![column(0, "id")],
                conditions: vec![
                    Condition::Eq(column(0, "k"), SqlExpr::Param("one".into())),
                    Condition::Eq(
                        column(1, "id"),
                        SqlExpr::Concat(vec![SqlExpr::Param("p/".into()), column(0, "id")]),
                    ),
                    Condition::IsNotNull(column(2, "v")),
                ],
                left_joins: Vec::new(),
                fixed_columns: Vec::new(),
            }],
            distinct: true,
            base_iri: None,
        };
        let rendered = plan.to_sql();
        assert_eq!(
            rendered.sql,
            "SELECT DISTINCT t0.\"id\" AS \"x\"\n\
             FROM \"a\" t0\n\
             JOIN (SELECT * FROM b) t1 ON t1.\"id\" = $1 || t0.\"id\"::text\n\
             CROSS JOIN \"c\" t2\n\
             WHERE t0.\"k\" = $2 AND t2.\"v\" IS NOT NULL"
        );
        assert_eq!(rendered.params, ["p/", "one"]);
        assert_eq!(rendered.param_branches, [0, 0]);
    }

    #[test]
    fn to_sql_with_text_params_casts_the_compared_column() {
        let branch = |value: &str, datatype: Option<&str>| SqlBranch {
            arm: 0,
            from: vec![LogicalTable::TableName("t".into())],
            select: vec![column(0, "id")],
            conditions: vec![
                Condition::Eq(
                    column(0, "a"),
                    SqlExpr::Concat(vec![SqlExpr::Param("x/".into()), column(0, "b")]),
                ),
                Condition::Eq(
                    column(0, "v"),
                    SqlExpr::Param(Param {
                        value: value.into(),
                        datatype: datatype.map(Into::into),
                    }),
                ),
            ],
            left_joins: Vec::new(),
            fixed_columns: Vec::new(),
        };
        let plan = SqlPlan {
            columns: vec!["x".into()],
            hidden_columns: Vec::new(),
            distinct_order: Vec::new(),
            split_columns: Vec::new(),
            lexical_hidden_columns: Default::default(),
            projection: Vec::new(),
            branches: vec![branch("1", None), branch("2", Some("http://ex/dt"))],
            distinct: false,
            base_iri: None,
        };
        let rendered = plan
            .to_sql_with(&RenderOptions {
                text_params: &[3],
                ..RenderOptions::default()
            })
            .unwrap();
        assert_eq!(
            rendered.sql,
            "SELECT t0.\"id\" AS \"x\"\n\
             FROM \"t\" t0\n\
             WHERE t0.\"a\" = $1 || t0.\"b\"::text AND t0.\"v\" = $2\n\
             UNION\n\
             SELECT t0.\"id\" AS \"x\"\n\
             FROM \"t\" t0\n\
             WHERE t0.\"a\" = $3 || t0.\"b\"::text AND t0.\"v\"::text = $4"
        );
        assert_eq!(rendered.params, ["x/", "1", "x/", "2"]);
        assert_eq!(
            rendered.param_datatypes,
            [None, None, None, Some("http://ex/dt".to_owned())]
        );
        assert_eq!(rendered.param_branches, [0, 0, 1, 1]);
        assert_eq!(
            plan.to_sql().sql,
            rendered.sql.replace("::text = $4", " = $4")
        );
    }

    fn ty(group: &str, comparison: Comparison, lexical: Option<Lexical>) -> ColumnType {
        ColumnType {
            name: group.to_owned(),
            sql_type: format!("pg_catalog.{group}"),
            union_class: group.to_owned(),
            natural_datatype: (group == "integer")
                .then(|| "http://www.w3.org/2001/XMLSchema#integer".to_owned()),
            group: group.to_owned(),
            comparison,
            lexical,
        }
    }

    fn side(table: usize, name: &str, datatype: Option<TermDatatype>) -> TermSql {
        TermSql {
            value: TermSqlValue::Column {
                table,
                name: name.to_owned(),
            },
            datatype,
            base_iri: None,
        }
    }

    /// `WHERE` of a two-table branch with one `SameTerm`, rendered with
    /// the given column types for `a` (table `l`) and `b` (table `r`).
    fn same_term_sql(
        left: TermSql,
        right: TermSql,
        a: ColumnType,
        b: ColumnType,
    ) -> Result<String, Error> {
        let (l, r) = (
            LogicalTable::TableName("l".into()),
            LogicalTable::TableName("r".into()),
        );
        let plan = SqlPlan {
            columns: Vec::new(),
            hidden_columns: Vec::new(),
            distinct_order: Vec::new(),
            split_columns: Vec::new(),
            lexical_hidden_columns: Default::default(),
            projection: Vec::new(),
            branches: vec![SqlBranch {
                arm: 0,
                from: vec![l.clone(), r.clone()],
                select: Vec::new(),
                conditions: vec![Condition::SameTerm(left, right)],
                left_joins: Vec::new(),
                fixed_columns: Vec::new(),
            }],
            distinct: false,
            base_iri: None,
        };
        assert_eq!(plan.term_columns().len(), 2);
        let types: ColumnTypes = [((l, "a".to_owned()), a), ((r, "b".to_owned()), b)].into();
        let rendered = plan.to_sql_with(&RenderOptions {
            column_types: Some(&types),
            ..RenderOptions::default()
        })?;
        Ok(rendered.sql.split(" ON ").nth(1).unwrap().to_owned())
    }

    #[test]
    fn same_term_with_column_types_compares_as_the_group_says() {
        let int = || ty("integer", Comparison::Value, Some(Lexical::Text));
        let text = || ty("text", Comparison::Value, Some(Lexical::Text));
        let double = || ty("double", Comparison::Text, None);
        let boolean = || ty("boolean", Comparison::Value, Some(Lexical::Boolean));
        let numeric = || ty("numeric", Comparison::Value, Some(Lexical::Decimal));
        let plain = |t, n| side(t, n, None);
        let natural = |t, n| side(t, n, Some(TermDatatype::Natural));

        // Same group: `=` keeps indexes usable.
        assert_eq!(
            same_term_sql(plain(0, "a"), plain(1, "b"), int(), int()).unwrap(),
            r#"t0."a" = t1."b""#
        );
        assert_eq!(
            same_term_sql(plain(0, "a"), plain(1, "b"), double(), double()).unwrap(),
            r#"t0."a"::text = t1."b"::text"#
        );
        // Across groups: natural lexical forms, never an operator error.
        assert_eq!(
            same_term_sql(plain(0, "a"), plain(1, "b"), int(), text()).unwrap(),
            r#"t0."a"::text = t1."b"::text"#
        );
        assert_eq!(
            same_term_sql(plain(0, "a"), plain(1, "b"), boolean(), text()).unwrap(),
            r#"CASE t0."a" WHEN true THEN 'true' WHEN false THEN 'false' END = t1."b"::text"#
        );
        // Natural datatypes that differ never give the same literal.
        assert_eq!(
            same_term_sql(natural(0, "a"), natural(1, "b"), int(), text()).unwrap(),
            "false"
        );
        let integer = TermDatatype::Iri("http://www.w3.org/2001/XMLSchema#integer".into());
        assert_eq!(
            same_term_sql(natural(0, "a"), side(1, "b", Some(integer)), int(), text()).unwrap(),
            r#"t0."a"::text = t1."b"::text"#
        );
        // numeric: the canonical decimal, computed by trim_scale.
        assert_eq!(
            same_term_sql(plain(0, "a"), plain(1, "b"), numeric(), int()).unwrap(),
            r#"trim_scale(t0."a")::text = t1."b"::text"#
        );
        // No SQL for double's canonical form: rejected, not guessed.
        let err = same_term_sql(plain(0, "a"), plain(1, "b"), double(), int()).unwrap_err();
        assert!(
            matches!(&err, Error::UnsupportedFeature(m) if m.contains("column a of table l (double)")),
            "{err}"
        );
    }

    const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

    fn union_type(class: &str, natural: Option<&str>, lexical: Option<Lexical>) -> ColumnType {
        ColumnType {
            name: class.to_owned(),
            sql_type: format!("pg_catalog.{class}"),
            union_class: class.to_owned(),
            natural_datatype: natural.map(|n| format!("{XSD}{n}")),
            group: class.to_owned(),
            comparison: Comparison::Value,
            lexical,
        }
    }

    /// `?v` from column `v` of table `a` and of table `b`, as a literal of
    /// natural datatype or in an IRI template, and a hidden `?h`.
    fn union_plan(natural: bool) -> SqlPlan {
        let branch = |t: &str| SqlBranch {
            arm: 0,
            from: vec![LogicalTable::TableName(t.into())],
            select: vec![column(0, "v"), column(0, "h")],
            conditions: Vec::new(),
            left_joins: Vec::new(),
            fixed_columns: vec![BTreeSet::new()],
        };
        let (term_type, value) = if natural {
            (TermType::Literal, ResultValue::Column("v".into()))
        } else {
            (
                TermType::Iri,
                ResultValue::Template(vec![
                    TemplatePart::Literal("http://ex/".into()),
                    TemplatePart::Column("v".into()),
                ]),
            )
        };
        SqlPlan {
            columns: vec!["v".into()],
            hidden_columns: vec!["h".into()],
            distinct_order: vec![1, 0],
            projection: vec![ProjectedTerm {
                variable: "v".into(),
                terms: vec![ResultTerm {
                    term_type,
                    value,
                    datatype: None,
                    language: None,
                    guard: None,
                }],
            }],
            split_columns: Vec::new(),
            lexical_hidden_columns: Default::default(),
            branches: vec![branch("a"), branch("b")],
            distinct: false,
            base_iri: None,
        }
    }

    fn with_union_types(plan: &SqlPlan, a: ColumnType, b: ColumnType) -> Result<SqlPlan, Error> {
        let types: ColumnTypes = [
            ((LogicalTable::TableName("a".into()), "v".to_owned()), a),
            ((LogicalTable::TableName("b".into()), "v".to_owned()), b),
        ]
        .into();
        plan.with_union_types(&types)
    }

    #[test]
    fn union_columns_lists_the_columns_of_several_branches() {
        let plan = union_plan(true);
        let column = |t: &str, c: &str| (LogicalTable::TableName(t.into()), c.to_owned());
        assert_eq!(
            plan.union_columns(),
            [
                column("a", "v"),
                column("a", "h"),
                column("b", "v"),
                column("b", "h")
            ]
        );
        let mut single = plan;
        single.branches.pop();
        assert!(single.union_columns().is_empty());
    }

    #[test]
    fn union_of_one_class_keeps_the_columns() {
        let int = || union_type("integer", Some("integer"), Some(Lexical::Text));
        for natural in [true, false] {
            let plan = union_plan(natural);
            assert_eq!(with_union_types(&plan, int(), int()).unwrap(), plan);
        }
    }

    /// Branches where an `OPTIONAL` part cannot match select `Unbound`;
    /// two of them before a typed column would make `UNION` resolve the
    /// column to text first.
    #[test]
    fn union_gives_unbound_columns_the_type_of_the_others() {
        let mut plan = union_plan(true);
        let mut third = plan.branches[0].clone();
        third.from = vec![LogicalTable::TableName("c".into())];
        plan.branches.insert(0, third);
        plan.branches[0].select[0] = SqlExpr::Unbound;
        plan.branches[1].select[0] = SqlExpr::Unbound;
        plan.branches[0].select[1] = SqlExpr::Unbound;
        let int = || union_type("integer", Some("integer"), Some(Lexical::Text));
        let typed = with_union_types(&plan, int(), int()).unwrap();
        let selects: Vec<&[SqlExpr]> = typed.branches.iter().map(|b| &b.select[..]).collect();
        let null = SqlExpr::Null("pg_catalog.integer".into());
        assert_eq!(
            selects,
            [
                &[null.clone(), SqlExpr::Unbound][..],
                &[null, column(0, "h")][..],
                &[column(0, "v"), column(0, "h")][..],
            ]
        );
        // Different classes split as before; a branch without the column
        // leaves every split column NULL.
        plan.branches[1].select[0] = column(0, "v");
        let text = union_type("text", None, Some(Lexical::Text));
        let split = with_union_types(&plan, int(), text).unwrap();
        assert_eq!(split.columns, ["v", "v_1"]);
        assert_eq!(
            split.branches[0].select[..2],
            [
                SqlExpr::Null("pg_catalog.integer".into()),
                SqlExpr::Null("pg_catalog.text".into())
            ]
        );
    }

    #[test]
    fn union_splits_literals_of_different_natural_datatypes() {
        let int = union_type("integer", Some("integer"), Some(Lexical::Text));
        let text = union_type("text", None, Some(Lexical::Text));
        let plan = with_union_types(&union_plan(true), int, text).unwrap();
        assert_eq!(plan.columns, ["v", "v_1"]);
        assert_eq!(plan.split_columns, [["v", "v_1"]]);
        assert_eq!(plan.distinct_order, [2, 0, 1]);
        assert_eq!(
            plan.to_sql().sql,
            "SELECT q.\"v\", q.\"v_1\"\nFROM (\n\
             SELECT pg_typeof(t0.\"h\")::text || ' ' || t0.\"h\"::text AS \"h\", \
             t0.\"v\" AS \"v\", NULL::pg_catalog.text AS \"v_1\"\n\
             FROM \"a\" t0\n\
             UNION\n\
             SELECT pg_typeof(t0.\"h\")::text || ' ' || t0.\"h\"::text AS \"h\", \
             NULL::pg_catalog.integer AS \"v\", t0.\"v\" AS \"v_1\"\n\
             FROM \"b\" t0\n\
             ) q"
        );
    }

    #[test]
    fn union_compares_terms_that_can_be_equal_by_lexical_form() {
        let int = || union_type("integer", Some("integer"), Some(Lexical::Text));
        let numeric = union_type("numeric", Some("decimal"), Some(Lexical::Decimal));
        // Template values: 7 and 7.00 give the same IRI.
        let plan = with_union_types(&union_plan(false), numeric, int()).unwrap();
        assert!(plan.split_columns.is_empty());
        let sql = plan.to_sql().sql;
        assert!(
            sql.contains("trim_scale(t0.\"v\")::text AS \"v\"\nFROM \"a\"")
                && sql.contains("t0.\"v\"::text AS \"v\"\nFROM \"b\""),
            "{sql}"
        );
        // Plain literals from text and uuid columns.
        let text = union_type("text", None, Some(Lexical::Text));
        let uuid = union_type("uuid", None, Some(Lexical::Text));
        let plan = with_union_types(&union_plan(true), text, uuid).unwrap();
        assert_eq!(plan.columns, ["v"]);
        assert!(matches!(
            &plan.branches[1].select[0],
            SqlExpr::Lexical {
                lexical: Lexical::Text,
                ..
            }
        ));
    }

    #[test]
    fn union_compares_hidden_columns_by_lexical_form_where_terms_can_be_equal() {
        let int = || union_type("integer", Some("integer"), Some(Lexical::Text));
        let text = || union_type("text", None, Some(Lexical::Text));
        let double = || union_type("float8", Some("double"), None);
        let a = LogicalTable::TableName("a".into());
        let b = LogicalTable::TableName("b".into());
        let types = |h_a: ColumnType, h_b: ColumnType| -> ColumnTypes {
            [
                ((a.clone(), "v".to_owned()), int()),
                ((b.clone(), "v".to_owned()), int()),
                ((a.clone(), "h".to_owned()), h_a),
                ((b.clone(), "h".to_owned()), h_b),
            ]
            .into()
        };
        let mut plan = union_plan(true);
        // A natural literal: 7 and "7" are different terms, which the type
        // in the compared text keeps apart.
        assert_eq!(plan.with_union_types(&types(int(), text())).unwrap(), plan);
        // A template value: both give the same term.
        plan.lexical_hidden_columns.insert("h".into());
        let sql = plan
            .with_union_types(&types(int(), text()))
            .unwrap()
            .to_sql()
            .sql;
        assert_eq!(
            sql.matches("pg_typeof(t0.\"h\"::text)::text || ' ' || t0.\"h\"::text::text AS \"h\"")
                .count(),
            2,
            "{sql}"
        );
        // No SQL for a double's form: kept, at worst a duplicate stays.
        assert_eq!(
            plan.with_union_types(&types(double(), int())).unwrap(),
            plan
        );
    }

    #[test]
    fn union_without_sql_for_the_lexical_form_is_rejected() {
        let int = || union_type("integer", Some("integer"), Some(Lexical::Text));
        let double = union_type("float8", Some("double"), None);
        let err = with_union_types(&union_plan(false), double, int()).unwrap_err();
        assert!(
            matches!(&err, Error::UnsupportedFeature(m)
                if m.contains("?v") && m.contains("column v of table a (float8)")),
            "{err}"
        );
        // Natural literals of different datatypes need no lexical form.
        let double = union_type("float8", Some("double"), None);
        assert!(with_union_types(&union_plan(true), double, int()).is_ok());
        // Two classes of one natural datatype other than xsd:string: a
        // lexical form read back would lose the datatype.
        let a = union_type("numeric", Some("decimal"), Some(Lexical::Decimal));
        let b = union_type("numeric2", Some("decimal"), Some(Lexical::Decimal));
        assert!(matches!(
            with_union_types(&union_plan(true), a, b),
            Err(Error::UnsupportedFeature(_))
        ));
    }

    #[test]
    fn iri_safe_sql_keeps_nulls_and_uses_the_iunreserved_ranges() {
        let sql = iri_safe_sql("x");
        assert!(
            sql.starts_with("CASE WHEN x IS NULL THEN NULL ELSE"),
            "{sql}"
        );
        assert!(sql.contains(r"\u00A0-\uD7FF"), "{sql}");
        assert!(sql.contains(r"\U000E1000-\U000EFFFD]$'"), "{sql}");
    }

    fn keyed_plan(fixed: &[&str], hidden: bool) -> SqlPlan {
        SqlPlan {
            columns: vec!["s".into()],
            hidden_columns: if hidden { vec!["o".into()] } else { Vec::new() },
            distinct_order: Vec::new(),
            split_columns: Vec::new(),
            lexical_hidden_columns: Default::default(),
            projection: Vec::new(),
            branches: vec![SqlBranch {
                arm: 0,
                from: vec![LogicalTable::TableName("\"T\"".into())],
                select: vec![column(0, "\"Id\""), column(0, "v")],
                conditions: Vec::new(),
                left_joins: Vec::new(),
                fixed_columns: vec![fixed.iter().map(|c| c.to_string()).collect()],
            }],
            distinct: false,
            base_iri: None,
        }
    }

    #[test]
    fn duplicates_are_removed_over_all_variables_then_projected() {
        assert_eq!(
            keyed_plan(&[], true).to_sql().sql,
            "SELECT q.\"s\"\nFROM (\n\
             SELECT DISTINCT t0.\"Id\" AS \"s\", t0.\"v\" AS \"o\"\n\
             FROM \"T\" t0\n\
             ) q"
        );
        assert_eq!(
            keyed_plan(&[], false).to_sql().sql,
            "SELECT DISTINCT t0.\"Id\" AS \"s\"\nFROM \"T\" t0"
        );
        let mut no_columns = keyed_plan(&[], true);
        no_columns.columns.clear();
        no_columns.branches[0].select.remove(0);
        assert_eq!(
            no_columns.to_sql().sql,
            "SELECT 1\nFROM (\n\
             SELECT DISTINCT t0.\"v\" AS \"o\"\n\
             FROM \"T\" t0\n\
             ) q"
        );
    }

    #[test]
    fn a_unique_key_among_the_fixed_columns_keeps_the_rows() {
        let table = LogicalTable::TableName("\"T\"".into());
        let keys: TableKeys = [(table.clone(), vec![vec!["Id".to_owned()]])].into();
        let options = RenderOptions {
            keys: Some(&keys),
            ..RenderOptions::default()
        };
        let plan = keyed_plan(&["\"Id\"", "v"], true);
        assert_eq!(plan.key_tables(), std::slice::from_ref(&table));
        assert!(!plan.has_duplicates(Some(&keys)));
        assert_eq!(
            plan.to_sql_with(&options).unwrap().sql,
            "SELECT t0.\"Id\" AS \"s\"\nFROM \"T\" t0"
        );
        // Not fixed by the solution, unknown keys, or no metadata.
        assert!(keyed_plan(&["v"], true).has_duplicates(Some(&keys)));
        assert!(plan.has_duplicates(Some(&TableKeys::new())));
        assert!(plan.has_duplicates(None));
        // A composite key needs all its columns.
        let composite: TableKeys = [(table, vec![vec!["Id".to_owned(), "w".to_owned()]])].into();
        assert!(plan.has_duplicates(Some(&composite)));
    }

    #[test]
    fn keys_cannot_help_with_several_branches_views_or_distinct() {
        let mut two = keyed_plan(&["\"Id\""], false);
        two.branches.push(two.branches[0].clone());
        assert!(two.key_tables().is_empty());
        assert!(two.has_duplicates(Some(&TableKeys::new())));

        let mut view = keyed_plan(&["\"Id\""], false);
        view.branches[0].from[0] = LogicalTable::SqlQuery("SELECT 1".into());
        assert!(view.key_tables().is_empty());

        let mut distinct = keyed_plan(&[], true);
        distinct.distinct = true;
        assert!(distinct.key_tables().is_empty());
        assert!(!distinct.has_duplicates(None));
        assert_eq!(
            distinct.to_sql().sql,
            "SELECT DISTINCT t0.\"Id\" AS \"s\"\nFROM \"T\" t0"
        );
    }

    #[test]
    fn identifier_name_unquotes_delimited_names() {
        assert_eq!(identifier_name("id"), "id");
        assert_eq!(identifier_name("\"Id\""), "Id");
        assert_eq!(identifier_name("\"a\"\"b\""), "a\"b");
        assert_eq!(identifier_name("a\"b"), "a\"b");
    }

    #[test]
    fn to_sql_without_branches_returns_no_rows() {
        let plan = SqlPlan {
            columns: Vec::new(),
            hidden_columns: Vec::new(),
            distinct_order: Vec::new(),
            split_columns: Vec::new(),
            lexical_hidden_columns: Default::default(),
            projection: Vec::new(),
            branches: Vec::new(),
            distinct: false,
            base_iri: None,
        };
        assert_eq!(plan.to_sql().sql, "SELECT 1 WHERE false");
    }
}
