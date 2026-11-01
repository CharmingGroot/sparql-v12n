// SPDX-License-Identifier: Apache-2.0
//! Join elimination on foreign keys: a scan of the referenced table that
//! only rebuilds terms from the referenced columns reads them from the
//! referencing row instead.

use std::collections::{BTreeSet, HashMap};

use crate::mapping::LogicalTable;
use crate::sql::{Condition, SqlBranch, SqlExpr, SqlPlan, TermSql, TermSqlValue, identifier_name};

use super::{column_of, dedup_conditions, scope_conditions, scope_of, term_column};

/// A foreign key that the catalog shows to give every row of `child`
/// exactly one row of `parent` with equal key values, of the same types
/// on both sides (see [`eliminate_foreign_key_joins`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKeyJoin {
    pub child: LogicalTable,
    /// As the database stores them, in key order.
    pub columns: Vec<String>,
    pub parent: LogicalTable,
    /// The referenced columns, as the database stores them, in the order
    /// of `columns`.
    pub parent_columns: Vec<String>,
}

/// The plan without the scans that a foreign key makes redundant; `None`
/// when there are none.
///
/// R2RML §8 joins the parent logical table of a referencing object map on
/// its join conditions, and a basic graph pattern joins the scans of its
/// triple patterns on their shared variables. A scan `tP` of
/// `key.parent` is redundant next to a scan `tC` of `key.child` when
/// both are joined in the same scope (both required, or both directly in
/// the same `OPTIONAL` part), that scope's conditions, a conjunction,
/// hold `tP.p = tC.c` for every pair of columns of the key (a column
/// equality, or a term comparison of the two columns), and the plan reads
/// no other column of `tP`. The key's columns are `NOT NULL` and it
/// references a unique key, so each row of `tC` has exactly one row of
/// `tP` that the scope can keep, and the scope keeps the same rows, each
/// once, without `tP`. In each of them `tP.p` and `tC.c` are values of
/// one type that `=` finds equal, which give the same term, so reading
/// `tC.c` wherever the plan read `tP.p` gives the same results; where an
/// `OPTIONAL` part does not match, both are `NULL`.
///
/// The key equalities then compare a `NOT NULL` column with itself and are
/// dropped. A scan of the parent in another scope than the child's is
/// kept: across an `OPTIONAL` it decides whether the part matched.
pub fn eliminate_foreign_key_joins(plan: &SqlPlan, keys: &[ForeignKeyJoin]) -> Option<SqlPlan> {
    if keys.is_empty() {
        return None;
    }
    let mut out: Option<SqlPlan> = None;
    for (b, branch) in plan.branches.iter().enumerate() {
        let mut reduced: Option<SqlBranch> = None;
        while let Some(scan) = redundant_parent(reduced.as_ref().unwrap_or(branch), keys) {
            remove_parent(reduced.get_or_insert_with(|| branch.clone()), &scan);
        }
        if let Some(mut reduced) = reduced {
            dedup_conditions(&mut reduced);
            out.get_or_insert_with(|| plan.clone()).branches[b] = reduced;
        }
    }
    out
}

/// A parent scan to remove, with the child scan it reads from instead.
struct RedundantParent {
    parent: usize,
    child: usize,
    /// For each referenced column, by the name the database stores, the
    /// child's column as the plan writes it.
    renamed: HashMap<String, String>,
}

fn redundant_parent(branch: &SqlBranch, keys: &[ForeignKeyJoin]) -> Option<RedundantParent> {
    for parent in 0..branch.from.len() {
        for child in (0..branch.from.len()).filter(|&c| c != parent) {
            let scope = scope_of(branch, parent);
            if scope != scope_of(branch, child) {
                continue;
            }
            let conditions = scope_conditions(branch, scope);
            for key in keys {
                if branch.from[child] != key.child || branch.from[parent] != key.parent {
                    continue;
                }
                let Some(renamed) = joined_on_key(conditions, parent, child, key) else {
                    continue;
                };
                if reads_only_key(branch, parent, key) {
                    return Some(RedundantParent {
                        parent,
                        child,
                        renamed,
                    });
                }
            }
        }
    }
    None
}

/// Whether `conditions` hold `t{parent}.p = t{child}.c` for every pair of
/// columns of `key`; if so, each referenced column with the child's
/// column as the plan writes it.
fn joined_on_key(
    conditions: &[Condition],
    parent: usize,
    child: usize,
    key: &ForeignKeyJoin,
) -> Option<HashMap<String, String>> {
    key.parent_columns
        .iter()
        .zip(&key.columns)
        .map(|(p, c)| {
            let written = conditions
                .iter()
                .find_map(|condition| equated_child_column(condition, (parent, p), (child, c)))?;
            Some((p.clone(), written.to_owned()))
        })
        .collect()
}

/// The child column as the plan writes it, when `condition` equates
/// `parent` and `child`, each a table and a column the database stores:
/// by SQL `=`, or as terms of the two columns as they are, which for the
/// types of a [`ForeignKeyJoin`] renders as `=`.
fn equated_child_column<'c>(
    condition: &'c Condition,
    parent: (usize, &str),
    child: (usize, &str),
) -> Option<&'c str> {
    let (a, b) = match condition {
        Condition::Eq(a, b) => (column_ref(a)?, column_ref(b)?),
        Condition::SameTerm(a, b) if a.datatype == b.datatype => {
            (term_column_ref(a)?, term_column_ref(b)?)
        }
        _ => return None,
    };
    let is = |found: &ColumnRef<'_>, (table, column): (usize, &str)| {
        found.table == table && found.stored == column
    };
    if is(&a, parent) && is(&b, child) {
        Some(b.written)
    } else if is(&b, parent) && is(&a, child) {
        Some(a.written)
    } else {
        None
    }
}

/// A column of table `t{table}`, by the name the database stores and the
/// name the plan writes.
struct ColumnRef<'c> {
    table: usize,
    stored: String,
    written: &'c str,
}

fn column_ref(expr: &SqlExpr) -> Option<ColumnRef<'_>> {
    match expr {
        SqlExpr::Column { table, name } => Some(ColumnRef {
            table: *table,
            stored: identifier_name(name),
            written: name,
        }),
        _ => None,
    }
}

/// A term comparison side that is a column as it is: no base IRI to
/// resolve against, no template around it.
fn term_column_ref(term: &TermSql) -> Option<ColumnRef<'_>> {
    match &term.value {
        TermSqlValue::Column { table, name } if term.base_iri.is_none() => Some(ColumnRef {
            table: *table,
            stored: identifier_name(name),
            written: name,
        }),
        _ => None,
    }
}

/// Whether every column the branch reads of `t{parent}` is a referenced
/// column of `key`, so that the child row gives all of them.
fn reads_only_key(branch: &SqlBranch, parent: usize, key: &ForeignKeyJoin) -> bool {
    let mut read = BTreeSet::new();
    branch.clone().for_each_reference(&mut |table, names| {
        if *table == parent {
            read.extend(names.iter().map(|name| identifier_name(name)));
        }
    });
    read.iter()
        .all(|column| key.parent_columns.contains(column))
}

/// Removes the parent scan, reading the child's columns in place of the
/// referenced ones (see [`eliminate_foreign_key_joins`]).
fn remove_parent(branch: &mut SqlBranch, scan: &RedundantParent) {
    let scope = scope_of(branch, scan.parent);
    let shift = |n: usize| if n > scan.parent { n - 1 } else { n };
    let renamed = |name: &str| scan.renamed[&identifier_name(name)].clone();
    branch.for_each_reference(&mut |table, names| {
        if *table == scan.parent {
            for name in names.iter_mut() {
                **name = renamed(name);
            }
            *table = scan.child;
        }
        *table = shift(*table);
    });
    branch.from.remove(scan.parent);
    // The terms that fixed the referenced columns now read the child's.
    let fixed = branch.fixed_columns.remove(scan.parent);
    branch.fixed_columns[shift(scan.child)].extend(fixed.iter().map(|name| renamed(name)));
    for join in &mut branch.left_joins {
        join.tables = shift(join.tables.start)..shift(join.tables.end);
    }
    let child = shift(scan.child);
    let key_columns: Vec<String> = scan.renamed.values().map(|c| identifier_name(c)).collect();
    let reflexive = |a: Option<(usize, String)>, b: Option<(usize, String)>| match (a, b) {
        (Some((ta, ca)), Some((tb, cb))) => {
            ta == child && tb == child && ca == cb && key_columns.contains(&ca)
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

#[cfg(test)]
mod tests;
