// SPDX-License-Identifier: Apache-2.0
//! Reads the R2RML vocabulary out of a Turtle graph into the typed model.

use std::collections::{HashMap, HashSet};

use oxrdf::{NamedOrBlankNode, Term, Triple};
use oxttl::TurtleParser;

use super::{
    BASE_IRI, JoinCondition, LogicalTable, ObjectMap, PredicateObjectMap, RefObjectMap, SubjectMap,
    Template, TermMap, TermMapValue, TermType, TriplesMap, TriplesMapId,
};
use crate::Error;

const RR: &str = "http://www.w3.org/ns/r2rml#";
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// Valid R2RML properties this engine rejects instead of ignoring, since
/// dropping them would silently change query answers.
const UNSUPPORTED: &[(&str, &str)] = &[
    ("graph", "named graphs (rr:graph)"),
    ("graphMap", "named graphs (rr:graphMap)"),
    ("inverseExpression", "rr:inverseExpression"),
];

pub(super) fn parse(input: &str) -> Result<Vec<TriplesMap>, Error> {
    let graph = Graph::from_turtle(input)?;
    let nodes = graph.triples_map_nodes();
    if nodes.is_empty() {
        return Err(Error::InvalidMapping(
            "no triples maps found (expected resources with rr:logicalTable)".to_owned(),
        ));
    }
    let reader = Reader {
        graph: &graph,
        triples_maps: nodes.iter().collect(),
    };
    nodes.iter().map(|node| reader.triples_map(node)).collect()
}

struct Graph {
    triples: Vec<Triple>,
    by_subject: HashMap<NamedOrBlankNode, Vec<usize>>,
}

impl Graph {
    fn from_turtle(input: &str) -> Result<Self, Error> {
        let parser = TurtleParser::new()
            .with_base_iri(BASE_IRI)
            .expect("BASE_IRI is a valid IRI");
        let mut seen = HashSet::new();
        let mut triples = Vec::new();
        let mut by_subject: HashMap<_, Vec<usize>> = HashMap::new();
        for triple in parser.for_slice(input) {
            let triple = triple.map_err(|e| Error::MappingSyntax(e.to_string()))?;
            // An RDF graph is a set; repeated triples must not repeat classes or maps.
            if !seen.insert(triple.clone()) {
                continue;
            }
            by_subject
                .entry(triple.subject.clone())
                .or_default()
                .push(triples.len());
            triples.push(triple);
        }
        Ok(Self {
            triples,
            by_subject,
        })
    }

    /// R2RML §6 does not require `a rr:TriplesMap`, so any subject of a
    /// triples-map-only property counts, in order of first appearance.
    fn triples_map_nodes(&self) -> Vec<NamedOrBlankNode> {
        let mut seen = HashSet::new();
        let mut nodes = Vec::new();
        for t in &self.triples {
            let is_triples_map = match rr_local(t.predicate.as_str()) {
                Some(local) => matches!(
                    local,
                    "logicalTable" | "subjectMap" | "subject" | "predicateObjectMap"
                ),
                None => {
                    t.predicate.as_str() == RDF_TYPE
                        && matches!(&t.object, Term::NamedNode(n) if rr_local(n.as_str()) == Some("TriplesMap"))
                }
            };
            if is_triples_map && seen.insert(t.subject.clone()) {
                nodes.push(t.subject.clone());
            }
        }
        nodes
    }

    fn properties<'a>(&'a self, node: &NamedOrBlankNode) -> impl Iterator<Item = &'a Triple> {
        self.by_subject
            .get(node)
            .into_iter()
            .flatten()
            .map(|&i| &self.triples[i])
    }

    fn objects<'a>(
        &'a self,
        node: &NamedOrBlankNode,
        local: &str,
    ) -> impl Iterator<Item = &'a Term> {
        self.properties(node)
            .filter(move |t| rr_local(t.predicate.as_str()) == Some(local))
            .map(|t| &t.object)
    }
}

fn rr_local(iri: &str) -> Option<&str> {
    iri.strip_prefix(RR)
}

fn node_id(node: &NamedOrBlankNode) -> TriplesMapId {
    match node {
        NamedOrBlankNode::NamedNode(n) => TriplesMapId::Iri(n.as_str().to_owned()),
        NamedOrBlankNode::BlankNode(b) => TriplesMapId::BlankNode(b.as_str().to_owned()),
    }
}

/// Error context: every message names the triples map it comes from.
struct Ctx<'a>(&'a TriplesMapId);

impl std::fmt::Display for Ctx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "triples map {}", self.0)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Position {
    Subject,
    Predicate,
    Object,
}

struct Reader<'g> {
    graph: &'g Graph,
    triples_maps: HashSet<&'g NamedOrBlankNode>,
}

impl<'g> Reader<'g> {
    fn triples_map(&self, node: &NamedOrBlankNode) -> Result<TriplesMap, Error> {
        let id = node_id(node);
        let ctx = &Ctx(&id);
        self.check_properties(
            node,
            ctx,
            &[
                "logicalTable",
                "subjectMap",
                "subject",
                "predicateObjectMap",
            ],
        )?;

        let logical_table = match self.one(node, "logicalTable", ctx)? {
            Some(term) => self.logical_table(&resource(term, "logicalTable", ctx)?, ctx)?,
            None => return Err(Error::MissingLogicalTable(id.to_string())),
        };

        let subject_maps: Vec<_> = self.graph.objects(node, "subjectMap").collect();
        let subjects: Vec<_> = self.graph.objects(node, "subject").collect();
        let subject_map = match (subject_maps.as_slice(), subjects.as_slice()) {
            ([], []) => return Err(Error::MissingSubjectMap(id.to_string())),
            ([map], []) => self.subject_map(&resource(map, "subjectMap", ctx)?, ctx)?,
            ([], [constant]) => SubjectMap {
                term_map: constant_term_map(constant, Position::Subject, ctx)?,
                classes: Vec::new(),
            },
            _ => {
                return Err(Error::InvalidMapping(format!(
                    "{ctx}: has more than one subject map"
                )));
            }
        };

        let predicate_object_maps = self
            .graph
            .objects(node, "predicateObjectMap")
            .map(|term| {
                let pom = resource(term, "predicateObjectMap", ctx)?;
                self.predicate_object_map(&pom, ctx)
            })
            .collect::<Result<_, _>>()?;

        Ok(TriplesMap {
            id,
            logical_table,
            subject_map,
            predicate_object_maps,
        })
    }

    fn logical_table(&self, node: &NamedOrBlankNode, ctx: &Ctx<'_>) -> Result<LogicalTable, Error> {
        self.check_properties(node, ctx, &["tableName", "sqlQuery", "sqlVersion"])?;
        let table_name = self.one_string(node, "tableName", ctx)?;
        let sql_query = self.one_string(node, "sqlQuery", ctx)?;
        match (table_name, sql_query) {
            (Some(name), None) => Ok(LogicalTable::TableName(name)),
            (None, Some(query)) => Ok(LogicalTable::SqlQuery(query)),
            (Some(_), Some(_)) => Err(Error::InvalidMapping(format!(
                "{ctx}: logical table has both rr:tableName and rr:sqlQuery"
            ))),
            (None, None) => Err(Error::InvalidMapping(format!(
                "{ctx}: logical table needs rr:tableName or rr:sqlQuery"
            ))),
        }
    }

    fn subject_map(&self, node: &NamedOrBlankNode, ctx: &Ctx<'_>) -> Result<SubjectMap, Error> {
        self.check_properties(
            node,
            ctx,
            &[
                "constant", "column", "template", "termType", "datatype", "language", "class",
            ],
        )?;
        let term_map = self.term_map(node, Position::Subject, ctx)?;
        let classes = self
            .graph
            .objects(node, "class")
            .map(|term| iri(term, "class", ctx))
            .collect::<Result<_, _>>()?;
        Ok(SubjectMap { term_map, classes })
    }

    fn predicate_object_map(
        &self,
        node: &NamedOrBlankNode,
        ctx: &Ctx<'_>,
    ) -> Result<PredicateObjectMap, Error> {
        self.check_properties(
            node,
            ctx,
            &["predicate", "predicateMap", "object", "objectMap"],
        )?;

        let mut predicates = Vec::new();
        for term in self.graph.objects(node, "predicate") {
            predicates.push(iri(term, "predicate", ctx)?);
        }
        for term in self.graph.objects(node, "predicateMap") {
            let map = resource(term, "predicateMap", ctx)?;
            self.check_properties(
                &map,
                ctx,
                &[
                    "constant", "column", "template", "termType", "datatype", "language",
                ],
            )?;
            match self.term_map(&map, Position::Predicate, ctx)?.value {
                TermMapValue::Constant(Term::NamedNode(n)) => predicates.push(n.into_string()),
                _ => {
                    return Err(Error::UnsupportedFeature(format!(
                        "{ctx}: rr:predicateMap must be constant (rr:constant or rr:predicate)"
                    )));
                }
            }
        }
        if predicates.is_empty() {
            return Err(Error::InvalidMapping(format!(
                "{ctx}: predicate-object map needs rr:predicate or rr:predicateMap"
            )));
        }

        let mut object_maps = Vec::new();
        for term in self.graph.objects(node, "object") {
            object_maps.push(ObjectMap::Term(constant_term_map(
                term,
                Position::Object,
                ctx,
            )?));
        }
        for term in self.graph.objects(node, "objectMap") {
            object_maps.push(self.object_map(&resource(term, "objectMap", ctx)?, ctx)?);
        }
        if object_maps.is_empty() {
            return Err(Error::InvalidMapping(format!(
                "{ctx}: predicate-object map needs rr:object or rr:objectMap"
            )));
        }

        Ok(PredicateObjectMap {
            predicates,
            object_maps,
        })
    }

    fn object_map(&self, node: &NamedOrBlankNode, ctx: &Ctx<'_>) -> Result<ObjectMap, Error> {
        let Some(parent) = self.one(node, "parentTriplesMap", ctx)? else {
            self.check_properties(
                node,
                ctx,
                &[
                    "constant", "column", "template", "termType", "datatype", "language",
                ],
            )?;
            return Ok(ObjectMap::Term(self.term_map(
                node,
                Position::Object,
                ctx,
            )?));
        };

        self.check_properties(node, ctx, &["parentTriplesMap", "joinCondition"])?;
        let parent = resource(parent, "parentTriplesMap", ctx)?;
        if !self.triples_maps.contains(&&parent) {
            return Err(Error::UnknownParentTriplesMap {
                triples_map: ctx.0.to_string(),
                parent: node_id(&parent).to_string(),
            });
        }
        let join_conditions = self
            .graph
            .objects(node, "joinCondition")
            .map(|term| {
                let condition = resource(term, "joinCondition", ctx)?;
                self.check_properties(&condition, ctx, &["child", "parent"])?;
                let column = |local: &str| {
                    self.one_string(&condition, local, ctx)?.ok_or_else(|| {
                        Error::InvalidMapping(format!("{ctx}: rr:joinCondition needs rr:{local}"))
                    })
                };
                Ok(JoinCondition {
                    child: column("child")?,
                    parent: column("parent")?,
                })
            })
            .collect::<Result<_, Error>>()?;
        Ok(ObjectMap::Reference(RefObjectMap {
            parent_triples_map: node_id(&parent),
            join_conditions,
        }))
    }

    fn term_map(
        &self,
        node: &NamedOrBlankNode,
        position: Position,
        ctx: &Ctx<'_>,
    ) -> Result<TermMap, Error> {
        let constant = self.one(node, "constant", ctx)?;
        let column = self.one_string(node, "column", ctx)?;
        let template = self.one_string(node, "template", ctx)?;
        let datatype = self
            .one(node, "datatype", ctx)?
            .map(|t| iri(t, "datatype", ctx))
            .transpose()?;
        let language = self.one_string(node, "language", ctx)?;
        let term_type = self
            .one(node, "termType", ctx)?
            .map(|t| match t {
                Term::NamedNode(n) => match rr_local(n.as_str()) {
                    Some("IRI") => Ok(TermType::Iri),
                    Some("BlankNode") => Ok(TermType::BlankNode),
                    Some("Literal") => Ok(TermType::Literal),
                    _ => Err(Error::InvalidMapping(format!(
                        "{ctx}: rr:termType must be rr:IRI, rr:BlankNode or rr:Literal, got {t}"
                    ))),
                },
                _ => Err(Error::InvalidMapping(format!(
                    "{ctx}: rr:termType must be rr:IRI, rr:BlankNode or rr:Literal, got {t}"
                ))),
            })
            .transpose()?;

        let mut term_map = match (constant, column, template) {
            (Some(constant), None, None) => {
                if datatype.is_some() || language.is_some() {
                    return Err(Error::InvalidMapping(format!(
                        "{ctx}: rr:datatype and rr:language do not apply to rr:constant"
                    )));
                }
                let term_map = constant_term_map(constant, position, ctx)?;
                if term_type.is_some_and(|t| t != term_map.term_type) {
                    return Err(Error::InvalidMapping(format!(
                        "{ctx}: rr:termType contradicts the rr:constant {constant}"
                    )));
                }
                return Ok(term_map);
            }
            (None, Some(column), None) => TermMap {
                value: TermMapValue::Column(column),
                // R2RML §7.4: column-valued object maps default to literals.
                term_type: if position == Position::Object {
                    TermType::Literal
                } else {
                    TermType::Iri
                },
                datatype: None,
                language: None,
            },
            (None, None, Some(template)) => TermMap {
                value: TermMapValue::Template(Template::parse(&template)?),
                term_type: TermType::Iri,
                datatype: None,
                language: None,
            },
            _ => {
                return Err(Error::InvalidMapping(format!(
                    "{ctx}: a term map needs exactly one of rr:constant, rr:column, rr:template"
                )));
            }
        };

        // R2RML §7.4: a datatype or language tag also implies a literal.
        if let Some(term_type) = term_type {
            term_map.term_type = term_type;
        } else if position == Position::Object && (datatype.is_some() || language.is_some()) {
            term_map.term_type = TermType::Literal;
        }
        if datatype.is_some() && language.is_some() {
            return Err(Error::InvalidMapping(format!(
                "{ctx}: a term map cannot have both rr:datatype and rr:language"
            )));
        }
        if (datatype.is_some() || language.is_some()) && term_map.term_type != TermType::Literal {
            return Err(Error::InvalidMapping(format!(
                "{ctx}: rr:datatype and rr:language need a literal term map"
            )));
        }
        term_map.datatype = datatype;
        term_map.language = language;
        check_term_type(term_map.term_type, position, ctx)?;
        Ok(term_map)
    }

    fn one(
        &self,
        node: &NamedOrBlankNode,
        local: &str,
        ctx: &Ctx<'_>,
    ) -> Result<Option<&'g Term>, Error> {
        let mut values = self.graph.objects(node, local);
        let first = values.next();
        if values.next().is_some() {
            return Err(Error::InvalidMapping(format!(
                "{ctx}: rr:{local} must have a single value"
            )));
        }
        Ok(first)
    }

    fn one_string(
        &self,
        node: &NamedOrBlankNode,
        local: &str,
        ctx: &Ctx<'_>,
    ) -> Result<Option<String>, Error> {
        self.one(node, local, ctx)?
            .map(|term| match term {
                Term::Literal(l) => Ok(l.value().to_owned()),
                _ => Err(Error::InvalidMapping(format!(
                    "{ctx}: rr:{local} must be a string literal, got {term}"
                ))),
            })
            .transpose()
    }

    /// Rejects unknown `rr:` properties so typos fail loudly. Properties
    /// outside the `rr:` namespace (labels, comments, `rdf:type`) are left
    /// alone.
    fn check_properties(
        &self,
        node: &NamedOrBlankNode,
        ctx: &Ctx<'_>,
        allowed: &[&str],
    ) -> Result<(), Error> {
        for t in self.graph.properties(node) {
            let Some(local) = rr_local(t.predicate.as_str()) else {
                continue;
            };
            if let Some((_, feature)) = UNSUPPORTED.iter().find(|(p, _)| *p == local) {
                return Err(Error::UnsupportedFeature(format!("{ctx}: {feature}")));
            }
            if !allowed.contains(&local) {
                return Err(Error::InvalidMapping(format!(
                    "{ctx}: unexpected property rr:{local} on {}",
                    node_id(node)
                )));
            }
        }
        Ok(())
    }
}

/// The shortcuts `rr:subject`, `rr:predicate` and `rr:object` stand for a
/// term map with that `rr:constant` (R2RML §7.1).
fn constant_term_map(constant: &Term, position: Position, ctx: &Ctx<'_>) -> Result<TermMap, Error> {
    let term_type = match constant {
        Term::NamedNode(_) => TermType::Iri,
        Term::Literal(_) => TermType::Literal,
        _ => {
            return Err(Error::InvalidMapping(format!(
                "{ctx}: rr:constant must be an IRI or a literal, got {constant}"
            )));
        }
    };
    check_term_type(term_type, position, ctx)?;
    Ok(TermMap {
        value: TermMapValue::Constant(constant.clone()),
        term_type,
        datatype: None,
        language: None,
    })
}

fn check_term_type(term_type: TermType, position: Position, ctx: &Ctx<'_>) -> Result<(), Error> {
    match (position, term_type) {
        (Position::Subject, TermType::Literal) => Err(Error::InvalidMapping(format!(
            "{ctx}: a subject map cannot produce literals"
        ))),
        (Position::Predicate, TermType::BlankNode | TermType::Literal) => Err(
            Error::InvalidMapping(format!("{ctx}: a predicate map must produce IRIs")),
        ),
        _ => Ok(()),
    }
}

fn resource(term: &Term, local: &str, ctx: &Ctx<'_>) -> Result<NamedOrBlankNode, Error> {
    match term {
        Term::NamedNode(n) => Ok(n.clone().into()),
        Term::BlankNode(b) => Ok(b.clone().into()),
        _ => Err(Error::InvalidMapping(format!(
            "{ctx}: rr:{local} must point to a resource, got {term}"
        ))),
    }
}

fn iri(term: &Term, local: &str, ctx: &Ctx<'_>) -> Result<String, Error> {
    match term {
        Term::NamedNode(n) => Ok(n.as_str().to_owned()),
        _ => Err(Error::InvalidMapping(format!(
            "{ctx}: rr:{local} must be an IRI, got {term}"
        ))),
    }
}
