// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;

use super::*;
use crate::dialect::PostgresDialect;
use crate::mapping::{Mapping, MappingIndex};
use crate::sql::RenderOptions;
use crate::{sparql, unfold};

const MAPPING: &str = r#"@prefix rr: <http://www.w3.org/ns/r2rml#> .
@prefix ex: <http://example.org/> .

<#Asset> rr:logicalTable [ rr:tableName "assets" ] ;
  rr:subjectMap [ rr:template "http://example.org/asset/{id}" ; rr:class ex:Asset ] ;
  rr:predicateObjectMap [ rr:predicate ex:hostname ; rr:objectMap [ rr:column "hostname" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:criticality ; rr:objectMap [ rr:column "criticality" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:site ; rr:objectMap [ rr:column "site" ] ] .

<#Device> rr:logicalTable [ rr:tableName "assets" ] ;
  rr:subjectMap [ rr:template "http://example.org/asset/{id}" ; rr:class ex:Device ] .

<#Alert> rr:logicalTable [ rr:tableName "alerts" ] ;
  rr:subjectMap [ rr:template "http://example.org/alert/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:title ; rr:objectMap [ rr:column "title" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:severity ; rr:objectMap [ rr:column "severity_id" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:onAsset ;
    rr:objectMap [ rr:template "http://example.org/asset/{asset_id}" ] ] .

<#Link> rr:logicalTable [ rr:tableName "links" ] ;
  rr:subjectMap [ rr:template "http://example.org/link/{src}/{dst}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:note ; rr:objectMap [ rr:column "note" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:weight ; rr:objectMap [ rr:column "weight" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:from ;
    rr:objectMap [ rr:parentTriplesMap <#Asset> ; rr:joinCondition [ rr:child "src" ; rr:parent "id" ] ] ] .

<#Source> rr:logicalTable [ rr:tableName "links" ] ;
  rr:subjectMap [ rr:template "http://example.org/source/{src}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:linkNote ; rr:objectMap [ rr:column "note" ] ] .

<#Page> rr:logicalTable [ rr:tableName "pages" ] ;
  rr:subjectMap [ rr:column "url" ] ;
  rr:predicateObjectMap [ rr:predicate ex:pageTitle ; rr:objectMap [ rr:column "title" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:pageLang ; rr:objectMap [ rr:column "lang" ] ] .

<#View> rr:logicalTable [ rr:sqlQuery "SELECT * FROM assets" ] ;
  rr:subjectMap [ rr:template "http://example.org/view/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:viewName ; rr:objectMap [ rr:column "hostname" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:viewSite ; rr:objectMap [ rr:column "site" ] ] .
"#;

fn plan(query: &str) -> SqlPlan {
    let mapping = Mapping::from_turtle(MAPPING).unwrap();
    let query = sparql::parse(&format!("PREFIX ex: <http://example.org/>\n{query}")).unwrap();
    unfold::unfold(&query, &mapping, &MappingIndex::new(&mapping)).unwrap()
}

fn keys() -> TableKeys {
    [
        ("assets", vec![vec!["id"]]),
        ("alerts", vec![vec!["id"]]),
        ("links", vec![vec!["src", "dst"]]),
        ("pages", vec![vec!["url"]]),
    ]
    .into_iter()
    .map(|(table, keys)| {
        (
            LogicalTable::TableName(table.into()),
            keys.into_iter()
                .map(|k| k.into_iter().map(str::to_owned).collect())
                .collect(),
        )
    })
    .collect()
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

fn merged(query: &str) -> SqlPlan {
    eliminate_self_joins(&plan(query), &keys()).expect("some scans are merged")
}

/// The SQL with the keys, so that `DISTINCT` is left out where they
/// prove there are no duplicates.
fn sql(plan: &SqlPlan) -> String {
    rendered(plan).sql
}

fn rendered(plan: &SqlPlan) -> crate::sql::RenderedSql {
    let keys = keys();
    plan.to_sql_with(
        &PostgresDialect,
        &RenderOptions {
            keys: Some(&keys),
            ..RenderOptions::default()
        },
    )
    .unwrap()
}

#[test]
fn self_join_tables_lists_tables_scanned_twice_in_a_branch() {
    let plan = plan(
        "SELECT * WHERE { ?a ex:hostname ?h ; ex:site ?s . ?f ex:onAsset ?a . ?v ex:viewName ?n ; ex:viewSite ?vs }",
    );
    assert_eq!(
        self_join_tables(&plan),
        [LogicalTable::TableName("assets".into())]
    );
}

#[test]
fn star_on_one_table_reads_it_once() {
    let plan = merged(r#"SELECT ?a ?h WHERE { ?a ex:criticality "High" ; ex:hostname ?h }"#);
    let [branch] = plan.branches.as_slice() else {
        panic!("one branch");
    };
    assert_eq!(tables(branch), ["assets"]);
    assert_eq!(
        sql(&plan),
        "SELECT t0.\"id\" AS \"a\", t0.\"hostname\" AS \"h\"\n\
         FROM \"assets\" t0\n\
         WHERE t0.\"criticality\" = $1 AND t0.\"hostname\" IS NOT NULL"
    );
    let fixed: BTreeSet<String> = ["criticality", "hostname", "id"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_eq!(branch.fixed_columns, [fixed]);
}

#[test]
fn class_of_another_triples_map_over_the_table_is_merged() {
    let plan = merged("SELECT ?a ?h WHERE { ?a a ex:Device ; ex:hostname ?h }");
    assert_eq!(tables(&plan.branches[0]), ["assets"]);
}

#[test]
fn constant_subject_pins_both_scans_to_one_row() {
    let original =
        plan("SELECT ?t ?s WHERE { <http://example.org/alert/7> ex:title ?t ; ex:severity ?s }");
    assert!(sql(&original).contains("CROSS JOIN"));
    let plan = eliminate_self_joins(&original, &keys()).unwrap();
    let rendered = rendered(&plan);
    assert_eq!(
        rendered.sql,
        "SELECT t0.\"title\" AS \"t\", t0.\"severity_id\" AS \"s\"\n\
         FROM \"alerts\" t0\n\
         WHERE t0.\"id\" = $1 AND t0.\"title\" IS NOT NULL AND t0.\"severity_id\" IS NOT NULL"
    );
    assert_eq!(rendered.params, ["7"]);
}

#[test]
fn different_constants_do_not_pin_one_row() {
    let plan = plan(
        "SELECT ?t ?s WHERE { <http://example.org/alert/7> ex:title ?t . \
         <http://example.org/alert/8> ex:severity ?s }",
    );
    assert_eq!(eliminate_self_joins(&plan, &keys()), None);
}

#[test]
fn chain_of_scans_becomes_one_and_other_tables_keep_their_joins() {
    let plan = merged(
        "SELECT ?f ?h WHERE { ?f ex:severity 5 ; ex:onAsset ?a ; ex:title ?t . \
         ?a ex:hostname ?h ; ex:site ?s }",
    );
    assert_eq!(tables(&plan.branches[0]), ["alerts", "assets"]);
    let sql = sql(&plan);
    assert!(
        sql.contains("FROM \"alerts\" t0\nJOIN \"assets\" t1 ON t1.\"id\" = t0.\"asset_id\""),
        "{sql}"
    );
}

#[test]
fn composite_key_needs_every_column() {
    // Both columns of the template: one row.
    let plan = merged("SELECT ?l ?n ?w WHERE { ?l ex:note ?n ; ex:weight ?w }");
    assert_eq!(tables(&plan.branches[0]), ["links"]);
    // `src` alone is not a key: two links of one source stay two scans.
    let plan = plan_of("SELECT ?s ?n ?m WHERE { ?s ex:linkNote ?n , ?m }");
    assert_eq!(eliminate_self_joins(&plan, &keys()), None);
}

fn plan_of(query: &str) -> SqlPlan {
    plan(query)
}

#[test]
fn referencing_object_map_parent_merges_with_the_parent_scan() {
    // links t0 joins its parent assets t1 on `src = id` (R2RML §8), and
    // the hostname pattern scans assets again with the same subject.
    let plan = merged("SELECT ?l ?h WHERE { ?l ex:from ?a . ?a ex:hostname ?h }");
    assert_eq!(tables(&plan.branches[0]), ["links", "assets"]);
}

#[test]
fn join_through_another_column_is_kept() {
    // Two alerts on the same asset are different rows.
    let plan = plan("SELECT ?f ?g WHERE { ?f ex:onAsset ?a . ?g ex:onAsset ?a }");
    assert_eq!(eliminate_self_joins(&plan, &keys()), None);
}

#[test]
fn table_without_a_key_or_a_view_is_kept() {
    let plan = plan("SELECT ?a ?h WHERE { ?a ex:criticality ?c ; ex:hostname ?h }");
    let none = TableKeys::new();
    assert_eq!(eliminate_self_joins(&plan, &none), None);
    let other: TableKeys = [(
        LogicalTable::TableName("assets".into()),
        vec![vec!["site".to_owned()]],
    )]
    .into_iter()
    .collect();
    assert_eq!(eliminate_self_joins(&plan, &other), None);
    let view = plan_of("SELECT ?v ?n ?s WHERE { ?v ex:viewName ?n ; ex:viewSite ?s }");
    assert_eq!(eliminate_self_joins(&view, &keys()), None);
}

#[test]
fn term_comparison_of_an_iri_column_with_a_base_iri_is_kept() {
    let mapping = Mapping::from_turtle(MAPPING)
        .unwrap()
        .with_base_iri(crate::mapping::BaseIri::new("http://example.org/base/").unwrap());
    let query = sparql::parse(
        "PREFIX ex: <http://example.org/>\nSELECT ?p ?t ?l WHERE { ?p ex:pageTitle ?t ; ex:pageLang ?l }",
    )
    .unwrap();
    let plan = unfold::unfold(&query, &mapping, &MappingIndex::new(&mapping)).unwrap();
    assert_eq!(eliminate_self_joins(&plan, &keys()), None);
    // Without a base IRI the URL column is compared as it is.
    let plan = merged("SELECT ?p ?t ?l WHERE { ?p ex:pageTitle ?t ; ex:pageLang ?l }");
    assert_eq!(tables(&plan.branches[0]), ["pages"]);
}

#[test]
fn scans_inside_one_optional_part_are_merged() {
    let plan = merged(
        "SELECT ?a ?h ?f ?t WHERE { ?a ex:hostname ?h \
         OPTIONAL { ?f ex:onAsset ?a ; ex:title ?t } }",
    );
    let [branch] = plan.branches.as_slice() else {
        panic!("one branch");
    };
    assert_eq!(tables(branch), ["assets", "alerts"]);
    assert_eq!(branch.left_joins[0].tables, 1..2);
    assert_eq!(
        sql(&plan),
        "SELECT t0.\"id\" AS \"a\", t0.\"hostname\" AS \"h\", t1.\"id\" AS \"f\", t1.\"title\" AS \"t\"\n\
         FROM \"assets\" t0\n\
         LEFT JOIN \"alerts\" t1 ON t1.\"asset_id\" = t0.\"id\" AND t1.\"title\" IS NOT NULL\n\
         WHERE t0.\"id\" IS NOT NULL AND t0.\"hostname\" IS NOT NULL"
    );
}

/// bench q04: `LEFT JOIN assets t1 ON t1.id = t0.id AND t1.criticality IS
/// NOT NULL` can only match the row of `t0`, and matches it where its
/// criticality is not `NULL`, which is where `t0.criticality` is not.
#[test]
fn optional_part_on_the_same_row_reads_the_left_scan() {
    let plan = merged("SELECT ?a ?c WHERE { ?a a ex:Asset OPTIONAL { ?a ex:criticality ?c } }");
    let [branch] = plan.branches.as_slice() else {
        panic!("one branch");
    };
    let same_row = branch.left_joins[0].same_row.as_ref().expect("reads a row");
    assert_eq!((same_row.scan, same_row.row_of), (1, 0));
    assert_eq!(
        same_row.guard,
        [Condition::IsNotNull(SqlExpr::Column {
            table: 0,
            name: "criticality".into()
        })]
    );
    assert_eq!(
        sql(&plan),
        "SELECT t0.\"id\" AS \"a\", t0.\"criticality\" AS \"c\"\n\
         FROM \"assets\" t0\n\
         WHERE t0.\"id\" IS NOT NULL"
    );
    // The plan still has the scan, so what it fixes is unchanged.
    assert_eq!(tables(branch), ["assets", "assets"]);
    assert!(!plan.has_duplicates(Some(&keys())));
}

#[test]
fn each_optional_part_reads_the_row_on_its_own() {
    let plan = merged(
        "SELECT ?a ?c ?s WHERE { ?a ex:hostname ?h \
         OPTIONAL { ?a ex:criticality ?c } OPTIONAL { ?a ex:site ?s } }",
    );
    assert_eq!(
        sql(&plan),
        "SELECT t0.\"id\" AS \"a\", t0.\"criticality\" AS \"c\", t0.\"site\" AS \"s\"\n\
         FROM \"assets\" t0\n\
         WHERE t0.\"id\" IS NOT NULL AND t0.\"hostname\" IS NOT NULL"
    );
}

/// The part matches only where both columns are not `NULL`, so each is
/// `NULL` where the other is.
#[test]
fn columns_of_a_part_are_null_where_any_of_its_conditions_fails() {
    let plan = merged(
        "SELECT ?a ?c ?s WHERE { ?a ex:hostname ?h OPTIONAL { ?a ex:criticality ?c ; ex:site ?s } }",
    );
    assert_eq!(
        sql(&plan),
        "SELECT t0.\"id\" AS \"a\", \
         CASE WHEN t0.\"site\" IS NOT NULL THEN t0.\"criticality\" END AS \"c\", \
         CASE WHEN t0.\"criticality\" IS NOT NULL THEN t0.\"site\" END AS \"s\"\n\
         FROM \"assets\" t0\n\
         WHERE t0.\"id\" IS NOT NULL AND t0.\"hostname\" IS NOT NULL"
    );
}

/// The nested part reads the row of the outer part's scan, which is
/// `NULL` where the outer part does not match.
#[test]
fn nested_optional_part_reads_the_row_of_the_outer_part() {
    let plan = merged(
        "SELECT ?a ?c ?s WHERE { ?a ex:hostname ?h \
         OPTIONAL { ?a ex:criticality ?c OPTIONAL { ?a ex:site ?s } } }",
    );
    let rows: Vec<(usize, usize)> = plan.branches[0]
        .left_joins
        .iter()
        .map(|j| j.same_row.as_ref().map(|r| (r.scan, r.row_of)).unwrap())
        .collect();
    assert_eq!(rows, [(1, 0), (2, 1)]);
    assert_eq!(
        sql(&plan),
        "SELECT t0.\"id\" AS \"a\", t0.\"criticality\" AS \"c\", \
         CASE WHEN t0.\"criticality\" IS NOT NULL THEN t0.\"site\" END AS \"s\"\n\
         FROM \"assets\" t0\n\
         WHERE t0.\"id\" IS NOT NULL AND t0.\"hostname\" IS NOT NULL"
    );
}

/// A nested part reads the row of a part that joins another table.
#[test]
fn nested_part_reads_the_row_of_an_outer_part_that_joins() {
    let plan = merged(
        "SELECT ?a ?f ?t WHERE { ?a ex:hostname ?h \
         OPTIONAL { ?f ex:onAsset ?a OPTIONAL { ?f ex:title ?t } } }",
    );
    assert_eq!(
        sql(&plan),
        "SELECT t0.\"id\" AS \"a\", t1.\"id\" AS \"f\", t1.\"title\" AS \"t\"\n\
         FROM \"assets\" t0\n\
         LEFT JOIN \"alerts\" t1 ON t1.\"asset_id\" = t0.\"id\" AND t1.\"id\" IS NOT NULL\n\
         WHERE t0.\"id\" IS NOT NULL AND t0.\"hostname\" IS NOT NULL"
    );
}

/// The second part binds `?x` where the first did not, or to the same
/// value (SPARQL 1.1 §18.3); its guard reads the first part's column.
#[test]
fn guard_compares_with_an_earlier_part_that_reads_the_row() {
    let plan = merged(
        "SELECT ?a ?x WHERE { ?a ex:hostname ?h \
         OPTIONAL { ?a ex:criticality ?x } OPTIONAL { ?a ex:site ?x } }",
    );
    let sql = sql(&plan);
    assert!(
        sql.contains(
            "CASE WHEN (t0.\"criticality\" IS NULL OR t0.\"site\" = t0.\"criticality\") \
             THEN t0.\"site\" END"
        ) && !sql.contains("JOIN"),
        "{sql}"
    );
}

/// The constant that pins the part is the one the required scan has, so
/// the guard does not repeat it.
#[test]
fn constant_subject_pins_the_optional_part_to_the_row() {
    let plan = merged(
        "SELECT ?h ?c WHERE { <http://example.org/asset/7> ex:hostname ?h \
         OPTIONAL { <http://example.org/asset/7> ex:criticality ?c } }",
    );
    let rendered = rendered(&plan);
    assert_eq!(
        rendered.sql,
        "SELECT t0.\"hostname\" AS \"h\", t0.\"criticality\" AS \"c\"\n\
         FROM \"assets\" t0\n\
         WHERE t0.\"id\" = $1 AND t0.\"hostname\" IS NOT NULL"
    );
    assert_eq!(rendered.params, ["7"]);
}

/// Conditions after the part read its columns where they are, in the
/// `ON` clause of a later table, and `BOUND` and `ORDER BY` as well.
#[test]
fn later_conditions_read_the_columns_of_the_part() {
    let plan = merged(
        "SELECT ?a ?c ?b WHERE { ?a ex:hostname ?h OPTIONAL { ?a ex:criticality ?c } ?b ex:site ?c }",
    );
    let joined = sql(&plan);
    assert!(
        joined.contains(
            "JOIN \"assets\" t2 ON (t0.\"criticality\" IS NULL OR t2.\"site\" = t0.\"criticality\")"
        ),
        "{joined}"
    );
    let plan = merged(
        "SELECT ?a ?c WHERE { ?a ex:hostname ?h OPTIONAL { ?a ex:criticality ?c ; ex:site ?s } \
         FILTER(!BOUND(?c)) } ORDER BY ?c",
    );
    let sql = sql(&plan);
    assert!(
        sql.contains(
            "WHERE NOT ((CASE WHEN t0.\"site\" IS NOT NULL THEN t0.\"criticality\" END IS NOT NULL))"
        ) && !sql.contains("t1"),
        "{sql}"
    );
}

#[test]
fn optional_part_keeps_its_left_join_where_it_cannot_read_the_row() {
    for query in [
        // A FILTER and a constant render with parameters.
        "SELECT ?a ?c WHERE { ?a ex:hostname ?h OPTIONAL { ?a ex:criticality ?c FILTER(?c = \"High\") } }",
        "SELECT ?a WHERE { ?a ex:hostname ?h OPTIONAL { ?a ex:criticality \"High\" } }",
        // Another table in the part.
        "SELECT ?a ?f WHERE { ?a ex:hostname ?h OPTIONAL { ?a ex:criticality ?c . ?f ex:onAsset ?a } }",
        // Two alerts of one asset are different rows.
        "SELECT ?f ?g WHERE { ?f ex:onAsset ?a OPTIONAL { ?g ex:onAsset ?a } }",
        // A view has no keys.
        "SELECT ?v ?s WHERE { ?v ex:viewName ?n OPTIONAL { ?v ex:viewSite ?s } }",
        // Only `src` of the key `(src, dst)` is compared.
        "SELECT ?s ?n ?m WHERE { ?s ex:linkNote ?n OPTIONAL { ?s ex:linkNote ?m } }",
    ] {
        let original = plan_of(query);
        let kept = eliminate_self_joins(&original, &keys()).unwrap_or(original);
        assert!(
            kept.branches[0]
                .left_joins
                .iter()
                .all(|j| j.same_row.is_none()),
            "{query}"
        );
        assert!(sql(&kept).contains("LEFT JOIN"), "{query}");
    }
}

/// A part whose conditions all hold on the row extends every left row
/// by it.
#[test]
fn optional_part_that_always_matches_reads_the_columns_as_they_are() {
    let plan = merged("SELECT ?a ?h2 WHERE { ?a ex:hostname ?h OPTIONAL { ?a ex:hostname ?h2 } }");
    assert_eq!(
        plan.branches[0].left_joins[0]
            .same_row
            .as_ref()
            .unwrap()
            .guard,
        []
    );
    assert!(sql(&plan).starts_with(
        "SELECT t0.\"id\" AS \"a\", t0.\"hostname\" AS \"h2\"\nFROM \"assets\" t0\nWHERE"
    ));
}

#[test]
fn renaming_tables_renames_the_row_a_part_reads() {
    let mut branch =
        merged("SELECT ?a ?c WHERE { ?a a ex:Asset OPTIONAL { ?a ex:criticality ?c } }")
            .branches
            .remove(0);
    branch.rename_tables(&|t| t + 3);
    let same_row = branch.left_joins[0].same_row.as_ref().unwrap();
    assert_eq!((same_row.scan, same_row.row_of), (4, 3));
    assert_eq!(
        same_row.guard,
        [Condition::IsNotNull(SqlExpr::Column {
            table: 3,
            name: "criticality".into()
        })]
    );
}

#[test]
fn scans_after_an_optional_part_merge_with_the_required_ones() {
    let plan = merged(
        "SELECT ?a ?c ?h WHERE { ?a ex:site ?s OPTIONAL { ?a ex:criticality ?c } ?a ex:hostname ?h }",
    );
    let [branch] = plan.branches.as_slice() else {
        panic!("one branch");
    };
    // t0 site, t1 the part, t2 hostname merged into t0.
    assert_eq!(tables(branch), ["assets", "assets"]);
    assert_eq!(branch.left_joins[0].tables, 1..2);
    let sql = sql(&plan);
    assert!(
        sql.contains("t0.\"hostname\" AS \"h\"") && !sql.contains("t2"),
        "{sql}"
    );
}

#[test]
fn union_arms_are_merged_each_on_its_own() {
    let plan = merged(
        "SELECT * WHERE { { ?a ex:hostname ?h ; ex:site ?s } UNION { ?f ex:title ?t ; ex:severity ?v } }",
    );
    let found: Vec<Vec<&str>> = plan.branches.iter().map(tables).collect();
    assert_eq!(found, [vec!["assets"], vec!["alerts"]]);
}

#[test]
fn filter_and_order_refer_to_the_merged_scan() {
    let plan = merged(
        "SELECT ?f ?t WHERE { ?f ex:severity ?s ; ex:title ?t FILTER(?s > 3) } ORDER BY DESC(?s) LIMIT 5",
    );
    let sql = sql(&plan);
    assert!(!sql.contains("t1"), "{sql}");
    assert!(sql.contains("t0.\"severity_id\""), "{sql}");
}

#[test]
fn merged_plan_still_proves_there_are_no_duplicates() {
    let plan = merged("SELECT ?h WHERE { ?a ex:criticality ?c ; ex:hostname ?h }");
    let keys = keys();
    assert!(!plan.has_duplicates(Some(&keys)));
    let sql = plan
        .to_sql_with(
            &PostgresDialect,
            &RenderOptions {
                keys: Some(&keys),
                ..RenderOptions::default()
            },
        )
        .unwrap()
        .sql;
    assert!(!sql.contains("DISTINCT"), "{sql}");
}

/// Only the key equalities are dropped: a variable bound by two columns
/// of the merged row still compares them.
#[test]
fn comparisons_of_other_columns_stay() {
    let plan = merged("SELECT ?a ?x WHERE { ?a ex:hostname ?x ; ex:site ?x }");
    let sql = sql(&plan);
    assert!(sql.contains("t0.\"site\" = t0.\"hostname\""), "{sql}");
}

/// A column compared with itself is dropped only for key columns, which
/// are `NOT NULL`; elsewhere the comparison is what excludes `NULL`.
#[test]
fn comparison_of_a_nullable_column_with_itself_stays() {
    let plan = merged("SELECT ?a ?c WHERE { ?a ex:criticality ?c . ?a ex:criticality ?c }");
    let sql = sql(&plan);
    assert!(
        sql.contains("t0.\"criticality\" = t0.\"criticality\"")
            || sql.contains("t0.\"criticality\" IS NOT NULL"),
        "{sql}"
    );
}

/// A `NOT NULL` constraint drops the guard's condition on the row read
/// (#111): that row is a table row, or `NULL` where its own part does not
/// match, which keeps the nested column `NULL` there.
#[test]
fn not_null_constraint_drops_the_condition_on_the_row_read() {
    let not_null = [(LogicalTable::TableName("assets".into()), "site".to_owned())]
        .into_iter()
        .collect();
    let plan = merged(
        "SELECT ?a ?c ?s WHERE { ?a ex:hostname ?h OPTIONAL { ?a ex:criticality ?c ; ex:site ?s } }",
    )
    .without_implied_not_null(&not_null);
    assert!(
        sql(&plan).starts_with(
            "SELECT t0.\"id\" AS \"a\", t0.\"criticality\" AS \"c\", \
             CASE WHEN t0.\"criticality\" IS NOT NULL THEN t0.\"site\" END AS \"s\"\n"
        ),
        "{}",
        sql(&plan)
    );
    let plan = merged(
        "SELECT ?a ?c ?s WHERE { ?a ex:hostname ?h \
         OPTIONAL { ?a ex:criticality ?c OPTIONAL { ?a ex:site ?s } } }",
    )
    .without_implied_not_null(&not_null);
    assert_eq!(
        plan.branches[0].left_joins[1]
            .same_row
            .as_ref()
            .unwrap()
            .guard,
        []
    );
    assert!(
        sql(&plan)
            .contains("CASE WHEN t0.\"criticality\" IS NOT NULL THEN t0.\"site\" END AS \"s\""),
        "{}",
        sql(&plan)
    );
}

/// A condition of the outer part on the nested part's column would make
/// each guard read the other's columns: the outer part keeps its `LEFT
/// JOIN`, and the nested one reads the outer part's row inside it.
#[test]
fn part_whose_conditions_read_a_nested_part_keeps_its_left_join() {
    let plan = merged(
        "SELECT ?a ?c ?s WHERE { ?a ex:hostname ?h \
         OPTIONAL { ?a ex:criticality ?c OPTIONAL { ?a ex:site ?s } ?a ex:hostname ?s } }",
    );
    let [outer, nested] = plan.branches[0].left_joins.as_slice() else {
        panic!("two parts");
    };
    assert_eq!(outer.same_row, None);
    assert_eq!(nested.same_row.as_ref().map(|r| r.row_of), Some(1));
    let sql = sql(&plan);
    assert!(
        sql.contains("LEFT JOIN \"assets\" t1 ON ") && !sql.contains("t2"),
        "{sql}"
    );
}
