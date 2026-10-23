// SPDX-License-Identifier: Apache-2.0

use oxrdf::{BlankNode, Literal, NamedNode, Term};

use super::*;
use crate::mapping::{Mapping, MappingIndex, TemplatePart, TermType};
use crate::sql::{ProjectedTerm, ResultTerm, ResultValue, SqlPlan};
use crate::{sparql, unfold};

fn lexical(value: SqlValue) -> String {
    value.natural_lexical_form().unwrap().into_owned()
}

#[test]
fn natural_mapping_assigns_xsd_datatypes() {
    assert_eq!(SqlValue::Integer(-7).natural_datatype(), Some(XSD_INTEGER));
    assert_eq!(SqlValue::String("x".into()).natural_datatype(), None);
    assert_eq!(
        SqlValue::Boolean(true).natural_datatype(),
        Some(XSD_BOOLEAN)
    );
    assert_eq!(
        SqlValue::DateTime("2011-08-23T22:17:00Z".into()).natural_datatype(),
        Some(XSD_DATE_TIME)
    );
    assert_eq!(SqlValue::Null.natural_lexical_form(), None);
}

#[test]
fn natural_lexical_form_is_canonical() {
    assert_eq!(lexical(SqlValue::Integer(-7)), "-7");
    assert_eq!(lexical(SqlValue::Boolean(false)), "false");
    // R2RML §10.5 example.
    assert_eq!(
        lexical(SqlValue::Binary(vec![0x52, 0x32, 0x52, 0x4d, 0x4c])),
        "5232524D4C"
    );
    assert_eq!(lexical(SqlValue::Decimal("0120.50".into())), "120.5");
    // XML Schema 1.1 decimalCanonicalMap: integers have no decimal point.
    assert_eq!(lexical(SqlValue::Decimal("-0.000".into())), "0");
    assert_eq!(lexical(SqlValue::Decimal("7".into())), "7");
    assert_eq!(lexical(SqlValue::Decimal("3.00".into())), "3");
    assert_eq!(lexical(SqlValue::Decimal("-12.50".into())), "-12.5");
    assert_eq!(lexical(SqlValue::Decimal("-0.0010".into())), "-0.001");
    assert_eq!(lexical(SqlValue::Decimal("100".into())), "100");
    assert_eq!(lexical(SqlValue::Decimal("NaN".into())), "NaN");
    assert_eq!(lexical(SqlValue::Double(120.5)), "1.205E2");
    assert_eq!(lexical(SqlValue::Double(100.0)), "1.0E2");
    // The W3C R2RML test D005 expects 30 as "3.0E1"^^xsd:double.
    assert_eq!(lexical(SqlValue::Double(30.0)), "3.0E1");
    assert_eq!(lexical(SqlValue::Double(-1.5e-7)), "-1.5E-7");
    assert_eq!(lexical(SqlValue::Double(0.0)), "0.0E0");
    assert_eq!(lexical(SqlValue::Double(f64::NEG_INFINITY)), "-INF");
}

#[test]
fn canonical_decimal_rejects_non_decimals() {
    assert_eq!(canonical_decimal("+.5"), Some("0.5".into()));
    assert_eq!(canonical_decimal("3."), Some("3".into()));
    assert_eq!(canonical_decimal("-.0"), Some("0".into()));
    assert_eq!(canonical_decimal("+0010.0100"), Some("10.01".into()));
    for bad in ["", ".", "-", "1e3", "1.2.3", " 1", "Infinity"] {
        assert_eq!(canonical_decimal(bad), None, "{bad:?}");
    }
}

fn plan(columns: &[&str], projection: Vec<(&str, Option<ResultTerm>)>) -> SqlPlan {
    SqlPlan {
        columns: columns.iter().map(|c| (*c).to_owned()).collect(),
        hidden_columns: Vec::new(),
        distinct_order: Vec::new(),
        split_columns: Vec::new(),
        lexical_hidden_columns: Default::default(),
        projection: projection
            .into_iter()
            .map(|(variable, term)| ProjectedTerm {
                variable: variable.to_owned(),
                term,
            })
            .collect(),
        branches: Vec::new(),
        distinct: false,
    }
}

fn term(term_type: TermType, value: ResultValue) -> Option<ResultTerm> {
    Some(ResultTerm {
        term_type,
        value,
        datatype: None,
        language: None,
    })
}

fn template(parts: &[(&str, bool)]) -> ResultValue {
    ResultValue::Template(
        parts
            .iter()
            .map(|&(text, column)| {
                if column {
                    TemplatePart::Column(text.to_owned())
                } else {
                    TemplatePart::Literal(text.to_owned())
                }
            })
            .collect(),
    )
}

fn iri(value: &str) -> Option<Term> {
    Some(NamedNode::new(value).unwrap().into())
}

#[test]
fn build_iri_template_percent_encodes_column_values() {
    let plan = plan(
        &["s", "s_1"],
        vec![(
            "s",
            term(
                TermType::Iri,
                template(&[
                    ("http://ex/", false),
                    ("s", true),
                    ("/", false),
                    ("s_1", true),
                ]),
            ),
        )],
    );
    let builder = SolutionBuilder::new(&plan);
    assert_eq!(builder.variables(), ["s"]);
    let solution = builder
        .build(&[SqlValue::String("a b/c".into()), SqlValue::Integer(7)])
        .unwrap();
    assert_eq!(solution.values(), [iri("http://ex/a%20b%2Fc/7")]);
}

#[test]
fn build_split_column_reads_the_entry_the_row_fills() {
    let mut plan = plan(
        &["v", "w", "v_1"],
        vec![
            (
                "v",
                term(TermType::Literal, ResultValue::Column("v".into())),
            ),
            (
                "w",
                term(TermType::Literal, ResultValue::Column("w".into())),
            ),
        ],
    );
    plan.split_columns = vec![vec!["v".into(), "v_1".into()]];
    let builder = SolutionBuilder::new(&plan);
    let integer = NamedNode::new_unchecked(XSD_INTEGER);
    let build = |row: &[SqlValue]| builder.build(row).unwrap().values().to_vec();
    assert_eq!(
        build(&[SqlValue::Integer(7), SqlValue::Null, SqlValue::Null]),
        [Some(Literal::new_typed_literal("7", integer).into()), None]
    );
    assert_eq!(
        build(&[SqlValue::Null, SqlValue::Null, SqlValue::String("7".into())]),
        [Some(Literal::new_simple_literal("7").into()), None]
    );
    assert_eq!(
        build(&[SqlValue::Null, SqlValue::Null, SqlValue::Null]),
        [None, None]
    );
}

#[test]
fn build_literal_template_keeps_values_verbatim() {
    let plan = plan(
        &["l"],
        vec![(
            "l",
            term(TermType::Literal, template(&[("a b/", false), ("l", true)])),
        )],
    );
    let solution = SolutionBuilder::new(&plan)
        .build(&[SqlValue::Boolean(true)])
        .unwrap();
    assert_eq!(
        solution.values(),
        [Some(Literal::new_simple_literal("a b/true").into())]
    );
}

#[test]
fn build_null_in_column_or_template_is_unbound() {
    let plan = plan(
        &["a", "b"],
        vec![
            (
                "a",
                term(TermType::Literal, ResultValue::Column("a".into())),
            ),
            (
                "b",
                term(
                    TermType::Iri,
                    template(&[("http://ex/", false), ("b", true)]),
                ),
            ),
            ("c", None),
        ],
    );
    let solution = SolutionBuilder::new(&plan)
        .build(&[SqlValue::Null, SqlValue::Null])
        .unwrap();
    assert_eq!(solution.values(), [None, None, None]);
}

#[test]
fn build_column_literal_uses_natural_datatype_override_or_language() {
    let mut typed = term(TermType::Literal, ResultValue::Column("b".into()));
    typed.as_mut().unwrap().datatype = Some("http://ex/money".into());
    let mut tagged = term(TermType::Literal, ResultValue::Column("c".into()));
    tagged.as_mut().unwrap().language = Some("ko".into());
    let plan = plan(
        &["a", "b", "c"],
        vec![
            (
                "a",
                term(TermType::Literal, ResultValue::Column("a".into())),
            ),
            ("b", typed),
            ("c", tagged),
        ],
    );
    let solution = SolutionBuilder::new(&plan)
        .build(&[
            SqlValue::Decimal("10.50".into()),
            SqlValue::Integer(5),
            SqlValue::String("김".into()),
        ])
        .unwrap();
    assert_eq!(
        solution.values(),
        [
            Some(Literal::new_typed_literal("10.5", NamedNode::new_unchecked(XSD_DECIMAL)).into()),
            Some(
                Literal::new_typed_literal("5", NamedNode::new_unchecked("http://ex/money")).into()
            ),
            Some(
                Literal::new_language_tagged_literal("김", "ko")
                    .unwrap()
                    .into()
            ),
        ]
    );
}

#[test]
fn build_constant_and_blank_node_terms() {
    let constant: Term = NamedNode::new_unchecked("http://ex/c").into();
    let plan = plan(
        &["b"],
        vec![
            (
                "c",
                term(TermType::Iri, ResultValue::Constant(constant.clone())),
            ),
            (
                "b",
                term(TermType::BlankNode, ResultValue::Column("b".into())),
            ),
        ],
    );
    let solution = SolutionBuilder::new(&plan)
        .build(&[SqlValue::String("x 1".into())])
        .unwrap();
    assert_eq!(
        solution.values(),
        [
            Some(constant),
            Some(BlankNode::new_unchecked("b782031").into())
        ]
    );
}

#[test]
fn build_relative_iri_from_column_is_a_data_error() {
    let plan = plan(
        &["i"],
        vec![("i", term(TermType::Iri, ResultValue::Column("i".into())))],
    );
    let err = SolutionBuilder::new(&plan)
        .build(&[SqlValue::String("not an iri".into())])
        .unwrap_err();
    assert!(matches!(err, Error::InvalidTerm(_)), "{err}");
}

#[test]
fn build_from_unfolded_shop_plan() {
    let mapping =
        Mapping::from_turtle(include_str!("../../../../examples/shop/mapping.ttl")).unwrap();
    let query = sparql::parse(include_str!(
        "../../../../examples/shop/queries/vip-orders.rq"
    ))
    .unwrap();
    let plan = unfold::unfold(&query, &mapping, &MappingIndex::new(&mapping)).unwrap();
    let builder = SolutionBuilder::new(&plan);
    assert_eq!(builder.variables(), ["order", "amount"]);
    let solution = builder
        .build(&[SqlValue::Integer(1001), SqlValue::Integer(120000)])
        .unwrap();
    assert_eq!(
        solution.values(),
        [
            iri("http://example.org/order/1001"),
            Some(
                Literal::new_typed_literal("120000", NamedNode::new_unchecked(XSD_INTEGER)).into()
            )
        ]
    );
}
