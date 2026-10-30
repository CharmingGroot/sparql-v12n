// SPDX-License-Identifier: Apache-2.0
//! The driving port for work without a database: checking a mapping,
//! listing its terms and planning queries.

use crate::Error;
use crate::catalog::CatalogRequest;
use crate::mapping::{Mapping, MappingIndex};
use crate::sparql;
use crate::sql::SqlPlan;
use crate::unfold;

/// A mapping with its index, ready to plan queries against.
///
/// The index is built once, so a long-running front end shares one
/// `Planner` across queries.
#[derive(Debug)]
pub struct Planner {
    mapping: Mapping,
    index: MappingIndex,
    catalog_request: CatalogRequest,
}

impl Planner {
    pub fn new(mapping: Mapping) -> Self {
        let index = MappingIndex::new(&mapping);
        let catalog_request = CatalogRequest::for_mapping(&mapping);
        Self {
            mapping,
            index,
            catalog_request,
        }
    }

    pub fn mapping(&self) -> &Mapping {
        &self.mapping
    }

    pub fn index(&self) -> &MappingIndex {
        &self.index
    }

    /// The logical tables and columns the mapping reads.
    pub fn catalog_request(&self) -> &CatalogRequest {
        &self.catalog_request
    }

    /// Parses a SPARQL query and unfolds it into SQL branches.
    ///
    /// Fails with [`Error::QuerySyntax`] for text that is not SPARQL,
    /// [`Error::TooManyBranches`] for a query that unfolds into too many
    /// branches, and [`Error::UnsupportedFeature`] for SPARQL that cannot
    /// be translated yet.
    pub fn plan(&self, query: &str) -> Result<SqlPlan, Error> {
        let query = sparql::parse(query)?;
        unfold::unfold(&query, &self.mapping, &self.index)
    }
}
