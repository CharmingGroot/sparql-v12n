// SPDX-License-Identifier: Apache-2.0
//! `v12n serve`: the SPARQL 1.1 Protocol endpoint.

use std::io;
use std::path::Path;

use v12n_core::mapping::BaseIri;
use v12n_server::config::Config;

use crate::connector;
use crate::failure::{EXIT_INPUT, Failure, server_failure};

pub(crate) fn serve(config_path: Option<&Path>, base_iri: Option<BaseIri>) -> Result<(), Failure> {
    init_logging();
    let mut config = Config::load(config_path).map_err(server_failure)?;
    if base_iri.is_some() {
        config.base_iri = base_iri;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Failure::new(EXIT_INPUT, "runtime", e.to_string()))?;
    let connector = connector(&config.database.url, config.database.password.as_deref());
    runtime
        .block_on(v12n_server::serve(config, connector))
        .map_err(server_failure)
}

/// `RUST_LOG` takes `info`, `v12n_server=debug,warn` and the like.
fn init_logging() {
    use tracing_subscriber::filter::{LevelFilter, Targets};
    use tracing_subscriber::prelude::*;
    let filter = std::env::var("RUST_LOG")
        .ok()
        .and_then(|value| value.parse::<Targets>().ok())
        .unwrap_or_else(|| Targets::new().with_default(LevelFilter::INFO));
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(io::stderr)
                .with_ansi(false),
        )
        .with(filter)
        .init();
}
