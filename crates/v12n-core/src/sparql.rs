// SPDX-License-Identifier: Apache-2.0
//! SPARQL parsing into algebra and the supported-feature gate.

use oxrdf::Term;
use spargebra::algebra::GraphPattern;
use spargebra::term::{NamedNodePattern, TermPattern};
use spargebra::{Query, SparqlParser};

use crate::Error;

/// IRI of `rdf:type`, whose constant objects are looked up as classes.
pub const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// A `SELECT` query in the supported subset: a projection, optionally
/// `DISTINCT`, over a graph pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectQuery {
    /// Projected variable names, without `?`, in `SELECT` order.
    pub variables: Vec<String>,
    pub distinct: bool,
    pub pattern: Pattern,
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
    /// `LeftJoin(left, right, true)` from `OPTIONAL` (SPARQL 1.1 §18.5).
    /// The algebra's filter expression is always `true` here: a `FILTER`
    /// inside `OPTIONAL` would become it (§18.2.2.6, Translate Graph
    /// Patterns), and is not supported yet.
    LeftJoin(Box<Pattern>, Box<Pattern>),
}

impl Pattern {
    /// Every triple pattern, in the order the pattern is written.
    pub fn triple_patterns(&self) -> Vec<&TriplePattern> {
        let mut out = Vec::new();
        self.collect_triple_patterns(&mut out, true);
        out
    }

    /// The triple patterns outside the right side of every `LeftJoin`,
    /// which every solution matches, in the order they are written.
    pub fn required_triple_patterns(&self) -> Vec<&TriplePattern> {
        let mut out = Vec::new();
        self.collect_triple_patterns(&mut out, false);
        out
    }

    fn collect_triple_patterns<'p>(&'p self, out: &mut Vec<&'p TriplePattern>, optional: bool) {
        match self {
            Self::Bgp(patterns) => out.extend(patterns),
            Self::Join(left, right) => {
                left.collect_triple_patterns(out, optional);
                right.collect_triple_patterns(out, optional);
            }
            Self::LeftJoin(left, right) => {
                left.collect_triple_patterns(out, optional);
                if optional {
                    right.collect_triple_patterns(out, optional);
                }
            }
        }
    }

    /// Number of `LeftJoin` nodes in the pattern.
    pub fn left_joins(&self) -> usize {
        match self {
            Self::Bgp(_) => 0,
            Self::Join(left, right) => left.left_joins() + right.left_joins(),
            Self::LeftJoin(left, right) => left.left_joins() + 1 + right.left_joins(),
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

    let (distinct, projected) = match pattern {
        GraphPattern::Distinct { inner } => (true, *inner),
        other => (false, other),
    };
    let GraphPattern::Project { inner, variables } = projected else {
        return Err(unsupported(describe(&projected)));
    };
    let pattern = group(&inner)?;
    if pattern.triple_patterns().is_empty() {
        return Err(unsupported("empty graph patterns"));
    }
    Ok(SelectQuery {
        variables: variables.iter().map(|v| v.as_str().to_owned()).collect(),
        distinct,
        pattern,
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
            (left, right) => Pattern::Join(Box::new(left), Box::new(right)),
        }),
        GraphPattern::LeftJoin {
            left,
            right,
            expression,
        } => {
            if expression.is_some() {
                return Err(unsupported("FILTER inside OPTIONAL"));
            }
            let left = group(left)?;
            Ok(match group(right)? {
                // `{ }` has one solution that binds nothing, which extends
                // every solution unchanged.
                Pattern::Bgp(right) if right.is_empty() => left,
                // LeftJoin(Z, P), with Z the empty pattern, is P when P
                // has solutions and Z otherwise. As the right side of
                // another LeftJoin, both cases give LeftJoin(left, P).
                Pattern::LeftJoin(inner_left, inner_right) if matches!(&*inner_left, Pattern::Bgp(p) if p.is_empty()) => {
                    Pattern::LeftJoin(Box::new(left), inner_right)
                }
                right => Pattern::LeftJoin(Box::new(left), Box::new(right)),
            })
        }
        other => Err(unsupported(describe(other))),
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
        GraphPattern::OrderBy { .. } => "ORDER BY",
        GraphPattern::Project { .. } => "subqueries",
        GraphPattern::Distinct { .. } => "nested DISTINCT",
        GraphPattern::Reduced { .. } => "REDUCED",
        GraphPattern::Slice { .. } => "LIMIT and OFFSET",
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
        Pattern::LeftJoin(Box::new(left), Box::new(right))
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
        assert_eq!(q.pattern.required_triple_patterns().len(), 1);
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
            .required_triple_patterns()
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

    #[test]
    fn parse_unsupported_features_are_rejected_by_name() {
        let cases = [
            (
                "SELECT * WHERE { ?c ex:name ?n OPTIONAL { ?c ex:tier ?t FILTER(?t = \"VIP\") } }",
                "FILTER inside OPTIONAL",
            ),
            (
                "SELECT * WHERE { ?c ex:tier ?t FILTER(?t = \"VIP\") }",
                "FILTER",
            ),
            (
                "SELECT * WHERE { { ?c ex:name ?n } UNION { ?c ex:tier ?n } }",
                "UNION",
            ),
            ("SELECT * WHERE { ?c ?p ?o }", "variable predicates"),
            ("ASK { ?c ex:name ?n }", "ASK"),
            (
                "CONSTRUCT { ?c ex:label ?n } WHERE { ?c ex:name ?n }",
                "CONSTRUCT",
            ),
            ("DESCRIBE ?c WHERE { ?c ex:name ?n }", "DESCRIBE"),
            ("SELECT * WHERE { ?c ex:name ?n } ORDER BY ?n", "ORDER BY"),
            ("SELECT * WHERE { ?c ex:name ?n } LIMIT 1", "LIMIT"),
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
    fn parse_invalid_sparql_is_a_syntax_error() {
        assert!(matches!(
            parse_err("SELECT ?x WHERE { ?x"),
            Error::QuerySyntax(_)
        ));
    }
}
