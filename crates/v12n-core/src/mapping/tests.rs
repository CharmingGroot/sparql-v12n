// SPDX-License-Identifier: Apache-2.0

use oxrdf::{Literal, NamedNode};

use super::*;

const SHOP: &str = include_str!("../../../../examples/shop/mapping.ttl");

const PREFIXES: &str = "@prefix rr: <http://www.w3.org/ns/r2rml#> .
@prefix ex: <http://example.org/> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
";

fn parse(body: &str) -> Result<Mapping, Error> {
    Mapping::from_turtle(&format!("{PREFIXES}{body}"))
}

fn id(local: &str) -> TriplesMapId {
    TriplesMapId::Iri(format!("{DOCUMENT_BASE_IRI}#{local}"))
}

fn ex(local: &str) -> String {
    format!("http://example.org/{local}")
}

fn column(name: &str, term_type: TermType) -> TermMap {
    TermMap {
        value: TermMapValue::Column(name.to_owned()),
        term_type,
        datatype: None,
        language: None,
    }
}

#[test]
fn from_turtle_shop_example_builds_two_triples_maps() {
    let mapping = Mapping::from_turtle(SHOP).unwrap();
    let [customer, order] = mapping.triples_maps() else {
        panic!("expected two triples maps");
    };

    assert_eq!(customer.id, id("CustomerMap"));
    assert_eq!(customer.id.to_string(), "<#CustomerMap>");
    assert_eq!(
        customer.logical_table,
        LogicalTable::TableName("customers".into())
    );
    let TermMapValue::Template(template) = &customer.subject_map.term_map.value else {
        panic!("subject should be a template");
    };
    assert_eq!(template.columns().collect::<Vec<_>>(), ["id"]);
    assert_eq!(customer.subject_map.term_map.term_type, TermType::Iri);
    assert_eq!(customer.subject_map.classes, [ex("Customer")]);
    assert_eq!(customer.predicate_object_maps.len(), 2);
    assert_eq!(customer.predicate_object_maps[0].predicates, [ex("name")]);
    assert_eq!(
        customer.predicate_object_maps[0].object_maps,
        [ObjectMap::Term(column("name", TermType::Literal))]
    );

    assert_eq!(order.id, id("OrderMap"));
    assert_eq!(order.subject_map.classes, [ex("Order")]);
    let placed_by = &order.predicate_object_maps[1];
    assert_eq!(placed_by.predicates, [ex("placedBy")]);
    let [ObjectMap::Term(object)] = placed_by.object_maps.as_slice() else {
        panic!("expected one term object map");
    };
    assert_eq!(object.term_type, TermType::Iri);
    assert!(matches!(&object.value, TermMapValue::Template(t)
        if t.columns().eq(["customer_id"])));
    assert_eq!(mapping.triples_map(&id("OrderMap")), Some(order));
}

#[test]
fn mapping_index_shop_example_lists_terms_in_document_order() {
    let mapping = Mapping::from_turtle(SHOP).unwrap();
    let index = MappingIndex::new(&mapping);

    let classes: Vec<_> = index.classes().map(|(iri, _)| iri.to_owned()).collect();
    assert_eq!(classes, [ex("Customer"), ex("Order")]);
    assert_eq!(index.class(&ex("Customer")), [id("CustomerMap")]);
    assert_eq!(index.class(&ex("Order")), [id("OrderMap")]);
    assert!(index.class(&ex("Missing")).is_empty());

    let properties: Vec<_> = index.properties().map(|(iri, _)| iri.to_owned()).collect();
    assert_eq!(
        properties,
        [ex("name"), ex("tier"), ex("amount"), ex("placedBy")]
    );
    assert_eq!(
        index.property(&ex("placedBy")),
        [PropertyCandidate {
            triples_map: id("OrderMap"),
            predicate_object_map: 1,
        }]
    );
}

#[test]
fn mapping_index_shared_class_keeps_every_candidate() {
    let mapping = parse(
        r#"
<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/{id}" ; rr:class ex:Thing ] ;
  rr:predicateObjectMap [ rr:predicate ex:p ; rr:objectMap [ rr:column "x" ] ] .
<#B> rr:logicalTable [ rr:tableName "b" ] ;
  rr:subjectMap [ rr:template "http://ex/{id}" ; rr:class ex:Thing ] ;
  rr:predicateObjectMap [ rr:predicate ex:q ; rr:objectMap [ rr:column "y" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:p ; rr:objectMap [ rr:column "z" ] ] .
"#,
    )
    .unwrap();
    let index = MappingIndex::new(&mapping);
    assert_eq!(index.class(&ex("Thing")), [id("A"), id("B")]);
    let p: Vec<_> = index
        .property(&ex("p"))
        .iter()
        .map(|c| (c.triples_map.clone(), c.predicate_object_map))
        .collect();
    assert_eq!(p, [(id("A"), 0), (id("B"), 1)]);
}

#[test]
fn from_turtle_shortcut_properties_become_constant_term_maps() {
    let mapping = parse(
        r#"
<#Shop> rr:logicalTable [ rr:tableName "shop" ] ;
  rr:subject ex:theShop ;
  rr:predicateObjectMap [ rr:predicate ex:kind ; rr:object ex:Retail ] ;
  rr:predicateObjectMap [
    rr:predicateMap [ rr:constant ex:motto ] ;
    rr:objectMap [ rr:constant "cheap"@en ]
  ] .
"#,
    )
    .unwrap();
    let tm = &mapping.triples_maps()[0];
    assert_eq!(
        tm.subject_map.term_map,
        TermMap {
            value: TermMapValue::Constant(NamedNode::new_unchecked(ex("theShop")).into()),
            term_type: TermType::Iri,
            datatype: None,
            language: None,
        }
    );
    assert!(tm.subject_map.classes.is_empty());
    let [kind, motto] = tm.predicate_object_maps.as_slice() else {
        panic!("expected two predicate-object maps");
    };
    assert_eq!(kind.predicates, [ex("kind")]);
    assert!(matches!(&kind.object_maps[..], [ObjectMap::Term(TermMap {
        value: TermMapValue::Constant(Term::NamedNode(n)),
        term_type: TermType::Iri,
        ..
    })] if n.as_str() == ex("Retail")));
    assert_eq!(motto.predicates, [ex("motto")]);
    let literal = Literal::new_language_tagged_literal_unchecked("cheap", "en");
    assert!(matches!(&motto.object_maps[..], [ObjectMap::Term(TermMap {
        value: TermMapValue::Constant(Term::Literal(l)),
        term_type: TermType::Literal,
        ..
    })] if *l == literal));
}

#[test]
fn from_turtle_referencing_object_map_keeps_join_conditions() {
    let mapping = parse(
        r#"
<#Customer> rr:logicalTable [ rr:tableName "customers" ] ;
  rr:subjectMap [ rr:template "http://ex/customer/{id}" ] .
<#Order> a rr:TriplesMap ;
  rr:logicalTable [ rr:sqlQuery "SELECT id, customer_id, region FROM orders" ] ;
  rr:subjectMap [ rr:template "http://ex/order/{id}" ] ;
  rr:predicateObjectMap [
    rr:predicate ex:placedBy ;
    rr:objectMap [
      rr:parentTriplesMap <#Customer> ;
      rr:joinCondition [ rr:child "customer_id" ; rr:parent "id" ] ;
      rr:joinCondition [ rr:child "region" ; rr:parent "region" ]
    ]
  ] .
"#,
    )
    .unwrap();
    let order = mapping.triples_map(&id("Order")).unwrap();
    assert_eq!(
        order.logical_table,
        LogicalTable::SqlQuery("SELECT id, customer_id, region FROM orders".into())
    );
    assert_eq!(
        order.predicate_object_maps[0].object_maps,
        [ObjectMap::Reference(RefObjectMap {
            parent_triples_map: id("Customer"),
            join_conditions: vec![
                JoinCondition {
                    child: "customer_id".into(),
                    parent: "id".into(),
                },
                JoinCondition {
                    child: "region".into(),
                    parent: "region".into(),
                },
            ],
        })]
    );
}

#[test]
fn from_turtle_datatype_language_and_term_type_are_read() {
    let mapping = parse(
        r#"
_:people rr:logicalTable [ rr:tableName "people" ] ;
  rr:subjectMap [ rr:column "id" ; rr:termType rr:BlankNode ] ;
  rr:predicateObjectMap [ rr:predicate ex:age ;
    rr:objectMap [ rr:column "age" ; rr:datatype xsd:integer ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:bio ;
    rr:objectMap [ rr:template "{first} {last}" ; rr:language "en" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:home ;
    rr:objectMap [ rr:column "home" ; rr:termType rr:IRI ] ] .
"#,
    )
    .unwrap();
    let tm = &mapping.triples_maps()[0];
    assert_eq!(tm.id, TriplesMapId::BlankNode("people".into()));
    assert_eq!(tm.id.to_string(), "_:people");
    assert_eq!(tm.subject_map.term_map.term_type, TermType::BlankNode);

    let objects: Vec<_> = tm
        .predicate_object_maps
        .iter()
        .map(|pom| match &pom.object_maps[..] {
            [ObjectMap::Term(t)] => (t.term_type, t.datatype.clone(), t.language.clone()),
            other => panic!("unexpected object maps {other:?}"),
        })
        .collect();
    assert_eq!(
        objects,
        [
            (
                TermType::Literal,
                Some("http://www.w3.org/2001/XMLSchema#integer".into()),
                None
            ),
            (TermType::Literal, None, Some("en".into())),
            (TermType::Iri, None, None),
        ]
    );
}

#[test]
fn from_turtle_language_tags_are_kept_in_lower_case() {
    // RDF 1.1 compares language tags case-insensitively, and query
    // constants and results carry them in lower case.
    let mapping = parse(
        r#"
<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:p ;
    rr:objectMap [ rr:column "x" ; rr:language "EN-us" ] ] .
"#,
    )
    .unwrap();
    match &mapping.triples_maps()[0].predicate_object_maps[0].object_maps[..] {
        [ObjectMap::Term(t)] => assert_eq!(t.language.as_deref(), Some("en-us")),
        other => panic!("unexpected object maps {other:?}"),
    }
}

#[test]
fn from_turtle_language_tags_need_a_registrable_primary_subtag() {
    let mapping = |tag: &str| {
        format!(
            r#"
<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/{{id}}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:p ;
    rr:objectMap [ rr:column "x" ; rr:language "{tag}" ] ] .
"#
        )
    };
    for tag in [
        "en",
        "gsw",
        "zh-Hant-TW",
        "sgn-BE-FR",
        "i-klingon",
        "x-private",
    ] {
        parse(&mapping(tag)).unwrap_or_else(|e| panic!("{tag}: {e}"));
    }
    // Well-formed BCP 47, but no primary language subtag of 4 to 8 letters
    // is registered (W3C test R2RMLTC0015b uses "english" and "spanish").
    for tag in ["english", "spanish", "abcd", "abcdefgh-US"] {
        let err = parse_err(&mapping(tag));
        assert!(matches!(err, Error::InvalidMapping(_)), "{tag} => {err}");
        assert!(err.to_string().contains("letters"), "{tag} => {err}");
    }
}

#[test]
fn from_turtle_repeated_triples_do_not_duplicate_classes() {
    let mapping = parse(
        r#"
<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap <#S> .
<#S> rr:template "http://ex/{id}" ; rr:class ex:Thing, ex:Thing .
<#S> rr:class ex:Thing .
"#,
    )
    .unwrap();
    assert_eq!(mapping.triples_maps()[0].subject_map.classes, [ex("Thing")]);
}

fn parse_err(body: &str) -> Error {
    parse(body).expect_err("mapping should be rejected")
}

#[test]
fn from_turtle_missing_logical_table_errors() {
    let err = parse_err(r#"<#A> rr:subjectMap [ rr:template "http://ex/{id}" ] ."#);
    assert_eq!(err, Error::MissingLogicalTable("<#A>".into()));
}

#[test]
fn from_turtle_missing_subject_map_errors() {
    let err = parse_err(r#"<#A> rr:logicalTable [ rr:tableName "a" ] ."#);
    assert_eq!(err, Error::MissingSubjectMap("<#A>".into()));
}

#[test]
fn from_turtle_unknown_parent_triples_map_errors() {
    let err = parse_err(
        r#"
<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:p ;
    rr:objectMap [ rr:parentTriplesMap <#Nope> ] ] .
"#,
    );
    assert_eq!(
        err,
        Error::UnknownParentTriplesMap {
            triples_map: "<#A>".into(),
            parent: "<#Nope>".into(),
        }
    );
}

#[test]
fn from_turtle_malformed_template_errors() {
    let err = parse_err(
        r#"<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/{id" ] ."#,
    );
    assert!(
        matches!(&err, Error::InvalidTemplate { template, .. } if template == "http://ex/{id"),
        "{err}"
    );
}

#[test]
fn from_turtle_bad_turtle_is_a_syntax_error() {
    let err = parse_err("<#A> rr:logicalTable [ rr:tableName \"a\" ");
    assert!(matches!(err, Error::MappingSyntax(_)), "{err}");
}

#[test]
fn from_turtle_no_triples_maps_errors() {
    let err = parse_err("ex:a ex:b ex:c .");
    assert!(matches!(err, Error::InvalidMapping(_)), "{err}");
}

#[test]
fn from_turtle_named_graphs_are_unsupported() {
    let err = parse_err(
        r#"<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/{id}" ; rr:graph ex:g ] ."#,
    );
    assert!(matches!(err, Error::UnsupportedFeature(_)), "{err}");
}

#[test]
fn from_turtle_non_constant_predicate_map_is_unsupported() {
    let err = parse_err(
        r#"<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/{id}" ] ;
  rr:predicateObjectMap [ rr:predicateMap [ rr:column "p" ] ; rr:object ex:o ] ."#,
    );
    assert!(matches!(err, Error::UnsupportedFeature(_)), "{err}");
}

#[test]
fn from_turtle_invalid_constructs_error() {
    let cases = [
        // Typo in an rr: property.
        r#"<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:tempate "http://ex/{id}" ] ."#,
        r#"<#A> rr:logicalTable [ rr:tableName "a", "b" ] ;
  rr:subjectMap [ rr:template "http://ex/{id}" ] ."#,
        r#"<#A> rr:logicalTable [ rr:tableName "a" ; rr:sqlQuery "SELECT 1" ] ;
  rr:subjectMap [ rr:template "http://ex/{id}" ] ."#,
        r#"<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/{id}" ; rr:column "id" ] ."#,
        r#"<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:column "id" ; rr:termType rr:Literal ] ."#,
        r#"<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:p ;
    rr:objectMap [ rr:column "x" ; rr:datatype xsd:string ; rr:language "en" ] ] ."#,
        r#"<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:p ;
    rr:objectMap [ rr:column "x" ; rr:language "not a tag" ] ] ."#,
        r#"<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:p ;
    rr:objectMap [ rr:column "x" ; rr:datatype <http://www.w3.org/1999/02/22-rdf-syntax-ns#langString> ] ] ."#,
        r#"<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/{id}" ] ;
  rr:predicateObjectMap [ rr:objectMap [ rr:column "x" ] ] ."#,
        r#"<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:p ] ."#,
        r#"<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:p ;
    rr:objectMap [ rr:parentTriplesMap <#A> ; rr:joinCondition [ rr:child "x" ] ] ] ."#,
    ];
    for case in cases {
        let err = parse_err(case);
        assert!(matches!(err, Error::InvalidMapping(_)), "{case}\n=> {err}");
        assert!(err.to_string().contains("<#A>"), "{err}");
    }
}

const ANONYMOUS: &str = r#"
[] rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/a/{id}" ] .
_:named rr:logicalTable [ rr:tableName "b" ] ;
  rr:subjectMap [ rr:template "http://ex/b/{id}" ] .
[ rr:logicalTable [ rr:tableName "c" ] ;
  rr:subjectMap [ rr:template "http://ex/c/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:p ;
    rr:objectMap [ rr:parentTriplesMap [
      rr:logicalTable [ rr:tableName "d" ] ;
      rr:subjectMap [ rr:template "http://ex/d/{id}" ] ] ;
      rr:joinCondition [ rr:child "d_id" ; rr:parent "id" ] ] ] ] .
"#;

#[test]
fn from_turtle_anonymous_triples_maps_get_ids_from_document_order() {
    let first = parse(ANONYMOUS).unwrap();
    let ids: Vec<_> = first
        .triples_maps()
        .iter()
        .map(|tm| tm.id.to_string())
        .collect();
    assert_eq!(ids, ["[]#1", "_:named", "[]#3", "[]#4"]);
    let [.., c, d] = first.triples_maps() else {
        panic!("expected four triples maps");
    };
    assert_eq!(d.logical_table, LogicalTable::TableName("d".into()));
    assert_eq!(
        c.predicate_object_maps[0].object_maps[0],
        ObjectMap::Reference(RefObjectMap {
            parent_triples_map: TriplesMapId::Anonymous(4),
            join_conditions: vec![JoinCondition {
                child: "d_id".into(),
                parent: "id".into(),
            }],
        })
    );
    // The parser labels `[]` randomly; the IDs must not follow it.
    assert_eq!(parse(ANONYMOUS).unwrap(), first);
}

#[test]
fn from_turtle_errors_in_anonymous_nodes_are_reproducible() {
    let body = r#"[] rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:tempate "http://ex/{id}" ] ."#;
    let err = parse_err(body);
    assert_eq!(
        err.to_string(),
        "invalid mapping: triples map []#1: unexpected property rr:tempate on []"
    );
    assert_eq!(parse_err(body), err);
}

fn referencing_without_join(parent_table: &str) -> String {
    format!(
        r#"
<#Child> rr:logicalTable [ rr:tableName "people" ] ;
  rr:subjectMap [ rr:template "http://ex/person/{{id}}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:dept ;
    rr:objectMap [ rr:parentTriplesMap <#Parent> ] ] .
<#Parent> rr:logicalTable [ {parent_table} ] ;
  rr:subjectMap [ rr:template "http://ex/dept/{{dept}}" ] .
"#
    )
}

#[test]
fn from_turtle_reference_without_join_condition_needs_the_same_logical_table() {
    for same in [
        r#"rr:tableName "people""#,
        // Identical effective SQL query (R2RML §5.1, §8).
        r#"rr:sqlQuery "SELECT * FROM people""#,
    ] {
        parse(&referencing_without_join(same)).unwrap_or_else(|e| panic!("{same}: {e}"));
    }
    for different in [
        r#"rr:tableName "depts""#,
        r#"rr:tableName "\"people\"""#,
        r#"rr:sqlQuery "SELECT * FROM people WHERE active""#,
    ] {
        let err = parse_err(&referencing_without_join(different));
        assert!(
            matches!(err, Error::InvalidMapping(_)),
            "{different}: {err}"
        );
        let message = err.to_string();
        assert!(
            message.contains("<#Child>")
                && message.contains("<#Parent>")
                && message.contains("rr:joinCondition"),
            "{message}"
        );
    }
}

#[test]
fn compact_iri_uses_the_longest_namespace() {
    let mapping = parse(
        r#"
@prefix shop: <http://example.org/shop/> .
@prefix a: <http://example.org/> .
<#A> rr:logicalTable [ rr:tableName "a" ] ;
  rr:subjectMap [ rr:template "http://ex/{id}" ] .
"#,
    )
    .unwrap();
    assert_eq!(
        mapping.prefixes().get("shop").map(String::as_str),
        Some("http://example.org/shop/")
    );
    let compact = |iri: &str| mapping.compact_iri(iri);
    assert_eq!(
        compact("http://example.org/shop/Order").as_deref(),
        Some("shop:Order")
    );
    // `a:` and `ex:` share a namespace; the smaller prefix name wins.
    assert_eq!(
        compact("http://example.org/Customer").as_deref(),
        Some("a:Customer")
    );
    assert_eq!(
        compact("http://www.w3.org/2001/XMLSchema#integer").as_deref(),
        Some("xsd:integer")
    );
    for iri in [
        "http://other.org/x",
        "http://example.org/a/b",
        "http://example.org/x?y",
        "http://example.org/.x",
        "http://example.org/x.",
    ] {
        assert_eq!(compact(iri), None, "{iri}");
    }
}
