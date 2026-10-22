// SPDX-License-Identifier: Apache-2.0
//! Pure translation core: parse SPARQL, index the R2RML mapping, unfold
//! the query into SQL and optimize it. Nothing here touches a database,
//! so every stage can be tested and explained offline.

pub mod mapping;
pub mod optimize;
pub mod sparql;
pub mod sql;
pub mod unfold;
