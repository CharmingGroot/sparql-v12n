// SPDX-License-Identifier: Apache-2.0
//! Rewrites on the SQL plan: self-join elimination, and join elimination
//! on foreign keys.

mod foreign_keys;

pub use foreign_keys::{ForeignKeyJoin, eliminate_foreign_key_joins};

use crate::mapping::LogicalTable;
use crate::sql::{
    Condition, SqlBranch, SqlExpr, SqlPlan, TableKeys, TermDatatype, TermSql, TermSqlValue,
    identifier_name,
};

/// The tables that some branch scans more than once, each once: those
/// whose keys [`eliminate_self_joins`] needs. An `rr:sqlQuery` has no keys
/// and is never listed.
pub fn self_join_tables(plan: &SqlPlan) -> Vec<LogicalTable> {
    let mut out: Vec<LogicalTable> = Vec::new();
    for branch in &plan.branches {
        for (j, table) in branch.from.iter().enumerate() {
            if matches!(table, LogicalTable::TableName(_))
                && branch.from[..j].contains(table)
                && !out.contains(table)
            {
                out.push(table.clone());
            }
        }
    }
    out
}

/// The plan with every two scans of one table that the conditions pin to
/// the same row merged into one; `None` when there are none.
///
/// `keys` are unique keys that identify a row by SQL `=`: every column
/// `NOT NULL`, compared by the same equality as the unique index, without
/// a cast or a [`TermKey`](crate::sql::TermKey). Scans `tI` and `tJ` (`I
/// < J`) of one table are the same row in every row of their scope when
/// they are joined in the same scope (both required, or both directly in
/// the same `OPTIONAL` part) and that scope's conditions, a conjunction,
/// hold for every column `k` of one key either `tI.k = tJ.k` (a column
/// equality, or a term comparison of the two columns, which renders as
/// `=` for these types) or the same comparison of both with one
/// expression: `tI.k = $n` and `tJ.k = $n` with the same constant, or the
/// same column of a third scan, comparisons of one kind being transitive.
/// A unique key has at most one row per value, so each combination of
/// rows that the scope keeps has the same row in both scans, and replacing
/// `tJ` by `tI` everywhere keeps exactly those combinations, each once:
/// the result is the same bag of rows.
///
/// The key equalities then compare a `NOT NULL` column with itself and are
/// dropped. Every other condition stays, rewritten, so a column whose
/// `IS NOT NULL` was implied by a dropped equality cannot exist: key
/// columns are `NOT NULL`.
///
/// Scans on two sides of an `OPTIONAL` are never merged: where the part
/// does not match, its columns are `NULL` while the left side's are not,
/// and a `LEFT JOIN` on the key is not the same row in every row. Branches
/// are separate `SELECT`s, so scans of different `UNION` arms are never
/// merged either.
pub fn eliminate_self_joins(plan: &SqlPlan, keys: &TableKeys) -> Option<SqlPlan> {
    if keys.is_empty() {
        return None;
    }
    let mut out: Option<SqlPlan> = None;
    for (b, branch) in plan.branches.iter().enumerate() {
        let mut merged: Option<SqlBranch> = None;
        while let Some((keep, drop, key)) = mergeable(merged.as_ref().unwrap_or(branch), keys) {
            merge(
                merged.get_or_insert_with(|| branch.clone()),
                keep,
                drop,
                &key,
            );
        }
        if let Some(mut merged) = merged {
            dedup_conditions(&mut merged);
            out.get_or_insert_with(|| plan.clone()).branches[b] = merged;
        }
    }
    out
}

/// The `OPTIONAL` part whose own tables include `table`: the innermost
/// one whose range holds it, or `None` for the required tables.
fn scope_of(branch: &SqlBranch, table: usize) -> Option<usize> {
    branch
        .left_joins
        .iter()
        .rposition(|j| j.tables.contains(&table))
}

fn scope_conditions(branch: &SqlBranch, scope: Option<usize>) -> &[Condition] {
    match scope {
        None => &branch.conditions,
        Some(j) => &branch.left_joins[j].conditions,
    }
}

/// Two scans to merge, the earlier first, with the key that pins them to
/// one row.
fn mergeable(branch: &SqlBranch, keys: &TableKeys) -> Option<(usize, usize, Vec<String>)> {
    for j in 0..branch.from.len() {
        let Some(table_keys) = keys.get(&branch.from[j]) else {
            continue;
        };
        for i in 0..j {
            if branch.from[i] != branch.from[j] {
                continue;
            }
            let scope = scope_of(branch, i);
            if scope != scope_of(branch, j) {
                continue;
            }
            let conditions = scope_conditions(branch, scope);
            if let Some(key) = table_keys
                .iter()
                .find(|key| key.iter().all(|k| same_value(conditions, i, j, k)))
            {
                return Some((i, j, key.clone()));
            }
        }
    }
    None
}

/// Column `name` of table `table`, by the name the database stores.
fn column_of(expr: &SqlExpr) -> Option<(usize, String)> {
    match expr {
        SqlExpr::Column { table, name } => Some((*table, identifier_name(name))),
        _ => None,
    }
}

/// The column a term comparison side is, when it is one as it is: no base
/// IRI to resolve against, no template around it.
fn term_column(term: &TermSql) -> Option<(usize, String)> {
    match &term.value {
        TermSqlValue::Column { table, name } if term.base_iri.is_none() => {
            Some((*table, identifier_name(name)))
        }
        _ => None,
    }
}

/// What a condition of the scope compares a column with: an expression by
/// SQL `=`, or a term by [`Condition::SameTerm`], with the column's
/// datatype.
#[derive(PartialEq)]
enum Anchor<'c> {
    Eq(&'c SqlExpr),
    Same(&'c Option<TermDatatype>, &'c TermSql),
}

/// Everything `conditions` compare `t{table}.key` with.
fn anchors<'c>(conditions: &'c [Condition], table: usize, key: &str) -> Vec<Anchor<'c>> {
    let is_key =
        |found: Option<(usize, String)>| found.is_some_and(|(t, c)| t == table && c == key);
    let mut out = Vec::new();
    for condition in conditions {
        match condition {
            Condition::Eq(a, b) => {
                if is_key(column_of(a)) {
                    out.push(Anchor::Eq(b));
                }
                if is_key(column_of(b)) {
                    out.push(Anchor::Eq(a));
                }
            }
            Condition::SameTerm(a, b) => {
                if is_key(term_column(a)) {
                    out.push(Anchor::Same(&a.datatype, b));
                }
                if is_key(term_column(b)) {
                    out.push(Anchor::Same(&b.datatype, a));
                }
            }
            _ => {}
        }
    }
    out
}

/// Whether `conditions` require `t{i}.key = t{j}.key`: they compare the
/// two columns with each other, or both with the same expression in the
/// same way (a constant, or a column of a third scan). Comparisons of
/// one kind are transitive: SQL `=` within a btree operator family, and
/// term equality, which for the column types of `keys` holds exactly for
/// equal values.
fn same_value(conditions: &[Condition], i: usize, j: usize, key: &str) -> bool {
    let left = anchors(conditions, i, key);
    let direct = left.iter().any(|anchor| match anchor {
        Anchor::Eq(e) => column_of(e).is_some_and(|(t, c)| t == j && c == key),
        Anchor::Same(datatype, t) => {
            t.datatype == **datatype && term_column(t).is_some_and(|(t, c)| t == j && c == key)
        }
    });
    direct || {
        let right = anchors(conditions, j, key);
        left.iter().any(|a| right.contains(a))
    }
}

/// Merges scan `drop` into scan `keep` (`keep < drop`), which `key` pins
/// to the same row (see [`eliminate_self_joins`]).
fn merge(branch: &mut SqlBranch, keep: usize, drop: usize, key: &[String]) {
    let scope = scope_of(branch, keep);
    branch.rename_tables(&|t| match t {
        t if t == drop => keep,
        t if t > drop => t - 1,
        t => t,
    });
    branch.from.remove(drop);
    let fixed = branch.fixed_columns.remove(drop);
    branch.fixed_columns[keep].extend(fixed);
    for join in &mut branch.left_joins {
        let shift = |n: usize| if n > drop { n - 1 } else { n };
        join.tables = shift(join.tables.start)..shift(join.tables.end);
    }
    let reflexive = |a: Option<(usize, String)>, b: Option<(usize, String)>| match (a, b) {
        (Some((ta, ca)), Some((tb, cb))) => {
            ta == keep && tb == keep && ca == cb && key.contains(&ca)
        }
        _ => false,
    };
    let conditions = match scope {
        None => &mut branch.conditions,
        Some(j) => &mut branch.left_joins[j].conditions,
    };
    conditions.retain(|c| match c {
        Condition::Eq(a, b) => !reflexive(column_of(a), column_of(b)),
        Condition::SameTerm(a, b) => {
            !(a.datatype == b.datatype && reflexive(term_column(a), term_column(b)))
        }
        _ => true,
    });
}

/// Drops repeated conditions of a conjunction, such as the two equalities
/// of merged scans with the same constant.
fn dedup_conditions(branch: &mut SqlBranch) {
    fn dedup(conditions: &mut Vec<Condition>) {
        let mut seen: Vec<Condition> = Vec::with_capacity(conditions.len());
        conditions.retain(|c| {
            if seen.contains(c) {
                false
            } else {
                seen.push(c.clone());
                true
            }
        });
    }
    dedup(&mut branch.conditions);
    for join in &mut branch.left_joins {
        dedup(&mut join.conditions);
    }
}

#[cfg(test)]
mod tests;
