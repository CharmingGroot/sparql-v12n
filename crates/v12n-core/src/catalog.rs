// SPDX-License-Identifier: Apache-2.0
//! What the database catalog says about the logical tables of a mapping,
//! read once by a [`Backend`](crate::Backend) and checked on every query by
//! a fingerprint (Discussion #187).

use std::collections::HashMap;
use std::fmt;

use crate::mapping::{LogicalTable, Mapping, ObjectMap, TermMap, TermMapValue};
use crate::sql::{ColumnType, Comparison, describe_table};

/// The columns a mapping reads from each of its logical tables, named as
/// the mapping writes them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogRequest {
    tables: Vec<TableRequest>,
}

/// One logical table of a [`CatalogRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRequest {
    pub table: LogicalTable,
    /// Each once, in document order. Empty for a table whose terms are
    /// all constants, which still has to exist.
    pub columns: Vec<String>,
}

impl CatalogRequest {
    /// Every logical table of `mapping`, with the columns its term maps
    /// and the join conditions that refer to it read.
    pub fn for_mapping(mapping: &Mapping) -> Self {
        let mut request = Self::default();
        for tm in mapping.triples_maps() {
            let table = &tm.logical_table;
            request.table_mut(table);
            request.add_term_map(table, &tm.subject_map.term_map);
            for object_map in tm
                .predicate_object_maps
                .iter()
                .flat_map(|pom| &pom.object_maps)
            {
                match object_map {
                    ObjectMap::Term(term_map) => request.add_term_map(table, term_map),
                    ObjectMap::Reference(reference) => {
                        let parent = mapping
                            .triples_map(&reference.parent_triples_map)
                            .map(|parent| &parent.logical_table);
                        for join in &reference.join_conditions {
                            request.add_column(table, &join.child);
                            if let Some(parent) = parent {
                                request.add_column(parent, &join.parent);
                            }
                        }
                    }
                }
            }
        }
        request
    }

    pub fn tables(&self) -> &[TableRequest] {
        &self.tables
    }

    fn add_term_map(&mut self, table: &LogicalTable, term_map: &TermMap) {
        match &term_map.value {
            TermMapValue::Constant(_) => {}
            TermMapValue::Column(column) => self.add_column(table, column),
            TermMapValue::Template(template) => {
                for column in template.columns() {
                    self.add_column(table, column);
                }
            }
        }
    }

    fn add_column(&mut self, table: &LogicalTable, column: &str) {
        let columns = &mut self.table_mut(table).columns;
        if !columns.iter().any(|c| c == column) {
            columns.push(column.to_owned());
        }
    }

    fn table_mut(&mut self, table: &LogicalTable) -> &mut TableRequest {
        let position = match self.tables.iter().position(|t| t.table == *table) {
            Some(position) => position,
            None => {
                self.tables.push(TableRequest {
                    table: table.clone(),
                    columns: Vec::new(),
                });
                self.tables.len() - 1
            }
        };
        &mut self.tables[position]
    }
}

/// The catalog entries of the tables of a [`CatalogRequest`], as the
/// database reported them at one time.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CatalogSnapshot {
    tables: HashMap<LogicalTable, TableCatalog>,
}

impl CatalogSnapshot {
    pub fn new(tables: impl IntoIterator<Item = (LogicalTable, TableCatalog)>) -> Self {
        Self {
            tables: tables.into_iter().collect(),
        }
    }

    pub fn table(&self, table: &LogicalTable) -> Option<&TableCatalog> {
        self.tables.get(table)
    }

    /// What the tables of `request` lack, in the order of `request`.
    pub fn problems(&self, request: &CatalogRequest) -> Vec<CatalogProblem> {
        request
            .tables()
            .iter()
            .filter_map(|t| {
                let table = t.table.clone();
                match &self.table(&t.table)?.status {
                    TableStatus::Readable(relation) if relation.missing_columns.is_empty() => None,
                    TableStatus::Readable(relation) => Some(CatalogProblem::MissingColumns {
                        table,
                        columns: relation.missing_columns.clone(),
                    }),
                    TableStatus::Missing => Some(CatalogProblem::MissingTable(table)),
                    TableStatus::Unreadable(message) => Some(CatalogProblem::Unreadable {
                        table,
                        message: message.clone(),
                    }),
                }
            })
            .collect()
    }
}

/// The catalog entry of one logical table.
#[derive(Debug, Clone, PartialEq)]
pub struct TableCatalog {
    /// Changes whenever the catalog rows the entry was read from change.
    /// `None` for an `rr:sqlQuery`, whose dependencies the catalog does
    /// not list.
    pub fingerprint: Option<Fingerprint>,
    pub status: TableStatus,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TableStatus {
    Readable(Relation),
    /// An `rr:tableName` that names no table or view.
    Missing,
    /// The database refuses to read the logical table, with its message:
    /// an `rr:sqlQuery` that does not compile, or a table the user may
    /// not read.
    Unreadable(String),
}

/// A logical table the database can read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Relation {
    /// The requested columns that exist, by the name the mapping writes.
    pub columns: HashMap<String, Column>,
    /// The requested columns that do not exist, in request order.
    pub missing_columns: Vec<String>,
    /// Unique keys that hold for every row at any time; none for an
    /// `rr:sqlQuery`.
    pub unique_keys: Vec<UniqueKey>,
    /// Validated foreign keys; none for an `rr:sqlQuery`.
    pub foreign_keys: Vec<ForeignKey>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    /// As the database stores it: a name the mapping writes without
    /// quotes is the name the database resolved it to.
    pub name: String,
    pub ty: ColumnType,
    /// Always false for a column of an `rr:sqlQuery`.
    pub not_null: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UniqueKey {
    /// As the database stores them.
    pub columns: Vec<String>,
    /// The type of each column; `None` for a type the adapter does not
    /// describe.
    pub types: Vec<Option<ColumnType>>,
    /// Whether the index decides equality as the SQL `=` of the plan does:
    /// every column is `NOT NULL` (a unique index allows several `NULL`s)
    /// and compared with its own collation and the type's default
    /// operator class.
    pub exact_index: bool,
}

impl UniqueKey {
    /// Whether two rows that differ in the key give different terms. A
    /// column whose distinct values can give the same term
    /// (`ColumnType::term_normal`) breaks that.
    pub fn separates_terms(&self) -> bool {
        self.types
            .iter()
            .flatten()
            .all(|ty| ty.term_normal.is_none())
    }

    /// Whether two scans of the table that agree on the key read the same
    /// row: the index is exact and every type compares by `=` without a
    /// cast or a term key.
    pub fn identifies_rows(&self) -> bool {
        self.separates_terms()
            && self.exact_index
            && self.types.iter().all(|ty| {
                ty.as_ref()
                    .is_some_and(|ty| ty.comparison == Comparison::Value && ty.term_key.is_none())
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKey {
    /// As the database stores them, in key order.
    pub columns: Vec<String>,
    /// The referenced table, schema-qualified where the search path does
    /// not find it.
    pub referenced_table: String,
    pub referenced_columns: Vec<String>,
}

/// A hash of catalog rows, computed by the database.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Fingerprint(String);

impl Fingerprint {
    pub fn new(hash: String) -> Self {
        Self(hash)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Something the mapping names that the database lacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogProblem {
    MissingTable(LogicalTable),
    MissingColumns {
        table: LogicalTable,
        columns: Vec<String>,
    },
    Unreadable {
        table: LogicalTable,
        message: String,
    },
}

impl CatalogProblem {
    pub fn table(&self) -> &LogicalTable {
        match self {
            Self::MissingTable(table)
            | Self::MissingColumns { table, .. }
            | Self::Unreadable { table, .. } => table,
        }
    }
}

impl fmt::Display for CatalogProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let table = describe_table(self.table());
        match self {
            Self::MissingTable(_) => write!(f, "{table} does not exist"),
            Self::MissingColumns { columns, .. } => {
                write!(f, "{table} has no column {}", columns.join(", "))
            }
            Self::Unreadable { message, .. } => write!(f, "{table} cannot be read: {message}"),
        }
    }
}

#[cfg(test)]
mod tests;
