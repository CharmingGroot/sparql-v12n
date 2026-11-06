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
    #[error(
        "query unfolds into more than {limit} UNION branches{}",
        branch_sources(.patterns, *.omitted)
    )]
    TooManyBranches {
        limit: usize,
        /// The triple patterns of the query with more than one candidate,
        /// most candidates first, at most
        /// [`MAX_REPORTED_PATTERNS`](crate::unfold::MAX_REPORTED_PATTERNS).
        patterns: Vec<PatternCandidates>,
        /// The patterns with more than one candidate left out of
        /// `patterns`.
        omitted: usize,
    },
    /// The `VALUES` blocks of the query hold more values than the limit,
    /// [`MAX_VALUES`](crate::sparql::MAX_VALUES).
    #[error("VALUES holds {values} values, more than the limit of {limit}")]
    TooManyValues {
        limit: usize,
        /// Rows times variables of every block, `UNDEF` included.
        values: usize,
    },
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

/// A triple pattern and the number of ways the mapping can produce it,
/// for [`Error::TooManyBranches`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternCandidates {
    /// The pattern with full IRIs, cut after
    /// [`MAX_PATTERN_CHARS`](crate::unfold::MAX_PATTERN_CHARS) characters.
    pub pattern: String,
    /// The triples maps that can produce the pattern, counted once per
    /// object map of the predicate.
    pub triples_maps: usize,
}

fn branch_sources(patterns: &[PatternCandidates], omitted: usize) -> String {
    let mut out: String = patterns
        .iter()
        .map(|p| format!("; {}: {} triples maps", p.pattern, p.triples_maps))
        .collect();
    if omitted > 0 {
        out.push_str(&format!(
            "; {omitted} more patterns with several triples maps"
        ));
    }
    out
}
