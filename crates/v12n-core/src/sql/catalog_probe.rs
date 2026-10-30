// SPDX-License-Identifier: Apache-2.0
//! Statements prepared, not run, so that the database reports the types
//! and resolved names of columns.

use crate::mapping::LogicalTable;

use super::identifier::logical_table;
use super::{folded_identifier, quote_identifier};

/// A statement that selects `columns` of one logical table, to prepare
/// (not run) so that the database reports their types and the names it
/// resolved: a [`folded_identifier`] is written without quotes, so the
/// database resolves it as it resolves a regular identifier. The statement
/// names the table even when `columns` is empty, so preparing it shows
/// whether the database can read the table at all.
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

#[cfg(test)]
mod tests {
    use super::*;

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
