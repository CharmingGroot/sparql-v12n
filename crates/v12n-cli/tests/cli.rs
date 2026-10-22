// SPDX-License-Identifier: Apache-2.0

use std::io::Write;
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

const SHOP: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../examples/shop/mapping.ttl"
);

fn v12n(args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_v12n"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn v12n");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("stdout should be JSON")
}

#[test]
fn check_valid_mapping_exits_zero_with_json_summary() {
    let output = v12n(&["check", SHOP], "");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        stdout_json(&output),
        json!({ "ok": true, "triples_maps": 2, "classes": 2, "properties": 4 })
    );
}

#[test]
fn check_invalid_mapping_from_stdin_exits_two_with_json_error() {
    let mapping = "<#A> <http://www.w3.org/ns/r2rml#logicalTable> [ <http://www.w3.org/ns/r2rml#tableName> \"a\" ] .";
    let output = v12n(&["check", "--format", "json", "-"], mapping);
    assert_eq!(output.status.code(), Some(2));
    let error = stdout_json(&output);
    assert_eq!(error["ok"], json!(false));
    assert_eq!(error["error"]["kind"], json!("mapping"));
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("rr:subjectMap")
    );
}

#[test]
fn check_invalid_mapping_table_format_reports_on_stderr() {
    let output = v12n(&["check", "--format", "table", "-"], "not turtle");
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).starts_with("error: mapping syntax error"));
}

#[test]
fn check_missing_file_exits_one() {
    let output = v12n(&["check", "does-not-exist.ttl"], "");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout_json(&output)["error"]["kind"], json!("input"));
}

#[test]
fn check_without_mapping_argument_exits_one() {
    let output = v12n(&["check"], "");
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn terms_json_lists_classes_and_properties_in_order() {
    let output = v12n(&["terms", "--format", "json", SHOP], "");
    assert_eq!(output.status.code(), Some(0));
    let term = |iri: &str, tm: &str| json!({ "iri": format!("http://example.org/{iri}"), "triples_maps": [tm] });
    assert_eq!(
        stdout_json(&output),
        json!({
            "classes": [term("Customer", "<#CustomerMap>"), term("Order", "<#OrderMap>")],
            "properties": [
                term("name", "<#CustomerMap>"),
                term("tier", "<#CustomerMap>"),
                term("amount", "<#OrderMap>"),
                term("placedBy", "<#OrderMap>"),
            ],
        })
    );
}

#[test]
fn terms_table_format_prints_one_row_per_term() {
    let output = v12n(&["terms", "--format", "table", SHOP], "");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<_> = stdout.lines().collect();
    assert_eq!(lines.len(), 7);
    assert!(lines[0].starts_with("KIND"));
    assert!(lines[1].starts_with("class     <http://example.org/Customer>"));
    assert!(lines[6].ends_with("<#OrderMap>"));
}

const VIP_ORDERS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../examples/shop/queries/vip-orders.rq"
);

#[test]
fn explain_json_prints_sql_params_and_variables() {
    let output = v12n(&["explain", SHOP, VIP_ORDERS], "");
    assert_eq!(output.status.code(), Some(0));
    let plan = stdout_json(&output);
    let sql = plan["sql"].as_str().unwrap();
    assert!(
        sql.contains(r#"JOIN "orders" t1 ON t1."customer_id" = t0."id""#),
        "{sql}"
    );
    assert!(sql.contains(r#"t0."tier" = $1"#), "{sql}");
    assert_eq!(plan["params"], json!(["VIP"]));
    assert_eq!(
        plan["variables"],
        json!([
            { "name": "order", "term_type": "iri", "columns": ["order"] },
            { "name": "amount", "term_type": "literal", "columns": ["amount"] },
        ])
    );
}

#[test]
fn explain_query_from_stdin_table_format_prints_sql() {
    let output = v12n(
        &["explain", "--format", "table", SHOP, "-"],
        "SELECT ?c WHERE { ?c a <http://example.org/Customer> }",
    );
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.starts_with("SELECT t0.\"id\" AS \"c\"\nFROM \"customers\" t0"),
        "{stdout}"
    );
}

#[test]
fn explain_unsupported_feature_exits_one() {
    let output = v12n(
        &["explain", SHOP, "-"],
        "SELECT * WHERE { ?c <http://example.org/name> ?n OPTIONAL { ?c <http://example.org/tier> ?t } }",
    );
    assert_eq!(output.status.code(), Some(1));
    let error = stdout_json(&output);
    assert_eq!(error["error"]["kind"], json!("unsupported"));
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("OPTIONAL")
    );
}

#[test]
fn explain_query_syntax_error_exits_one() {
    let output = v12n(&["explain", SHOP, "-"], "SELECT ?x WHERE {");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout_json(&output)["error"]["kind"], json!("query"));
}

#[test]
fn explain_invalid_mapping_exits_two() {
    let output = v12n(&["explain", "-", VIP_ORDERS], "not turtle");
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stdout_json(&output)["error"]["kind"], json!("mapping"));
}

#[test]
fn explain_missing_query_file_exits_one() {
    let output = v12n(&["explain", SHOP, "does-not-exist.rq"], "");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout_json(&output)["error"]["kind"], json!("input"));
}
