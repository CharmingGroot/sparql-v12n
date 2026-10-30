// SPDX-License-Identifier: Apache-2.0
//! The crate-wide error type.

/// Everything that can go wrong in the translation core.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The mapping is not valid Turtle.
    #[error("mapping syntax error: {0}")]
    MappingSyntax(String),
    /// The mapping is valid Turtle but not a valid R2RML mapping.
    #[error("invalid mapping: {0}")]
    InvalidMapping(String),
    /// A triples map has no `rr:logicalTable`.
    #[error("triples map {0} has no rr:logicalTable")]
    MissingLogicalTable(String),
    /// A triples map has neither `rr:subjectMap` nor `rr:subject`.
    #[error("triples map {0} has no rr:subjectMap")]
    MissingSubjectMap(String),
    /// A referencing object map points at a node that is not a triples map.
    #[error("triples map {triples_map} refers to unknown parent triples map {parent}")]
    UnknownParentTriplesMap { triples_map: String, parent: String },
    /// An `rr:template` string is malformed.
    #[error("invalid template {template:?}: {reason}")]
    InvalidTemplate { template: String, reason: String },
    /// The base IRI is not an absolute IRI.
    #[error("invalid base IRI {0}")]
    InvalidBaseIri(String),
    /// The query is not valid SPARQL.
    #[error("query syntax error: {0}")]
    QuerySyntax(String),
    /// The query would unfold into more UNION branches than the limit.
    #[error("query unfolds into more than {limit} UNION branches")]
    TooManyBranches { limit: usize },
    /// A result row cannot be turned into an RDF term, such as a relative
    /// IRI without a base IRI (an R2RML data error).
    #[error("invalid term: {0}")]
    InvalidTerm(String),
    /// A logical table or column that the mapping reads and the database
    /// lacks, as the catalog shows it at the time of the query.
    #[error("{0}")]
    Catalog(String),
    /// A valid construct this engine does not support yet.
    #[error("unsupported feature: {0}")]
    UnsupportedFeature(String),
}
