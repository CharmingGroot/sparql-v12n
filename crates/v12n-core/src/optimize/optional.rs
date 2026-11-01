// SPDX-License-Identifier: Apache-2.0
//! Self-join elimination across `OPTIONAL`: a part that can only match
//! the row of a scan of its left side reads that row instead of joining.
//!
//! A part `P` with scan `tJ` reads the row of `tI` when
//!
//! 1. `tJ` is the only table of `P` itself, and every part nested in `P`
//!    reads a row too, so `P` joins nothing else;
//! 2. `tI` is a table of the scope `P` is directly in (required at the
//!    top level, or of the parent part), before `P`, of the same logical
//!    table;
//! 3. for every column `k` of one key that identifies rows, `P`'s
//!    conditions compare `tJ.k` with `tI.k`, or with an expression that
//!    `tI.k` is compared with in the same way by `P`'s conditions or its
//!    scope's: the same constant, or the same column of a table outside
//!    `P` (see [`eliminate_self_joins`](super::eliminate_self_joins));
//! 4. `P`'s other conditions, with `tJ` replaced by `tI`, form the guard:
//!    those that compare a key column with itself or require it not to be
//!    `NULL` always hold and are left out, as are those that the scope
//!    requires as well. What is left must render without parameters and
//!    read no table of `P`: `IS NULL`, `IS NOT NULL`, equalities and term
//!    comparisons of plain columns, and disjunctions of these. A `FILTER`,
//!    a constant or a template keeps the `LEFT JOIN`.
//!
//! In each row the scope keeps, 3 makes `tJ.k` equal to `tI.k` for the
//! whole key, directly or because one comparison relation is transitive,
//! and the key is unique and `NOT NULL`, so the only row `P` can match is
//! the row of `tI`. It matches it exactly where the guard holds, the
//! conditions left out being true there. The `LEFT JOIN` therefore extends
//! each left row by that one row or by `NULL`s: one row either way, so no
//! solution is added or lost (SPARQL 1.1 §18.5, LeftJoin), and each column
//! of `tJ` is `CASE WHEN <guard> THEN tI.c END`. Rows the scope does not
//! keep are dropped whatever the part gives them. Where the scope is a
//! part that did not match, `tI` is `NULL`, and so is every column read
//! from it, as the columns of `P` are; this is also why a part nested in
//! `P` can read the row of `tJ`.

use std::ops::Range;

use super::{Anchor, anchors, key_equals_itself, scope_conditions, scope_of, term_column};
use crate::sql::{
    Condition, SameRow, SqlBranch, SqlExpr, TableKeys, condition_tables, identifier_name,
};

/// The `OPTIONAL` parts of `branch` that read a row of their left side,
/// each with how it does.
pub(super) fn same_rows(branch: &SqlBranch, keys: &TableKeys) -> Vec<(usize, SameRow)> {
    let mut found: Vec<(usize, SameRow)> = Vec::new();
    // A nested part comes after the part it is nested in, so in reverse
    // order its own rule is known when the outer part needs it.
    for part in (0..branch.left_joins.len()).rev() {
        let reads_row = |nested: usize| found.iter().any(|(p, _)| *p == nested);
        if let Some(same_row) = same_row(branch, part, keys, &reads_row) {
            found.push((part, same_row));
        }
    }
    found
}

/// How `part` reads a row of its left side, if it can.
fn same_row(
    branch: &SqlBranch,
    part: usize,
    keys: &TableKeys,
    reads_row: &impl Fn(usize) -> bool,
) -> Option<SameRow> {
    let scan = sole_scan(branch, part, reads_row)?;
    let table_keys = keys.get(&branch.from[scan])?;
    let join = &branch.left_joins[part];
    own_tables(branch, join.parent)
        .filter(|&i| i < join.tables.start && branch.from[i] == branch.from[scan])
        .find_map(|row_of| {
            let key = table_keys
                .iter()
                .find(|key| key.iter().all(|k| pinned(branch, part, row_of, scan, k)))?;
            Some(SameRow {
                scan,
                row_of,
                guard: guard(branch, part, row_of, scan, key)?,
            })
        })
}

/// The only table of `part` itself, when every part nested in it reads a
/// row and so joins nothing.
fn sole_scan(branch: &SqlBranch, part: usize, reads_row: &impl Fn(usize) -> bool) -> Option<usize> {
    let nested_read = branch
        .left_joins
        .iter()
        .enumerate()
        .filter(|(_, j)| j.parent == Some(part))
        .all(|(nested, _)| reads_row(nested));
    let mut own = own_tables(branch, Some(part));
    match (own.next(), own.next()) {
        (Some(scan), None) if nested_read => Some(scan),
        _ => None,
    }
}

/// The tables of `scope` itself, not of a part nested in it.
fn own_tables(branch: &SqlBranch, scope: Option<usize>) -> impl Iterator<Item = usize> + '_ {
    (0..branch.from.len()).filter(move |&t| scope_of(branch, t) == scope)
}

/// Whether, in every row the scope of `part` keeps, the conditions make
/// `t{scan}.key` of the part equal to `t{row_of}.key` (rule 3).
fn pinned(branch: &SqlBranch, part: usize, row_of: usize, scan: usize, key: &str) -> bool {
    let join = &branch.left_joins[part];
    let scan_anchors = anchors(&join.conditions, scan, key);
    if scan_anchors.iter().any(|a| a.is_column(row_of, key)) {
        return true;
    }
    let row_anchors: Vec<Anchor<'_>> = anchors(&join.conditions, row_of, key)
        .into_iter()
        .chain(anchors(scope_conditions(branch, join.parent), row_of, key))
        .collect();
    scan_anchors
        .iter()
        .any(|a| !reads_any(a, &join.tables) && row_anchors.contains(a))
}

/// Whether `anchor` reads one of `tables`.
fn reads_any(anchor: &Anchor<'_>, tables: &Range<usize>) -> bool {
    match anchor {
        Anchor::Eq(e) => condition_tables(&Condition::IsNotNull((*e).clone()))
            .iter()
            .any(|t| tables.contains(t)),
        Anchor::Same(_, term) => term.columns().iter().any(|(t, _)| tables.contains(t)),
    }
}

/// The conditions of `part` on the row of `t{row_of}` that decide whether
/// it matches (rule 4), or `None` when they cannot be repeated where its
/// columns are read.
fn guard(
    branch: &SqlBranch,
    part: usize,
    row_of: usize,
    scan: usize,
    key: &[String],
) -> Option<Vec<Condition>> {
    let mut renamed = branch.clone();
    renamed.rename_tables(&|t| if t == scan { row_of } else { t });
    let join = &branch.left_joins[part];
    let scope = scope_conditions(branch, join.parent);
    let mut out: Vec<Condition> = Vec::new();
    for condition in &renamed.left_joins[part].conditions {
        if holds_on_key(condition, row_of, key)
            || implied(condition, scope)
            || out.contains(condition)
        {
            continue;
        }
        let reads_part = condition_tables(condition)
            .iter()
            .any(|t| join.tables.contains(t));
        if reads_part || !renders_without_parameters(condition) {
            return None;
        }
        out.push(condition.clone());
    }
    Some(out)
}

/// Whether `condition` holds wherever `t{table}` has a row: it compares a
/// column of `key` with itself or requires it not to be `NULL`.
fn holds_on_key(condition: &Condition, table: usize, key: &[String]) -> bool {
    match condition {
        Condition::IsNotNull(SqlExpr::Column { table: t, name }) => {
            *t == table && key.contains(&identifier_name(name))
        }
        _ => key_equals_itself(condition, table, key),
    }
}

/// Whether `scope` holds `condition` too, with its sides in either order.
fn implied(condition: &Condition, scope: &[Condition]) -> bool {
    let swapped = match condition {
        Condition::Eq(a, b) => Some(Condition::Eq(b.clone(), a.clone())),
        Condition::SameTerm(a, b) => Some(Condition::SameTerm(b.clone(), a.clone())),
        _ => None,
    };
    scope
        .iter()
        .any(|c| c == condition || swapped.as_ref() == Some(c))
}

/// Whether `condition` renders without a parameter, so that one guard can
/// be rendered once and repeated.
fn renders_without_parameters(condition: &Condition) -> bool {
    let plain = |e: &SqlExpr| matches!(e, SqlExpr::Column { .. } | SqlExpr::Lexical { .. });
    match condition {
        Condition::IsNotNull(e) | Condition::IsNull(e) => plain(e),
        Condition::Eq(a, b) => plain(a) && plain(b),
        Condition::SameTerm(a, b) => term_column(a).is_some() && term_column(b).is_some(),
        Condition::Or(alternatives) => alternatives
            .iter()
            .flatten()
            .all(renders_without_parameters),
        Condition::EqAny(..) | Condition::Filter(_) => false,
    }
}
