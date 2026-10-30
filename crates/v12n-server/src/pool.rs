// SPDX-License-Identifier: Apache-2.0
//! Reuses database sessions across requests. The request permit bounds
//! how many are leased at once, so the pool needs no size limit of its
//! own.

use std::sync::{Arc, Mutex, PoisonError};

use v12n_core::backend::{Backend, Connector};
use v12n_core::{Catalog, Engine, Limits, Planner};

type BackendError<C> = <<C as Connector>::Backend as Backend>::Error;

pub(crate) struct Pool<C: Connector> {
    connector: C,
    planner: Arc<Planner>,
    catalog: Arc<Catalog>,
    limits: Limits,
    idle: Mutex<Vec<Engine<C::Backend>>>,
}

impl<C: Connector> Pool<C> {
    pub fn new(
        connector: C,
        planner: Arc<Planner>,
        catalog: Arc<Catalog>,
        limits: Limits,
    ) -> Arc<Self> {
        Arc::new(Self {
            connector,
            planner,
            catalog,
            limits,
            idle: Mutex::new(Vec::new()),
        })
    }

    /// An idle session that is still open, or a new one.
    pub async fn get(self: &Arc<Self>) -> Result<Lease<C>, BackendError<C>> {
        loop {
            let idle = self
                .idle
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop();
            match idle {
                Some(engine) if engine.backend().is_closed() => continue,
                Some(engine) => return Ok(self.lease(engine)),
                None => break,
            }
        }
        let backend = self.connector.connect(self.limits).await?;
        let engine = Engine::new(
            Arc::clone(&self.planner),
            Arc::clone(&self.catalog),
            backend,
        );
        Ok(self.lease(engine))
    }

    fn lease(self: &Arc<Self>, engine: Engine<C::Backend>) -> Lease<C> {
        Lease {
            engine,
            pool: Arc::clone(self),
        }
    }
}

/// A session taken from the pool. Dropping it closes the session; only
/// [`release`](Self::release) returns it, because a lease dropped
/// mid-query may leave a statement running that the next request would
/// wait behind.
pub(crate) struct Lease<C: Connector> {
    engine: Engine<C::Backend>,
    pool: Arc<Pool<C>>,
}

impl<C: Connector> Lease<C> {
    pub fn engine(&self) -> &Engine<C::Backend> {
        &self.engine
    }

    /// Returns the session after its result has been read to the end.
    pub fn release(self) {
        if !self.engine.backend().is_closed() {
            self.pool
                .idle
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(self.engine);
        }
    }
}
