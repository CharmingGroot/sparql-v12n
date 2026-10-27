// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::sparql;
use crate::sql::{Filter, RenderOptions, TableKeys, TermMatch, TermSql, TermSqlValue};

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
        base_iri: None,
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
                terms: vec![ResultTerm {
                    term_type: TermType::Iri,
                    value: ResultValue::Template(vec![
                        TemplatePart::Literal("http://example.org/order/".into()),
                        TemplatePart::Column("order".into()),
                    ]),
                    datatype: None,
                    language: None,
                    guard: None,
                }],
            },
            ProjectedTerm {
                variable: "amount".into(),
                terms: vec![ResultTerm {
                    term_type: TermType::Literal,
                    value: ResultValue::Column("amount".into()),
                    datatype: None,
                    language: None,
                    guard: None,
                }],
            },
            ProjectedTerm {
                variable: "missing".into(),
                terms: Vec::new(),
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
    assert_eq!(plan.projection[0].terms[0].value.columns(), ["s", "s_2"]);
}

#[test]
fn unfold_constant_iri_outside_template_has_no_rows() {
    let plan = unfold_query(
        SHOP,
        "SELECT ?n WHERE { <http://example.org/order/7> ex:name ?n }",
    )
    .unwrap();
    assert!(plan.branches.is_empty());
    assert_eq!(plan.to_sql().unwrap().sql, "SELECT 1 WHERE false");
}

#[test]
fn unfold_unknown_property_has_no_rows() {
    let plan = unfold_query(SHOP, "SELECT ?c WHERE { ?c ex:nope ?x }").unwrap();
    assert!(plan.branches.is_empty());
    assert_eq!(plan.projection[0].terms, []);
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
fn unfold_differently_shaped_projection_takes_columns_per_shape() {
    let mapping = with_prefixes(
        r#"<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/a/{id}" ; rr:class ex:Thing ] .
<#B> rr:logicalTable [ rr:tableName "b" ] ;
  rr:subjectMap [ rr:template "http://ex/b/{id}" ; rr:class ex:Thing ] ."#,
    );
    let plan = unfold_query(&mapping, "SELECT ?x WHERE { ?x a ex:Thing }").unwrap();
    // The templates never render the same IRI: each has its columns, which
    // the other branch leaves NULL, and the first bound one is the value.
    assert_eq!(plan.columns, ["x", "x_1"]);
    assert_eq!(plan.projection[0].terms.len(), 2);
    assert_eq!(
        plan.branches[0].select,
        [
            SqlExpr::Column {
                table: 0,
                name: "id".into()
            },
            SqlExpr::Unbound
        ]
    );
    assert_eq!(plan.branches[1].select[0], SqlExpr::Unbound);
    // Also under DISTINCT, since no term is in two columns.
    assert!(unfold_query(&mapping, "SELECT DISTINCT ?x WHERE { ?x a ex:Thing }").is_ok());
}

/// Shapes whose terms can be the same, compared when duplicates are
/// removed, share one column holding the term's string.
#[test]
fn unfold_overlapping_shapes_share_a_computed_column() {
    let mapping = with_prefixes(
        r#"<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/a/{id}" ; rr:class ex:Thing ] .
<#B> rr:logicalTable [ rr:tableName "b" ] ;
  rr:subjectMap [ rr:column "iri" ; rr:class ex:Thing ] .
<#C> rr:logicalTable [ rr:tableName "c" ] ;
  rr:subjectMap [ rr:template "http://ex/c/{id}" ; rr:class ex:Other ] ."#,
    );
    let plan = unfold_query(&mapping, "SELECT ?x WHERE { ?x a ex:Thing }").unwrap();
    assert_eq!(plan.columns, ["x"]);
    assert_eq!(
        plan.projection[0].terms,
        [ResultTerm {
            term_type: TermType::Iri,
            value: ResultValue::Column("x".into()),
            datatype: None,
            language: None,
            guard: None,
        }]
    );
    assert!(
        plan.branches
            .iter()
            .all(|b| matches!(&b.select[..], [SqlExpr::Term { guard: None, .. }])),
        "{plan:?}"
    );
    let sql = plan.to_sql().unwrap().sql;
    assert!(
        sql.starts_with("SELECT ($1 || CASE WHEN t0.\"id\"::text ~ '^[")
            && sql.contains("\nUNION\nSELECT (t0.\"iri\"::text)::text AS \"x\""),
        "{sql}"
    );
    // In different arms of a UNION, the shapes are never compared.
    let plan = unfold_query(
        &mapping,
        "SELECT ?x WHERE { { ?x a ex:Thing } UNION { ?x a ex:Other } }",
    )
    .unwrap();
    assert_eq!(plan.columns, ["x", "x_1"]);
    let plan = unfold_query(
        &mapping,
        "SELECT ?x WHERE { { ?x a ex:Other } UNION { ?x a ex:Other } }",
    )
    .unwrap();
    assert_eq!(plan.columns, ["x"]);
}

#[test]
fn unfold_overlapping_literals_of_a_natural_datatype_are_unsupported() {
    let mapping = with_prefixes(
        r#"<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/a/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:label ; rr:objectMap [ rr:column "name" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:tag ; rr:objectMap [ rr:column "code" ] ] .
<#B> rr:logicalTable [ rr:tableName "b" ] ;
  rr:subjectMap [ rr:template "http://ex/b/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:label ;
    rr:objectMap [ rr:template "{first} {last}" ; rr:termType rr:Literal ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:tag ;
    rr:objectMap [ rr:column "tag" ; rr:language "en" ] ] ."#,
    );
    let err = unfold_query(&mapping, "SELECT ?l WHERE { ?s ex:label ?l }").unwrap_err();
    assert!(
        matches!(&err, Error::UnsupportedFeature(f) if f.contains("?l") && f.contains("natural datatype")),
        "{err}"
    );
    // A language-tagged literal is never a literal of a natural datatype.
    let plan = unfold_query(&mapping, "SELECT DISTINCT ?l WHERE { ?s ex:tag ?l }").unwrap();
    assert_eq!(plan.projection[0].terms.len(), 2);
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
        .unwrap()
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
    let rendered = plan.to_sql().unwrap();
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
    // Literals of natural datatype get their datatype from the SQL type.
    assert_eq!(
        plan.lexical_hidden_columns,
        ["c", "_:b"].map(str::to_owned).into()
    );
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
fn unfold_hidden_variable_of_overlapping_shapes_is_compared_as_its_string() {
    let mapping = with_prefixes(
        r#"<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/a/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:p ; rr:objectMap [ rr:column "v" ] ] .
<#B> rr:logicalTable [ rr:tableName "b" ] ;
  rr:subjectMap [ rr:column "iri" ] ;
  rr:predicateObjectMap [ rr:predicate ex:p ; rr:objectMap [ rr:column "v" ] ] ."#,
    );
    let plan = unfold_query(&mapping, "SELECT ?v WHERE { ?s ex:p ?v }").unwrap();
    assert_eq!(plan.hidden_columns, ["s"]);
    assert!(plan.lexical_hidden_columns.contains("s"));
    assert!(matches!(plan.branches[1].select[1], SqlExpr::Term { .. }));
    assert!(unfold_query(&mapping, "SELECT DISTINCT ?v WHERE { ?s ex:p ?v }").is_ok());
}

const RELATIVE: &str = r#"
<#Person> rr:logicalTable [ rr:tableName "people" ] ;
  rr:subjectMap [ rr:template "{name}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:homepage ;
    rr:objectMap [ rr:column "page" ; rr:termType rr:IRI ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:alias ;
    rr:objectMap [ rr:template "{scheme}:{rest}" ; rr:termType rr:IRI ] ] .
"#;

fn unfold_with_base(query: &str) -> Result<SqlPlan, Error> {
    let mapping = Mapping::from_turtle(&with_prefixes(RELATIVE))
        .unwrap()
        .with_base_iri(BaseIri::new("http://example.com/base/").unwrap());
    let index = MappingIndex::new(&mapping);
    let query = sparql::parse(&format!("PREFIX ex: <http://example.org/>\n{query}"))?;
    unfold(&query, &mapping, &index)
}

fn param(value: &str) -> SqlExpr {
    SqlExpr::Param(value.into())
}

fn page() -> SqlExpr {
    SqlExpr::Column {
        table: 0,
        name: "page".into(),
    }
}

#[test]
fn unfold_base_iri_prefixes_relative_templates_and_keeps_the_plan_base() {
    let plan = unfold_with_base("SELECT ?s WHERE { ?s ex:homepage ?p }").unwrap();
    assert_eq!(
        plan.base_iri.as_ref().map(BaseIri::as_str),
        Some("http://example.com/base/")
    );
    let [
        ResultTerm {
            value: ResultValue::Template(parts),
            ..
        },
    ] = plan.projection[0].terms.as_slice()
    else {
        panic!("{:?}", plan.projection);
    };
    assert_eq!(
        parts,
        &[
            TemplatePart::Literal("http://example.com/base/".into()),
            TemplatePart::Column("s".into()),
        ]
    );
}

#[test]
fn unfold_base_iri_constant_matches_the_column_value_or_its_relative_rest() {
    let conditions = |iri: &str| {
        unfold_with_base(&format!("SELECT ?s WHERE {{ ?s ex:homepage <{iri}> }}"))
            .unwrap()
            .branches[0]
            .conditions
            .clone()
    };
    assert!(
        conditions("http://example.com/base/a.html").contains(&Condition::EqAny(
            page(),
            vec![param("http://example.com/base/a.html"), param("a.html")]
        ))
    );
    // Outside the base, or a rest with a scheme of its own, which a column
    // value would keep as it is: only the IRI itself.
    for iri in ["http://other.example/a", "http://example.com/base/urn:x"] {
        assert!(
            conditions(iri).contains(&Condition::Eq(page(), param(iri))),
            "{iri}"
        );
    }
}

#[test]
fn unfold_base_iri_compares_mixed_templates_as_generated_strings() {
    let plan = unfold_with_base("SELECT ?s WHERE { ?s ex:alias <http://example.com/base/x%3Ay> }")
        .unwrap();
    let [Condition::SameTerm(left, _), ..] = plan.branches[0].conditions.as_slice() else {
        panic!("{:?}", plan.branches[0].conditions);
    };
    assert_eq!(left.base_iri.as_deref(), Some("http://example.com/base/"));
}

#[test]
fn unfold_base_iri_distinct_over_resolved_column_is_unsupported() {
    let err = unfold_with_base("SELECT DISTINCT ?p WHERE { ?s ex:homepage ?p }").unwrap_err();
    assert!(matches!(err, Error::UnsupportedFeature(_)), "{err}");
    // A prefixed template renders distinct strings for distinct values.
    assert!(unfold_with_base("SELECT DISTINCT ?s WHERE { ?s ex:homepage ?p }").is_ok());
    // Without a base IRI nothing is resolved, so DISTINCT stays exact.
    let mapping = with_prefixes(RELATIVE);
    assert!(unfold_query(&mapping, "SELECT DISTINCT ?p WHERE { ?s ex:homepage ?p }").is_ok());
}

fn is_null(table: usize, name: &str) -> Condition {
    Condition::IsNull(SqlExpr::Column {
        table,
        name: name.to_owned(),
    })
}

#[test]
fn unfold_optional_left_joins_its_tables_with_the_link_in_on() {
    let plan = unfold_query(
        SHOP,
        "SELECT ?c ?o WHERE { ?c ex:name ?n OPTIONAL { ?o ex:placedBy ?c ; ex:amount ?a } }",
    )
    .unwrap();
    let [branch] = plan.branches.as_slice() else {
        panic!("expected one branch");
    };
    assert_eq!(
        branch.from,
        ["customers", "orders", "orders"].map(|t| LogicalTable::TableName(t.into()))
    );
    let [join] = branch.left_joins.as_slice() else {
        panic!("{:?}", branch.left_joins);
    };
    assert_eq!(join.tables, 1..3);
    assert_eq!(join.parent, None);
    // The link to ?c and the conditions within the part decide whether it
    // matches; none of them filters the customers.
    assert!(join.conditions.contains(&same(1, "customer_id", 0, "id")));
    assert!(join.conditions.contains(&same(2, "id", 1, "id")));
    assert!(
        join.conditions
            .contains(&Condition::IsNotNull(SqlExpr::Column {
                table: 2,
                name: "amount".into()
            }))
    );
    assert!(
        branch
            .conditions
            .iter()
            .all(|c| matches!(c, Condition::IsNotNull(SqlExpr::Column { table: 0, .. })))
    );
}

#[test]
fn unfold_optional_that_cannot_match_is_left_out() {
    // No triples map produces ex:missing, and orders are never customers.
    for optional in ["?c ex:missing ?x", "?c ex:amount ?x"] {
        let plan = unfold_query(
            SHOP,
            &format!("SELECT ?c ?x WHERE {{ ?c ex:name ?n OPTIONAL {{ {optional} }} }}"),
        )
        .unwrap();
        let [branch] = plan.branches.as_slice() else {
            panic!("expected one branch");
        };
        assert!(branch.left_joins.is_empty(), "{optional}");
        assert_eq!(branch.from.len(), 1);
        assert_eq!(plan.projection[1].terms, [], "{optional}");
    }
}

#[test]
fn unfold_nested_optional_is_a_left_join_inside_the_outer_one() {
    let plan = unfold_query(
        SHOP,
        "SELECT * WHERE { ?c ex:name ?n OPTIONAL { ?o ex:placedBy ?c OPTIONAL { ?o ex:amount ?a } } }",
    )
    .unwrap();
    let joins = &plan.branches[0].left_joins;
    assert_eq!(joins.len(), 2);
    assert_eq!((joins[0].tables.clone(), joins[0].parent), (1..3, None));
    assert_eq!((joins[1].tables.clone(), joins[1].parent), (2..3, Some(0)));
    assert!(joins[1].conditions.contains(&same(2, "id", 1, "id")));
    // ?o is bound by the outer part wherever the inner one binds it, so
    // it has one column.
    assert_eq!(plan.columns, ["a", "c", "n", "o"]);
}

/// SPARQL 1.1 §18.6: the inner LeftJoin is evaluated on its own, so a
/// variable it binds and the left side of the outer one also binds is only
/// compared after it, in the outer part's conditions, where unbound is
/// compatible.
#[test]
fn unfold_variable_bound_by_a_nested_optional_and_the_left_side_compares_in_the_outer_part() {
    let plan = unfold_query(
        SHOP,
        "SELECT * WHERE { ?o ex:placedBy ?c OPTIONAL { ?o ex:amount ?a OPTIONAL { ?c ex:name ?n } } }",
    )
    .unwrap();
    let joins = &plan.branches[0].left_joins;
    assert_eq!(joins.len(), 2);
    assert!(
        !joins[1]
            .conditions
            .iter()
            .any(|c| condition_reads_table(c, 0)),
        "{:?}",
        joins[1].conditions
    );
    assert!(joins[0].conditions.contains(&Condition::Or(vec![
        vec![is_null(2, "id")],
        vec![same(2, "id", 0, "customer_id")],
    ])));
}

fn condition_reads_table(condition: &Condition, table: usize) -> bool {
    let reads = |e: &SqlExpr| matches!(e, SqlExpr::Column { table: t, .. } if *t == table);
    match condition {
        Condition::Eq(l, r) => reads(l) || reads(r),
        Condition::EqAny(l, r) => reads(l) || r.iter().any(reads),
        Condition::IsNotNull(e) | Condition::IsNull(e) => reads(e),
        Condition::Or(alternatives) => alternatives
            .iter()
            .flatten()
            .any(|c| condition_reads_table(c, table)),
        Condition::SameTerm(l, r) => l
            .columns()
            .into_iter()
            .chain(r.columns())
            .any(|(t, _)| t == table),
        Condition::Filter(_) => false,
    }
}

#[test]
fn unfold_variable_unbound_on_the_left_is_compatible_with_any_value() {
    // ?c is bound by the first OPTIONAL only.
    let plan = unfold_query(
        SHOP,
        "SELECT ?o ?c WHERE { ?o ex:amount ?a OPTIONAL { ?o ex:placedBy ?c } \
         OPTIONAL { ?c ex:name ?n } }",
    )
    .unwrap();
    let joins = &plan.branches[0].left_joins;
    assert_eq!(joins.len(), 2);
    assert!(joins[1].conditions.contains(&Condition::Or(vec![
        vec![is_null(1, "customer_id")],
        vec![same(2, "id", 1, "customer_id")],
    ])));
    // Either part can bind ?c: the first one bound in a row is its value.
    assert_eq!(plan.columns, ["o", "c", "c_1"]);
    assert_eq!(plan.projection[1].terms.len(), 2);
}

#[test]
fn unfold_pattern_after_optional_joins_where_unbound_or_equal() {
    let plan = unfold_query(
        SHOP,
        "SELECT ?o ?c WHERE { ?o ex:amount ?a OPTIONAL { ?o ex:placedBy ?c } ?c ex:name ?n }",
    )
    .unwrap();
    let branch = &plan.branches[0];
    assert!(branch.conditions.contains(&Condition::Or(vec![
        vec![is_null(1, "customer_id")],
        vec![same(2, "id", 1, "customer_id")],
    ])));
    // The required pattern binds ?c in every row.
    assert_eq!(plan.columns, ["o", "c"]);
    assert_eq!(plan.projection[1].terms.len(), 1);
}

#[test]
fn unfold_optional_over_several_matching_triples_maps_splits_the_branch() {
    let mapping = with_prefixes(
        r#"<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:p ; rr:objectMap [ rr:column "v" ] ] .
<#B> rr:logicalTable [ rr:tableName "b" ] ;
  rr:subjectMap [ rr:template "http://ex/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:p ; rr:objectMap [ rr:column "w" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:q ; rr:objectMap [ rr:column "x" ] ] ."#,
    );
    let plan = unfold_query(
        &mapping,
        "SELECT * WHERE { ?s ex:q ?x OPTIONAL { ?s ex:p ?v } }",
    )
    .unwrap();
    // One branch per matching choice, where its part matched, and one
    // where none did; all of one arm, whose duplicates are removed.
    assert_eq!(plan.branches.len(), 3);
    assert!(plan.branches.iter().all(|b| b.arm == 0));
    let witness = |table| SqlExpr::Column {
        table,
        name: "id".into(),
    };
    for b in &plan.branches[..2] {
        assert_eq!(b.left_joins.len(), 1);
        assert_eq!(b.conditions[0], Condition::IsNotNull(witness(1)));
    }
    let rest = &plan.branches[2];
    assert_eq!(rest.left_joins.len(), 2);
    assert_eq!(
        rest.conditions[..2],
        [Condition::IsNull(witness(1)), Condition::IsNull(witness(2))]
    );
    // ?v is unbound where no part matched; the choices share its column.
    assert_eq!(rest.select[1], SqlExpr::Unbound);
    assert_eq!(plan.projection[1].terms.len(), 1);
    let sql = plan.to_sql().unwrap().sql;
    assert!(
        sql.contains(
            "LEFT JOIN \"a\" t1 ON t1.\"id\" = t0.\"id\" AND t1.\"v\" IS NOT NULL\n\
                      LEFT JOIN \"b\" t2 ON t2.\"id\" = t0.\"id\" AND t2.\"w\" IS NOT NULL\n\
                      WHERE t1.\"id\" IS NULL AND t2.\"id\" IS NULL"
        ),
        "{sql}"
    );
}

/// An `OPTIONAL` that splits inside another part makes the outer part's
/// choices, which split the branch in turn.
#[test]
fn unfold_split_optional_nested_in_a_part_splits_the_outer_part() {
    let mapping = with_prefixes(
        r#"<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:p ; rr:objectMap [ rr:column "v" ] ] .
<#B> rr:logicalTable [ rr:tableName "b" ] ;
  rr:subjectMap [ rr:template "http://ex/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:p ; rr:objectMap [ rr:column "w" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:q ; rr:objectMap [ rr:column "x" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:r ; rr:objectMap [ rr:column "y" ] ] ."#,
    );
    let plan = unfold_query(
        &mapping,
        "SELECT * WHERE { ?s ex:q ?x OPTIONAL { ?s ex:r ?y OPTIONAL { ?s ex:p ?v } } }",
    )
    .unwrap();
    // Three choices for the outer part (the inner one matched through A,
    // through B, or not at all), and the branch where none matched.
    assert_eq!(plan.branches.len(), 4);
    // The inner part's witness conditions decide whether the outer part
    // matches: they are in its ON clause.
    let first = &plan.branches[0];
    assert_eq!(first.left_joins[1].parent, Some(0));
    assert!(
        first.left_joins[0]
            .conditions
            .contains(&Condition::IsNotNull(SqlExpr::Column {
                table: 2,
                name: "id".into()
            }))
    );
    // Where none matched, every choice's parts are left joined: the outer
    // part with the inner one through A, through B, and with both.
    assert_eq!(plan.branches[3].left_joins.len(), 2 + 2 + 3);
}

/// A `UNION` with an empty side: its one solution extends every left
/// solution unchanged, so no branch is left for those nothing extends.
#[test]
fn unfold_optional_over_a_union_with_an_empty_side() {
    let plan =
        union_query("SELECT ?x ?v WHERE { ?x ex:q ?w OPTIONAL { { } UNION { ?x ex:p ?v } } }")
            .unwrap();
    let arms: Vec<usize> = plan.branches.iter().map(|b| b.arm).collect();
    assert_eq!(arms, [0, 1]);
    assert_eq!(plan.branches[0].left_joins.len(), 1);
    assert!(plan.branches[1].left_joins.is_empty());
    let err = union_query(
        "SELECT ?x ?v WHERE { ?x ex:q ?w OPTIONAL { { } UNION { ?x ex:p ?v } FILTER(?v) } }",
    )
    .unwrap_err();
    assert!(
        matches!(&err, Error::UnsupportedFeature(f) if f.contains("empty side")),
        "{err}"
    );
}

#[test]
fn unfold_optional_absent_from_a_branch_selects_unbound_there() {
    let mapping = with_prefixes(
        r#"<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/a/{id}" ; rr:class ex:C ] ;
  rr:predicateObjectMap [ rr:predicate ex:p ; rr:objectMap [ rr:column "v" ] ] .
<#B> rr:logicalTable [ rr:tableName "b" ] ;
  rr:subjectMap [ rr:template "http://ex/b/{id}" ; rr:class ex:C ] ."#,
    );
    let plan = unfold_query(
        &mapping,
        "SELECT DISTINCT ?v WHERE { ?s a ex:C OPTIONAL { ?s ex:p ?v } }",
    )
    .unwrap();
    assert_eq!(plan.branches.len(), 2);
    assert_eq!(plan.branches[0].left_joins.len(), 1);
    assert!(plan.branches[1].left_joins.is_empty());
    assert_eq!(plan.branches[1].select, [SqlExpr::Unbound]);
}

#[test]
fn unfold_constant_of_an_optional_part_is_selected_as_a_flag() {
    let mapping = with_prefixes(
        r#"<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:p ; rr:objectMap [ rr:column "v" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:kind ; rr:object "staff" ] ."#,
    );
    let plan = unfold_query(
        &mapping,
        "SELECT ?s ?k WHERE { ?s ex:p ?v OPTIONAL { ?s ex:kind ?k } }",
    )
    .unwrap();
    assert_eq!(plan.columns, ["s", "k"]);
    assert_eq!(
        plan.branches[0].select[1],
        SqlExpr::Matched(Box::new(SqlExpr::Column {
            table: 1,
            name: "id".into()
        }))
    );
    let [term] = plan.projection[1].terms.as_slice() else {
        panic!("{:?}", plan.projection);
    };
    assert_eq!(term.guard.as_deref(), Some("k"));
    assert!(matches!(term.value, ResultValue::Constant(_)));
}

#[test]
fn unfold_distinct_over_a_variable_of_two_optional_parts_is_unsupported() {
    let err = unfold_query(
        SHOP,
        "SELECT DISTINCT ?c WHERE { ?o ex:amount ?a OPTIONAL { ?o ex:placedBy ?c } \
         OPTIONAL { ?c ex:name ?n } }",
    )
    .unwrap_err();
    assert!(
        matches!(&err, Error::UnsupportedFeature(f) if f.contains("DISTINCT")),
        "{err}"
    );
}

#[test]
fn unfold_class_implied_only_within_the_same_part() {
    // `?c a ex:Customer` is implied by `?c ex:name ?n` from the same
    // triples map only when both are required: an OPTIONAL part may not
    // match.
    let plan = unfold_query(
        SHOP,
        "SELECT ?c WHERE { ?c a ex:Customer OPTIONAL { ?c ex:name ?n } }",
    )
    .unwrap();
    assert_eq!(plan.branches[0].from.len(), 2);
    assert_eq!(plan.branches[0].left_joins[0].tables, 1..2);
}

#[test]
fn unfold_optional_with_empty_left_side_has_only_the_left_join() {
    let plan = unfold_query(SHOP, "SELECT ?c WHERE { OPTIONAL { ?c ex:tier ?t } }").unwrap();
    let branch = &plan.branches[0];
    assert!(branch.conditions.is_empty());
    assert_eq!(branch.left_joins[0].tables, 0..1);
}

fn filters(conditions: &[Condition]) -> Vec<&Filter> {
    conditions
        .iter()
        .filter_map(|c| match c {
            Condition::Filter(f) => Some(f),
            _ => None,
        })
        .collect()
}

#[test]
fn unfold_group_filter_goes_to_the_branch_and_optional_filter_to_its_join() {
    // After the OPTIONAL: the filter applies to the whole group.
    let plan = unfold_query(
        SHOP,
        "SELECT ?c WHERE { ?c ex:name ?n OPTIONAL { ?o ex:placedBy ?c ; ex:amount ?a } FILTER(?a > 5) }",
    )
    .unwrap();
    let branch = &plan.branches[0];
    assert_eq!(filters(&branch.conditions).len(), 1);
    assert!(filters(&branch.left_joins[0].conditions).is_empty());
    // ?a is bound only where the part matched, so the filter checks that.
    assert!(matches!(
        filters(&branch.conditions)[0],
        Filter::Cases(cases) if cases.len() == 1 && cases[0].0 == [Condition::IsNotNull(column(2, "amount"))]
    ));

    // Inside the OPTIONAL: the filter decides whether the part matches,
    // and both sides are bound there.
    let plan = unfold_query(
        SHOP,
        "SELECT ?c WHERE { ?c ex:name ?n OPTIONAL { ?o ex:placedBy ?c ; ex:amount ?a FILTER(?a > 5 && ?n = \"x\") } }",
    )
    .unwrap();
    let branch = &plan.branches[0];
    assert!(filters(&branch.conditions).is_empty());
    let [Filter::And(a, n)] = filters(&branch.left_joins[0].conditions)[..] else {
        panic!("{:?}", branch.left_joins[0].conditions);
    };
    assert!(matches!(**a, Filter::Compare { .. }));
    assert!(matches!(**n, Filter::Compare { .. }));
}

#[test]
fn unfold_filter_in_a_nested_group_sees_only_its_variables() {
    let plan = unfold_query(
        SHOP,
        "SELECT ?c WHERE { ?c ex:name ?n { ?c ex:tier ?t FILTER(?n = \"x\" || BOUND(?n)) } }",
    )
    .unwrap();
    assert_eq!(
        filters(&plan.branches[0].conditions),
        [&Filter::Or(
            Box::new(Filter::Constant(None)),
            Box::new(Filter::Constant(Some(false)))
        )]
    );
}

#[test]
fn unfold_filter_in_a_nested_optional_does_not_see_the_outer_left_side() {
    let plan = unfold_query(
        SHOP,
        "SELECT * WHERE { ?c ex:name ?n OPTIONAL { ?o ex:placedBy ?c OPTIONAL { ?o ex:amount ?a FILTER(BOUND(?n) && BOUND(?c)) } } }",
    )
    .unwrap();
    let joins = &plan.branches[0].left_joins;
    assert_eq!(joins[1].parent, Some(0));
    assert_eq!(
        filters(&joins[1].conditions),
        [&Filter::And(
            Box::new(Filter::Constant(Some(false))),
            Box::new(Filter::Constant(Some(true)))
        )]
    );
}

#[test]
fn unfold_filter_over_a_variable_of_two_optional_parts_has_a_case_for_each() {
    let plan = unfold_query(
        SHOP,
        "SELECT ?o WHERE { ?o ex:amount ?a OPTIONAL { ?o ex:placedBy ?c } \
         OPTIONAL { ?c ex:name ?n } FILTER(BOUND(?c) && isIRI(?c)) }",
    )
    .unwrap();
    let branch = &plan.branches[0];
    let [Filter::And(bound, test)] = filters(&branch.conditions)[..] else {
        panic!("{:?}", branch.conditions);
    };
    let guards = vec![
        Condition::IsNotNull(column(1, "customer_id")),
        Condition::IsNotNull(column(2, "id")),
    ];
    assert_eq!(**bound, Filter::Any(guards.clone()));
    let Filter::Cases(cases) = &**test else {
        panic!("{test:?}");
    };
    let case_guards: Vec<Vec<Condition>> = cases.iter().map(|(g, _)| g.clone()).collect();
    assert_eq!(
        case_guards,
        [vec![guards[0].clone()], vec![guards[1].clone()]]
    );
}

#[test]
fn unfold_filter_compares_terms_through_template_decomposition() {
    let plan = unfold_query(
        SHOP,
        "SELECT ?o WHERE { ?o ex:placedBy ?c FILTER(?c = <http://example.org/customer/7>) }",
    )
    .unwrap();
    let [Filter::Compare { same_term, .. }] = filters(&plan.branches[0].conditions)[..] else {
        panic!();
    };
    assert_eq!(
        *same_term,
        Some(TermMatch::When(vec![Condition::Eq(
            column(0, "customer_id"),
            SqlExpr::Param("7".into())
        )]))
    );
}

const UNION_MAPPING: &str = r#"<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/{id}" ; rr:class ex:Thing ] ;
  rr:predicateObjectMap [ rr:predicate ex:p ; rr:objectMap [ rr:column "v" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:kind ; rr:object "a" ] .
<#B> rr:logicalTable [ rr:tableName "b" ] ;
  rr:subjectMap [ rr:template "http://ex/{id}" ; rr:class ex:Thing ] ;
  rr:predicateObjectMap [ rr:predicate ex:q ; rr:objectMap [ rr:column "w" ] ] ."#;

fn union_query(query: &str) -> Result<SqlPlan, Error> {
    unfold_query(&with_prefixes(UNION_MAPPING), query)
}

#[test]
fn unfold_union_gives_each_arm_its_branches() {
    let plan =
        union_query("SELECT ?x ?v ?w WHERE { { ?x ex:p ?v } UNION { ?x ex:q ?w } }").unwrap();
    let arms: Vec<usize> = plan.branches.iter().map(|b| b.arm).collect();
    assert_eq!(arms, [0, 1]);
    // A variable of one side is unbound in the other.
    assert_eq!(plan.branches[0].select[2], SqlExpr::Unbound);
    assert_eq!(plan.branches[1].select[1], SqlExpr::Unbound);
    // Bag union of the arms; duplicates are removed within each.
    assert_eq!(
        plan.to_sql().unwrap().sql,
        "SELECT DISTINCT t0.\"id\" AS \"x\", t0.\"v\" AS \"v\", NULL AS \"w\"\n\
         FROM \"a\" t0\n\
         WHERE t0.\"id\" IS NOT NULL AND t0.\"v\" IS NOT NULL\n\
         UNION ALL\n\
         SELECT DISTINCT t0.\"id\" AS \"x\", NULL AS \"v\", t0.\"w\" AS \"w\"\n\
         FROM \"b\" t0\n\
         WHERE t0.\"id\" IS NOT NULL AND t0.\"w\" IS NOT NULL"
    );
    // The same side twice counts twice; under DISTINCT once.
    let plan = union_query("SELECT ?x WHERE { { ?x ex:p ?v } UNION { ?x ex:p ?v } }").unwrap();
    assert!(plan.to_sql().unwrap().sql.contains("\nUNION ALL\n"));
    let plan =
        union_query("SELECT DISTINCT ?x WHERE { { ?x ex:p ?v } UNION { ?x ex:p ?v } }").unwrap();
    let sql = plan.to_sql().unwrap().sql;
    assert!(
        sql.contains("\nUNION\n") && !sql.contains("UNION ALL"),
        "{sql}"
    );
}

#[test]
fn unfold_union_arm_with_several_branches_is_parenthesized() {
    let plan = union_query("SELECT ?x WHERE { { ?x a ex:Thing } UNION { ?x ex:p ?v } }").unwrap();
    let arms: Vec<usize> = plan.branches.iter().map(|b| b.arm).collect();
    assert_eq!(arms, [0, 0, 1]);
    let sql = plan.to_sql().unwrap().sql;
    assert!(
        sql.starts_with("SELECT q.\"x\"\nFROM (\n(SELECT ")
            && sql.contains("\nUNION\nSELECT ")
            && sql.contains(")\nUNION ALL\nSELECT DISTINCT "),
        "{sql}"
    );
}

#[test]
fn unfold_union_with_keys_keeps_the_rows_of_proven_arms() {
    let plan =
        union_query("SELECT ?x ?v WHERE { { ?x ex:p ?v } UNION { ?x a ex:Thing } }").unwrap();
    let keys: TableKeys = ["a", "b"]
        .into_iter()
        .map(|t| {
            (
                LogicalTable::TableName(t.into()),
                vec![vec!["id".to_owned()]],
            )
        })
        .collect();
    // The second arm has two branches, so only the first can be proven.
    assert_eq!(plan.key_tables(), [LogicalTable::TableName("a".into())]);
    let sql = plan
        .to_sql_with(&RenderOptions {
            keys: Some(&keys),
            ..RenderOptions::default()
        })
        .unwrap()
        .sql;
    assert!(
        sql.starts_with("SELECT t0.\"id\" AS \"x\", t0.\"v\" AS \"v\"\nFROM \"a\" t0")
            && sql.contains("UNION ALL\n(SELECT "),
        "{sql}"
    );
    assert!(plan.has_duplicates(Some(&keys)));
}

#[test]
fn unfold_filter_in_a_union_side_applies_to_that_side_only() {
    let plan = union_query(
        "SELECT ?x WHERE { { ?x ex:p ?v FILTER(?v > 1) } UNION { ?x ex:q ?w } FILTER(BOUND(?w)) }",
    )
    .unwrap();
    let count = |b: &SqlBranch| {
        b.conditions
            .iter()
            .filter(|c| matches!(c, Condition::Filter(_)))
            .count()
    };
    assert_eq!(count(&plan.branches[0]), 2);
    assert_eq!(count(&plan.branches[1]), 1);
    // `?w` is unbound in the first side: BOUND is false there.
    assert!(
        plan.branches[0]
            .conditions
            .contains(&Condition::Filter(Filter::Constant(Some(false))))
    );
}

#[test]
fn unfold_union_side_that_cannot_match_has_no_branch() {
    let plan =
        union_query("SELECT ?x WHERE { { ?x ex:p ?v } UNION { ?x ex:missing ?v } }").unwrap();
    assert_eq!(plan.branches.len(), 1);
}

#[test]
fn unfold_constant_in_one_union_side_is_selected_as_a_flag() {
    let plan = union_query("SELECT ?k WHERE { { ?x ex:kind ?k } UNION { ?x ex:q ?w } }").unwrap();
    assert_eq!(plan.branches[0].select[0], SqlExpr::True);
    assert_eq!(
        plan.branches[1].select[0],
        SqlExpr::Null(BOOLEAN.to_owned())
    );
    assert_eq!(plan.projection[0].terms[0].guard.as_deref(), Some("k"));
}

#[test]
fn unfold_union_inside_optional_splits_where_both_sides_match() {
    let plan = union_query(
        "SELECT ?x ?v WHERE { ?x ex:q ?w OPTIONAL { { ?x ex:p ?v } UNION { ?x ex:missing ?v } } }",
    )
    .unwrap();
    assert_eq!(plan.branches.len(), 1);
    assert_eq!(plan.branches[0].left_joins.len(), 1);
    let plan = union_query(
        "SELECT ?x ?v WHERE { ?x ex:q ?w OPTIONAL { { ?x ex:p ?v } UNION { ?x ex:q ?v } } }",
    )
    .unwrap();
    // The sides' solutions are a bag: each side's branch is its own arm.
    // The branch where neither matched can share an arm with either.
    let arms: Vec<usize> = plan.branches.iter().map(|b| b.arm).collect();
    assert_eq!(arms, [0, 0, 1]);
    assert_eq!(plan.branches[1].left_joins.len(), 2);
}

#[test]
fn unfold_union_counts_arms_towards_the_branch_limit() {
    let sides: Vec<String> = (0..=MAX_BRANCHES)
        .map(|_| "{ ?x ex:p ?v }".to_owned())
        .collect();
    let err = union_query(&format!("SELECT ?x WHERE {{ {} }}", sides.join(" UNION "))).unwrap_err();
    assert_eq!(
        err,
        Error::TooManyBranches {
            limit: MAX_BRANCHES
        }
    );
}
