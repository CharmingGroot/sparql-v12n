// SPDX-License-Identifier: Apache-2.0
//! SPARQL parsing into algebra and the supported-feature gate.

use std::fmt;

use oxrdf::{Literal, NamedNode, Term};
use spargebra::algebra::{Expression as SparqlExpression, Function, GraphPattern, OrderExpression};
use spargebra::term::{GroundTerm, NamedNodePattern, TermPattern, Variable};
use spargebra::{Query, SparqlParser};

use crate::Error;

/// IRI of `rdf:type`, whose constant objects are looked up as classes.
pub const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// Most values, `UNDEF` included, that the `VALUES` blocks of one query
/// may hold together. Each value becomes a few SQL parameters in every
/// branch that reads its variable.
pub const MAX_VALUES: usize = 1000;

/// A `SELECT` or `ASK` query in the supported subset: a graph pattern
/// with the solution modifiers in the order SPARQL 1.1 §18.2.5 applies
/// them (Converting Solution Modifiers): `ORDER BY`, projection,
/// `DISTINCT`, then `OFFSET` and `LIMIT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectQuery {
    pub form: QueryForm,
    /// Projected variable names, without `?`, in `SELECT` order.
    pub variables: Vec<String>,
    pub distinct: bool,
    pub pattern: Pattern,
    /// The variables that `BIND` and `SELECT (expr AS ?v)` compute, in
    /// the order they are written.
    pub bindings: Vec<Binding>,
    /// The `VALUES` blocks, in the order they are written, each joined
    /// with the whole pattern (see [`InlineData`]).
    pub values: Vec<InlineData>,
    /// The order comparators of `ORDER BY`, in order; empty without one.
    pub order_by: Vec<OrderCondition>,
    /// Solutions skipped from the start (`OFFSET`), 0 without one.
    pub offset: usize,
    /// Most solutions returned (`LIMIT`).
    pub limit: Option<usize>,
}

/// The query form (SPARQL 1.1 §16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryForm {
    Select,
    /// `ASK` (§16.3): whether the pattern has a solution. The query
    /// projects no variable and returns at most one solution, whose
    /// presence is the answer, so duplicates need not be removed.
    Ask,
}

/// `Extend(P, ?v, expr)` from `BIND(expr AS ?v)` or `SELECT (expr AS ?v)`
/// (SPARQL 1.1 §18.2.1, §18.2.4.1, §18.5 Extend): `?v` is the value of the
/// expression, or unbound where it is an error.
///
/// It is evaluated over the solutions of the whole pattern rather than of
/// the group it is written in. The parser accepts it only where both give
/// the same value: the variables of the expression that the rest of the
/// pattern binds are bound in every solution of the group, and nothing
/// else in the query reads `?v`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub variable: String,
    /// A variable, a constant, or arithmetic.
    pub expression: Expression,
}

/// The inline data of a `VALUES` block (SPARQL 1.1 §10.2), inside a group
/// or after the query, and `Join(P, data)` with the rest of its group.
///
/// It is joined with the solutions of the whole pattern rather than of
/// its group. The parser accepts it only where both give the same
/// solutions and the join can only drop solutions: outside `OPTIONAL`
/// and `UNION`, every variable is bound in every solution of the rest of
/// its group, and no two rows can be compatible with one solution. A
/// solution is then kept once when a row is compatible with it (SPARQL
/// 1.1 §18.3), an `UNDEF` being compatible with every value, and dropped
/// otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineData {
    /// Variable names without `?`.
    pub variables: Vec<String>,
    /// One term per variable in each row, `None` for `UNDEF`.
    pub rows: Vec<Vec<Option<Term>>>,
}

/// One order comparator of `ORDER BY` (SPARQL 1.1 §15.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderCondition {
    /// A variable or an expression of the `FILTER` subset.
    pub expression: Expression,
    pub descending: bool,
}

/// A graph pattern in the supported subset, as the SPARQL algebra builds
/// it (SPARQL 1.1 §18.2, Translation to the SPARQL Algebra).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pattern {
    /// A basic graph pattern. Joins of basic graph patterns are merged
    /// into one.
    Bgp(Vec<TriplePattern>),
    /// `Join(left, right)`: a group with an `OPTIONAL` before other
    /// patterns, such as `{ A OPTIONAL { B } C }`.
    Join(Box<Pattern>, Box<Pattern>),
    /// `LeftJoin(left, right, expr)` from `OPTIONAL` (SPARQL 1.1 §18.5).
    /// The expression is the `FILTER` of the optional group (§18.2.2.6,
    /// Translate Graph Patterns), `None` for `true`. It is evaluated over
    /// the merged solutions of both sides, so it can see the left side's
    /// variables.
    LeftJoin(Box<Pattern>, Box<Pattern>, Option<Expression>),
    /// `Filter(expr, pattern)`: the `FILTER`s of a group, which apply to
    /// the whole group (SPARQL 1.1 §18.2.2.6) and see only its variables.
    Filter(Box<Pattern>, Expression),
    /// `Union(left, right)` from `{ A } UNION { B }`: the solutions of both
    /// sides as a bag (SPARQL 1.1 §18.5).
    Union(Box<Pattern>, Box<Pattern>),
    /// `Minus(left, right)` from `A MINUS { B }` (SPARQL 1.1 §18.5): the
    /// solutions of the left side that no solution of the right side is
    /// compatible with while sharing a variable with it. The right side is
    /// evaluated on its own (§8.3.3), so its `FILTER`s see only its
    /// variables, and its variables are not in the solutions.
    Minus(Box<Pattern>, Box<Pattern>),
}

/// A `FILTER` expression in the supported subset (SPARQL 1.1 §17).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expression {
    /// An IRI or a literal.
    Constant(Term),
    /// A variable name without `?`.
    Variable(String),
    Or(Box<Expression>, Box<Expression>),
    And(Box<Expression>, Box<Expression>),
    /// `!`; `A != B` is `!(A = B)` (SPARQL 1.1 §17.3, Operator Mapping).
    Not(Box<Expression>),
    Compare(CompareOp, Box<Expression>, Box<Expression>),
    SameTerm(Box<Expression>, Box<Expression>),
    Bound(String),
    Test(TermTest, Box<Expression>),
    /// `STR` (SPARQL 1.1 §17.4.2.5).
    Str(Box<Expression>),
    /// `LANG` (SPARQL 1.1 §17.4.2.6).
    Lang(Box<Expression>),
    /// `DATATYPE` (SPARQL 1.1 §17.4.2.7).
    Datatype(Box<Expression>),
    /// `REGEX(text, pattern, flags)` (SPARQL 1.1 §17.4.3.14), with the
    /// pattern and the flags constants.
    Regex(Box<Expression>, Term, Option<Term>),
    /// `+`, `-`, `*` and `/` (SPARQL 1.1 §17.3, op:numeric-add and the
    /// others).
    Arith(ArithOp, Box<Expression>, Box<Expression>),
    /// Unary minus of an expression that is not a numeric literal
    /// (op:numeric-unary-minus).
    Negate(Box<Expression>),
    /// `ABS`, `ROUND`, `CEIL` or `FLOOR` (SPARQL 1.1 §17.4.4) of a number.
    Numeric(NumericFunction, Box<Expression>),
    /// `IF(cond, a, b)` (SPARQL 1.1 §17.4.1.2): `a` where the effective
    /// boolean value of `cond` is true, `b` where it is false, and an
    /// error where it is an error.
    If(Box<Expression>, Box<Expression>, Box<Expression>),
    /// `COALESCE` (SPARQL 1.1 §17.4.1.3): the first argument that is not
    /// an error.
    Coalesce(Vec<Expression>),
    /// `lhs IN (…)` (SPARQL 1.1 §17.4.1.9), which is `(lhs = e1) || (lhs =
    /// e2) || …` and false for an empty list. `NOT IN` is its negation
    /// (§17.4.1.10).
    In(Box<Expression>, Vec<Expression>),
}

impl Expression {
    /// The variables the expression mentions, `BOUND`'s included.
    pub fn variables(&self) -> Vec<&str> {
        let mut out = Vec::new();
        self.collect_variables(&mut out);
        out
    }

    fn collect_variables<'e>(&'e self, out: &mut Vec<&'e str>) {
        match self {
            Self::Constant(_) => {}
            Self::Variable(v) | Self::Bound(v) => out.push(v),
            Self::Or(a, b)
            | Self::And(a, b)
            | Self::Compare(_, a, b)
            | Self::SameTerm(a, b)
            | Self::Arith(_, a, b) => {
                a.collect_variables(out);
                b.collect_variables(out);
            }
            Self::Not(a)
            | Self::Test(_, a)
            | Self::Str(a)
            | Self::Lang(a)
            | Self::Datatype(a)
            | Self::Regex(a, _, _)
            | Self::Negate(a)
            | Self::Numeric(_, a) => a.collect_variables(out),
            Self::If(c, a, b) => {
                c.collect_variables(out);
                a.collect_variables(out);
                b.collect_variables(out);
            }
            Self::Coalesce(args) => args.iter().for_each(|a| a.collect_variables(out)),
            Self::In(a, list) => {
                a.collect_variables(out);
                list.iter().for_each(|b| b.collect_variables(out));
            }
        }
    }

    /// Whether the expression is an RDF term rather than a boolean.
    pub(crate) fn is_term(&self) -> bool {
        matches!(
            self,
            Self::Constant(_)
                | Self::Variable(_)
                | Self::Str(_)
                | Self::Lang(_)
                | Self::Datatype(_)
                | Self::Arith(..)
                | Self::Negate(_)
                | Self::Numeric(..)
                | Self::If(..)
                | Self::Coalesce(_)
        )
    }
}

/// The arithmetic operators of SPARQL 1.1 §17.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Subtract,
    Multiply,
    Divide,
}

impl ArithOp {
    /// The SQL operator.
    pub fn sql(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Subtract => "-",
            Self::Multiply => "*",
            Self::Divide => "/",
        }
    }
}

/// The numeric functions of SPARQL 1.1 §17.4.4 other than `RAND`. The
/// result has the class of the argument: an integer for an integer, a
/// decimal for a decimal and a double for a double (XPath F&O 3.1 §4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumericFunction {
    /// `fn:abs`.
    Abs,
    /// `fn:round`, which rounds halves towards positive infinity.
    Round,
    /// `fn:ceiling`.
    Ceil,
    /// `fn:floor`.
    Floor,
}

/// The relational operators of SPARQL 1.1 §17.3, `!=` aside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Equal,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
}

impl CompareOp {
    /// The SQL operator.
    pub fn sql(self) -> &'static str {
        match self {
            Self::Equal => "=",
            Self::Less => "<",
            Self::LessOrEqual => "<=",
            Self::Greater => ">",
            Self::GreaterOrEqual => ">=",
        }
    }
}

/// `isIRI`, `isBlank` and `isLiteral` (SPARQL 1.1 §17.4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermTest {
    IsIri,
    IsBlank,
    IsLiteral,
}

impl Pattern {
    /// Every triple pattern, in the order the pattern is written.
    pub fn triple_patterns(&self) -> Vec<&TriplePattern> {
        let mut out = Vec::new();
        self.collect_triple_patterns(&mut out, true);
        out
    }

    /// The triple patterns whose variables a solution can bind: all but
    /// those on the right side of a `Minus`, in the order they are written.
    pub fn solution_triple_patterns(&self) -> Vec<&TriplePattern> {
        let mut out = Vec::new();
        self.collect_triple_patterns(&mut out, false);
        out
    }

    fn collect_triple_patterns<'p>(&'p self, out: &mut Vec<&'p TriplePattern>, minus: bool) {
        match self {
            Self::Bgp(patterns) => out.extend(patterns),
            Self::Join(left, right) | Self::Union(left, right) | Self::LeftJoin(left, right, _) => {
                left.collect_triple_patterns(out, minus);
                right.collect_triple_patterns(out, minus);
            }
            Self::Minus(left, right) => {
                left.collect_triple_patterns(out, minus);
                if minus {
                    right.collect_triple_patterns(out, minus);
                }
            }
            Self::Filter(inner, _) => inner.collect_triple_patterns(out, minus),
        }
    }

    /// The ways to choose one side of every `Union` outside the right side
    /// of every `LeftJoin`, each the sides taken (`true` for the right one)
    /// in the order the chosen `Union`s are written. Join, LeftJoin (on its
    /// left side) and Filter distribute over a union (SPARQL 1.1 §18.5), so
    /// the solutions of the pattern are the bag union of the solutions of
    /// each choice, an arm without `Union`s. `None` when there are more
    /// than `limit`.
    pub fn arms(&self, limit: usize) -> Option<Vec<Vec<bool>>> {
        let arms = match self {
            Self::Bgp(_) => vec![Vec::new()],
            Self::Join(left, right) => {
                let (left, right) = (left.arms(limit)?, right.arms(limit)?);
                if left.len().checked_mul(right.len())? > limit {
                    return None;
                }
                let mut out = Vec::with_capacity(left.len() * right.len());
                for l in &left {
                    for r in &right {
                        out.push(l.iter().chain(r).copied().collect());
                    }
                }
                out
            }
            Self::LeftJoin(left, _, _) | Self::Minus(left, _) | Self::Filter(left, _) => {
                left.arms(limit)?
            }
            Self::Union(left, right) => {
                let side = |side: bool, arms: Vec<Vec<bool>>| {
                    arms.into_iter().map(move |arm| {
                        let mut out = vec![side];
                        out.extend(arm);
                        out
                    })
                };
                let left = left.arms(limit)?;
                let right = right.arms(limit)?;
                side(false, left).chain(side(true, right)).collect()
            }
        };
        (arms.len() <= limit).then_some(arms)
    }

    /// The triple patterns that every solution of an arm (see
    /// [`arms`](Self::arms)) matches: those on the chosen side of every
    /// `Union` and outside the right side of every `LeftJoin`, in the order
    /// they are written.
    pub fn required_triple_patterns(&self, arm: &[bool]) -> Vec<&TriplePattern> {
        let mut out = Vec::new();
        let mut sides = arm.iter().copied();
        self.collect_required(&mut out, &mut sides);
        out
    }

    fn collect_required<'p>(
        &'p self,
        out: &mut Vec<&'p TriplePattern>,
        sides: &mut impl Iterator<Item = bool>,
    ) {
        match self {
            Self::Bgp(patterns) => out.extend(patterns),
            Self::Join(left, right) => {
                left.collect_required(out, sides);
                right.collect_required(out, sides);
            }
            Self::LeftJoin(inner, _, _) | Self::Minus(inner, _) | Self::Filter(inner, _) => {
                inner.collect_required(out, sides);
            }
            Self::Union(left, right) => {
                if sides.next().expect("one side per Union of the arm") {
                    right.collect_required(out, sides);
                } else {
                    left.collect_required(out, sides);
                }
            }
        }
    }

    /// The variables that every solution of the pattern binds: those of
    /// its triple patterns outside `OPTIONAL` parts, of both sides of a
    /// `UNION`.
    fn certain_variables(&self) -> Vec<&str> {
        match self {
            Self::Bgp(patterns) => patterns.iter().flat_map(|p| p.variables()).collect(),
            Self::Join(left, right) => {
                let mut out = left.certain_variables();
                out.extend(right.certain_variables());
                out
            }
            Self::LeftJoin(inner, _, _) | Self::Minus(inner, _) | Self::Filter(inner, _) => {
                inner.certain_variables()
            }
            Self::Union(left, right) => {
                let right = right.certain_variables();
                left.certain_variables()
                    .into_iter()
                    .filter(|v| right.contains(v))
                    .collect()
            }
        }
    }

    /// The variables of the `FILTER` expressions, those of `OPTIONAL`
    /// included.
    fn filter_variables(&self) -> Vec<&str> {
        match self {
            Self::Bgp(_) => Vec::new(),
            Self::Join(left, right) | Self::Union(left, right) | Self::Minus(left, right) => {
                let mut out = left.filter_variables();
                out.extend(right.filter_variables());
                out
            }
            Self::LeftJoin(left, right, expression) => {
                let mut out = left.filter_variables();
                out.extend(right.filter_variables());
                out.extend(expression.iter().flat_map(Expression::variables));
                out
            }
            Self::Filter(inner, expression) => {
                let mut out = inner.filter_variables();
                out.extend(expression.variables());
                out
            }
        }
    }

    /// Number of `LeftJoin` and `Minus` nodes in the pattern. The right
    /// side of each is a `LEFT JOIN` when unfolded.
    pub fn left_joins(&self) -> usize {
        match self {
            Self::Bgp(_) => 0,
            Self::Join(left, right) | Self::Union(left, right) => {
                left.left_joins() + right.left_joins()
            }
            Self::LeftJoin(left, right, _) | Self::Minus(left, right) => {
                left.left_joins() + 1 + right.left_joins()
            }
            Self::Filter(inner, _) => inner.left_joins(),
        }
    }
}

/// A triple pattern whose predicate is a constant IRI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriplePattern {
    pub subject: PatternTerm,
    pub predicate: String,
    pub object: PatternTerm,
}

impl TriplePattern {
    pub(crate) fn variables(&self) -> impl Iterator<Item = &str> {
        [&self.subject, &self.object]
            .into_iter()
            .filter_map(|term| match term {
                PatternTerm::Variable(name) => Some(name.as_str()),
                PatternTerm::Constant(_) => None,
            })
    }
}

/// The pattern in SPARQL syntax with full IRIs, such as
/// `?c <http://example.org/p1> ?v`.
impl fmt::Display for TriplePattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} <{}> {}", self.subject, self.predicate, self.object)
    }
}

impl fmt::Display for PatternTerm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Variable(name) if name.starts_with("_:") => f.write_str(name),
            Self::Variable(name) => write!(f, "?{name}"),
            Self::Constant(term) => write!(f, "{term}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PatternTerm {
    /// A variable name without `?`. Blank nodes in the pattern are
    /// variables too (SPARQL 1.1 §4.1.4) and use their `_:` label, which
    /// cannot clash with a variable name.
    Variable(String),
    Constant(Term),
}

/// Parses a SPARQL query and checks that it is in the supported subset.
pub fn parse(query: &str) -> Result<SelectQuery, Error> {
    let query = SparqlParser::new()
        .parse_query(query)
        .map_err(|e| Error::QuerySyntax(e.to_string()))?;
    let (form, dataset, pattern) = match query {
        Query::Select {
            dataset, pattern, ..
        } => (QueryForm::Select, dataset, pattern),
        Query::Ask {
            dataset, pattern, ..
        } => (QueryForm::Ask, dataset, pattern),
        Query::Construct { .. } => return Err(unsupported("CONSTRUCT queries")),
        Query::Describe { .. } => return Err(unsupported("DESCRIBE queries")),
    };
    if dataset.is_some() {
        return Err(unsupported("FROM and FROM NAMED"));
    }

    // spargebra nests the modifiers as the algebra does (SPARQL 1.1
    // §18.2.5): Slice(Distinct(Project(OrderBy(P)))).
    let (offset, limit, sliced) = match pattern {
        GraphPattern::Slice {
            inner,
            start,
            length,
        } => (start, length, *inner),
        other => (0, None, other),
    };
    // SPARQL 1.1 §18.2.5 slices the solutions before ASK tests whether one
    // is left, so `LIMIT 0` gives false. Engines differ here, so a sliced
    // ASK is rejected rather than answered differently from some of them.
    if form == QueryForm::Ask && (offset > 0 || limit.is_some()) {
        return Err(unsupported("LIMIT and OFFSET in ASK queries"));
    }
    let (distinct, projected) = match sliced {
        GraphPattern::Distinct { inner } => (true, *inner),
        other => (false, other),
    };
    let GraphPattern::Project { inner, variables } = projected else {
        return Err(unsupported(describe(&projected)));
    };
    let (order_by, inner) = match *inner {
        // The order never adds or removes a solution (SPARQL 1.1 §15.1).
        GraphPattern::OrderBy { inner, .. } if form == QueryForm::Ask => (Vec::new(), *inner),
        GraphPattern::OrderBy { inner, expression } => {
            let conditions = expression
                .iter()
                .map(order_condition)
                .collect::<Result<Vec<_>, _>>()?;
            (conditions, *inner)
        }
        other => (Vec::new(), other),
    };
    let mut extends = Vec::new();
    let mut values = Vec::new();
    let pattern = group(&inner, Place::Required, &mut extends, &mut values)?;
    if pattern.solution_triple_patterns().is_empty() {
        return Err(unsupported("empty graph patterns"));
    }
    let bindings = check_extends(&pattern, extends, &order_by)?;
    let (variables, limit) = match form {
        QueryForm::Select => (
            variables.iter().map(|v| v.as_str().to_owned()).collect(),
            limit,
        ),
        // The first solution decides the answer (SPARQL 1.1 §16.3).
        QueryForm::Ask => (Vec::new(), Some(1)),
    };
    Ok(SelectQuery {
        form,
        variables,
        distinct,
        pattern,
        bindings,
        values,
        order_by,
        offset,
        limit,
    })
}

/// A `BIND` or `SELECT (expr AS ?v)` with the group it extends.
struct Extend {
    binding: Binding,
    group: Pattern,
}

/// The bindings of `extends`, evaluated over the solutions of `pattern`
/// (see [`Binding`]), or an error for one whose value would differ there,
/// or whose variable the query reads.
fn check_extends(
    pattern: &Pattern,
    extends: Vec<Extend>,
    order_by: &[OrderCondition],
) -> Result<Vec<Binding>, Error> {
    let computed: Vec<&str> = extends
        .iter()
        .map(|e| e.binding.variable.as_str())
        .collect();
    let count = |patterns: Vec<&TriplePattern>, v: &str| {
        patterns
            .iter()
            .filter(|t| t.variables().any(|u| u == v))
            .count()
    };
    // The right side of a MINUS binds no variable of the solutions, but it
    // reads the value of a variable it shares with them.
    let binding = |p: &Pattern, v: &str| count(p.solution_triple_patterns(), v);
    let occurrences = |p: &Pattern, v: &str| count(p.triple_patterns(), v);
    let filters = pattern.filter_variables();
    for extend in &extends {
        let variable = extend.binding.variable.as_str();
        let certain = extend.group.certain_variables();
        for used in extend.binding.expression.variables() {
            if computed.contains(&used) {
                return Err(unsupported(&format!(
                    "an expression over ?{used}, which BIND or SELECT computes"
                )));
            }
            // Joined with the rest of the pattern, a solution of the group
            // that leaves the variable unbound can get a value for it.
            if !certain.contains(&used) && binding(pattern, used) > binding(&extend.group, used) {
                return Err(unsupported(&format!(
                    "BIND over ?{used}, which its group may leave unbound and a later part of \
                     the pattern binds"
                )));
            }
        }
        if occurrences(pattern, variable) > 0 {
            return Err(unsupported(&format!(
                "a triple pattern over ?{variable}, which BIND computes"
            )));
        }
        if filters.contains(&variable) {
            return Err(unsupported(&format!(
                "FILTER over ?{variable}, which BIND or SELECT computes"
            )));
        }
        if order_by
            .iter()
            .any(|c| c.expression.variables().contains(&variable))
        {
            return Err(unsupported(&format!(
                "ORDER BY over ?{variable}, which BIND or SELECT computes"
            )));
        }
    }
    Ok(extends.into_iter().map(|e| e.binding).collect())
}

/// Where a group is written, for the `BIND`s it may hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    /// Outside `OPTIONAL`, `UNION` and the right side of `MINUS`.
    Required,
    Optional,
    Union,
    Minus,
}

impl Place {
    /// The keyword of the group a non-required place is inside.
    fn keyword(self) -> &'static str {
        match self {
            Self::Required => "the WHERE clause",
            Self::Optional => "OPTIONAL",
            Self::Union => "UNION",
            Self::Minus => "MINUS",
        }
    }
}

/// The expression of a `BIND` or `SELECT (expr AS ?v)`: a variable, a
/// constant, arithmetic, or `IF` and `COALESCE` whose results are numbers.
fn bound_expression(expression: &SparqlExpression) -> Result<Expression, Error> {
    if let SparqlExpression::Not(inner) = expression
        && matches!(**inner, SparqlExpression::In(..))
    {
        return Err(unsupported(
            "IN and NOT IN as the value of BIND and SELECT expressions",
        ));
    }
    let expression = expression_in(expression, Site::Bind)?;
    let what = match &expression {
        Expression::Constant(_)
        | Expression::Variable(_)
        | Expression::Arith(..)
        | Expression::Negate(_)
        | Expression::Numeric(..) => return Ok(expression),
        // The value of a computed variable is a number in SQL, so every
        // result must be one or an error.
        Expression::If(..) | Expression::Coalesce(_) if numeric_results(&expression) => {
            return Ok(expression);
        }
        Expression::If(..) | Expression::Coalesce(_) => {
            "IF and COALESCE with a result other than arithmetic or a numeric constant"
        }
        Expression::In(..) => "IN and NOT IN",
        Expression::Or(..) | Expression::And(..) | Expression::Not(_) => "logical operators",
        Expression::Compare(..) | Expression::SameTerm(..) => "comparisons",
        Expression::Bound(_) => "BOUND",
        Expression::Test(..) => "isIRI, isBlank and isLiteral",
        Expression::Str(_) => "STR",
        Expression::Lang(_) => "LANG",
        Expression::Datatype(_) => "DATATYPE",
        Expression::Regex(..) => "REGEX",
    };
    Err(unsupported(&format!(
        "{what} as the value of BIND and SELECT expressions"
    )))
}

/// Whether every result of `IF` and `COALESCE` in `expression` is
/// arithmetic or a numeric constant, so that its value is a number or an
/// error.
fn numeric_results(expression: &Expression) -> bool {
    match expression {
        Expression::Arith(..) | Expression::Negate(_) | Expression::Numeric(..) => true,
        Expression::Constant(Term::Literal(literal)) => matches!(
            crate::xsd::value(literal.value(), literal.datatype().as_str()),
            crate::xsd::Value::Numeric(_)
        ),
        Expression::If(_, a, b) => numeric_results(a) && numeric_results(b),
        Expression::Coalesce(args) => args.iter().all(numeric_results),
        _ => false,
    }
}

/// An order comparator: a term, compared as SPARQL 1.1 §15.1 orders them,
/// or a boolean expression, whose value is a boolean literal or an error.
fn order_condition(condition: &OrderExpression) -> Result<OrderCondition, Error> {
    let (expression, descending) = match condition {
        OrderExpression::Asc(e) => (e, false),
        OrderExpression::Desc(e) => (e, true),
    };
    Ok(OrderCondition {
        expression: expression_in(expression, Site::OrderBy)?,
        descending,
    })
}

fn group(
    pattern: &GraphPattern,
    place: Place,
    extends: &mut Vec<Extend>,
    values: &mut Vec<InlineData>,
) -> Result<Pattern, Error> {
    if let GraphPattern::Join { .. } = pattern {
        let mut operands = Vec::new();
        join_operands(pattern, &mut operands);
        if operands
            .iter()
            .any(|p| matches!(p, GraphPattern::Values { .. }))
        {
            return values_join(&operands, place, extends, values);
        }
    }
    // A group inside `OPTIONAL` or `UNION` stays there.
    let mut nested = |p: &GraphPattern, inside: Place| {
        let inside = if place == Place::Required {
            inside
        } else {
            place
        };
        group(p, inside, extends, values)
    };
    match pattern {
        GraphPattern::Bgp { patterns } => {
            let mut out = Vec::with_capacity(patterns.len());
            for p in patterns {
                let NamedNodePattern::NamedNode(predicate) = &p.predicate else {
                    return Err(unsupported("variable predicates"));
                };
                out.push(TriplePattern {
                    subject: pattern_term(&p.subject)?,
                    predicate: predicate.as_str().to_owned(),
                    object: pattern_term(&p.object)?,
                });
            }
            Ok(Pattern::Bgp(out))
        }
        GraphPattern::Join { left, right } => Ok(join(nested(left, place)?, nested(right, place)?)),
        GraphPattern::Union { left, right } => Ok(Pattern::Union(
            Box::new(nested(left, Place::Union)?),
            Box::new(nested(right, Place::Union)?),
        )),
        GraphPattern::LeftJoin {
            left,
            right,
            expression,
        } => {
            let expression = expression
                .as_ref()
                .map(|e| expression_in(e, Site::Filter))
                .transpose()?;
            let left = nested(left, place)?;
            Ok(match nested(right, Place::Optional)? {
                // `{ }` has one solution that binds nothing, which extends
                // every solution unchanged whether the expression holds
                // or not.
                Pattern::Bgp(right) if right.is_empty() => left,
                // LeftJoin(Z, P, f), with Z the empty pattern, is the
                // solutions of P for which f holds, or Z when there are
                // none. As the right side of another LeftJoin, both cases
                // give LeftJoin(left, Filter(f, P), expr).
                Pattern::LeftJoin(inner_left, inner_right, inner_expression) if matches!(&*inner_left, Pattern::Bgp(p) if p.is_empty()) =>
                {
                    let right = match inner_expression {
                        Some(f) => Box::new(Pattern::Filter(inner_right, f)),
                        None => inner_right,
                    };
                    Pattern::LeftJoin(Box::new(left), right, expression)
                }
                right => Pattern::LeftJoin(Box::new(left), Box::new(right), expression),
            })
        }
        GraphPattern::Filter { expr, inner } => Ok(Pattern::Filter(
            Box::new(nested(inner, place)?),
            expression_in(expr, Site::Filter)?,
        )),
        GraphPattern::Extend {
            inner,
            variable,
            expression,
        } if !has_group(inner) => {
            if place != Place::Required {
                return Err(unsupported(&format!("BIND inside {}", place.keyword())));
            }
            let inner = nested(inner, place)?;
            extends.push(Extend {
                binding: Binding {
                    variable: variable.as_str().to_owned(),
                    expression: bound_expression(expression)?,
                },
                group: inner.clone(),
            });
            Ok(inner)
        }
        // Not joined with patterns that bind its variables.
        GraphPattern::Values { .. } => values_join(&[pattern], place, extends, values),
        // Unfolding evaluates Minus where it is written. Inside OPTIONAL,
        // UNION and another MINUS it is rejected, as untested shapes.
        GraphPattern::Minus { left, right } => {
            if place != Place::Required {
                return Err(unsupported(&format!("MINUS inside {}", place.keyword())));
            }
            let left = nested(left, place)?;
            let right = nested(right, Place::Minus)?;
            Ok(Pattern::Minus(Box::new(left), Box::new(right)))
        }
        other => Err(unsupported(describe(other))),
    }
}

/// A join of basic graph patterns is the basic graph pattern of all their
/// triples: blank node labels cannot be shared across them (SPARQL 1.1
/// §4.1.4), so nothing is captured by merging.
fn join(left: Pattern, right: Pattern) -> Pattern {
    match (left, right) {
        (Pattern::Bgp(mut left), Pattern::Bgp(right)) => {
            left.extend(right);
            Pattern::Bgp(left)
        }
        // Join(Z, P) = P for the empty pattern Z, whose one solution
        // binds nothing.
        (Pattern::Bgp(empty), other) | (other, Pattern::Bgp(empty)) if empty.is_empty() => other,
        (left, right) => Pattern::Join(Box::new(left), Box::new(right)),
    }
}

/// The operands of nested joins, in the order they are written. Join is
/// associative and commutative (SPARQL 1.1 §18.5), so the solutions do
/// not depend on how they are nested.
fn join_operands<'p>(pattern: &'p GraphPattern, out: &mut Vec<&'p GraphPattern>) {
    match pattern {
        GraphPattern::Join { left, right } => {
            join_operands(left, out);
            join_operands(right, out);
        }
        other => out.push(other),
    }
}

/// The join of `operands`, of which the `VALUES` blocks go to `values`
/// (see [`InlineData`]) and the others make the pattern returned.
fn values_join(
    operands: &[&GraphPattern],
    place: Place,
    extends: &mut Vec<Extend>,
    values: &mut Vec<InlineData>,
) -> Result<Pattern, Error> {
    if place != Place::Required {
        return Err(unsupported(&format!("VALUES inside {}", place.keyword())));
    }
    let mut joined = Pattern::Bgp(Vec::new());
    let mut data = Vec::new();
    for operand in operands {
        match operand {
            GraphPattern::Values {
                variables,
                bindings,
            } => data.push(inline_data(variables, bindings)?),
            other => joined = join(joined, group(other, place, extends, values)?),
        }
    }
    let certain = joined.certain_variables();
    for variable in data.iter().flat_map(|d| &d.variables) {
        if !certain.contains(&variable.as_str()) {
            return Err(unsupported(&format!(
                "VALUES over ?{variable}, which the rest of its group may leave unbound"
            )));
        }
    }
    values.extend(data);
    let total = values
        .iter()
        .map(|d| d.rows.len().saturating_mul(d.variables.len()))
        .fold(0, usize::saturating_add);
    if total > MAX_VALUES {
        return Err(Error::TooManyValues {
            limit: MAX_VALUES,
            values: total,
        });
    }
    Ok(joined)
}

/// A `VALUES` block, or an error for one whose rows the parser does not
/// accept (see [`InlineData`]).
fn inline_data(
    variables: &[Variable],
    bindings: &[Vec<Option<GroundTerm>>],
) -> Result<InlineData, Error> {
    if variables.is_empty() {
        return Err(unsupported("VALUES without variables"));
    }
    let values = bindings.len().saturating_mul(variables.len());
    if values > MAX_VALUES {
        return Err(Error::TooManyValues {
            limit: MAX_VALUES,
            values,
        });
    }
    let rows: Vec<Vec<Option<Term>>> = bindings
        .iter()
        .map(|row| row.iter().map(|t| t.clone().map(Term::from)).collect())
        .collect();
    // Two rows are compatible with one solution exactly when every
    // variable bound in both is the same term in both; the join would
    // then repeat the solution.
    for (i, row) in rows.iter().enumerate() {
        for other in &rows[..i] {
            let differ = row
                .iter()
                .zip(other)
                .any(|pair| matches!(pair, (Some(a), Some(b)) if !same_term(a, b)));
            if !differ {
                return Err(unsupported(
                    "VALUES with two rows that can match one solution: equal rows, or rows \
                     that differ only where one of them is UNDEF",
                ));
            }
        }
    }
    Ok(InlineData {
        variables: variables.iter().map(|v| v.as_str().to_owned()).collect(),
        rows,
    })
}

/// RDF term equality, with language tags compared in lower case (RDF 1.1
/// Concepts §3.3).
fn same_term(a: &Term, b: &Term) -> bool {
    match (a, b) {
        (Term::Literal(a), Term::Literal(b)) => {
            a.value() == b.value()
                && a.datatype() == b.datatype()
                && match (a.language(), b.language()) {
                    (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
                    (x, y) => x == y,
                }
        }
        _ => a == b,
    }
}

/// Where an expression is written, for error messages.
#[derive(Debug, Clone, Copy)]
enum Site {
    Filter,
    OrderBy,
    Bind,
}

impl Site {
    fn name(self) -> &'static str {
        match self {
            Self::Filter => "FILTER",
            Self::OrderBy => "ORDER BY",
            Self::Bind => "BIND and SELECT expressions",
        }
    }
}

/// A `FILTER` or `ORDER BY` expression, checked against the supported
/// subset, which is the same for both.
fn expression_in(expression: &SparqlExpression, site: Site) -> Result<Expression, Error> {
    let boxed = |e: &SparqlExpression| expression_in(e, site).map(Box::new);
    let operand = |e: &SparqlExpression| operand(e, site);
    let compare = |op, a, b| compare(op, a, b, site);
    let in_site = |what: &str| unsupported(&format!("{what} in {}", site.name()));
    Ok(match expression {
        SparqlExpression::NamedNode(n) => Expression::Constant(n.clone().into()),
        SparqlExpression::Literal(l) => Expression::Constant(l.clone().into()),
        SparqlExpression::Variable(v) => Expression::Variable(v.as_str().to_owned()),
        SparqlExpression::Or(a, b) => Expression::Or(boxed(a)?, boxed(b)?),
        SparqlExpression::And(a, b) => Expression::And(boxed(a)?, boxed(b)?),
        SparqlExpression::Not(a) => Expression::Not(boxed(a)?),
        SparqlExpression::Equal(a, b) => compare(CompareOp::Equal, a, b)?,
        SparqlExpression::Less(a, b) => compare(CompareOp::Less, a, b)?,
        SparqlExpression::LessOrEqual(a, b) => compare(CompareOp::LessOrEqual, a, b)?,
        SparqlExpression::Greater(a, b) => compare(CompareOp::Greater, a, b)?,
        SparqlExpression::GreaterOrEqual(a, b) => compare(CompareOp::GreaterOrEqual, a, b)?,
        SparqlExpression::SameTerm(a, b) => Expression::SameTerm(operand(a)?, operand(b)?),
        SparqlExpression::Bound(v) => Expression::Bound(v.as_str().to_owned()),
        SparqlExpression::FunctionCall(Function::Regex, args) => {
            let constant = |e: &SparqlExpression| match e {
                SparqlExpression::NamedNode(n) => Ok(Term::from(n.clone())),
                SparqlExpression::Literal(l) => Ok(Term::from(l.clone())),
                _ => Err(unsupported(
                    "REGEX with a pattern or flags that are not constants",
                )),
            };
            match args.as_slice() {
                [text, pattern] => Expression::Regex(operand(text)?, constant(pattern)?, None),
                [text, pattern, flags] => {
                    Expression::Regex(operand(text)?, constant(pattern)?, Some(constant(flags)?))
                }
                _ => {
                    return Err(Error::QuerySyntax(
                        "REGEX takes two or three arguments".into(),
                    ));
                }
            }
        }
        // RAND has a new value in every call (SPARQL 1.1 §17.4.4.5), so
        // results differ per row and per run, and no other engine can
        // confirm them.
        SparqlExpression::FunctionCall(Function::Rand, _) => {
            return Err(unsupported("RAND, whose value differs per row and per run"));
        }
        // Expressions in ORDER BY come with #108.
        SparqlExpression::FunctionCall(
            function @ (Function::Abs | Function::Round | Function::Ceil | Function::Floor),
            _,
        ) if matches!(site, Site::OrderBy) => {
            return Err(in_site(&format!("{function}")));
        }
        SparqlExpression::FunctionCall(function, args) => {
            let [arg] = args.as_slice() else {
                return Err(match function {
                    Function::IsIri
                    | Function::IsBlank
                    | Function::IsLiteral
                    | Function::Str
                    | Function::Lang
                    | Function::Datatype
                    | Function::Abs
                    | Function::Round
                    | Function::Ceil
                    | Function::Floor => {
                        Error::QuerySyntax(format!("{function} takes one argument"))
                    }
                    other => in_site(&format!("the {other} function")),
                });
            };
            let numeric = |f| Ok::<_, Error>(Expression::Numeric(f, operand(arg)?));
            match function {
                Function::IsIri => Expression::Test(TermTest::IsIri, operand(arg)?),
                Function::IsBlank => Expression::Test(TermTest::IsBlank, operand(arg)?),
                Function::IsLiteral => Expression::Test(TermTest::IsLiteral, operand(arg)?),
                Function::Str => Expression::Str(operand(arg)?),
                Function::Lang => Expression::Lang(operand(arg)?),
                Function::Datatype => Expression::Datatype(operand(arg)?),
                Function::Abs => numeric(NumericFunction::Abs)?,
                Function::Round => numeric(NumericFunction::Round)?,
                Function::Ceil => numeric(NumericFunction::Ceil)?,
                Function::Floor => numeric(NumericFunction::Floor)?,
                other => return Err(in_site(&format!("the {other} function"))),
            }
        }
        // Expressions in ORDER BY come with #108.
        SparqlExpression::If(..) | SparqlExpression::Coalesce(_)
            if matches!(site, Site::OrderBy) =>
        {
            return Err(in_site(match expression {
                SparqlExpression::If(..) => "IF",
                _ => "COALESCE",
            }));
        }
        SparqlExpression::If(condition, a, b) => {
            Expression::If(boxed(condition)?, operand(a)?, operand(b)?)
        }
        SparqlExpression::Coalesce(args) => Expression::Coalesce(
            args.iter()
                .map(|a| operand(a).map(|a| *a))
                .collect::<Result<_, _>>()?,
        ),
        SparqlExpression::In(a, list) => Expression::In(
            operand(a)?,
            list.iter()
                .map(|b| operand(b).map(|b| *b))
                .collect::<Result<_, _>>()?,
        ),
        SparqlExpression::Exists(_) => return Err(unsupported("EXISTS and NOT EXISTS")),
        SparqlExpression::UnaryMinus(inner) | SparqlExpression::UnaryPlus(inner)
            if numeric_literal(inner).is_some() =>
        {
            let (lexical, datatype) = numeric_literal(inner).expect("checked by the guard");
            let lexical = match expression {
                SparqlExpression::UnaryMinus(_) => format!("-{lexical}"),
                _ => lexical.to_owned(),
            };
            Expression::Constant(Literal::new_typed_literal(lexical, datatype).into())
        }
        // Expressions in ORDER BY come with #108.
        SparqlExpression::Add(..)
        | SparqlExpression::Subtract(..)
        | SparqlExpression::Multiply(..)
        | SparqlExpression::Divide(..)
        | SparqlExpression::UnaryPlus(_)
        | SparqlExpression::UnaryMinus(_)
            if matches!(site, Site::OrderBy) =>
        {
            return Err(in_site("arithmetic"));
        }
        SparqlExpression::Add(a, b) => Expression::Arith(ArithOp::Add, operand(a)?, operand(b)?),
        SparqlExpression::Subtract(a, b) => {
            Expression::Arith(ArithOp::Subtract, operand(a)?, operand(b)?)
        }
        SparqlExpression::Multiply(a, b) => {
            Expression::Arith(ArithOp::Multiply, operand(a)?, operand(b)?)
        }
        SparqlExpression::Divide(a, b) => {
            Expression::Arith(ArithOp::Divide, operand(a)?, operand(b)?)
        }
        SparqlExpression::UnaryMinus(a) => Expression::Negate(operand(a)?),
        // op:numeric-unary-plus returns its operand, and is a type error
        // for anything but a number, as subtracting zero is; adding zero
        // would turn a double -0 into 0 (IEEE 754: -0 + 0 is 0, -0 - 0 is
        // -0).
        SparqlExpression::UnaryPlus(a) => Expression::Arith(
            ArithOp::Subtract,
            operand(a)?,
            Box::new(Expression::Constant(
                Literal::new_typed_literal(
                    "0",
                    NamedNode::new_unchecked("http://www.w3.org/2001/XMLSchema#integer"),
                )
                .into(),
            )),
        ),
    })
}

/// A literal of a numeric datatype the grammar produces, `-3` being the
/// unary minus applied to `3` (SPARQL 1.1 §19.8, Grammar). Its negation is
/// written with a sign, as op:numeric-unary-minus would return it.
fn numeric_literal(expression: &SparqlExpression) -> Option<(&str, NamedNode)> {
    const NUMERIC: [&str; 3] = [
        "http://www.w3.org/2001/XMLSchema#integer",
        "http://www.w3.org/2001/XMLSchema#decimal",
        "http://www.w3.org/2001/XMLSchema#double",
    ];
    match expression {
        SparqlExpression::Literal(literal)
            if NUMERIC.contains(&literal.datatype().as_str())
                && !literal.value().starts_with(['+', '-'])
                && literal.value() != "NaN" =>
        {
            Some((literal.value(), literal.datatype().into_owned()))
        }
        _ => None,
    }
}

fn compare(
    op: CompareOp,
    a: &SparqlExpression,
    b: &SparqlExpression,
    site: Site,
) -> Result<Expression, Error> {
    Ok(Expression::Compare(
        op,
        operand(a, site)?,
        operand(b, site)?,
    ))
}

/// An argument that must be an RDF term: a variable, a constant, or an
/// expression whose value is a term, such as `STR`, arithmetic or `IF`.
/// Other expressions as arguments, such as a comparison compared with
/// `true`, are not supported.
fn operand(expression: &SparqlExpression, site: Site) -> Result<Box<Expression>, Error> {
    let operand = expression_in(expression, site)?;
    if operand.is_term() {
        Ok(Box::new(operand))
    } else {
        Err(unsupported(&format!(
            "expressions other than terms as arguments in {}",
            site.name()
        )))
    }
}

fn pattern_term(term: &TermPattern) -> Result<PatternTerm, Error> {
    Ok(match term {
        TermPattern::Variable(v) => PatternTerm::Variable(v.as_str().to_owned()),
        TermPattern::BlankNode(b) => PatternTerm::Variable(format!("_:{}", b.as_str())),
        TermPattern::NamedNode(n) => PatternTerm::Constant(n.clone().into()),
        TermPattern::Literal(l) => PatternTerm::Constant(l.clone().into()),
        // Quoted triples exist only with spargebra's `sparql-12` feature.
        #[allow(unreachable_patterns)]
        _ => return Err(unsupported("quoted triple patterns")),
    })
}

fn describe(pattern: &GraphPattern) -> &'static str {
    match pattern {
        GraphPattern::Bgp { .. } | GraphPattern::Join { .. } => "nested basic graph patterns",
        GraphPattern::Path { .. } => "property paths",
        GraphPattern::LeftJoin { .. } => "OPTIONAL",
        GraphPattern::Filter { .. } => "FILTER",
        GraphPattern::Union { .. } => "UNION",
        GraphPattern::Graph { .. } => "GRAPH",
        GraphPattern::Extend { inner, .. } if has_group(inner) => "GROUP BY and aggregates",
        GraphPattern::Extend { .. } => "BIND and SELECT expressions",
        GraphPattern::Minus { .. } => "MINUS",
        GraphPattern::Values { .. } => "VALUES",
        // Solution modifiers inside a pattern come from a subquery.
        GraphPattern::OrderBy { .. }
        | GraphPattern::Project { .. }
        | GraphPattern::Distinct { .. }
        | GraphPattern::Slice { .. } => "subqueries",
        GraphPattern::Reduced { .. } => "REDUCED",
        GraphPattern::Group { .. } => "GROUP BY and aggregates",
        GraphPattern::Service { .. } => "SERVICE",
        // Variants that exist only with spargebra's optional features.
        #[allow(unreachable_patterns)]
        _ => "this graph pattern",
    }
}

/// Aggregates show up as `Extend` over `Group` in the algebra.
fn has_group(pattern: &GraphPattern) -> bool {
    match pattern {
        GraphPattern::Group { .. } => true,
        GraphPattern::Extend { inner, .. } => has_group(inner),
        _ => false,
    }
}

fn unsupported(feature: &str) -> Error {
    Error::UnsupportedFeature(feature.to_owned())
}

#[cfg(test)]
mod tests {
    use oxrdf::{Literal, NamedNode};

    use super::*;

    const PREFIX: &str = "PREFIX ex: <http://example.org/>\n";

    fn parse_ok(body: &str) -> SelectQuery {
        parse(&format!("{PREFIX}{body}")).unwrap()
    }

    fn parse_err(body: &str) -> Error {
        parse(&format!("{PREFIX}{body}")).expect_err("query should be rejected")
    }

    fn var(name: &str) -> PatternTerm {
        PatternTerm::Variable(name.to_owned())
    }

    #[test]
    fn parse_arithmetic_in_filter() {
        let q = parse(
            "SELECT * WHERE { ?c <http://example.org/age> ?a FILTER(-?a * 2 + +?a / 3 > 1) }",
        )
        .unwrap();
        let Pattern::Filter(_, filter) = &q.pattern else {
            panic!("{q:?}")
        };
        let Expression::Compare(CompareOp::Greater, left, _) = filter else {
            panic!("{filter:?}")
        };
        let Expression::Arith(ArithOp::Add, product, quotient) = left.as_ref() else {
            panic!("{left:?}")
        };
        assert!(
            matches!(product.as_ref(), Expression::Arith(ArithOp::Multiply, a, _)
            if matches!(a.as_ref(), Expression::Negate(_)))
        );
        // Unary plus is subtracting zero.
        assert!(
            matches!(quotient.as_ref(), Expression::Arith(ArithOp::Divide, a, _)
            if matches!(a.as_ref(), Expression::Arith(ArithOp::Subtract, ..)))
        );
        assert_eq!(filter.variables(), ["a", "a"]);
    }

    #[test]
    fn parse_ask_projects_nothing_and_asks_for_one_solution() {
        let q = parse_ok("ASK { ?c ex:name ?n }");
        assert_eq!(q.form, QueryForm::Ask);
        assert!(q.variables.is_empty());
        assert!(!q.distinct);
        assert_eq!((q.offset, q.limit), (0, Some(1)));
        assert_eq!(q.pattern.triple_patterns().len(), 1);

        // ORDER BY is not even checked, since it cannot change the answer.
        let q = parse_ok("ASK { ?c ex:name ?n } ORDER BY UCASE(?n)");
        assert!(q.order_by.is_empty());

        let select = parse_ok("SELECT * WHERE { ?c ex:name ?n }");
        assert_eq!(select.form, QueryForm::Select);
    }

    #[test]
    fn parse_ask_rejects_slices_and_what_select_rejects() {
        for (query, feature) in [
            ("ASK { ?c ex:name ?n } LIMIT 0", "LIMIT and OFFSET in ASK"),
            ("ASK { ?c ex:name ?n } LIMIT 1", "LIMIT and OFFSET in ASK"),
            ("ASK { ?c ex:name ?n } OFFSET 1", "LIMIT and OFFSET in ASK"),
            ("ASK FROM ex:g { ?c ex:name ?n }", "FROM"),
            ("ASK { ?c ?p ?o }", "variable predicates"),
            ("ASK { ?c ex:name ?n FILTER(UCASE(?n) = \"A\") }", "UCASE"),
        ] {
            let Error::UnsupportedFeature(message) = parse_err(query) else {
                panic!("{query}")
            };
            assert!(message.contains(feature), "{query}: {message}");
        }
    }

    #[test]
    fn parse_select_bgp_keeps_projection_and_patterns() {
        let q = parse_ok(r#"SELECT ?c ?n WHERE { ?c a ex:Customer ; ex:name ?n ; ex:tier "VIP" }"#);
        assert_eq!(q.variables, ["c", "n"]);
        assert!(!q.distinct);
        assert_eq!(
            q.pattern,
            Pattern::Bgp(vec![
                TriplePattern {
                    subject: var("c"),
                    predicate: RDF_TYPE.to_owned(),
                    object: PatternTerm::Constant(
                        NamedNode::new_unchecked("http://example.org/Customer").into()
                    ),
                },
                TriplePattern {
                    subject: var("c"),
                    predicate: "http://example.org/name".to_owned(),
                    object: var("n"),
                },
                TriplePattern {
                    subject: var("c"),
                    predicate: "http://example.org/tier".to_owned(),
                    object: PatternTerm::Constant(Literal::new_simple_literal("VIP").into()),
                },
            ])
        );
    }

    #[test]
    fn parse_distinct_and_select_star_are_supported() {
        let q = parse_ok("SELECT DISTINCT * WHERE { ?o ex:placedBy ?c }");
        assert!(q.distinct);
        assert_eq!(q.variables.len(), 2);
    }

    #[test]
    fn parse_blank_node_becomes_a_variable() {
        let q = parse_ok("SELECT ?o WHERE { ?o ex:placedBy [] }");
        assert!(
            matches!(&q.pattern.triple_patterns()[0].object, PatternTerm::Variable(v) if v.starts_with("_:"))
        );
    }

    fn bgp(triples: &[(&str, &str, &str)]) -> Pattern {
        Pattern::Bgp(
            triples
                .iter()
                .map(|&(s, p, o)| TriplePattern {
                    subject: var(s),
                    predicate: format!("http://example.org/{p}"),
                    object: var(o),
                })
                .collect(),
        )
    }

    fn left_join(left: Pattern, right: Pattern) -> Pattern {
        Pattern::LeftJoin(Box::new(left), Box::new(right), None)
    }

    #[test]
    fn parse_optional_is_a_left_join_in_written_order() {
        let q = parse_ok(
            "SELECT * WHERE { ?c ex:name ?n OPTIONAL { ?c ex:tier ?t } OPTIONAL { ?c ex:city ?y } }",
        );
        assert_eq!(
            q.pattern,
            left_join(
                left_join(bgp(&[("c", "name", "n")]), bgp(&[("c", "tier", "t")])),
                bgp(&[("c", "city", "y")])
            )
        );
        assert_eq!(q.pattern.left_joins(), 2);
        assert_eq!(q.pattern.required_triple_patterns(&[]).len(), 1);
        assert_eq!(q.pattern.triple_patterns().len(), 3);
    }

    fn minus(left: Pattern, right: Pattern) -> Pattern {
        Pattern::Minus(Box::new(left), Box::new(right))
    }

    #[test]
    fn parse_minus_applies_to_the_group_before_it() {
        let q = parse_ok("SELECT * WHERE { ?c ex:name ?n MINUS { ?c ex:tier ?t } }");
        assert_eq!(q.variables, ["c", "n"]);
        let expected = minus(bgp(&[("c", "name", "n")]), bgp(&[("c", "tier", "t")]));
        assert_eq!(q.pattern, expected);
        assert_eq!(q.pattern.left_joins(), 1);
        assert_eq!(q.pattern.required_triple_patterns(&[]).len(), 1);
        assert_eq!(q.pattern.triple_patterns().len(), 2);
        assert_eq!(q.pattern.solution_triple_patterns().len(), 1);

        // A pattern after MINUS joins its result, and a FILTER of the
        // group applies to the whole group.
        let q = parse_ok(
            "SELECT * WHERE { ?c ex:name ?n MINUS { ?c ex:tier ?t } ?o ex:placedBy ?c \
             FILTER(?n != \"Kim\") }",
        );
        let Pattern::Filter(inner, _) = &q.pattern else {
            panic!("{q:?}")
        };
        assert_eq!(
            **inner,
            Pattern::Join(Box::new(expected), Box::new(bgp(&[("o", "placedBy", "c")])))
        );
        // The FILTER of the right side stays inside it.
        let q =
            parse_ok("SELECT * WHERE { ?c ex:name ?n MINUS { ?c ex:tier ?t FILTER(?t = ?n) } }");
        let Pattern::Minus(_, right) = &q.pattern else {
            panic!("{q:?}")
        };
        assert!(matches!(&**right, Pattern::Filter(..)));

        // The right side binds no variable of the solutions, so a BIND
        // over ?t reads the value of the OPTIONAL part.
        let q = parse_ok(
            "SELECT ?u WHERE { ?c ex:name ?n OPTIONAL { ?c ex:tier ?t } BIND(?t AS ?u) \
             MINUS { ?x ex:tier ?t } }",
        );
        assert_eq!(q.bindings.len(), 1);
    }

    #[test]
    fn parse_patterns_after_optional_join_the_left_join() {
        let q =
            parse_ok("SELECT * WHERE { ?c ex:name ?n OPTIONAL { ?c ex:tier ?t } ?c ex:city ?y }");
        assert_eq!(
            q.pattern,
            Pattern::Join(
                Box::new(left_join(
                    bgp(&[("c", "name", "n")]),
                    bgp(&[("c", "tier", "t")])
                )),
                Box::new(bgp(&[("c", "city", "y")]))
            )
        );
        let required: Vec<&str> = q
            .pattern
            .required_triple_patterns(&[])
            .iter()
            .map(|p| p.predicate.as_str())
            .collect();
        assert_eq!(
            required,
            ["http://example.org/name", "http://example.org/city"]
        );
    }

    #[test]
    fn parse_nested_optional_and_empty_left_sides() {
        let q = parse_ok(
            "SELECT * WHERE { ?c ex:name ?n OPTIONAL { ?c ex:placed ?o OPTIONAL { ?o ex:amount ?a } } }",
        );
        assert_eq!(
            q.pattern,
            left_join(
                bgp(&[("c", "name", "n")]),
                left_join(bgp(&[("c", "placed", "o")]), bgp(&[("o", "amount", "a")]))
            )
        );
        // `{ OPTIONAL { P } }` keeps its empty left side.
        let q = parse_ok("SELECT * WHERE { OPTIONAL { ?c ex:name ?n } }");
        assert_eq!(
            q.pattern,
            left_join(Pattern::Bgp(Vec::new()), bgp(&[("c", "name", "n")]))
        );
        // As the right side of another OPTIONAL it is the same as P.
        let q =
            parse_ok("SELECT * WHERE { ?c ex:name ?n OPTIONAL { OPTIONAL { ?c ex:tier ?t } } }");
        assert_eq!(
            q.pattern,
            left_join(bgp(&[("c", "name", "n")]), bgp(&[("c", "tier", "t")]))
        );
        // An empty OPTIONAL changes nothing.
        let q = parse_ok("SELECT * WHERE { ?c ex:name ?n OPTIONAL { } }");
        assert_eq!(q.pattern, bgp(&[("c", "name", "n")]));
    }

    fn union(left: Pattern, right: Pattern) -> Pattern {
        Pattern::Union(Box::new(left), Box::new(right))
    }

    #[test]
    fn parse_union_keeps_both_sides_and_lists_the_arms() {
        let q =
            parse_ok("SELECT * WHERE { ?c ex:name ?n { ?c ex:tier ?t } UNION { ?c ex:city ?y } }");
        assert_eq!(
            q.pattern,
            Pattern::Join(
                Box::new(bgp(&[("c", "name", "n")])),
                Box::new(union(
                    bgp(&[("c", "tier", "t")]),
                    bgp(&[("c", "city", "y")])
                ))
            )
        );
        let arms = q.pattern.arms(64).unwrap();
        assert_eq!(arms, [vec![false], vec![true]]);
        let required = |arm: &[bool]| -> Vec<String> {
            q.pattern
                .required_triple_patterns(arm)
                .iter()
                .map(|p| p.predicate.replace("http://example.org/", ""))
                .collect()
        };
        assert_eq!(required(&arms[0]), ["name", "tier"]);
        assert_eq!(required(&arms[1]), ["name", "city"]);
        assert_eq!(q.pattern.triple_patterns().len(), 3);

        // A union on each side of a join: every pair of sides, the sides
        // listed in written order.
        let q = parse_ok(
            "SELECT * WHERE { { ?c ex:a ?x } UNION { { ?c ex:b ?x } UNION { ?c ex:c ?x } } \
             { ?c ex:d ?y } UNION { ?c ex:e ?y } }",
        );
        assert_eq!(
            q.pattern.arms(64).unwrap(),
            [
                vec![false, false],
                vec![false, true],
                vec![true, false, false],
                vec![true, false, true],
                vec![true, true, false],
                vec![true, true, true],
            ]
        );
        assert_eq!(q.pattern.arms(5), None);

        // Unions inside OPTIONAL do not distribute over the left join.
        let q = parse_ok(
            "SELECT * WHERE { ?c ex:name ?n OPTIONAL { { ?c ex:a ?x } UNION { ?c ex:b ?x } } }",
        );
        assert_eq!(q.pattern.arms(64).unwrap(), [Vec::<bool>::new()]);
        assert_eq!(q.pattern.left_joins(), 1);

        // `{ } UNION { P }` keeps the empty side, whose one solution binds
        // nothing; `{ } P` is P.
        let q = parse_ok("SELECT * WHERE { { } UNION { ?c ex:name ?n } }");
        assert_eq!(
            q.pattern,
            union(Pattern::Bgp(Vec::new()), bgp(&[("c", "name", "n")]))
        );
        let q = parse_ok("SELECT * WHERE { { } { ?c ex:name ?n } }");
        assert_eq!(q.pattern, bgp(&[("c", "name", "n")]));
    }

    fn variable(name: &str) -> Box<Expression> {
        Box::new(Expression::Variable(name.to_owned()))
    }

    fn integer(value: i64) -> Box<Expression> {
        Box::new(Expression::Constant(Literal::from(value).into()))
    }

    #[test]
    fn parse_group_filters_wrap_the_whole_group() {
        // Written between the triple patterns, the filters still apply to
        // the group, joined with `&&`.
        let q = parse_ok(
            "SELECT * WHERE { ?c ex:name ?n FILTER(?a > 1) ?c ex:amount ?a FILTER(!BOUND(?x)) }",
        );
        assert_eq!(
            q.pattern,
            Pattern::Filter(
                Box::new(bgp(&[("c", "name", "n"), ("c", "amount", "a")])),
                Expression::And(
                    Box::new(Expression::Compare(
                        CompareOp::Greater,
                        variable("a"),
                        integer(1)
                    )),
                    Box::new(Expression::Not(Box::new(Expression::Bound("x".into()))))
                )
            )
        );
    }

    #[test]
    fn parse_filter_in_optional_is_the_left_join_expression() {
        let q = parse_ok(
            "SELECT * WHERE { ?c ex:name ?n OPTIONAL { ?c ex:tier ?t FILTER(isIRI(?n) && ?t != 2) } }",
        );
        let expression = Expression::And(
            Box::new(Expression::Test(TermTest::IsIri, variable("n"))),
            Box::new(Expression::Not(Box::new(Expression::Compare(
                CompareOp::Equal,
                variable("t"),
                integer(2),
            )))),
        );
        assert_eq!(
            q.pattern,
            Pattern::LeftJoin(
                Box::new(bgp(&[("c", "name", "n")])),
                Box::new(bgp(&[("c", "tier", "t")])),
                Some(expression)
            )
        );
        // A filter in a nested group stays in it, joined with the rest.
        let q = parse_ok("SELECT * WHERE { ?c ex:name ?n { ?c ex:tier ?t FILTER(?t) } }");
        assert_eq!(
            q.pattern,
            Pattern::Join(
                Box::new(bgp(&[("c", "name", "n")])),
                Box::new(Pattern::Filter(
                    Box::new(bgp(&[("c", "tier", "t")])),
                    *variable("t")
                ))
            )
        );
        // `OPTIONAL { OPTIONAL { P FILTER(f) } FILTER(e) }`: f filters P.
        let q = parse_ok(
            "SELECT * WHERE { ?c ex:name ?n OPTIONAL { OPTIONAL { ?c ex:tier ?t FILTER(?t) } FILTER(?n) } }",
        );
        assert_eq!(
            q.pattern,
            Pattern::LeftJoin(
                Box::new(bgp(&[("c", "name", "n")])),
                Box::new(Pattern::Filter(
                    Box::new(bgp(&[("c", "tier", "t")])),
                    *variable("t")
                )),
                Some(*variable("n"))
            )
        );
        assert_eq!(q.pattern.left_joins(), 1);
    }

    #[test]
    fn parse_bind_and_select_expressions_in_the_order_written() {
        let q = parse_ok(
            "SELECT ?c (?a + 1 AS ?b) ?d WHERE { ?c ex:amount ?a BIND(?a AS ?d) ?c ex:name ?n }",
        );
        assert_eq!(q.variables, ["c", "b", "d"]);
        assert_eq!(q.pattern.triple_patterns().len(), 2);
        assert_eq!(
            q.bindings,
            [
                Binding {
                    variable: "d".to_owned(),
                    expression: *variable("a"),
                },
                Binding {
                    variable: "b".to_owned(),
                    expression: Expression::Arith(
                        ArithOp::Add,
                        variable("a"),
                        Box::new(Expression::Constant(
                            Literal::new_typed_literal(
                                "1",
                                NamedNode::new_unchecked(
                                    "http://www.w3.org/2001/XMLSchema#integer"
                                )
                            )
                            .into()
                        )),
                    ),
                },
            ]
        );
    }

    /// A `BIND` is evaluated over the whole pattern, which gives the same
    /// value when the rest of the pattern binds nothing its group may leave
    /// unbound.
    #[test]
    fn parse_bind_over_variables_its_group_binds() {
        for body in [
            "SELECT * WHERE { ?c ex:name ?n OPTIONAL { ?o ex:placedBy ?c ; ex:amount ?a } \
             BIND(?a + 1 AS ?b) }",
            "SELECT * WHERE { ?o ex:amount ?a BIND(?a + 1 AS ?b) OPTIONAL { ?o ex:amount ?a } }",
            "SELECT * WHERE { { ?o ex:amount ?a BIND(-?a AS ?b) } ?o ex:placedBy ?c }",
            "SELECT ?b WHERE { ?c ex:name ?n BIND(?x * 2 AS ?b) }",
        ] {
            assert_eq!(parse_ok(body).bindings.len(), 1, "{body}");
        }
    }

    #[test]
    fn parse_functional_forms() {
        let q = parse_ok(
            "SELECT * WHERE { ?c ex:tier ?t ; ex:name ?n \
             FILTER(?t NOT IN (1, ?n) && COALESCE(?x, IF(?t > 1, ?t, ?n)) = 2) }",
        );
        let Pattern::Filter(_, Expression::And(not_in, coalesce)) = &q.pattern else {
            panic!("{q:?}")
        };
        assert_eq!(
            **not_in,
            Expression::Not(Box::new(Expression::In(
                variable("t"),
                vec![*integer(1), *variable("n")]
            )))
        );
        assert_eq!(coalesce.variables(), ["x", "t", "t", "n"]);
        let Expression::Compare(CompareOp::Equal, coalesce, _) = coalesce.as_ref() else {
            panic!("{coalesce:?}")
        };
        assert!(coalesce.is_term());
        assert!(!not_in.is_term());

        // IF and COALESCE whose results are numbers or errors.
        let q = parse_ok(
            "SELECT ?o (COALESCE(?a / ?b, 0) AS ?q) (IF(?a > 1, ?a * 2, 1.5) AS ?d) \
             WHERE { ?o ex:amount ?a ; ex:b ?b }",
        );
        assert_eq!(q.bindings.len(), 2);
        // IN is a boolean, as ORDER BY takes it.
        let q = parse_ok("SELECT * WHERE { ?c ex:name ?n } ORDER BY (?n IN (\"a\", \"b\"))");
        assert!(matches!(q.order_by[0].expression, Expression::In(..)));
    }

    #[test]
    fn parse_numeric_functions() {
        let q = parse_ok(
            "SELECT * WHERE { ?o ex:amount ?a FILTER(ROUND(?a / 2) = ABS(-?a) && CEIL(FLOOR(?a))) }",
        );
        let Pattern::Filter(_, Expression::And(compare, ebv)) = &q.pattern else {
            panic!("{q:?}")
        };
        let Expression::Compare(CompareOp::Equal, round, abs) = compare.as_ref() else {
            panic!("{compare:?}")
        };
        assert_eq!(
            **round,
            Expression::Numeric(
                NumericFunction::Round,
                Box::new(Expression::Arith(
                    ArithOp::Divide,
                    variable("a"),
                    integer(2)
                ))
            )
        );
        assert_eq!(
            **abs,
            Expression::Numeric(
                NumericFunction::Abs,
                Box::new(Expression::Negate(variable("a")))
            )
        );
        assert_eq!(
            **ebv,
            Expression::Numeric(
                NumericFunction::Ceil,
                Box::new(Expression::Numeric(NumericFunction::Floor, variable("a")))
            )
        );
        assert!(ebv.is_term());
        assert_eq!(ebv.variables(), ["a"]);

        // A number or an error, as the value of BIND and SELECT.
        let q = parse_ok(
            "SELECT ?o (ROUND(?a) AS ?r) (COALESCE(ABS(?a), 0) AS ?m) \
             WHERE { ?o ex:amount ?a BIND(FLOOR(?a * 1.5) AS ?f) }",
        );
        assert_eq!(q.bindings.len(), 3);
    }

    #[test]
    fn parse_unsupported_features_are_rejected_by_name() {
        let cases = [
            (
                "SELECT * WHERE { ?c ex:name ?n OPTIONAL { ?c ex:tier ?t BIND(?t AS ?u) } }",
                "BIND inside OPTIONAL",
            ),
            (
                "SELECT * WHERE { { ?c ex:name ?n BIND(?n AS ?m) } UNION { ?c ex:tier ?m } }",
                "BIND inside UNION",
            ),
            (
                "SELECT * WHERE { ?c ex:name ?n BIND(?a + 1 AS ?b) ?o ex:amount ?a }",
                "BIND over ?a, which its group may leave unbound",
            ),
            (
                "SELECT * WHERE { ?c ex:name ?n BIND(?n AS ?m) ?d ex:name ?m }",
                "a triple pattern over ?m, which BIND computes",
            ),
            (
                "SELECT * WHERE { ?o ex:amount ?a BIND(?a * 2 AS ?b) FILTER(?b > 10) }",
                "FILTER over ?b",
            ),
            (
                "SELECT ?o (?a * 2 AS ?b) WHERE { ?o ex:amount ?a } ORDER BY ?b",
                "ORDER BY over ?b",
            ),
            (
                "SELECT (?a * 2 AS ?b) (?b + 1 AS ?c) WHERE { ?o ex:amount ?a }",
                "an expression over ?b",
            ),
            (
                "SELECT (STR(?n) AS ?s) WHERE { ?c ex:name ?n }",
                "STR as the value of BIND and SELECT expressions",
            ),
            (
                "SELECT * WHERE { ?o ex:amount ?a BIND(?a > 1 AS ?big) }",
                "comparisons as the value",
            ),
            (
                "SELECT (IF(BOUND(?n), ?n, 0) AS ?x) WHERE { ?c ex:name ?n }",
                "IF and COALESCE with a result other than arithmetic or a numeric constant",
            ),
            (
                "SELECT (COALESCE(?a * 2, \"none\") AS ?x) WHERE { ?o ex:amount ?a }",
                "IF and COALESCE with a result other than arithmetic or a numeric constant",
            ),
            (
                "SELECT (?a IN (1, 2) AS ?x) WHERE { ?o ex:amount ?a }",
                "IN and NOT IN as the value of BIND",
            ),
            (
                "SELECT * WHERE { ?o ex:amount ?a BIND(?a NOT IN (1) AS ?x) }",
                "IN and NOT IN as the value of BIND",
            ),
            (
                "SELECT * WHERE { ?c ex:tier ?t FILTER(?t IN (?t > 1)) }",
                "expressions other than terms",
            ),
            (
                "SELECT * WHERE { ?c ex:tier ?t FILTER(IF(?t, ?t > 1, false)) }",
                "expressions other than terms",
            ),
            (
                "SELECT * WHERE { ?c ex:tier ?t FILTER(COALESCE(?t > 1, false)) }",
                "expressions other than terms",
            ),
            (
                "SELECT * WHERE { ?c ex:name ?n } ORDER BY IF(BOUND(?n), ?n, \"\")",
                "IF in ORDER BY",
            ),
            (
                "SELECT * WHERE { ?c ex:name ?n } ORDER BY COALESCE(?n, \"\")",
                "COALESCE in ORDER BY",
            ),
            (
                "SELECT * WHERE { ?c ex:tier ?t FILTER(RAND() < 1) }",
                "RAND, whose value differs per row and per run",
            ),
            (
                "SELECT ?c (RAND() AS ?x) WHERE { ?c ex:tier ?t }",
                "RAND, whose value differs per row and per run",
            ),
            (
                "SELECT * WHERE { ?o ex:amount ?a } ORDER BY ABS(?a)",
                "ABS in ORDER BY",
            ),
            (
                "SELECT * WHERE { ?o ex:amount ?a } ORDER BY DESC(ROUND(?a)) ?o",
                "ROUND in ORDER BY",
            ),
            (
                "SELECT * WHERE { ?o ex:amount ?a FILTER(ABS(?a > 1) = 1) }",
                "expressions other than terms",
            ),
            (
                "SELECT * WHERE { ?c ex:tier ?t FILTER(STRLEN(?t) > 2) }",
                "the STRLEN function",
            ),
            (
                "SELECT * WHERE { ?c ex:tier ?t FILTER(STR(UCASE(?t)) = \"A\") }",
                "the UCASE function",
            ),
            (
                "SELECT * WHERE { ?c ex:tier ?t FILTER(UCASE(?t)) }",
                "the UCASE function",
            ),
            (
                "SELECT * WHERE { ?c ex:tier ?t FILTER((?t > 1) = true) }",
                "expressions other than terms",
            ),
            (
                "SELECT * WHERE { ?c ex:tier ?t FILTER(REGEX(?t, ?t)) }",
                "REGEX with a pattern or flags that are not constants",
            ),
            (
                "SELECT * WHERE { ?c ex:tier ?t FILTER EXISTS { ?c ex:name ?n } }",
                "EXISTS",
            ),
            ("SELECT * WHERE { ?c ?p ?o }", "variable predicates"),
            (
                "CONSTRUCT { ?c ex:label ?n } WHERE { ?c ex:name ?n }",
                "CONSTRUCT",
            ),
            ("DESCRIBE ?c WHERE { ?c ex:name ?n }", "DESCRIBE"),
            (
                "SELECT * WHERE { ?c ex:name ?n } ORDER BY UCASE(?n)",
                "the UCASE function in ORDER BY",
            ),
            (
                "SELECT * WHERE { ?c ex:amount ?a } ORDER BY (?a + 1)",
                "arithmetic in ORDER BY",
            ),
            ("SELECT REDUCED * WHERE { ?c ex:name ?n }", "REDUCED"),
            (
                "SELECT * WHERE { { SELECT ?c WHERE { ?c ex:name ?n } LIMIT 1 } }",
                "subqueries",
            ),
            ("SELECT * WHERE { ?c ex:knows+ ?d }", "property paths"),
            ("SELECT * FROM ex:g WHERE { ?c ex:name ?n }", "FROM"),
            ("SELECT * WHERE { GRAPH ?g { ?c ex:name ?n } }", "GRAPH"),
            (
                "SELECT (COUNT(*) AS ?n) WHERE { ?c ex:name ?x }",
                "GROUP BY",
            ),
            ("SELECT * WHERE { }", "empty graph patterns"),
            (
                "SELECT * WHERE { ?c ex:name ?n OPTIONAL { ?c ex:tier ?t VALUES ?t { \"VIP\" } } }",
                "VALUES inside OPTIONAL",
            ),
            (
                "SELECT * WHERE { { ?c ex:name ?n VALUES ?n { \"Kim\" } } UNION { ?c ex:tier ?n } }",
                "VALUES inside UNION",
            ),
            (
                "SELECT * WHERE { ?c ex:name ?n VALUES ?x { 1 } }",
                "VALUES over ?x, which the rest of its group may leave unbound",
            ),
            (
                "SELECT * WHERE { ?c ex:name ?n OPTIONAL { ?c ex:tier ?t } VALUES ?t { \"VIP\" } }",
                "VALUES over ?t",
            ),
            (
                "SELECT * WHERE { { ?c ex:name ?n } UNION { ?c ex:tier ?t } VALUES ?t { \"VIP\" } }",
                "VALUES over ?t",
            ),
            (
                "SELECT * WHERE { VALUES ?n { \"Kim\" } OPTIONAL { ?c ex:name ?n } }",
                "VALUES over ?n",
            ),
            // The FILTER of the inner group would see ?x bound by VALUES.
            (
                "SELECT * WHERE { { ?c ex:name ?n VALUES ?x { 1 } FILTER(BOUND(?x)) } ?c ex:amount ?x }",
                "VALUES over ?x",
            ),
            ("SELECT * WHERE { VALUES ?x { 1 } }", "VALUES over ?x"),
            (
                "SELECT * WHERE { ?c ex:name ?n VALUES ?n { \"Kim\" \"Kim\" } }",
                "VALUES with two rows that can match one solution",
            ),
            (
                "SELECT * WHERE { ?c ex:name ?n VALUES (?c ?n) { (ex:a UNDEF) (ex:a \"Kim\") } }",
                "VALUES with two rows that can match one solution",
            ),
            (
                "SELECT * WHERE { ?c ex:name ?n VALUES ?n { \"Kim\"@EN \"Kim\"@en } }",
                "VALUES with two rows that can match one solution",
            ),
            (
                "SELECT * WHERE { ?c ex:name ?n VALUES () { () } }",
                "VALUES without variables",
            ),
            (
                "SELECT * WHERE { ?c ex:name ?n OPTIONAL { ?c ex:tier ?t MINUS { ?c ex:city ?y } } }",
                "MINUS inside OPTIONAL",
            ),
            (
                "SELECT * WHERE { { ?c ex:name ?n MINUS { ?c ex:tier ?t } } UNION { ?c ex:city ?n } }",
                "MINUS inside UNION",
            ),
            (
                "SELECT * WHERE { ?c ex:name ?n MINUS { ?c ex:tier ?t MINUS { ?c ex:city ?y } } }",
                "MINUS inside MINUS",
            ),
            (
                "SELECT * WHERE { ?c ex:name ?n MINUS { ?c ex:tier ?t BIND(?t AS ?u) } }",
                "BIND inside MINUS",
            ),
            (
                "SELECT * WHERE { ?c ex:name ?n MINUS { ?c ex:tier ?t VALUES ?t { \"VIP\" } } }",
                "VALUES inside MINUS",
            ),
            (
                "SELECT * WHERE { ?c ex:name ?n BIND(?n AS ?m) MINUS { ?d ex:name ?m } }",
                "a triple pattern over ?m, which BIND computes",
            ),
            (
                "SELECT * WHERE { MINUS { ?c ex:name ?n } }",
                "empty graph patterns",
            ),
        ];
        for (query, feature) in cases {
            let err = parse_err(query);
            assert!(
                matches!(&err, Error::UnsupportedFeature(f) if f.contains(feature)),
                "{query}\n=> {err}"
            );
        }
    }

    #[test]
    fn parse_solution_modifiers_in_algebra_order() {
        let q = parse_ok(
            "SELECT DISTINCT ?c WHERE { ?c ex:name ?n } ORDER BY DESC(?n) STR(?c) (?n > \"K\") \
             LIMIT 5 OFFSET 10",
        );
        assert!(q.distinct);
        assert_eq!(q.variables, ["c"]);
        assert_eq!((q.offset, q.limit), (10, Some(5)));
        assert_eq!(
            q.order_by,
            [
                OrderCondition {
                    expression: *variable("n"),
                    descending: true,
                },
                OrderCondition {
                    expression: Expression::Str(variable("c")),
                    descending: false,
                },
                OrderCondition {
                    expression: Expression::Compare(
                        CompareOp::Greater,
                        variable("n"),
                        Box::new(Expression::Constant(
                            Literal::new_simple_literal("K").into()
                        )),
                    ),
                    descending: false,
                },
            ]
        );
        assert_eq!(q.order_by[2].expression.variables(), ["n"]);

        let q = parse_ok("SELECT * WHERE { ?c ex:name ?n }");
        assert_eq!((q.order_by.len(), q.offset, q.limit), (0, 0, None));
        let q = parse_ok("SELECT * WHERE { ?c ex:name ?n } OFFSET 3");
        assert_eq!((q.offset, q.limit), (3, None));
        let q = parse_ok("SELECT * WHERE { ?c ex:name ?n } LIMIT 0");
        assert_eq!(q.limit, Some(0));
    }

    fn literal(value: &str) -> Option<Term> {
        Some(Literal::new_simple_literal(value).into())
    }

    #[test]
    fn parse_values_in_a_group_and_after_the_query() {
        let q = parse_ok(
            "SELECT * WHERE { ?c ex:name ?n VALUES ?n { \"Kim\" \"Lee\" } ?c ex:tier ?t }",
        );
        // The triple patterns around the block are one basic graph pattern.
        assert_eq!(q.pattern, bgp(&[("c", "name", "n"), ("c", "tier", "t")]));
        assert_eq!(
            q.values,
            [InlineData {
                variables: vec!["n".to_owned()],
                rows: vec![vec![literal("Kim")], vec![literal("Lee")]],
            }]
        );

        let q = parse_ok(
            "SELECT ?c WHERE { ?c ex:name ?n } VALUES (?c ?n) { (ex:a UNDEF) (ex:b \"Lee\") }",
        );
        assert_eq!(q.pattern, bgp(&[("c", "name", "n")]));
        let iri = |name: &str| {
            Some(NamedNode::new_unchecked(format!("http://example.org/{name}")).into())
        };
        assert_eq!(
            q.values,
            [InlineData {
                variables: vec!["c".to_owned(), "n".to_owned()],
                rows: vec![vec![iri("a"), None], vec![iri("b"), literal("Lee")]],
            }]
        );

        // Before an OPTIONAL, in a nested group, and one block per VALUES.
        let q = parse_ok(
            "SELECT * WHERE { VALUES ?n { \"Kim\" } ?c ex:name ?n OPTIONAL { ?c ex:tier ?t } \
             { ?c ex:amount ?a VALUES ?a { 1 } } }",
        );
        assert_eq!(
            q.pattern,
            Pattern::Join(
                Box::new(left_join(
                    bgp(&[("c", "name", "n")]),
                    bgp(&[("c", "tier", "t")])
                )),
                Box::new(bgp(&[("c", "amount", "a")]))
            )
        );
        assert_eq!(q.values.len(), 2);
        assert_eq!(q.values[1].rows, [vec![Some(Literal::from(1).into())]]);

        // A group's FILTER wraps the join with the block.
        let q = parse_ok("SELECT * WHERE { ?c ex:name ?n FILTER(?n) VALUES ?n { \"Kim\" } }");
        assert!(matches!(q.pattern, Pattern::Filter(..)));
        assert_eq!(q.values.len(), 1);
    }

    #[test]
    fn parse_values_over_the_limit_is_a_limit_error() {
        let block = |variable: &str, n: usize| {
            let values: Vec<String> = (0..n).map(|i| i.to_string()).collect();
            format!("VALUES ?{variable} {{ {} }}", values.join(" "))
        };
        let q = parse_ok(&format!(
            "SELECT * WHERE {{ ?c ex:amount ?a {} }}",
            block("a", MAX_VALUES)
        ));
        assert_eq!(q.values[0].rows.len(), MAX_VALUES);
        assert_eq!(
            parse_err(&format!(
                "SELECT * WHERE {{ ?c ex:amount ?a {} }}",
                block("a", MAX_VALUES + 1)
            )),
            Error::TooManyValues {
                limit: MAX_VALUES,
                values: MAX_VALUES + 1,
            }
        );
        // The blocks of one query count together.
        let half = MAX_VALUES / 2 + 1;
        assert_eq!(
            parse_err(&format!(
                "SELECT * WHERE {{ ?c ex:amount ?a ; ex:id ?i {} }} {}",
                block("a", half),
                block("i", half)
            )),
            Error::TooManyValues {
                limit: MAX_VALUES,
                values: 2 * half,
            }
        );
    }

    #[test]
    fn parse_invalid_sparql_is_a_syntax_error() {
        assert!(matches!(
            parse_err("SELECT ?x WHERE { ?x"),
            Error::QuerySyntax(_)
        ));
    }
}
