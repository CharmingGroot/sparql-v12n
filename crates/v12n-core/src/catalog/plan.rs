// SPDX-License-Identifier: Apache-2.0
//! The steps between unfolding and rendering that need the database's
//! catalog, taken from a [`CatalogSnapshot`].

use std::collections::HashMap;
use std::sync::Arc;

use crate::Error;
use crate::mapping::LogicalTable;
use crate::optimize;
use crate::sql::{
    ColumnTypes, RenderOptions, SqlPlan, TableKeys, describe_table, folded_identifier,
    quote_identifier,
};

use super::{CatalogSnapshot, Column, TableCatalog, TableStatus};

/// A [`SqlPlan`] completed from one [`CatalogSnapshot`]: column names
/// resolved, scans of one row merged, joins that foreign keys make
/// redundant removed, duplicate removal decided and `UNION` branches
/// typed. What a [`Backend`](crate::Backend) runs.
#[derive(Debug, Clone)]
pub struct CatalogPlan {
    plan: SqlPlan,
    keys: Option<TableKeys>,
    column_types: Option<ColumnTypes>,
    snapshot: Arc<CatalogSnapshot>,
    tables: Vec<LogicalTable>,
    /// Whether the backend checks the entries before it runs the plan.
    check: bool,
}

impl CatalogPlan {
    /// Fails with [`Error::Catalog`] when the snapshot does not show every
    /// table and column `plan` reads, since keys and types would then be
    /// missing without a sign.
    ///
    /// Each step reads what the one before it changed: self-joins merge on
    /// resolved names, a foreign key removes a parent scan only if the
    /// merged plan reads nothing of it but the key, and both change which
    /// duplicates can occur and so which columns need types.
    pub fn new(plan: &SqlPlan, snapshot: Arc<CatalogSnapshot>) -> Result<Self, Error> {
        if let Some(problem) = snapshot.problem_for(plan) {
            return Err(Error::Catalog(problem.to_string()));
        }
        // Taken before any scan is removed: the entry of a removed parent
        // holds the key that its removal relies on.
        let tables = plan.logical_tables();
        if let Some(unread) = tables.iter().find(|t| snapshot.table(t).is_none()) {
            return Err(Error::Catalog(format!(
                "{} was not read from the catalog",
                describe_table(unread)
            )));
        }
        let folded = plan.folded_columns();
        let plan = if folded.is_empty() {
            plan.clone()
        } else {
            plan.with_resolved_columns(&snapshot.resolved_names(&folded))
        };
        let keys = snapshot.plan_keys(&plan);
        let plan = keys
            .as_ref()
            .and_then(|k| optimize::eliminate_self_joins(&plan, &k.mergeable))
            .unwrap_or(plan);
        let plan = optimize::eliminate_foreign_key_joins(&plan, &snapshot.foreign_key_joins(&plan))
            .unwrap_or(plan);
        let keys = keys.map(|k| k.unique);
        // `DISTINCT` and `UNION` compare values with `=`, so the types of
        // the columns they compare tell which need a key next to them.
        let removes_duplicates = plan.distinct || plan.has_duplicates(keys.as_ref());
        let typed = typed_columns(&plan, removes_duplicates);
        let column_types = (!typed.is_empty()).then(|| snapshot.column_types(&typed));
        let plan = match &column_types {
            Some(types) if plan.branches.len() > 1 => plan.with_union_types(types)?,
            _ => plan,
        };
        Ok(Self {
            plan,
            keys,
            column_types,
            snapshot,
            tables,
            check: true,
        })
    }

    pub fn plan(&self) -> &SqlPlan {
        &self.plan
    }

    /// The keys and column types for rendering; the backend adds what it
    /// learns from the prepared statement.
    pub fn render_options(&self) -> RenderOptions<'_> {
        RenderOptions {
            column_types: self.column_types.as_ref(),
            keys: self.keys.as_ref(),
            ..RenderOptions::default()
        }
    }

    /// The plan without the check, for entries read from the database in
    /// the round trip before: checking them would compare them with
    /// themselves, and a change committed since is no more likely than
    /// one committed between a check and the statement.
    pub fn without_check(self) -> Self {
        Self {
            check: false,
            ..self
        }
    }

    /// Whether the backend has to check [`entries`](Self::entries) before it
    /// runs the plan.
    pub fn needs_check(&self) -> bool {
        self.check
    }

    /// The catalog entries the plan was completed from. The backend runs
    /// the plan only while the database still matches them.
    pub fn entries(&self) -> impl Iterator<Item = (&LogicalTable, &TableCatalog)> {
        self.tables
            .iter()
            .filter_map(|t| Some((t, self.snapshot.table(t)?)))
    }
}

/// The unique keys a plan can use, see [`UniqueKey`](super::UniqueKey).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanKeys {
    /// Every key that separates terms, for removing duplicates.
    pub unique: TableKeys,
    /// Those that also identify a row, for merging scans.
    pub mergeable: TableKeys,
}

impl CatalogSnapshot {
    /// The keys of the tables whose keys can show that `plan` has no
    /// duplicates or that two of its scans read one row. `None` when there
    /// are no such tables, which tells rendering that keys cannot help, as
    /// opposed to tables that have none.
    pub fn plan_keys(&self, plan: &SqlPlan) -> Option<PlanKeys> {
        let tables = keyed_tables(plan);
        if tables.is_empty() {
            return None;
        }
        let mut keys = PlanKeys {
            unique: TableKeys::new(),
            mergeable: TableKeys::new(),
        };
        for table in &tables {
            let Some(relation) = self.relation(table) else {
                continue;
            };
            for key in relation.unique_keys.iter().filter(|k| k.separates_terms()) {
                if key.identifies_rows() {
                    keys.mergeable
                        .entry(table.clone())
                        .or_default()
                        .push(key.columns.clone());
                }
                keys.unique
                    .entry(table.clone())
                    .or_default()
                    .push(key.columns.clone());
            }
        }
        Some(keys)
    }

    /// The names the database stores for `columns`, all
    /// [`folded_identifier`]s as the mapping writes them.
    fn resolved_names(
        &self,
        columns: &[(LogicalTable, String)],
    ) -> HashMap<(LogicalTable, String), String> {
        columns
            .iter()
            .filter_map(|(table, written)| {
                let column = self.relation(table)?.columns.get(written)?;
                Some(((table.clone(), written.clone()), column.name.clone()))
            })
            .collect()
    }

    /// The types of `columns`, named as the plan names them once its names
    /// are resolved.
    fn column_types(&self, columns: &[(LogicalTable, String)]) -> ColumnTypes {
        columns
            .iter()
            .filter_map(|(table, name)| {
                let column = self
                    .relation(table)?
                    .columns
                    .iter()
                    .find(|(written, column)| plan_name(written, column) == *name)?
                    .1;
                Some(((table.clone(), name.clone()), column.ty.clone()))
            })
            .collect()
    }

    pub(super) fn relation(&self, table: &LogicalTable) -> Option<&super::Relation> {
        match &self.table(table)?.status {
            TableStatus::Readable(relation) => Some(relation),
            TableStatus::Missing | TableStatus::Unreadable(_) => None,
        }
    }
}

/// How a resolved plan names a column the mapping writes as `written`:
/// [`SqlPlan::with_resolved_columns`] quotes the stored name of a folded
/// identifier and keeps every other name as written.
fn plan_name(written: &str, column: &Column) -> String {
    if folded_identifier(written) {
        quote_identifier(&column.name)
    } else {
        written.to_owned()
    }
}

/// The tables whose keys the plan can use: those of `key_tables` and
/// `self_join_tables`, each once.
fn keyed_tables(plan: &SqlPlan) -> Vec<LogicalTable> {
    let mut tables = plan.key_tables();
    for table in optimize::self_join_tables(plan) {
        if !tables.contains(&table) {
            tables.push(table);
        }
    }
    tables
}

/// The columns whose types rendering needs: those the plan compares as
/// RDF terms or combines with `UNION`, and every selected column when the
/// query removes duplicates.
fn typed_columns(plan: &SqlPlan, removes_duplicates: bool) -> Vec<(LogicalTable, String)> {
    let mut columns = plan.term_columns();
    let selected = if removes_duplicates {
        plan.selected_columns()
    } else {
        plan.union_columns()
    };
    for column in selected {
        if !columns.contains(&column) {
            columns.push(column);
        }
    }
    columns
}
