// SPDX-License-Identifier: Apache-2.0
//! R2RML mapping model and the class/property index (`MappingIndex`).

mod index;
mod parse;
pub(crate) mod template;

use std::collections::BTreeMap;
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
    prefixes: BTreeMap<String, String>,
}

impl Mapping {
    /// Parses and validates an R2RML mapping written in Turtle.
    pub fn from_turtle(input: &str) -> Result<Self, Error> {
        parse::parse(input).map(|(triples_maps, prefixes)| Self {
            triples_maps,
            prefixes,
        })
    }

    pub fn triples_maps(&self) -> &[TriplesMap] {
        &self.triples_maps
    }

    /// The `@prefix` declarations of the document, prefix name to
    /// namespace IRI. A prefix declared twice keeps its last namespace.
    pub fn prefixes(&self) -> &BTreeMap<String, String> {
        &self.prefixes
    }

    /// `iri` as a Turtle/SPARQL prefixed name (`ex:Customer`) using the
    /// mapping's prefixes, or `None` when no prefix fits.
    ///
    /// The longest matching namespace wins, then the smallest prefix name,
    /// so the result does not depend on declaration order. Local names are
    /// limited to a conservative subset of `PN_LOCAL` that needs no
    /// escaping in either language.
    pub fn compact_iri(&self, iri: &str) -> Option<String> {
        self.prefixes
            .iter()
            .filter_map(|(prefix, namespace)| {
                let local = iri.strip_prefix(namespace.as_str())?;
                is_simple_local_name(local).then_some((namespace.len(), prefix, local))
            })
            .min_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)))
            .map(|(_, prefix, local)| format!("{prefix}:{local}"))
    }

    pub fn triples_map(&self, id: &TriplesMapId) -> Option<&TriplesMap> {
        self.triples_maps.iter().find(|tm| &tm.id == id)
    }
}

/// Letters, digits, `_` and `-`, with `.` allowed inside; empty is
/// allowed too (`ex:`).
fn is_simple_local_name(local: &str) -> bool {
    local
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        && !local.starts_with(['-', '.'])
        && !local.ends_with('.')
}

/// The resource that identifies a triples map.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TriplesMapId {
    Iri(String),
    /// A blank node written with a label (`_:people`).
    BlankNode(String),
    /// A blank node written as `[ ... ]`, which has no label in the
    /// document: its 1-based position among the triples maps, in document
    /// order.
    Anonymous(usize),
}

impl fmt::Display for TriplesMapId {
    /// IRIs under [`BASE_IRI`] are shown relative (`<#CustomerMap>`), as
    /// they are usually written in the mapping file. Anonymous triples maps
    /// show as `[]#3` for the third triples map of the document.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Iri(iri) => write!(f, "<{}>", iri.strip_prefix(BASE_IRI).unwrap_or(iri)),
            Self::BlankNode(label) => write!(f, "_:{label}"),
            Self::Anonymous(position) => write!(f, "[]#{position}"),
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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LogicalTable {
    TableName(String),
    SqlQuery(String),
}

impl LogicalTable {
    /// R2RML §5: `SELECT * FROM {table}` for a table or view, the query
    /// itself for an R2RML view. Two logical tables are the same exactly
    /// when these strings are identical (R2RML §8).
    pub fn effective_sql_query(&self) -> String {
        match self {
            Self::TableName(name) => format!("SELECT * FROM {name}"),
            Self::SqlQuery(query) => query.clone(),
        }
    }
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
