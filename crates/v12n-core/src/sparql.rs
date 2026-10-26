// SPDX-License-Identifier: Apache-2.0
//! SPARQL parsing into algebra and the supported-feature gate.

use oxrdf::{Literal, NamedNode, Term};
use spargebra::algebra::{Expression as SparqlExpression, Function, GraphPattern, OrderExpression};
use spargebra::term::{NamedNodePattern, TermPattern};
use spargebra::{Query, SparqlParser};

use crate::Error;

/// IRI of `rdf:type`, whose constant objects are looked up as classes.
pub const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// A `SELECT` query in the supported subset: a graph pattern with the
/// solution modifiers in the order SPARQL 1.1 §18.2.5 applies them
/// (Converting Solution Modifiers): `ORDER BY`, projection, `DISTINCT`,
/// then `OFFSET` and `LIMIT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectQuery {
    /// Projected variable names, without `?`, in `SELECT` order.
    pub variables: Vec<String>,
    pub distinct: bool,
    pub pattern: Pattern,
    /// The order comparators of `ORDER BY`, in order; empty without one.
    pub order_by: Vec<OrderCondition>,
    /// Solutions skipped from the start (`OFFSET`), 0 without one.
    pub offset: usize,
    /// Most solutions returned (`LIMIT`).
    pub limit: Option<usize>,
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
            Self::Or(a, b) | Self::And(a, b) | Self::Compare(_, a, b) | Self::SameTerm(a, b) => {
                a.collect_variables(out);
                b.collect_variables(out);
            }
            Self::Not(a)
            | Self::Test(_, a)
            | Self::Str(a)
            | Self::Lang(a)
            | Self::Datatype(a)
            | Self::Regex(a, _, _) => a.collect_variables(out),
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
        )
    }
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

    fn collect_triple_patterns<'p>(&'p self, out: &mut Vec<&'p TriplePattern>, optional: bool) {
        match self {
            Self::Bgp(patterns) => out.extend(patterns),
            Self::Join(left, right) | Self::Union(left, right) => {
                left.collect_triple_patterns(out, optional);
                right.collect_triple_patterns(out, optional);
            }
            Self::LeftJoin(left, right, _) => {
                left.collect_triple_patterns(out, optional);
                if optional {
                    right.collect_triple_patterns(out, optional);
                }
            }
            Self::Filter(inner, _) => inner.collect_triple_patterns(out, optional),
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
            Self::LeftJoin(left, _, _) | Self::Filter(left, _) => left.arms(limit)?,
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
            Self::LeftJoin(inner, _, _) | Self::Filter(inner, _) => {
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

    /// Number of `LeftJoin` nodes in the pattern.
    pub fn left_joins(&self) -> usize {
        match self {
            Self::Bgp(_) => 0,
            Self::Join(left, right) | Self::Union(left, right) => {
                left.left_joins() + right.left_joins()
            }
            Self::LeftJoin(left, right, _) => left.left_joins() + 1 + right.left_joins(),
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
    let (dataset, pattern) = match query {
        Query::Select {
            dataset, pattern, ..
        } => (dataset, pattern),
        Query::Ask { .. } => return Err(unsupported("ASK queries")),
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
    let (distinct, projected) = match sliced {
        GraphPattern::Distinct { inner } => (true, *inner),
        other => (false, other),
    };
    let GraphPattern::Project { inner, variables } = projected else {
        return Err(unsupported(describe(&projected)));
    };
    let (order_by, inner) = match *inner {
        GraphPattern::OrderBy { inner, expression } => {
            let conditions = expression
                .iter()
                .map(order_condition)
                .collect::<Result<Vec<_>, _>>()?;
            (conditions, *inner)
        }
        other => (Vec::new(), other),
    };
    let pattern = group(&inner)?;
    if pattern.triple_patterns().is_empty() {
        return Err(unsupported("empty graph patterns"));
    }
    Ok(SelectQuery {
        variables: variables.iter().map(|v| v.as_str().to_owned()).collect(),
        distinct,
        pattern,
        order_by,
        offset,
        limit,
    })
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

fn group(pattern: &GraphPattern) -> Result<Pattern, Error> {
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
        // A join of basic graph patterns is the basic graph pattern of all
        // their triples: blank node labels cannot be shared across them
        // (SPARQL 1.1 §4.1.4), so nothing is captured by merging.
        GraphPattern::Join { left, right } => Ok(match (group(left)?, group(right)?) {
            (Pattern::Bgp(mut left), Pattern::Bgp(right)) => {
                left.extend(right);
                Pattern::Bgp(left)
            }
            // Join(Z, P) = P for the empty pattern Z, whose one solution
            // binds nothing.
            (Pattern::Bgp(empty), other) | (other, Pattern::Bgp(empty)) if empty.is_empty() => {
                other
            }
            (left, right) => Pattern::Join(Box::new(left), Box::new(right)),
        }),
        GraphPattern::Union { left, right } => Ok(Pattern::Union(
            Box::new(group(left)?),
            Box::new(group(right)?),
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
            let left = group(left)?;
            Ok(match group(right)? {
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
            Box::new(group(inner)?),
            expression_in(expr, Site::Filter)?,
        )),
        other => Err(unsupported(describe(other))),
    }
}

/// Where an expression is written, for error messages.
#[derive(Debug, Clone, Copy)]
enum Site {
    Filter,
    OrderBy,
}

impl Site {
    fn name(self) -> &'static str {
        match self {
            Self::Filter => "FILTER",
            Self::OrderBy => "ORDER BY",
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
        SparqlExpression::FunctionCall(function, args) => {
            let [arg] = args.as_slice() else {
                return Err(match function {
                    Function::IsIri
                    | Function::IsBlank
                    | Function::IsLiteral
                    | Function::Str
                    | Function::Lang
                    | Function::Datatype => {
                        Error::QuerySyntax(format!("{function} takes one argument"))
                    }
                    other => in_site(&format!("the {other} function")),
                });
            };
            match function {
                Function::IsIri => Expression::Test(TermTest::IsIri, operand(arg)?),
                Function::IsBlank => Expression::Test(TermTest::IsBlank, operand(arg)?),
                Function::IsLiteral => Expression::Test(TermTest::IsLiteral, operand(arg)?),
                Function::Str => Expression::Str(operand(arg)?),
                Function::Lang => Expression::Lang(operand(arg)?),
                Function::Datatype => Expression::Datatype(operand(arg)?),
                other => return Err(in_site(&format!("the {other} function"))),
            }
        }
        SparqlExpression::In(..) => return Err(in_site("IN and NOT IN")),
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
        SparqlExpression::Add(..)
        | SparqlExpression::Subtract(..)
        | SparqlExpression::Multiply(..)
        | SparqlExpression::Divide(..)
        | SparqlExpression::UnaryPlus(_)
        | SparqlExpression::UnaryMinus(_) => {
            return Err(in_site("arithmetic"));
        }
        SparqlExpression::If(..) => return Err(in_site("IF")),
        SparqlExpression::Coalesce(_) => return Err(in_site("COALESCE")),
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

/// An argument that must be an RDF term: a variable, a constant, or `STR`,
/// `LANG` or `DATATYPE` of one. Other expressions as arguments, such as a
/// comparison compared with `true`, are not supported.
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
    fn parse_unsupported_features_are_rejected_by_name() {
        let cases = [
            (
                "SELECT * WHERE { ?c ex:tier ?t FILTER(?t IN (1, 2)) }",
                "IN",
            ),
            (
                "SELECT * WHERE { ?c ex:tier ?t FILTER(?t + 1 > 2) }",
                "arithmetic",
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
            ("ASK { ?c ex:name ?n }", "ASK"),
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

    #[test]
    fn parse_invalid_sparql_is_a_syntax_error() {
        assert!(matches!(
            parse_err("SELECT ?x WHERE { ?x"),
            Error::QuerySyntax(_)
        ));
    }
}
