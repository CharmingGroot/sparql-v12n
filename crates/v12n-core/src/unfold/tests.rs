// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::sparql;
use crate::sql::{TermSql, TermSqlValue};

const SHOP: &str = include_str!("../../../../examples/shop/mapping.ttl");

const PREFIXES: &str = "@prefix rr: <http://www.w3.org/ns/r2rml#> .
@prefix ex: <http://example.org/> .
";

fn unfold_query(mapping: &str, query: &str) -> Result<SqlPlan, Error> {
    let mapping = Mapping::from_turtle(mapping).unwrap();
    let index = MappingIndex::new(&mapping);
    let query = sparql::parse(&format!("PREFIX ex: <http://example.org/>\n{query}"))?;
    unfold(&query, &mapping, &index)
}

fn with_prefixes(body: &str) -> String {
    format!("{PREFIXES}{body}")
}

/// Template columns compared pairwise.
fn same(lt: usize, l: &str, rt: usize, r: &str) -> Condition {
    let side = |table, name: &str| TermSql {
        value: TermSqlValue::Column {
            table,
            name: name.to_owned(),
        },
        datatype: None,
    };
    Condition::SameTerm(side(lt, l), side(rt, r))
}

#[test]
fn unfold_shared_variable_emits_column_join() {
    let plan = unfold_query(
        SHOP,
        r#"SELECT ?order WHERE {
            ?customer a ex:Customer ; ex:tier "VIP" .
            ?order ex:placedBy ?customer ; ex:amount ?amount .
        }"#,
    )
    .unwrap();
    let [branch] = plan.branches.as_slice() else {
        panic!("expected one branch");
    };
    assert_eq!(branch.from.len(), 3);
    assert!(branch.conditions.contains(&same(1, "customer_id", 0, "id")));
    assert!(branch.conditions.contains(&same(2, "id", 1, "id")));
}

#[test]
fn unfold_class_pattern_alone_scans_its_table() {
    let plan = unfold_query(SHOP, "SELECT ?c WHERE { ?c a ex:Customer }").unwrap();
    assert_eq!(plan.branches[0].from.len(), 1);
}

#[test]
fn unfold_class_pattern_implied_by_same_triples_map_adds_no_table() {
    let plan = unfold_query(SHOP, "SELECT ?c ?n WHERE { ?c a ex:Customer ; ex:name ?n }").unwrap();
    assert_eq!(
        plan.branches[0].from,
        [LogicalTable::TableName("customers".into())]
    );
    // A class of a different triples map still needs its own scan.
    let plan = unfold_query(SHOP, "SELECT ?c WHERE { ?c a ex:Order ; ex:name ?n }").unwrap();
    assert!(plan.branches.is_empty());
}

#[test]
fn unfold_projection_records_how_to_rebuild_terms() {
    let plan = unfold_query(
        SHOP,
        "SELECT ?order ?amount ?missing WHERE { ?order ex:amount ?amount }",
    )
    .unwrap();
    assert_eq!(plan.columns, ["order", "amount"]);
    assert_eq!(
        plan.projection,
        [
            ProjectedTerm {
                variable: "order".into(),
                term: Some(ResultTerm {
                    term_type: TermType::Iri,
                    value: ResultValue::Template(vec![
                        TemplatePart::Literal("http://example.org/order/".into()),
                        TemplatePart::Column("order".into()),
                    ]),
                    datatype: None,
                    language: None,
                }),
            },
            ProjectedTerm {
                variable: "amount".into(),
                term: Some(ResultTerm {
                    term_type: TermType::Literal,
                    value: ResultValue::Column("amount".into()),
                    datatype: None,
                    language: None,
                }),
            },
            ProjectedTerm {
                variable: "missing".into(),
                term: None,
            },
        ]
    );
}

#[test]
fn unfold_multi_column_template_gets_one_alias_per_column() {
    let mapping = with_prefixes(
        r#"<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/{x}/{y}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:p ; rr:objectMap [ rr:column "v" ] ] ."#,
    );
    let plan = unfold_query(&mapping, "SELECT ?s ?s_1 WHERE { ?s ex:p ?s_1 }").unwrap();
    assert_eq!(plan.columns, ["s", "s_2", "s_1"]);
    assert_eq!(
        plan.projection[0].term.as_ref().unwrap().value.columns(),
        ["s", "s_2"]
    );
}

#[test]
fn unfold_constant_iri_outside_template_has_no_rows() {
    let plan = unfold_query(
        SHOP,
        "SELECT ?n WHERE { <http://example.org/order/7> ex:name ?n }",
    )
    .unwrap();
    assert!(plan.branches.is_empty());
    assert_eq!(plan.to_sql().sql, "SELECT 1 WHERE false");
}

#[test]
fn unfold_unknown_property_has_no_rows() {
    let plan = unfold_query(SHOP, "SELECT ?c WHERE { ?c ex:nope ?x }").unwrap();
    assert!(plan.branches.is_empty());
    assert_eq!(plan.projection[0].term, None);
}

#[test]
fn unfold_variable_used_as_iri_and_literal_has_no_rows() {
    let plan = unfold_query(SHOP, "SELECT ?x WHERE { ?x ex:name ?n . ?y ex:tier ?x }").unwrap();
    assert!(plan.branches.is_empty());
}

#[test]
fn unfold_same_variable_in_one_pattern_compares_within_the_row() {
    let mapping = with_prefixes(
        r#"<#P> rr:logicalTable [ rr:tableName "people" ] ;
  rr:subjectMap [ rr:template "http://ex/person/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:knows ;
    rr:objectMap [ rr:template "http://ex/person/{friend}" ] ] ."#,
    );
    let plan = unfold_query(&mapping, "SELECT ?x WHERE { ?x ex:knows ?x }").unwrap();
    assert_eq!(plan.branches[0].conditions, [same(0, "friend", 0, "id")]);
}

#[test]
fn unfold_disjoint_templates_drop_the_branch() {
    let mapping = with_prefixes(
        r#"<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/a/{id}" ; rr:class ex:Thing ] ;
  rr:predicateObjectMap [ rr:predicate ex:p ; rr:objectMap [ rr:column "v" ] ] .
<#B> rr:logicalTable [ rr:tableName "b" ] ;
  rr:subjectMap [ rr:template "http://ex/b/{id}" ; rr:class ex:Thing ] ."#,
    );
    let plan = unfold_query(&mapping, "SELECT ?x ?v WHERE { ?x a ex:Thing ; ex:p ?v }").unwrap();
    assert_eq!(plan.branches.len(), 1);
    assert_eq!(plan.branches[0].from, [LogicalTable::TableName("a".into())]);
}

#[test]
fn unfold_differently_shaped_projection_is_unsupported() {
    let mapping = with_prefixes(
        r#"<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/a/{id}" ; rr:class ex:Thing ] .
<#B> rr:logicalTable [ rr:tableName "b" ] ;
  rr:subjectMap [ rr:template "http://ex/b/{id}" ; rr:class ex:Thing ] ."#,
    );
    let err = unfold_query(&mapping, "SELECT ?x WHERE { ?x a ex:Thing }").unwrap_err();
    assert!(
        matches!(&err, Error::UnsupportedFeature(f) if f.contains("?x")),
        "{err}"
    );
}

#[test]
fn unfold_distinct_over_non_injective_template_is_unsupported() {
    let mapping = with_prefixes(
        r#"<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/{x}-{y}" ; rr:class ex:Thing ] ."#,
    );
    let err = unfold_query(&mapping, "SELECT DISTINCT ?s WHERE { ?s a ex:Thing }").unwrap_err();
    assert!(
        matches!(&err, Error::UnsupportedFeature(f) if f.contains("DISTINCT")),
        "{err}"
    );
    assert!(unfold_query(&mapping, "SELECT ?s WHERE { ?s a ex:Thing }").is_ok());
}

#[test]
fn unfold_variable_class_is_unsupported() {
    let err = unfold_query(SHOP, "SELECT ?c WHERE { ?x a ?c }").unwrap_err();
    assert!(
        matches!(&err, Error::UnsupportedFeature(f) if f.contains("rdf:type")),
        "{err}"
    );
}

#[test]
fn unfold_too_many_branches_errors() {
    let maps: String = (0..9)
        .map(|i| {
            format!(
                r#"<#M{i}> rr:logicalTable [ rr:tableName "t{i}" ] ;
  rr:subjectMap [ rr:template "http://ex/{{id}}" ; rr:class ex:Thing ] .
"#
            )
        })
        .collect();
    let mapping = with_prefixes(&maps);
    let err =
        unfold_query(&mapping, "SELECT * WHERE { ?x a ex:Thing . ?y a ex:Thing }").unwrap_err();
    assert_eq!(
        err,
        Error::TooManyBranches {
            limit: MAX_BRANCHES
        }
    );
    assert_eq!(
        unfold_query(&mapping, "SELECT * WHERE { ?x a ex:Thing }")
            .unwrap()
            .branches
            .len(),
        9
    );
}

#[test]
fn unfold_distinct_over_union_uses_union() {
    let mapping = with_prefixes(
        r#"<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/{id}" ; rr:class ex:Thing ] .
<#B> rr:logicalTable [ rr:tableName "b" ] ;
  rr:subjectMap [ rr:template "http://ex/{id}" ; rr:class ex:Thing ] ."#,
    );
    let sql = unfold_query(&mapping, "SELECT DISTINCT ?x WHERE { ?x a ex:Thing }")
        .unwrap()
        .to_sql()
        .sql;
    assert_eq!(
        sql,
        "SELECT t0.\"id\" AS \"x\"\nFROM \"a\" t0\nWHERE t0.\"id\" IS NOT NULL\n\
         UNION\n\
         SELECT t0.\"id\" AS \"x\"\nFROM \"b\" t0\nWHERE t0.\"id\" IS NOT NULL"
    );
}

#[test]
fn unfold_literal_constants_are_parameters_not_sql_text() {
    let plan = unfold_query(
        SHOP,
        r#"SELECT ?c WHERE { ?c ex:tier "x'; DROP TABLE customers; --" }"#,
    )
    .unwrap();
    let rendered = plan.to_sql();
    assert!(!rendered.sql.contains("DROP"));
    assert_eq!(rendered.params, ["x'; DROP TABLE customers; --"]);
}

#[test]
fn unfold_selects_hidden_variables_for_removing_duplicates() {
    let plan = unfold_query(
        SHOP,
        r#"SELECT ?order WHERE { ?order ex:placedBy ?c ; ex:amount ?a . _:b ex:name ?n }"#,
    )
    .unwrap();
    assert_eq!(plan.columns, ["order"]);
    // Every variable of the pattern, blank nodes included, in order of
    // first occurrence.
    assert_eq!(plan.hidden_columns, ["c", "a", "_:b", "n"]);
    assert_eq!(plan.branches[0].select.len(), 5);
    // DISTINCT over the selected variables needs none.
    let plan = unfold_query(SHOP, "SELECT DISTINCT ?order WHERE { ?order ex:amount ?a }").unwrap();
    assert!(plan.hidden_columns.is_empty());
}

#[test]
fn unfold_fixed_columns_are_those_of_injective_terms() {
    let mapping = with_prefixes(
        r#"<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/{x}/{y}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:p ; rr:objectMap [ rr:template "{u}-{v}" ; rr:termType rr:Literal ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:q ; rr:objectMap [ rr:column "w" ] ] ."#,
    );
    let plan = unfold_query(&mapping, "SELECT ?s WHERE { ?s ex:p ?o ; ex:q ?w }").unwrap();
    let fixed: Vec<Vec<&str>> = plan.branches[0]
        .fixed_columns
        .iter()
        .map(|c| c.iter().map(String::as_str).collect())
        .collect();
    // The literal template can render the same string from different
    // values, so it fixes neither of its columns.
    assert_eq!(fixed, [vec!["x", "y"], vec!["w", "x", "y"]]);
}

#[test]
fn unfold_hidden_variable_of_different_shapes_is_unsupported() {
    let mapping = with_prefixes(
        r#"<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/a/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:p ; rr:objectMap [ rr:column "v" ] ] .
<#B> rr:logicalTable [ rr:tableName "b" ] ;
  rr:subjectMap [ rr:template "http://ex/b/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:p ; rr:objectMap [ rr:column "v" ] ] ."#,
    );
    let err = unfold_query(&mapping, "SELECT ?v WHERE { ?s ex:p ?v }").unwrap_err();
    assert!(
        matches!(&err, Error::UnsupportedFeature(f) if f.contains("?s")),
        "{err}"
    );
    assert!(unfold_query(&mapping, "SELECT DISTINCT ?v WHERE { ?s ex:p ?v }").is_ok());
}
