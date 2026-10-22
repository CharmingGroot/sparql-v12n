// SPDX-License-Identifier: Apache-2.0
//! Runs SQL produced by `v12n-core` on PostgreSQL, streams rows back as
//! SPARQL solutions, and reads key metadata (primary, unique and foreign
//! keys) that the optimizer relies on.
