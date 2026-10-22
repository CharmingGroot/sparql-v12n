// SPDX-License-Identifier: Apache-2.0
//! R2RML mapping model and the class/property index (`MappingIndex`).

mod index;
mod parse;
mod template;

use std::fmt;

pub use index::{MappingIndex, PropertyCandidate};
pub use oxrdf::Term;
pub use template::{Template, TemplatePart};

use crate::Error;

/// Base IRI for relative IRIs in a mapping document.
///
/// Fixed rather than derived from the file location so that triples map
/// IDs, and everything keyed on them, do not depend on where the mapping
/// file lives. `.invalid` is reserved (RFC 2606) and never resolves.
pub const BASE_IRI: &str = "http://v12n.invalid/";

/// A parsed R2RML mapping: its triples maps in document order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapping {
    triples_maps: Vec<TriplesMap>,
}

impl Mapping {
    /// Parses and validates an R2RML mapping written in Turtle.
    pub fn from_turtle(input: &str) -> Result<Self, Error> {
        parse::parse(input).map(|triples_maps| Self { triples_maps })
    }

    pub fn triples_maps(&self) -> &[TriplesMap] {
        &self.triples_maps
    }

    pub fn triples_map(&self, id: &TriplesMapId) -> Option<&TriplesMap> {
        self.triples_maps.iter().find(|tm| &tm.id == id)
    }
}

/// The resource that identifies a triples map.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TriplesMapId {
    Iri(String),
    BlankNode(String),
}

impl fmt::Display for TriplesMapId {
    /// IRIs under [`BASE_IRI`] are shown relative (`<#CustomerMap>`), as
    /// they are usually written in the mapping file.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Iri(iri) => write!(f, "<{}>", iri.strip_prefix(BASE_IRI).unwrap_or(iri)),
            Self::BlankNode(label) => write!(f, "_:{label}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriplesMap {
    pub id: TriplesMapId,
    pub logical_table: LogicalTable,
    pub subject_map: SubjectMap,
    pub predicate_object_maps: Vec<PredicateObjectMap>,
}

/// The SQL source of a triples map (`rr:tableName` or `rr:sqlQuery`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogicalTable {
    TableName(String),
    SqlQuery(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubjectMap {
    pub term_map: TermMap,
    /// `rr:class` IRIs, in document order.
    pub classes: Vec<String>,
}

/// A rule that produces one RDF term per logical table row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermMap {
    pub value: TermMapValue,
    pub term_type: TermType,
    pub datatype: Option<String>,
    pub language: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermMapValue {
    Constant(Term),
    Column(String),
    Template(Template),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TermType {
    Iri,
    BlankNode,
    Literal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PredicateObjectMap {
    /// Predicate IRIs. Only constant predicates are supported.
    pub predicates: Vec<String>,
    pub object_maps: Vec<ObjectMap>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectMap {
    Term(TermMap),
    Reference(RefObjectMap),
}

/// An object map that points at the subjects of another triples map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefObjectMap {
    pub parent_triples_map: TriplesMapId,
    pub join_conditions: Vec<JoinCondition>,
}

/// `child` is a column of this triples map's logical table, `parent` a
/// column of the parent's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinCondition {
    pub child: String,
    pub parent: String,
}

#[cfg(test)]
mod tests;
