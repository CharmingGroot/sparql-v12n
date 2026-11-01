// SPDX-License-Identifier: Apache-2.0
//! The foreign keys of a snapshot that can remove a join, see
//! [`eliminate_foreign_key_joins`](crate::optimize::eliminate_foreign_key_joins).

use crate::mapping::LogicalTable;
use crate::optimize::ForeignKeyJoin;
use crate::sql::{SqlPlan, stored_table_name};

use super::{CatalogSnapshot, Column, ForeignKey, Relation};

impl CatalogSnapshot {
    /// The foreign keys between tables that `plan` scans that give every
    /// row of the referencing table whose key columns are all non-`NULL`
    /// exactly one referenced row, whose values give the same terms as the
    /// referencing ones.
    pub(super) fn foreign_key_joins(&self, plan: &SqlPlan) -> Vec<ForeignKeyJoin> {
        let tables = plan.logical_tables();
        let mut out = Vec::new();
        for child in &tables {
            let Some(relation) = self.relation(child) else {
                continue;
            };
            for key in &relation.foreign_keys {
                for parent in tables.iter().filter(|t| is_referenced(key, t)) {
                    let Some(parent_relation) = self.relation(parent) else {
                        continue;
                    };
                    if let Some(nullable) = pins_one_row(relation, key, parent_relation) {
                        out.push(ForeignKeyJoin {
                            child: child.clone(),
                            columns: key.columns.clone(),
                            nullable,
                            parent: parent.clone(),
                            parent_columns: key.referenced_columns.clone(),
                        });
                    }
                }
            }
        }
        out
    }
}

/// Whether `table` is the table `key` references. The catalog names it
/// without its schema where the search path finds it, so a mapping that
/// names it with its schema is not recognized.
fn is_referenced(key: &ForeignKey, table: &LogicalTable) -> bool {
    match table {
        LogicalTable::TableName(name) => {
            stored_table_name(name) == stored_table_name(&key.referenced_table)
        }
        LogicalTable::SqlQuery(_) => false,
    }
}

/// Whether every row of `child` whose key columns are all non-`NULL` has
/// exactly one row of `parent` whose referenced columns `=` finds equal to
/// its own, and equal values give the same terms on both sides; if so,
/// whether each key column may be `NULL`. PostgreSQL checks a foreign key
/// only on rows whose key columns are all non-`NULL` (`MATCH SIMPLE`).
fn pins_one_row(child: &Relation, key: &ForeignKey, parent: &Relation) -> Option<Vec<bool>> {
    let columns = columns_named(child, &key.columns)?;
    let referenced = columns_named(parent, &key.referenced_columns)?;
    (references_one_row(parent, key)
        && same_types(&columns, &referenced)
        && compares_exactly(&columns)
        && compares_exactly(&referenced))
    .then(|| columns.iter().map(|c| !c.not_null).collect())
}

/// Whether the referenced columns are a unique key that decides equality
/// as the join's `=` does. A foreign key compares with the equality of a
/// unique index on exactly its referenced columns, and any of them may be
/// the one, so every such index must be exact. A table with inheritance
/// children has no keys: a scan of it also reads rows that its foreign
/// keys and indexes do not cover.
fn references_one_row(parent: &Relation, key: &ForeignKey) -> bool {
    let same_columns = |columns: &[String]| {
        columns.len() == key.referenced_columns.len()
            && columns.iter().all(|c| key.referenced_columns.contains(c))
    };
    let mut keys = parent
        .unique_keys
        .iter()
        .filter(|k| same_columns(&k.columns))
        .peekable();
    keys.peek().is_some() && keys.all(|k| k.identifies_rows())
}

/// The terms of the referenced columns are built from the referencing
/// ones instead. Within one type of [`references_one_row`]'s keys, values
/// that `=` finds equal have the same natural lexical form (R2RML §10.2);
/// across types, such as `int4` and `int8`, the SQL that builds a term can
/// differ.
fn same_types(columns: &[&Column], referenced: &[&Column]) -> bool {
    columns.iter().zip(referenced).all(|(c, p)| c.ty == p.ty)
}

/// R2RML §8 joins with SQL `=`, so the term of the referenced row is built
/// from its own value. Under a nondeterministic collation `=` also finds
/// a referencing value equal that is a different string, which would
/// build a different term.
fn compares_exactly(columns: &[&Column]) -> bool {
    columns.iter().all(|c| c.deterministic)
}

/// The columns of `relation` the database stores as `names`, when the
/// snapshot has all of them.
fn columns_named<'r>(relation: &'r Relation, names: &[String]) -> Option<Vec<&'r Column>> {
    names
        .iter()
        .map(|name| relation.columns.values().find(|c| c.name == *name))
        .collect()
}
