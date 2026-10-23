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
        .env_clear()
        .envs(std::env::vars().filter(|(k, _)| !k.starts_with("V12N_")))
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
    let term = |local: &str, tm: &str| {
        json!({
            "iri": format!("http://example.org/{local}"),
            "prefixed_name": format!("ex:{local}"),
            "triples_maps": [tm],
        })
    };
    assert_eq!(
        stdout_json(&output),
        json!({
            "prefixes": { "ex": "http://example.org/" },
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
fn terms_table_format_prints_prefixes_and_one_row_per_term() {
    let output = v12n(&["terms", "--format", "table", SHOP], "");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<_> = stdout.lines().collect();
    assert_eq!(lines.len(), 9, "{stdout}");
    assert_eq!(lines[0], "PREFIX ex: <http://example.org/>");
    assert_eq!(lines[1], "");
    assert!(lines[2].starts_with("KIND"));
    assert!(lines[3].starts_with("class     ex:Customer"), "{stdout}");
    assert!(lines[8].ends_with("<#OrderMap>"));
}

#[test]
fn terms_without_a_matching_prefix_prints_full_iris() {
    let mapping = r#"
        @prefix rr: <http://www.w3.org/ns/r2rml#> .
        [] rr:logicalTable [ rr:tableName "t" ] ;
          rr:subjectMap [ rr:template "http://ex/{id}" ; rr:class <http://other.org/Thing> ] .
    "#;
    let output = v12n(&["terms", "--format", "json", "-"], mapping);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        stdout_json(&output),
        json!({
            "prefixes": {},
            "classes": [{
                "iri": "http://other.org/Thing",
                "prefixed_name": null,
                "triples_maps": ["[]#1"],
            }],
            "properties": [],
        })
    );
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
    // Without a database there are no keys to show that `customers` has
    // no duplicate rows, so the SQL removes them.
    assert!(
        stdout.starts_with("SELECT DISTINCT t0.\"id\" AS \"c\"\nFROM \"customers\" t0"),
        "{stdout}"
    );
}

#[test]
fn explain_unsupported_feature_exits_one() {
    let output = v12n(
        &["explain", SHOP, "-"],
        "SELECT * WHERE { ?c <http://example.org/knows>+ ?d }",
    );
    assert_eq!(output.status.code(), Some(1));
    let error = stdout_json(&output);
    assert_eq!(error["error"]["kind"], json!("unsupported"));
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("property paths")
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

const RELATIVE_TEMPLATE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/w3c/rdb2rdf/D020-1table1column5rows/r2rmla.ttl"
);
const BOB: &str =
    "SELECT ?t WHERE { <http://example.com/base/Bob> a <http://xmlns.com/foaf/0.1/Person> }";

#[test]
fn explain_base_iri_from_option_or_environment_resolves_relative_templates() {
    let from_option = v12n(
        &[
            "explain",
            "--base-iri",
            "http://example.com/base/",
            RELATIVE_TEMPLATE,
            "-",
        ],
        BOB,
    );
    assert_eq!(from_option.status.code(), Some(0));
    assert_eq!(stdout_json(&from_option)["params"], json!(["Bob"]));

    let from_env = Command::new(env!("CARGO_BIN_EXE_v12n"))
        .args(["explain", RELATIVE_TEMPLATE, "-"])
        .env("V12N_BASE_IRI", "http://example.com/base/")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child.stdin.take().unwrap().write_all(BOB.as_bytes())?;
            child.wait_with_output()
        })
        .unwrap();
    assert_eq!(stdout_json(&from_env)["params"], json!(["Bob"]));

    // Without a base IRI the template only renders relative IRIs, which
    // never equal the constant.
    let without = v12n(&["explain", RELATIVE_TEMPLATE, "-"], BOB);
    assert_eq!(stdout_json(&without)["params"], json!([]));
}

#[test]
fn explain_relative_base_iri_exits_one() {
    let output = v12n(&["explain", "--base-iri", "base/", SHOP, VIP_ORDERS], "");
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("base IRI"));
}

#[test]
fn explain_missing_query_file_exits_one() {
    let output = v12n(&["explain", SHOP, "does-not-exist.rq"], "");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout_json(&output)["error"]["kind"], json!("input"));
}

#[test]
fn query_without_database_url_exits_one() {
    let output = v12n(&["query", SHOP, VIP_ORDERS], "");
    assert_eq!(output.status.code(), Some(1));
    let error = stdout_json(&output);
    assert_eq!(error["error"]["kind"], json!("input"));
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("V12N_DATABASE_URL")
    );
}

#[test]
fn query_unreachable_database_exits_three() {
    let output = v12n(
        &[
            "query",
            SHOP,
            VIP_ORDERS,
            "--db",
            "postgresql://nobody:secret-password@127.0.0.1:1/shop",
        ],
        "",
    );
    assert_eq!(output.status.code(), Some(3));
    let error = stdout_json(&output);
    assert_eq!(error["error"]["kind"], json!("database"));
    assert!(!error.to_string().contains("secret-password"), "{error}");
}

#[test]
fn query_help_documents_timeout_and_row_limit_exit_codes() {
    let output = v12n(&["query", "--help"], "");
    assert_eq!(output.status.code(), Some(0));
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("  4  statement timeout"), "{help}");
    assert!(help.contains("  5  row limit reached"), "{help}");
}

#[test]
fn query_invalid_timeout_exits_one() {
    let output = v12n(&["query", SHOP, VIP_ORDERS, "--timeout", "soon"], "");
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn query_unsupported_feature_fails_before_connecting() {
    let output = v12n(
        &["query", "--db", "postgresql://127.0.0.1:1/shop", SHOP, "-"],
        "ASK { ?c a <http://example.org/Customer> }",
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout_json(&output)["error"]["kind"], json!("unsupported"));
}

/// Runs against the `testenv` PostgreSQL; skipped unless
/// `V12N_TEST_DATABASE_URL` is set.
#[test]
fn query_shop_vip_orders_prints_sparql_json_results() {
    let Ok(url) = std::env::var("V12N_TEST_DATABASE_URL") else {
        eprintln!("skipped: V12N_TEST_DATABASE_URL is not set");
        return;
    };
    let output = v12n(&["query", SHOP, VIP_ORDERS, "--db", &url], "");
    assert_eq!(output.status.code(), Some(0));
    let results = stdout_json(&output);
    assert_eq!(results["head"], json!({ "vars": ["order", "amount"] }));
    let mut orders: Vec<&str> = results["results"]["bindings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["order"]["value"].as_str().unwrap())
        .collect();
    orders.sort_unstable();
    assert_eq!(
        orders,
        [
            "http://example.org/order/1001",
            "http://example.org/order/1003",
            "http://example.org/order/1004"
        ]
    );

    // The same settings from the environment, as `serve` reads them.
    let output = Command::new(env!("CARGO_BIN_EXE_v12n"))
        .args(["query", SHOP, VIP_ORDERS])
        .env_clear()
        .env("V12N_DATABASE_URL", &url)
        .env("V12N_ROW_LIMIT", "1")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(5));
}

fn bindings(output: &Output) -> usize {
    stdout_json(output)["results"]["bindings"]
        .as_array()
        .unwrap()
        .len()
}

#[test]
fn query_over_the_row_limit_exits_five_with_a_complete_result_and_a_warning() {
    let Ok(url) = std::env::var("V12N_TEST_DATABASE_URL") else {
        eprintln!("skipped: V12N_TEST_DATABASE_URL is not set");
        return;
    };
    let output = v12n(
        &["query", SHOP, VIP_ORDERS, "--db", &url, "--row-limit", "1"],
        "",
    );
    assert_eq!(output.status.code(), Some(5));
    // stdout parses as a whole, so it is terminated JSON.
    assert_eq!(bindings(&output), 1);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(
        stderr.starts_with("warning: result truncated at the row limit of 1;")
            && stderr.contains("--row-limit")
            && stderr.contains("V12N_ROW_LIMIT"),
        "{stderr}"
    );

    let output = v12n(
        &[
            "query",
            "--format",
            "table",
            SHOP,
            VIP_ORDERS,
            "--db",
            &url,
            "--row-limit",
            "2",
        ],
        "",
    );
    assert_eq!(output.status.code(), Some(5));
    // Header and two rows.
    assert_eq!(String::from_utf8_lossy(&output.stdout).lines().count(), 3);
    assert!(String::from_utf8_lossy(&output.stderr).starts_with("warning: "));
}

#[test]
fn query_with_exactly_row_limit_rows_is_not_truncated() {
    let Ok(url) = std::env::var("V12N_TEST_DATABASE_URL") else {
        eprintln!("skipped: V12N_TEST_DATABASE_URL is not set");
        return;
    };
    // vip-orders has three solutions.
    let output = v12n(
        &["query", SHOP, VIP_ORDERS, "--db", &url, "--row-limit", "3"],
        "",
    );
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(bindings(&output), 3);
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
}

/// A mapping over `rr:sqlQuery` rows of `id` and `pad`, plus a query file
/// for `?s <http://example.org/pad> ?p`.
fn slow_rows(sql: &str) -> (String, String) {
    let mapping = format!(
        "@prefix rr: <http://www.w3.org/ns/r2rml#> .\n\
         <#Slow> rr:logicalTable [ rr:sqlQuery \"\"\"{sql}\"\"\" ] ;\n\
         rr:subjectMap [ rr:template \"http://example.org/row/{{id}}\" ] ;\n\
         rr:predicateObjectMap [ rr:predicate <http://example.org/pad> ; rr:objectMap [ rr:column \"pad\" ] ] .\n"
    );
    let query = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("slow-rows.rq");
    std::fs::write(
        &query,
        "SELECT ?s ?p WHERE { ?s <http://example.org/pad> ?p }",
    )
    .unwrap();
    (mapping, query.to_str().unwrap().to_owned())
}

#[test]
fn query_timeout_before_the_first_row_exits_four_and_names_the_timeout() {
    let Ok(url) = std::env::var("V12N_TEST_DATABASE_URL") else {
        eprintln!("skipped: V12N_TEST_DATABASE_URL is not set");
        return;
    };
    let (mapping, query) = slow_rows("SELECT 1 AS id, 'x' AS pad FROM pg_sleep(2)");
    let output = v12n(
        &["query", "-", &query, "--db", &url, "--timeout", "200ms"],
        &mapping,
    );
    assert_eq!(output.status.code(), Some(4));
    let error = stdout_json(&output);
    assert_eq!(error["error"]["kind"], json!("timeout"));
    let message = error["error"]["message"].as_str().unwrap();
    assert!(
        message.starts_with("statement timeout of 200ms reached before the first row")
            && message.contains("V12N_STATEMENT_TIMEOUT"),
        "{message}"
    );
}

#[test]
fn query_timeout_after_output_started_exits_four_with_one_stderr_line() {
    let Ok(url) = std::env::var("V12N_TEST_DATABASE_URL") else {
        eprintln!("skipped: V12N_TEST_DATABASE_URL is not set");
        return;
    };
    // A cross join of 13 scans of the 3 customers has 3^13 (1.6 million)
    // rows, which stream: every scan is keyed by the subject, so no
    // duplicates need removing, and without a row limit no window function
    // holds rows back. Reading them takes the client far longer than the
    // timeout, and PostgreSQL waits for it.
    let patterns: String = (1..=13)
        .map(|i| format!("?c{i} <http://example.org/name> ?n{i} . "))
        .collect();
    let query = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("many-rows.rq");
    std::fs::write(&query, format!("SELECT ?n1 WHERE {{ {patterns}}}")).unwrap();
    let query = query.to_str().unwrap().to_owned();
    let mapping = std::fs::read_to_string(SHOP).unwrap();
    let output = v12n(
        &[
            "query",
            "-",
            &query,
            "--db",
            &url,
            "--timeout",
            "500ms",
            "--row-limit",
            "0",
        ],
        &mapping,
    );
    assert_eq!(output.status.code(), Some(4));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with("{\"head\""),
        "{}",
        &stdout[..stdout.len().min(80)]
    );
    assert!(serde_json::from_slice::<Value>(&output.stdout).is_err());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(
        stderr.starts_with("error: statement timeout of 500ms reached after ")
            && stderr.contains(" rows;"),
        "{stderr}"
    );
}

/// Runs `v12n serve` with only the given `V12N_*` settings; every case
/// here fails at startup, so the process exits on its own.
fn serve(args: &[&str], settings: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_v12n"));
    command.arg("serve").args(args);
    for (key, _) in std::env::vars() {
        if key.starts_with("V12N_") {
            command.env_remove(key);
        }
    }
    command.envs(settings.iter().copied());
    command.output().unwrap()
}

#[test]
fn serve_missing_config_file_exits_one() {
    let output = serve(&["--config", "does-not-exist.toml"], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stdout).contains("does-not-exist.toml"));
}

#[test]
fn serve_without_database_url_exits_one() {
    let output = serve(&[], &[("V12N_MAPPING", SHOP)]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stdout).contains("V12N_DATABASE_URL"));
}

#[test]
fn serve_invalid_mapping_exits_two() {
    let output = serve(
        &[],
        &[
            ("V12N_DATABASE_URL", "postgresql://v12n@127.0.0.1:1/shop"),
            ("V12N_MAPPING", VIP_ORDERS),
        ],
    );
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn serve_unreachable_database_exits_three_without_the_password() {
    let output = serve(
        &[],
        &[
            (
                "V12N_DATABASE_URL",
                "postgresql://v12n:hunter2@127.0.0.1:1/shop?connect_timeout=2",
            ),
            ("V12N_MAPPING", SHOP),
        ],
    );
    assert_eq!(output.status.code(), Some(3));
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!all.contains("hunter2"), "{all}");
}

#[test]
fn serve_warns_about_unrecognized_v12n_variables() {
    let output = serve(
        &[],
        &[
            ("V12N_DATABASE__URL", "postgresql://v12n@127.0.0.1:1/shop"),
            ("V12N_MAPPING", SHOP),
        ],
    );
    // The old-style name is not read, so there is no database URL.
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("WARN") && stderr.contains("V12N_DATABASE__URL"),
        "{stderr}"
    );
}

#[test]
fn serve_invalid_base_iri_exits_one_and_names_the_setting() {
    let output = serve(
        &[],
        &[
            ("V12N_DATABASE_URL", "postgresql://v12n@127.0.0.1:1/shop"),
            ("V12N_MAPPING", SHOP),
            ("V12N_BASE_IRI", "not an iri"),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stdout).contains("V12N_BASE_IRI"));
    // The option overrides the environment, and is checked as it is read.
    let output = serve(&["--base-iri", "base/"], &[("V12N_MAPPING", SHOP)]);
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn query_reads_the_statement_timeout_from_the_environment() {
    let mut command = Command::new(env!("CARGO_BIN_EXE_v12n"));
    command
        .args(["query", SHOP, VIP_ORDERS])
        .env_clear()
        .env("V12N_DATABASE_URL", "postgresql://v12n@127.0.0.1:1/shop")
        .env("V12N_STATEMENT_TIMEOUT", "soon");
    let output = command.output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("soon"));
}
