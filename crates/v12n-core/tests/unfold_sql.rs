// SPDX-License-Identifier: Apache-2.0
//! Snapshots of the SQL generated for example queries.

use v12n_core::mapping::{BaseIri, LogicalTable, Mapping, MappingIndex};
use v12n_core::sql::{ColumnType, ColumnTypes, Comparison, Lexical, RenderOptions, TableKeys};
use v12n_core::{sparql, unfold};

const SHOP: &str = include_str!("../../../examples/shop/mapping.ttl");
const VIP_ORDERS: &str = include_str!("../../../examples/shop/queries/vip-orders.rq");

const PREFIXES: &str = "PREFIX ex: <http://example.org/>\n";

fn explain(mapping: &str, query: &str) -> String {
    explain_with_keys(mapping, query, None)
}

fn explain_with_keys(mapping: &str, query: &str, keys: Option<&TableKeys>) -> String {
    render(Mapping::from_turtle(mapping).unwrap(), query, keys)
}

fn render(mapping: Mapping, query: &str, keys: Option<&TableKeys>) -> String {
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

const CONSTANTS: &str = include_str!("../../../tests/diff/0002-constants/mapping.ttl");

fn column_type(name: &str, natural: Option<&str>) -> ColumnType {
    let group = if name == "int4" { "integer" } else { "text" };
    ColumnType {
        name: name.to_owned(),
        sql_type: format!("\"pg_catalog\".\"{name}\""),
        union_class: group.to_owned(),
        natural_datatype: natural.map(|n| format!("http://www.w3.org/2001/XMLSchema#{n}")),
        group: group.to_owned(),
        comparison: Comparison::Value,
        lexical: Some(Lexical::Text),
        term_key: None,
    }
}

/// `ex:value` reads an integer column of `readings` and a text column of
/// `labels` (#24), with the types a database reports for them.
fn explain_with_union_types(query: &str) -> String {
    let mapping = Mapping::from_turtle(CONSTANTS).unwrap();
    let index = MappingIndex::new(&mapping);
    let query = sparql::parse(&format!("{PREFIXES}{query}")).unwrap();
    let plan = unfold::unfold(&query, &mapping, &index).unwrap();
    let int = || column_type("int4", Some("integer"));
    let types: ColumnTypes = [
        (("readings", "id"), int()),
        (("readings", "value"), int()),
        (("labels", "id"), int()),
        (("labels", "value"), column_type("text", None)),
    ]
    .into_iter()
    .map(|((table, column), ty)| ((LogicalTable::TableName(table.into()), column.into()), ty))
    .collect();
    let plan = plan.with_union_types(&types).unwrap();
    plan.to_sql_with(&RenderOptions {
        column_types: Some(&types),
        ..RenderOptions::default()
    })
    .unwrap()
    .sql
}

#[test]
fn union_splits_a_literal_over_columns_of_different_types() {
    insta::assert_snapshot!(explain_with_union_types(
        "SELECT ?s ?v WHERE { ?s ex:value ?v }"
    ));
}

#[test]
fn union_keeps_the_index_on_a_constant_subject() {
    insta::assert_snapshot!(explain_with_union_types(
        "SELECT ?v WHERE { <http://example.org/item/1> ex:value ?v }"
    ));
}

/// IRIs from a relative template, a relative template with a path, and
/// columns, in the style of the W3C tests R2RMLTC0019a and R2RMLTC0020a.
const RELATIVE: &str = r#"
@prefix rr: <http://www.w3.org/ns/r2rml#> .
@prefix ex: <http://example.org/> .

<#Person> rr:logicalTable [ rr:tableName "people" ] ;
  rr:subjectMap [ rr:template "{name}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:homepage ;
    rr:objectMap [ rr:column "page" ; rr:termType rr:IRI ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:knows ;
    rr:objectMap [ rr:template "person/{friend}" ; rr:termType rr:IRI ] ] .

<#Page> rr:logicalTable [ rr:tableName "pages" ] ;
  rr:subjectMap [ rr:column "iri" ] ;
  rr:predicateObjectMap [ rr:predicate ex:title ; rr:objectMap [ rr:column "title" ] ] .
"#;

fn explain_with_base(query: &str) -> String {
    let mapping = Mapping::from_turtle(RELATIVE)
        .unwrap()
        .with_base_iri(BaseIri::new("http://example.com/base/").unwrap());
    render(mapping, &format!("{PREFIXES}{query}"), None)
}

/// The template gets the base in front, so the constant still decomposes
/// into a comparison on the column.
#[test]
fn base_iri_constant_against_relative_template_decomposes() {
    insta::assert_snapshot!(explain_with_base(
        "SELECT ?p WHERE { <http://example.com/base/Bob%2FCharles> ex:homepage ?p }"
    ));
}

/// A column holding the IRI or the relative rest both produce it.
#[test]
fn base_iri_constant_against_iri_column_matches_both_forms() {
    insta::assert_snapshot!(explain_with_base(
        "SELECT ?t WHERE { <http://example.com/base/a.html> ex:title ?t }"
    ));
}

#[test]
fn base_iri_join_on_iri_column_compares_generated_iris() {
    insta::assert_snapshot!(explain_with_base(
        "SELECT ?s ?t WHERE { ?s ex:homepage ?p . ?p ex:title ?t }"
    ));
}

fn shop_keys() -> TableKeys {
    ["customers", "orders"]
        .into_iter()
        .map(|t| {
            (
                LogicalTable::TableName(t.into()),
                vec![vec!["id".to_owned()]],
            )
        })
        .collect()
}

/// The optional part is a `LEFT JOIN` whose `ON` holds the link to the
/// left side and its own conditions. Its key fixes its row too, so no
/// duplicates need removing.
#[test]
fn optional_left_joins_the_optional_part() {
    insta::assert_snapshot!(explain_with_keys(
        SHOP,
        &format!(
            "{PREFIXES}SELECT ?c ?n ?o ?a WHERE {{ ?c ex:name ?n \
             OPTIONAL {{ ?o ex:placedBy ?c ; ex:amount ?a }} }}"
        ),
        Some(&shop_keys())
    ));
}

#[test]
fn optional_nested_inside_optional() {
    insta::assert_snapshot!(shop(
        "SELECT ?c ?o ?a WHERE { ?c a ex:Customer \
         OPTIONAL { ?o ex:placedBy ?c OPTIONAL { ?o ex:amount ?a } } }"
    ));
}

/// ?c is unbound where the first OPTIONAL does not match, and then any
/// customer is compatible with it.
#[test]
fn optional_variable_unbound_on_the_left_compares_with_or() {
    insta::assert_snapshot!(shop(
        "SELECT ?o ?c ?n WHERE { ?o ex:amount ?a OPTIONAL { ?o ex:placedBy ?c } \
         OPTIONAL { ?c ex:name ?n } }"
    ));
}

#[test]
fn optional_followed_by_a_required_pattern() {
    insta::assert_snapshot!(shop(
        "SELECT ?o ?c ?n WHERE { ?o ex:amount ?a OPTIONAL { ?o ex:placedBy ?c } ?c ex:name ?n }"
    ));
}

#[test]
fn optional_with_an_empty_left_side() {
    insta::assert_snapshot!(shop("SELECT ?c ?t WHERE { OPTIONAL { ?c ex:tier ?t } }"));
}

/// Each UNION branch of the required part has its own `LEFT JOIN`.
#[test]
fn optional_in_each_union_branch() {
    insta::assert_snapshot!(explain_with_union_types(
        "SELECT ?s ?v ?d WHERE { ?s ex:value ?v OPTIONAL { ?s ex:measuredOn ?d } }"
    ));
}

/// The SQL with the shop's keys and the column types a database reports.
fn explain_shop_with_types(body: &str) -> String {
    let mapping = Mapping::from_turtle(SHOP).unwrap();
    let index = MappingIndex::new(&mapping);
    let query = sparql::parse(&format!("{PREFIXES}{body}")).unwrap();
    let plan = unfold::unfold(&query, &mapping, &index).unwrap();
    let int = || column_type("int4", Some("integer"));
    let types: ColumnTypes = [
        (("customers", "id"), int()),
        (("customers", "name"), column_type("text", None)),
        (("customers", "tier"), column_type("text", None)),
        (("orders", "id"), int()),
        (("orders", "customer_id"), int()),
        (("orders", "amount"), int()),
    ]
    .into_iter()
    .map(|((table, column), ty)| ((LogicalTable::TableName(table.into()), column.into()), ty))
    .collect();
    let keys = shop_keys();
    let rendered = plan
        .to_sql_with(&RenderOptions {
            column_types: Some(&types),
            keys: Some(&keys),
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

/// A FILTER inside OPTIONAL is part of the `LEFT JOIN`'s `ON` clause: it
/// decides whether the part matches, and sees the left side's variables.
#[test]
fn filter_inside_optional_is_in_the_left_join() {
    insta::assert_snapshot!(explain_shop_with_types(
        "SELECT ?c ?o WHERE { ?c ex:tier ?t OPTIONAL { ?o ex:placedBy ?c ; ex:amount ?a FILTER(?a > 100 && ?t = \"VIP\") } }"
    ));
}

/// After the OPTIONAL, the filter applies to the whole group, and a
/// variable of the optional part is checked to be bound first.
#[test]
fn filter_after_optional_is_in_the_where_clause() {
    insta::assert_snapshot!(explain_shop_with_types(
        "SELECT ?c ?o WHERE { ?c ex:tier ?t OPTIONAL { ?o ex:placedBy ?c ; ex:amount ?a } FILTER(!(?a > 100) || !BOUND(?o)) }"
    ));
}

/// The constant is converted to the column's type, so the comparison can
/// use an index on the column; IRIs compare through their templates.
#[test]
fn filter_comparisons_keep_the_column_as_it_is() {
    insta::assert_snapshot!(explain_shop_with_types(
        "SELECT ?o WHERE { ?o ex:amount ?a ; ex:placedBy ?c FILTER(?a >= 100.0 && ?c != <http://example.org/customer/7>) }"
    ));
}

const UNION: &str = include_str!("../../../tests/diff/0007-union/mapping.ttl");

fn union_keys() -> TableKeys {
    [
        ("customers", "id"),
        ("wholesalers", "code"),
        ("orders", "id"),
    ]
    .into_iter()
    .map(|(t, k)| (LogicalTable::TableName(t.into()), vec![vec![k.to_owned()]]))
    .collect()
}

/// Each side of the union is one arm; the arms' solutions are a bag, and
/// keys prove that neither arm has duplicates, so the rows are kept.
#[test]
fn query_union_combines_arms_with_union_all() {
    insta::assert_snapshot!(explain_with_keys(
        UNION,
        &format!(
            "{PREFIXES}SELECT ?x ?y ?a WHERE {{ {{ ?x ex:city ?y }} UNION {{ ?x ex:amount ?a }} }}"
        ),
        Some(&union_keys())
    ));
}

/// An arm read from two triples maps removes its own duplicates with
/// `UNION`, in parentheses, before the bag union with the other arm.
#[test]
fn query_union_arm_of_several_branches_removes_its_duplicates() {
    insta::assert_snapshot!(explain_with_keys(
        UNION,
        &format!("{PREFIXES}SELECT ?x WHERE {{ {{ ?x a ex:Client }} UNION {{ ?x a ex:Order }} }}"),
        Some(&union_keys())
    ));
}

/// Terms of different shapes that are never the same take their own
/// columns (#29): templates with different prefixes, and two constants.
#[test]
fn shapes_take_their_own_columns() {
    insta::assert_snapshot!(explain(
        UNION,
        &format!("{PREFIXES}SELECT ?c ?k WHERE {{ ?c ex:kind ?k }}")
    ));
}

/// A template and an IRI column can give the same IRI, so both select the
/// IRI's string and duplicates are removed on it.
#[test]
fn shapes_that_can_be_equal_compare_term_strings() {
    insta::assert_snapshot!(explain(
        UNION,
        &format!("{PREFIXES}SELECT ?c WHERE {{ ?c ex:name \"Ann\" }}")
    ));
}

const OPTIONAL: &str = include_str!("../../../tests/diff/0005-optional/mapping.ttl");

/// `ex:name` of a person comes from two triples maps: one branch where
/// each part matched (its `LEFT JOIN` with the witness not `NULL`, which
/// PostgreSQL plans as an inner join), and one where neither did.
#[test]
fn optional_matched_by_several_triples_maps_splits_the_branch() {
    insta::assert_snapshot!(explain(
        OPTIONAL,
        &format!(
            "{PREFIXES}SELECT ?o ?n WHERE {{ ?o ex:placedBy ?p OPTIONAL {{ ?p ex:name ?n }} }}"
        )
    ));
}

/// One plain `SELECT` sorts by the integer column itself, so an index on
/// it can deliver the rows in order and stop at the `LIMIT`.
#[test]
fn order_by_an_integer_column_sorts_the_column_itself() {
    insta::assert_snapshot!(explain_shop_with_types(
        "SELECT ?o ?a WHERE { ?o ex:amount ?a } ORDER BY DESC(?a) LIMIT 10 OFFSET 20"
    ));
}

/// IRIs sort as strings by code point, so a template's IRIs are built in
/// SQL and compared under `COLLATE "C"`.
#[test]
fn order_by_iris_compares_their_strings_by_code_point() {
    insta::assert_snapshot!(explain_shop_with_types(
        "SELECT ?c ?n WHERE { ?c ex:name ?n } ORDER BY ?n DESC(?c)"
    ));
}

/// A variable that may be unbound sorts first by its rank: no value, then
/// numbers.
#[test]
fn order_by_an_optional_variable_ranks_unbound_first() {
    insta::assert_snapshot!(explain_shop_with_types(
        "SELECT ?c ?a WHERE { ?c ex:name ?n OPTIONAL { ?o ex:placedBy ?c ; ex:amount ?a } } ORDER BY ?a"
    ));
}

/// Over a `UNION`, every arm selects the sort keys of its own terms, and
/// the rows of all arms are sorted together; a boolean expression ranks
/// its errors as no value.
#[test]
fn order_by_over_a_union_sorts_the_selected_keys() {
    insta::assert_snapshot!(explain_shop_with_types(
        "SELECT ?x ?v WHERE { { ?x ex:name ?v } UNION { ?x ex:amount ?v } } \
         ORDER BY ?v (?v > 100) LIMIT 5"
    ));
}

/// `DISTINCT` applies before `LIMIT`, and the sort keys, functions of the
/// projected terms, are selected with them.
#[test]
fn order_by_with_distinct_selects_the_keys_and_limits_after() {
    insta::assert_snapshot!(explain_shop_with_types(
        "SELECT DISTINCT ?n WHERE { ?c ex:tier ?n } ORDER BY DESC(?n) LIMIT 2"
    ));
}

/// Without `ORDER BY`, `LIMIT` applies to the bag union of the arms.
#[test]
fn limit_without_order_by_applies_to_the_whole_union() {
    insta::assert_snapshot!(explain_with_keys(
        UNION,
        &format!(
            "{PREFIXES}SELECT ?x WHERE {{ {{ ?x a ex:Client }} UNION {{ ?x a ex:Order }} }} LIMIT 3"
        ),
        Some(&union_keys())
    ));
}
