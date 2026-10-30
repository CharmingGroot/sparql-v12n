// SPDX-License-Identifier: Apache-2.0
//! The driving port for answering queries on a database.

use std::sync::Arc;

use crate::Error;
use crate::backend::Backend;
use crate::catalog::CatalogSnapshot;
use crate::planner::Planner;
use crate::sql::{RenderOptions, RenderedSql, SqlPlan};

/// A [`Planner`] paired with one [`Backend`] session.
///
/// Steps between planning and running a plan that need the database, such
/// as reading its catalog, belong here, so that every front end gets them.
pub struct Engine<B> {
    planner: Arc<Planner>,
    backend: B,
}

impl<B: Backend> Engine<B> {
    pub fn new(planner: Arc<Planner>, backend: B) -> Self {
        Self { planner, backend }
    }

    pub fn planner(&self) -> &Planner {
        &self.planner
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// The SQL of `plan` in the backend's dialect, without database
    /// information, as [`SqlPlan::to_sql`] renders it. The backend may run
    /// different SQL once it knows the column types and keys.
    pub fn render(&self, plan: &SqlPlan) -> Result<RenderedSql, Error> {
        plan.to_sql_with(self.backend.dialect(), &RenderOptions::default())
    }

    /// The catalog entries of every logical table of the mapping.
    pub async fn load_catalog(&self) -> Result<CatalogSnapshot, B::Error> {
        self.backend
            .load_catalog(self.planner.catalog_request())
            .await
    }

    /// Runs `plan` on the backend.
    pub async fn execute(&self, plan: &SqlPlan) -> Result<B::Solutions, B::Error> {
        self.backend.execute(plan).await
    }
}
