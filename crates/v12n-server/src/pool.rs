// SPDX-License-Identifier: Apache-2.0
//! Reuses PostgreSQL connections across requests. The request permit
//! bounds how many are leased at once, so the pool needs no size limit
//! of its own.

use std::sync::{Arc, Mutex, PoisonError};

use v12n_postgres::{Database, Error, Limits};

use crate::config::DatabaseConfig;

pub(crate) struct Pool {
    database: DatabaseConfig,
    limits: Limits,
    idle: Mutex<Vec<Database>>,
}

impl Pool {
    pub fn new(database: DatabaseConfig, limits: Limits) -> Arc<Self> {
        Arc::new(Self {
            database,
            limits,
            idle: Mutex::new(Vec::new()),
        })
    }

    /// An idle connection that is still open, or a new one.
    pub async fn get(self: &Arc<Self>) -> Result<Lease, Error> {
        loop {
            let idle = self
                .idle
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop();
            match idle {
                Some(database) if database.is_closed() => continue,
                Some(database) => return Ok(self.lease(database)),
                None => break,
            }
        }
        let database = match &self.database.password {
            Some(password) => {
                Database::connect_with_password(&self.database.url, password, self.limits).await?
            }
            None => Database::connect(&self.database.url, self.limits).await?,
        };
        Ok(self.lease(database))
    }

    fn lease(self: &Arc<Self>, database: Database) -> Lease {
        Lease {
            database,
            pool: Arc::clone(self),
        }
    }
}

/// A connection taken from the pool. Dropping it closes the connection;
/// only [`release`](Self::release) returns it, because a lease dropped
/// mid-query may leave a statement running that the next request would
/// wait behind.
pub(crate) struct Lease {
    database: Database,
    pool: Arc<Pool>,
}

impl Lease {
    pub fn database(&self) -> &Database {
        &self.database
    }

    /// Returns the connection after its result has been read to the end.
    pub fn release(self) {
        if !self.database.is_closed() {
            self.pool
                .idle
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(self.database);
        }
    }
}
