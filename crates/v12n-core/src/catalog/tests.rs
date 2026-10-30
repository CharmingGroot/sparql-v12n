// SPDX-License-Identifier: Apache-2.0

use crate::sql::{TermKey, TermNormal};

use super::*;

const SHOP: &str = include_str!("../../../../examples/shop/mapping.ttl");

fn table(name: &str) -> LogicalTable {
    LogicalTable::TableName(name.into())
}

fn request(mapping: &str) -> Vec<(LogicalTable, Vec<String>)> {
    CatalogRequest::for_mapping(&Mapping::from_turtle(mapping).unwrap())
        .tables()
        .iter()
        .map(|t| (t.table.clone(), t.columns.clone()))
        .collect()
}

fn names(columns: &[&str]) -> Vec<String> {
    columns.iter().map(|c| (*c).to_owned()).collect()
}

#[test]
fn request_lists_the_columns_of_every_term_map_once() {
    assert_eq!(
        request(SHOP),
        vec![
            (table("customers"), names(&["id", "name", "tier"])),
            (table("orders"), names(&["id", "amount", "customer_id"])),
        ]
    );
}

#[test]
fn request_reads_join_columns_from_child_and_parent_tables() {
    let mapping = r#"
        @prefix rr: <http://www.w3.org/ns/r2rml#> .
        @prefix ex: <http://example.org/> .
        <#Parent> rr:logicalTable [ rr:tableName "parent" ] ;
          rr:subjectMap [ rr:template "p/{ID}" ] .
        <#Child> rr:logicalTable [ rr:sqlQuery "SELECT 1 AS pid" ] ;
          rr:subjectMap [ rr:constant ex:only ] ;
          rr:predicateObjectMap [
            rr:predicate ex:parent ;
            rr:objectMap [ rr:parentTriplesMap <#Parent> ;
              rr:joinCondition [ rr:child "pid" ; rr:parent "\"Code\"" ] ]
          ] .
    "#;
    assert_eq!(
        request(mapping),
        vec![
            (table("parent"), names(&["ID", "\"Code\""])),
            (
                LogicalTable::SqlQuery("SELECT 1 AS pid".into()),
                names(&["pid"])
            ),
        ]
    );
}

#[test]
fn request_keeps_a_table_whose_terms_are_constants() {
    let mapping = r#"
        @prefix rr: <http://www.w3.org/ns/r2rml#> .
        @prefix ex: <http://example.org/> .
        <#Only> rr:logicalTable [ rr:tableName "t" ] ;
          rr:subjectMap [ rr:constant ex:s ] ;
          rr:predicateObjectMap [ rr:predicate ex:p ; rr:object "o" ] .
    "#;
    assert_eq!(request(mapping), vec![(table("t"), Vec::new())]);
}

fn column_type(comparison: Comparison) -> ColumnType {
    ColumnType {
        name: "int4".into(),
        sql_type: "\"pg_catalog\".\"int4\"".into(),
        union_class: "integer".into(),
        natural_datatype: None,
        group: "integer".into(),
        comparison,
        lexical: None,
        lexical_unreserved: true,
        term_key: None,
        term_normal: None,
    }
}

fn key(types: Vec<Option<ColumnType>>, exact_index: bool) -> UniqueKey {
    UniqueKey {
        columns: (0..types.len()).map(|i| format!("c{i}")).collect(),
        types,
        exact_index,
    }
}

#[test]
fn a_key_identifies_rows_only_with_an_exact_index_and_exact_types() {
    let int = Some(column_type(Comparison::Value));
    assert!(key(vec![int.clone()], true).identifies_rows());
    assert!(!key(vec![int.clone()], false).identifies_rows());
    assert!(!key(vec![Some(column_type(Comparison::Text))], true).identifies_rows());
    assert!(!key(vec![None], true).identifies_rows());
    let keyed = ColumnType {
        term_key: Some(TermKey::OctetLength),
        ..column_type(Comparison::Value)
    };
    assert!(!key(vec![int, Some(keyed)], true).identifies_rows());
}

#[test]
fn a_key_with_a_normalized_column_does_not_separate_terms() {
    let time = ColumnType {
        term_normal: Some(TermNormal::Time),
        ..column_type(Comparison::Value)
    };
    let with_time = key(vec![Some(column_type(Comparison::Value)), Some(time)], true);
    assert!(!with_time.separates_terms());
    assert!(!with_time.identifies_rows());
    assert!(key(vec![None], false).separates_terms());
}

#[test]
fn problems_follow_the_request_order() {
    let request = CatalogRequest::for_mapping(&Mapping::from_turtle(SHOP).unwrap());
    let readable = |missing: &[&str]| TableCatalog {
        fingerprint: Some(Fingerprint::new("f".into())),
        status: TableStatus::Readable(Relation {
            missing_columns: names(missing),
            ..Relation::default()
        }),
    };
    let snapshot = CatalogSnapshot::new([
        (table("orders"), readable(&["amount"])),
        (table("customers"), readable(&[])),
    ]);
    assert_eq!(
        snapshot
            .problems(&request)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["table orders has no column amount"]
    );

    let snapshot = CatalogSnapshot::new([
        (
            table("customers"),
            TableCatalog {
                fingerprint: Some(Fingerprint::new("f".into())),
                status: TableStatus::Missing,
            },
        ),
        (
            table("orders"),
            TableCatalog {
                fingerprint: Some(Fingerprint::new("g".into())),
                status: TableStatus::Unreadable("permission denied".into()),
            },
        ),
    ]);
    assert_eq!(
        snapshot
            .problems(&request)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        [
            "table customers does not exist",
            "table orders cannot be read: permission denied"
        ]
    );
}
