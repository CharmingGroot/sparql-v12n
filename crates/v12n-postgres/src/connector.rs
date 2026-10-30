// SPDX-License-Identifier: Apache-2.0
//! Opens [`PostgresBackend`] sessions for the core's [`Connector`] port.

use v12n_core::Limits;
use v12n_core::backend::Connector;

use crate::{Error, PostgresBackend};

/// The settings for opening sessions on one database.
///
/// Not `Debug`: the connection string and the password may hold secrets.
#[derive(Clone)]
pub struct PostgresConnector {
    url: String,
    password: Option<String>,
}

impl PostgresConnector {
    /// `url` is a libpq-style connection string or URL, as
    /// [`PostgresBackend::connect`] takes it.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            password: None,
        }
    }

    /// A password kept out of the URL, for example one read from a
    /// separate environment variable.
    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }
}

impl Connector for PostgresConnector {
    type Backend = PostgresBackend;

    async fn connect(&self, limits: Limits) -> Result<PostgresBackend, Error> {
        match &self.password {
            Some(password) => {
                PostgresBackend::connect_with_password(&self.url, password, limits).await
            }
            None => PostgresBackend::connect(&self.url, limits).await,
        }
    }
}
