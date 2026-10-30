// SPDX-License-Identifier: Apache-2.0
//! The driving port for answering queries on a database.

use std::sync::Arc;

use crate::Error;
use crate::backend::{Backend, BackendError, ErrorKind};
use crate::catalog::{Catalog, CatalogPlan, CatalogSnapshot};
use crate::planner::Planner;
use crate::sql::{RenderOptions, RenderedSql, SqlPlan};

/// A [`Planner`] paired with one [`Backend`] session and the [`Catalog`]
/// that the sessions of a front end share.
///
/// Steps between planning and running a plan that need the database, such
/// as reading its catalog, belong here, so that every front end gets them.
pub struct Engine<B> {
    planner: Arc<Planner>,
    catalog: Arc<Catalog>,
    backend: B,
}

impl<B: Backend> Engine<B> {
    pub fn new(planner: Arc<Planner>, catalog: Arc<Catalog>, backend: B) -> Self {
        Self {
            planner,
            catalog,
            backend,
        }
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

    /// Reads the catalog entries of every logical table of the mapping
    /// into the shared catalog, and returns the snapshot that results.
    pub async fn load_catalog(&self) -> Result<Arc<CatalogSnapshot>, B::Error> {
        let fresh = self
            .backend
            .load_catalog(self.planner.catalog_request())
            .await?;
        Ok(self.catalog.update(fresh))
    }

    /// Runs `plan` on the shared catalog snapshot (Discussion #187). The
    /// tables of `plan` are read first when the snapshot does not show all
    /// it reads, and read again once when the database no longer matches
    /// the snapshot.
    pub async fn execute(&self, plan: &SqlPlan) -> Result<B::Solutions, B::Error> {
        let snapshot = self.catalog.snapshot();
        let (snapshot, read_now) = if snapshot.covers(plan) {
            (snapshot, false)
        } else {
            (self.reload(plan, &snapshot).await?, true)
        };
        match self.run(plan, Arc::clone(&snapshot), read_now).await {
            Err(e) if matches!(e.kind(), ErrorKind::StaleCatalog) => {
                // Checked again, so that a catalog that keeps changing, as
                // during a migration, fails the query instead of running it
                // on entries already out of date (Discussion #187).
                let snapshot = self.reload(plan, &snapshot).await?;
                self.run(plan, snapshot, false).await
            }
            result => result,
        }
    }

    async fn reload(
        &self,
        plan: &SqlPlan,
        snapshot: &CatalogSnapshot,
    ) -> Result<Arc<CatalogSnapshot>, B::Error> {
        let fresh = self
            .backend
            .load_catalog(&snapshot.request_for(plan))
            .await?;
        Ok(self.catalog.update(fresh))
    }

    /// `read_now` tells that the snapshot holds the plan's tables as read
    /// in the round trip before, because the shared snapshot lacked them.
    async fn run(
        &self,
        plan: &SqlPlan,
        snapshot: Arc<CatalogSnapshot>,
        read_now: bool,
    ) -> Result<B::Solutions, B::Error> {
        let plan = CatalogPlan::new(plan, snapshot)?;
        let plan = if read_now { plan.without_check() } else { plan };
        self.backend.execute(&plan).await
    }
}
