// SPDX-License-Identifier: Apache-2.0
//! Pure translation core: parse SPARQL, index the R2RML mapping, unfold
//! the query into SQL and optimize it, then turn result rows back into
//! SPARQL solutions. Nothing here touches a database, so every stage can be
//! tested and explained offline.

mod error;
pub mod mapping;
pub mod optimize;
pub mod results;
pub mod solution;
pub mod sparql;
pub mod sql;
pub mod unfold;
mod xpath_regex;
mod xsd;

pub use error::Error;
