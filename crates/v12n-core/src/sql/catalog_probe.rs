// SPDX-License-Identifier: Apache-2.0
//! Statements prepared, not run, so that the database reports the types
//! and resolved names of columns.

use crate::mapping::LogicalTable;

use super::identifier::logical_table;
use super::{folded_identifier, quote_identifier};

/// A statement whose result columns have the types of `columns`, to
/// prepare (not run) so the database reports them. Each logical table
/// appears once.
pub fn column_type_probe(columns: &[(LogicalTable, String)]) -> String {
    probe(columns, quote_identifier)
}

/// A statement that refers to each of `columns`, all
/// [`folded_identifier`]s, as written, without quotes, to prepare (not
/// run): the database then resolves each one as it resolves a regular
/// identifier and reports the name it found as the result column's name.
/// Each logical table appears once.
pub fn column_name_probe(columns: &[(LogicalTable, String)]) -> String {
    assert!(
        columns.iter().all(|(_, name)| folded_identifier(name)),
        "only regular identifiers may appear unquoted"
    );
    probe(columns, str::to_owned)
}

/// One statement that does the work of [`column_name_probe`] for the
/// [`folded_identifier`]s among `columns` and of [`column_type_probe`] for
/// the others: each result column has the type of its column, and those
/// of folded names are named as the database resolved them. Each logical
/// table appears once.
pub fn column_probe(columns: &[(LogicalTable, String)]) -> String {
    probe(columns, |name| {
        if folded_identifier(name) {
            name.to_owned()
        } else {
            quote_identifier(name)
        }
    })
}

/// [`column_probe`] for `columns` of one logical table, which also names
/// the table when `columns` is empty, so preparing it shows whether the
/// database can read the table at all.
///
/// The columns are not qualified by the table: PostgreSQL reads `p0.f`
/// as the function call `f(p0)` when there is no column `f`, which turns a
/// missing column into an error about a function.
pub fn table_probe(table: &LogicalTable, columns: &[String]) -> String {
    let select: Vec<String> = columns
        .iter()
        .map(|name| {
            if folded_identifier(name) {
                name.clone()
            } else {
                quote_identifier(name)
            }
        })
        .collect();
    format!(
        "SELECT {}\nFROM {} p0\nLIMIT 0",
        select.join(", "),
        logical_table(table)
    )
}

fn probe(columns: &[(LogicalTable, String)], name_sql: impl Fn(&str) -> String) -> String {
    let mut tables: Vec<&LogicalTable> = Vec::new();
    let select: Vec<String> = columns
        .iter()
        .map(|(table, name)| {
            let position = tables.iter().position(|t| *t == table).unwrap_or_else(|| {
                tables.push(table);
                tables.len() - 1
            });
            format!("p{position}.{}", name_sql(name))
        })
        .collect();
    let from: Vec<String> = tables
        .iter()
        .enumerate()
        .map(|(i, table)| format!("{} p{i}", logical_table(table)))
        .collect();
    format!(
        "SELECT {}\nFROM {}\nLIMIT 0",
        select.join(", "),
        from.join("\nCROSS JOIN ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_name_probe_leaves_regular_identifiers_unquoted() {
        let view = LogicalTable::SqlQuery("SELECT 1 AS Id".into());
        let table = LogicalTable::TableName("t".into());
        assert_eq!(
            column_name_probe(&[
                (view.clone(), "Id".into()),
                (table, "Name".into()),
                (view, "LABEL".into()),
            ]),
            "SELECT p0.Id, p1.Name, p0.LABEL\nFROM (SELECT 1 AS Id) p0\nCROSS JOIN \"t\" p1\nLIMIT 0"
        );
    }

    #[test]
    fn column_probe_quotes_all_but_folded_identifiers() {
        let view = LogicalTable::SqlQuery("SELECT 1 AS Id".into());
        let table = LogicalTable::TableName("t".into());
        assert_eq!(
            column_probe(&[
                (view.clone(), "Id".into()),
                (table.clone(), "a; DROP".into()),
                (table, "name".into()),
                (view, "\"V\"".into()),
            ]),
            "SELECT p0.Id, p1.\"a; DROP\", p1.\"name\", p0.\"V\"\nFROM (SELECT 1 AS Id) p0\nCROSS JOIN \"t\" p1\nLIMIT 0"
        );
    }

    #[test]
    #[should_panic(expected = "only regular identifiers")]
    fn column_name_probe_refuses_other_names() {
        column_name_probe(&[(LogicalTable::TableName("t".into()), "a; DROP".into())]);
    }

    #[test]
    fn table_probe_names_the_table_without_columns() {
        let table = LogicalTable::TableName("s.t".into());
        assert_eq!(
            table_probe(&table, &[]),
            "SELECT \nFROM \"s\".\"t\" p0\nLIMIT 0"
        );
        assert_eq!(
            table_probe(&table, &["Id".into(), "v".into()]),
            "SELECT Id, \"v\"\nFROM \"s\".\"t\" p0\nLIMIT 0"
        );
    }
}
