// SPDX-License-Identifier: Apache-2.0
//! Pure translation core: parse SPARQL, index the R2RML mapping, unfold
//! the query into SQL and optimize it, then turn result rows back into
//! SPARQL solutions. Nothing here touches a database, so every stage can be
//! tested and explained offline.
//!
//! Front ends call the core through [`Planner`] and [`Engine`]; database
//! adapters implement [`Backend`] and the other traits of [`backend`].

pub mod backend;
pub mod catalog;
pub mod dialect;
mod engine;
mod error;
pub mod mapping;
pub mod optimize;
mod planner;
pub mod results;
pub mod solution;
pub mod sparql;
pub mod sql;
pub mod unfold;
mod xpath_regex;
mod xsd;

pub use backend::{Backend, Limits};
pub use catalog::Catalog;
pub use engine::Engine;
pub use error::Error;
pub use planner::Planner;
