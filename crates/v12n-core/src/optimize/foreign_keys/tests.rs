// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::dialect::PostgresDialect;
use crate::mapping::{Mapping, MappingIndex};
use crate::optimize::eliminate_self_joins;
use crate::sql::{RenderOptions, TableKeys};
use crate::{sparql, unfold};

const MAPPING: &str = r#"@prefix rr: <http://www.w3.org/ns/r2rml#> .
@prefix ex: <http://example.org/> .

<#Customer> rr:logicalTable [ rr:tableName "customers" ] ;
  rr:subjectMap [ rr:template "http://example.org/customer/{id}" ; rr:class ex:Customer ] ;
  rr:predicateObjectMap [ rr:predicate ex:name ; rr:objectMap [ rr:column "name" ] ] .

<#Order> rr:logicalTable [ rr:tableName "orders" ] ;
  rr:subjectMap [ rr:template "http://example.org/order/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:amount ; rr:objectMap [ rr:column "amount" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:placedBy ;
    rr:objectMap [ rr:parentTriplesMap <#Customer> ;
      rr:joinCondition [ rr:child "customer_id" ; rr:parent "id" ] ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:buyer ;
    rr:objectMap [ rr:template "http://example.org/customer/{customer_id}" ] ] .

<#Line> rr:logicalTable [ rr:tableName "lines" ] ;
  rr:subjectMap [ rr:template "http://example.org/line/{order_id}/{line_no}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:item ; rr:objectMap [ rr:column "item" ] ] .

<#Shipment> rr:logicalTable [ rr:tableName "shipments" ] ;
  rr:subjectMap [ rr:template "http://example.org/shipment/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:line ;
    rr:objectMap [ rr:parentTriplesMap <#Line> ;
      rr:joinCondition [ rr:child "order_id" ; rr:parent "order_id" ] ,
        [ rr:child "line_no" ; rr:parent "line_no" ] ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:swapped ;
    rr:objectMap [ rr:parentTriplesMap <#Line> ;
      rr:joinCondition [ rr:child "order_id" ; rr:parent "line_no" ] ,
        [ rr:child "line_no" ; rr:parent "order_id" ] ] ] .
"#;

fn plan(query: &str) -> SqlPlan {
    let mapping = Mapping::from_turtle(MAPPING).unwrap();
    let query = sparql::parse(&format!("PREFIX ex: <http://example.org/>\n{query}")).unwrap();
    unfold::unfold(&query, &mapping, &MappingIndex::new(&mapping)).unwrap()
}

fn table(name: &str) -> LogicalTable {
    LogicalTable::TableName(name.into())
}

fn names(columns: &[&str]) -> Vec<String> {
    columns.iter().map(|c| (*c).to_owned()).collect()
}

/// `orders.customer_id` references `customers.id`, and
/// `shipments (order_id, line_no)` references `lines (order_id, line_no)`.
fn keys() -> Vec<ForeignKeyJoin> {
    vec![
        ForeignKeyJoin {
            child: table("orders"),
            columns: names(&["customer_id"]),
            parent: table("customers"),
            parent_columns: names(&["id"]),
        },
        ForeignKeyJoin {
            child: table("shipments"),
            columns: names(&["order_id", "line_no"]),
            parent: table("lines"),
            parent_columns: names(&["order_id", "line_no"]),
        },
    ]
}

fn primary_keys() -> TableKeys {
    [
        ("customers", vec!["id"]),
        ("orders", vec!["id"]),
        ("lines", vec!["order_id", "line_no"]),
        ("shipments", vec!["id"]),
    ]
    .into_iter()
    .map(|(t, key)| (table(t), vec![names(&key)]))
    .collect()
}

fn reduced(query: &str) -> SqlPlan {
    eliminate_foreign_key_joins(&plan(query), &keys()).expect("a parent scan is removed")
}

fn tables(branch: &SqlBranch) -> Vec<&str> {
    branch
        .from
        .iter()
        .map(|t| match t {
            LogicalTable::TableName(name) => name.as_str(),
            LogicalTable::SqlQuery(_) => "(query)",
        })
        .collect()
}

fn sql(plan: &SqlPlan) -> String {
    let keys = primary_keys();
    plan.to_sql_with(
        &PostgresDialect,
        &RenderOptions {
            keys: Some(&keys),
            ..RenderOptions::default()
        },
    )
    .unwrap()
    .sql
}

#[test]
fn parent_that_only_gives_its_subject_is_not_scanned() {
    let original = plan("SELECT ?o ?c WHERE { ?o ex:placedBy ?c }");
    assert_eq!(tables(&original.branches[0]), ["orders", "customers"]);
    let plan = reduced("SELECT ?o ?c WHERE { ?o ex:placedBy ?c }");
    let [branch] = plan.branches.as_slice() else {
        panic!("one branch");
    };
    assert_eq!(tables(branch), ["orders"]);
    assert_eq!(
        sql(&plan),
        "SELECT t0.\"id\" AS \"o\", t0.\"customer_id\" AS \"c\"\n\
         FROM \"orders\" t0\n\
         WHERE t0.\"id\" IS NOT NULL"
    );
    assert!(branch.fixed_columns[0].contains("customer_id"));
    assert!(!plan.has_duplicates(Some(&primary_keys())));
}

#[test]
fn class_of_the_referenced_row_is_not_scanned() {
    let query = "SELECT ?o ?c WHERE { ?o ex:buyer ?c . ?c a ex:Customer }";
    assert_eq!(
        tables(&self::plan(query).branches[0]),
        ["orders", "customers"]
    );
    let plan = reduced(query);
    assert_eq!(tables(&plan.branches[0]), ["orders"]);
    let sql = sql(&plan);
    assert!(!sql.contains("t1"), "{sql}");
}

/// Once merged with the scan of `?c ex:name ?n`, the parent reads `name`,
/// which the referencing row does not have.
#[test]
fn parent_read_for_another_column_is_kept() {
    let plan = plan("SELECT ?o ?n WHERE { ?o ex:placedBy ?c . ?c ex:name ?n }");
    let merged = eliminate_self_joins(&plan, &primary_keys()).expect("the parent scans merge");
    assert_eq!(tables(&merged.branches[0]), ["orders", "customers"]);
    assert_eq!(eliminate_foreign_key_joins(&merged, &keys()), None);
}

#[test]
fn composite_key_is_matched_column_by_column() {
    let plan = reduced("SELECT ?s ?l WHERE { ?s ex:line ?l }");
    assert_eq!(tables(&plan.branches[0]), ["shipments"]);
    let sql = sql(&plan);
    assert!(
        sql.contains("t0.\"order_id\"") && sql.contains("t0.\"line_no\"") && !sql.contains("t1"),
        "{sql}"
    );
    // The join pairs each column with the other referenced one: a line
    // whose order number is the shipment's line number.
    let swapped = self::plan("SELECT ?s ?l WHERE { ?s ex:swapped ?l }");
    assert_eq!(eliminate_foreign_key_joins(&swapped, &keys()), None);
}

#[test]
fn join_without_a_matching_key_is_kept() {
    let plan = plan("SELECT ?o ?c WHERE { ?o ex:placedBy ?c }");
    assert_eq!(eliminate_foreign_key_joins(&plan, &[]), None);
    let other_column = ForeignKeyJoin {
        columns: names(&["amount"]),
        ..keys()[0].clone()
    };
    assert_eq!(eliminate_foreign_key_joins(&plan, &[other_column]), None);
    let other_table = ForeignKeyJoin {
        parent: table("lines"),
        ..keys()[0].clone()
    };
    assert_eq!(eliminate_foreign_key_joins(&plan, &[other_table]), None);
}

/// Both scans are in the part, which keeps its rows where the parent
/// scan is gone, and its `NULL`s where it does not match.
#[test]
fn parent_in_the_optional_part_of_its_child_is_not_scanned() {
    let plan = reduced("SELECT ?c ?n ?o WHERE { ?c ex:name ?n OPTIONAL { ?o ex:placedBy ?c } }");
    let [branch] = plan.branches.as_slice() else {
        panic!("one branch");
    };
    assert_eq!(tables(branch), ["customers", "orders"]);
    assert_eq!(branch.left_joins[0].tables, 1..2);
    assert_eq!(
        sql(&plan),
        "SELECT t0.\"id\" AS \"c\", t0.\"name\" AS \"n\", t1.\"id\" AS \"o\"\n\
         FROM \"customers\" t0\n\
         LEFT JOIN \"orders\" t1 ON t1.\"customer_id\" = t0.\"id\" AND t1.\"id\" IS NOT NULL\n\
         WHERE t0.\"id\" IS NOT NULL AND t0.\"name\" IS NOT NULL"
    );
}

/// Across `OPTIONAL`, the parent scan tells whether the part matched.
#[test]
fn parent_across_optional_is_kept() {
    let plan = plan("SELECT ?o ?c WHERE { ?o ex:buyer ?c OPTIONAL { ?c a ex:Customer } }");
    assert_eq!(tables(&plan.branches[0]), ["orders", "customers"]);
    assert_eq!(plan.branches[0].left_joins[0].tables, 1..2);
    assert_eq!(eliminate_foreign_key_joins(&plan, &keys()), None);
}

#[test]
fn filter_and_order_read_the_child_column() {
    let plan = reduced(
        "SELECT ?o ?c WHERE { ?o ex:placedBy ?c \
         FILTER(?c != <http://example.org/customer/1>) } ORDER BY DESC(?c) LIMIT 5",
    );
    let sql = sql(&plan);
    assert!(!sql.contains("t1"), "{sql}");
    assert!(sql.contains("t0.\"customer_id\""), "{sql}");
}

#[test]
fn union_arms_are_reduced_each_on_its_own() {
    let plan = reduced(
        "SELECT * WHERE { { ?o ex:placedBy ?c } UNION { ?c ex:name ?n } UNION { ?s ex:line ?l } }",
    );
    let found: Vec<Vec<&str>> = plan.branches.iter().map(tables).collect();
    assert_eq!(
        found,
        [vec!["orders"], vec!["customers"], vec!["shipments"]]
    );
}
