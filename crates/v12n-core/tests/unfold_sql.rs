// SPDX-License-Identifier: Apache-2.0
//! Snapshots of the SQL generated for example queries.

use v12n_core::mapping::{LogicalTable, Mapping, MappingIndex};
use v12n_core::sql::{RenderOptions, TableKeys};
use v12n_core::{sparql, unfold};

const SHOP: &str = include_str!("../../../examples/shop/mapping.ttl");
const VIP_ORDERS: &str = include_str!("../../../examples/shop/queries/vip-orders.rq");

const PREFIXES: &str = "PREFIX ex: <http://example.org/>\n";

fn explain(mapping: &str, query: &str) -> String {
    explain_with_keys(mapping, query, None)
}

fn explain_with_keys(mapping: &str, query: &str, keys: Option<&TableKeys>) -> String {
    let mapping = Mapping::from_turtle(mapping).unwrap();
    let index = MappingIndex::new(&mapping);
    let query = sparql::parse(query).unwrap();
    let plan = unfold::unfold(&query, &mapping, &index).unwrap();
    let rendered = plan
        .to_sql_with(&RenderOptions {
            keys,
            ..RenderOptions::default()
        })
        .unwrap();
    let mut out = rendered.sql;
    out.push_str("\n\n-- params\n");
    for (i, p) in rendered.params.iter().enumerate() {
        out.push_str(&format!("${} = {p:?}\n", i + 1));
    }
    out
}

fn shop(body: &str) -> String {
    explain(SHOP, &format!("{PREFIXES}{body}"))
}

#[test]
fn shop_class_pattern() {
    insta::assert_snapshot!(shop("SELECT ?c WHERE { ?c a ex:Customer }"));
}

#[test]
fn shop_constant_literal_object() {
    insta::assert_snapshot!(shop(r#"SELECT ?c WHERE { ?c ex:tier "VIP" }"#));
}

#[test]
fn shop_vip_orders_join_decomposes_templates() {
    insta::assert_snapshot!(explain(SHOP, VIP_ORDERS));
}

/// With the primary keys of `customers` and `orders`, every solution fixes
/// one row of each scan, so no duplicates need removing.
#[test]
fn shop_vip_orders_with_keys_keeps_rows() {
    let keys: TableKeys = ["customers", "orders"]
        .into_iter()
        .map(|t| {
            (
                LogicalTable::TableName(t.into()),
                vec![vec!["id".to_owned()]],
            )
        })
        .collect();
    insta::assert_snapshot!(explain_with_keys(SHOP, VIP_ORDERS, Some(&keys)));
}

#[test]
fn shop_constant_subject_iri() {
    insta::assert_snapshot!(shop(
        "SELECT ?name WHERE { <http://example.org/customer/7> ex:name ?name }"
    ));
}

#[test]
fn shop_distinct() {
    insta::assert_snapshot!(shop("SELECT DISTINCT ?c WHERE { ?o ex:placedBy ?c }"));
}

const MULTI: &str = r#"
@prefix rr: <http://www.w3.org/ns/r2rml#> .
@prefix ex: <http://example.org/> .

<#Retail> rr:logicalTable [ rr:tableName "retail_customers" ] ;
  rr:subjectMap [ rr:template "http://example.org/customer/{id}" ; rr:class ex:Customer ] ;
  rr:predicateObjectMap [ rr:predicate ex:name ; rr:objectMap [ rr:column "name" ] ] .

<#Wholesale> rr:logicalTable [ rr:sqlQuery "SELECT code, title FROM shop.wholesale" ] ;
  rr:subjectMap [ rr:template "http://example.org/customer/{code}" ; rr:class ex:Customer ] ;
  rr:predicateObjectMap [ rr:predicate ex:name ; rr:objectMap [ rr:column "title" ] ] .

<#Order> rr:logicalTable [ rr:tableName "orders" ] ;
  rr:subjectMap [ rr:template "http://example.org/order/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:placedBy ;
    rr:objectMap [ rr:parentTriplesMap <#Retail> ;
      rr:joinCondition [ rr:child "customer_id" ; rr:parent "id" ] ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:homepage ; rr:objectMap [ rr:column "url" ; rr:termType rr:IRI ] ] .
"#;

#[test]
fn multiple_triples_maps_union() {
    insta::assert_snapshot!(explain(
        MULTI,
        &format!("{PREFIXES}SELECT ?c ?name WHERE {{ ?c a ex:Customer ; ex:name ?name }}")
    ));
}

#[test]
fn referencing_object_map_joins_parent() {
    insta::assert_snapshot!(explain(
        MULTI,
        &format!("{PREFIXES}SELECT ?o ?name WHERE {{ ?o ex:placedBy ?c . ?c ex:name ?name }}")
    ));
}

#[test]
fn column_against_template_compares_strings() {
    insta::assert_snapshot!(explain(
        MULTI,
        &format!("{PREFIXES}SELECT ?o WHERE {{ ?o ex:homepage ?c . ?c ex:name ?name }}")
    ));
}

const AMBIGUOUS: &str = r#"
@prefix rr: <http://www.w3.org/ns/r2rml#> .
@prefix ex: <http://example.org/> .

<#Pair> rr:logicalTable [ rr:tableName "pairs" ] ;
  rr:subjectMap [ rr:template "http://example.org/pair/{a}-{b}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:label ; rr:objectMap [ rr:column "label" ] ] .

<#Ref> rr:logicalTable [ rr:tableName "refs" ] ;
  rr:subjectMap [ rr:template "http://example.org/ref/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:points ;
    rr:objectMap [ rr:template "http://example.org/pair/{code}" ] ] .
"#;

#[test]
fn ambiguous_templates_compare_iri_safe_strings() {
    insta::assert_snapshot!(explain(
        AMBIGUOUS,
        &format!("{PREFIXES}SELECT ?r ?l WHERE {{ ?r ex:points ?p . ?p ex:label ?l }}")
    ));
}

#[test]
fn constant_against_ambiguous_template_compares_iri_safe_strings() {
    insta::assert_snapshot!(explain(
        AMBIGUOUS,
        &format!("{PREFIXES}SELECT ?l WHERE {{ <http://example.org/pair/x%20y-z> ex:label ?l }}")
    ));
}
