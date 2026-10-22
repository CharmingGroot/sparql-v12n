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
/// `DISTINCT`, over one basic graph pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectQuery {
    /// Projected variable names, without `?`, in `SELECT` order.
    pub variables: Vec<String>,
    pub distinct: bool,
    pub patterns: Vec<TriplePattern>,
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
    let mut patterns = Vec::new();
    collect_bgp(&inner, &mut patterns)?;
    if patterns.is_empty() {
        return Err(unsupported("empty graph patterns"));
    }
    Ok(SelectQuery {
        variables: variables.iter().map(|v| v.as_str().to_owned()).collect(),
        distinct,
        patterns,
    })
}

/// A join of basic graph patterns is the basic graph pattern of all their
/// triples: blank node labels cannot be shared across them (SPARQL 1.1
/// §4.1.4), so nothing is captured by merging.
fn collect_bgp(pattern: &GraphPattern, out: &mut Vec<TriplePattern>) -> Result<(), Error> {
    match pattern {
        GraphPattern::Bgp { patterns } => {
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
            Ok(())
        }
        GraphPattern::Join { left, right } => {
            collect_bgp(left, out)?;
            collect_bgp(right, out)
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
            q.patterns,
            [
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
            ]
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
        assert!(matches!(&q.patterns[0].object, PatternTerm::Variable(v) if v.starts_with("_:")));
    }

    #[test]
    fn parse_unsupported_features_are_rejected_by_name() {
        let cases = [
            (
                "SELECT * WHERE { ?c ex:name ?n OPTIONAL { ?c ex:tier ?t } }",
                "OPTIONAL",
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
