// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

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
    let readable = |found: &[&str]| TableCatalog {
        fingerprint: Some(Fingerprint::new("f".into())),
        status: TableStatus::Readable(Relation {
            columns: found
                .iter()
                .map(|name| {
                    let column = Column {
                        name: (*name).to_owned(),
                        ty: column_type(Comparison::Value),
                        not_null: false,
                        deterministic: true,
                    };
                    ((*name).to_owned(), column)
                })
                .collect(),
            ..Relation::default()
        }),
    };
    let snapshot = CatalogSnapshot::new([
        (table("orders"), readable(&["id", "customer_id"])),
        (table("customers"), readable(&["id", "name", "tier"])),
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

fn shop_plan(query: &str) -> SqlPlan {
    let mapping = Mapping::from_turtle(SHOP).unwrap();
    let index = crate::mapping::MappingIndex::new(&mapping);
    crate::unfold::unfold(&crate::sparql::parse(query).unwrap(), &mapping, &index).unwrap()
}

const NAMES: &str = "SELECT ?c ?n WHERE { ?c <http://example.org/name> ?n }";

fn customers(columns: &[&str], keys: Vec<UniqueKey>) -> (LogicalTable, TableCatalog) {
    let columns = columns
        .iter()
        .map(|name| {
            let column = Column {
                name: (*name).to_owned(),
                ty: column_type(Comparison::Value),
                not_null: true,
                deterministic: true,
            };
            ((*name).to_owned(), column)
        })
        .collect();
    let entry = TableCatalog {
        fingerprint: Some(Fingerprint::new("f".into())),
        status: TableStatus::Readable(Relation {
            columns,
            unique_keys: keys,
            ..Relation::default()
        }),
    };
    (table("customers"), entry)
}

#[test]
fn a_plan_needs_every_table_and_column_it_reads() {
    let plan = shop_plan(NAMES);
    let unread = CatalogPlan::new(&plan, Arc::default()).unwrap_err();
    assert_eq!(
        unread,
        crate::Error::Catalog("table customers was not read from the catalog".into())
    );
    let snapshot = CatalogSnapshot::new([customers(&["id"], Vec::new())]);
    assert!(!snapshot.covers(&plan));
    let missing = CatalogPlan::new(&plan, Arc::new(snapshot)).unwrap_err();
    assert_eq!(
        missing,
        crate::Error::Catalog("table customers has no column name".into())
    );
}

#[test]
fn keys_from_the_snapshot_drop_duplicate_removal() {
    let plan = shop_plan(NAMES);
    let without = CatalogSnapshot::new([customers(&["id", "name"], Vec::new())]);
    let ready = CatalogPlan::new(&plan, Arc::new(without)).unwrap();
    assert!(ready.plan().has_duplicates(ready.render_options().keys));

    let id = key(vec![Some(column_type(Comparison::Value))], true);
    let id = UniqueKey {
        columns: names(&["id"]),
        ..id
    };
    let with = CatalogSnapshot::new([customers(&["id", "name"], vec![id])]);
    let ready = CatalogPlan::new(&plan, Arc::new(with)).unwrap();
    assert!(!ready.plan().has_duplicates(ready.render_options().keys));
    assert_eq!(
        ready.entries().map(|(t, _)| t.clone()).collect::<Vec<_>>(),
        [table("customers")]
    );
}

#[test]
fn reading_tables_again_keeps_the_columns_other_plans_need() {
    let catalog = Catalog::new(CatalogSnapshot::new([customers(
        &["id", "name", "tier"],
        Vec::new(),
    )]));
    let snapshot = catalog.snapshot();
    let request = snapshot.request_for(&shop_plan(NAMES));
    let [customers_request] = request.tables() else {
        panic!("{request:?}");
    };
    let mut columns = customers_request.columns.clone();
    columns.sort();
    assert_eq!(columns, names(&["id", "name", "tier"]));

    let replaced = catalog.update(CatalogSnapshot::new([customers(&["id"], Vec::new())]));
    assert!(Arc::ptr_eq(&replaced, &catalog.snapshot()));
    assert!(!replaced.covers(&shop_plan(NAMES)));
    assert!(
        snapshot.covers(&shop_plan(NAMES)),
        "a running query keeps its snapshot"
    );
}

const BUYERS: &str = "SELECT ?o ?c WHERE { ?o <http://example.org/placedBy> ?c . \
                      ?c a <http://example.org/Customer> }";

fn int(name: &str) -> ColumnType {
    ColumnType {
        name: name.into(),
        sql_type: format!("\"pg_catalog\".\"{name}\""),
        ..column_type(Comparison::Value)
    }
}

fn entry(columns: &[(&str, ColumnType, bool)], relation: Relation) -> TableCatalog {
    let columns = columns
        .iter()
        .map(|(name, ty, not_null)| {
            let column = Column {
                name: (*name).to_owned(),
                ty: ty.clone(),
                not_null: *not_null,
                deterministic: true,
            };
            ((*name).to_owned(), column)
        })
        .collect();
    TableCatalog {
        fingerprint: Some(Fingerprint::new("f".into())),
        status: TableStatus::Readable(Relation {
            columns,
            ..relation
        }),
    }
}

fn id_key(exact_index: bool) -> UniqueKey {
    UniqueKey {
        columns: names(&["id"]),
        types: vec![Some(int("int4"))],
        exact_index,
    }
}

/// How the shop's orders reference their customers in the snapshot.
struct Reference {
    referenced_table: &'static str,
    not_null: bool,
    child_type: &'static str,
    deterministic: bool,
    parent_keys: Vec<UniqueKey>,
}

impl Default for Reference {
    fn default() -> Self {
        Self {
            referenced_table: "customers",
            not_null: true,
            child_type: "int4",
            deterministic: true,
            parent_keys: vec![id_key(true)],
        }
    }
}

/// The tables `BUYERS` scans once its plan is completed from `reference`.
fn scanned(reference: Reference) -> Vec<LogicalTable> {
    let mut orders = entry(
        &[
            ("id", int("int4"), true),
            ("customer_id", int(reference.child_type), reference.not_null),
        ],
        Relation {
            unique_keys: vec![id_key(true)],
            foreign_keys: vec![ForeignKey {
                columns: names(&["customer_id"]),
                referenced_table: reference.referenced_table.into(),
                referenced_columns: names(&["id"]),
            }],
            ..Relation::default()
        },
    );
    if let TableStatus::Readable(relation) = &mut orders.status {
        relation
            .columns
            .get_mut("customer_id")
            .expect("the column is in the entry")
            .deterministic = reference.deterministic;
    }
    let customers = entry(
        &[("id", int("int4"), true)],
        Relation {
            unique_keys: reference.parent_keys,
            ..Relation::default()
        },
    );
    let snapshot =
        CatalogSnapshot::new([(table("orders"), orders), (table("customers"), customers)]);
    let ready = CatalogPlan::new(&shop_plan(BUYERS), Arc::new(snapshot)).unwrap();
    assert_eq!(
        ready.entries().map(|(t, _)| t.clone()).collect::<Vec<_>>(),
        [table("orders"), table("customers")],
        "the entries that justify the plan are checked"
    );
    ready.plan().branches[0].from.clone()
}

#[test]
fn foreign_key_on_not_null_columns_removes_the_parent_scan() {
    assert_eq!(scanned(Reference::default()), [table("orders")]);
    let quoted = Reference {
        referenced_table: "\"customers\"",
        ..Reference::default()
    };
    assert_eq!(scanned(quoted), [table("orders")]);
}

#[test]
fn foreign_key_that_cannot_vouch_for_one_row_keeps_the_parent_scan() {
    let both = [table("orders"), table("customers")];
    let cases = [
        Reference {
            not_null: false,
            ..Reference::default()
        },
        // `=` under a nondeterministic collation finds another string equal.
        Reference {
            deterministic: false,
            ..Reference::default()
        },
        Reference {
            child_type: "int8",
            ..Reference::default()
        },
        Reference {
            parent_keys: Vec::new(),
            ..Reference::default()
        },
        Reference {
            parent_keys: vec![id_key(false)],
            ..Reference::default()
        },
        // Either index may be the one the key checks against.
        Reference {
            parent_keys: vec![id_key(true), id_key(false)],
            ..Reference::default()
        },
        // Another table, or the same one under a name the plan does not use.
        Reference {
            referenced_table: "shop.customers",
            ..Reference::default()
        },
    ];
    for case in cases {
        assert_eq!(scanned(case), both);
    }
}
