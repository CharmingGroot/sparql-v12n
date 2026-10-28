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
fn version_names_the_commit() {
    let output = v12n(&["--version"], "");
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout).unwrap();
    let expected = format!("v12n {} (", env!("CARGO_PKG_VERSION"));
    assert!(text.starts_with(&expected), "{text}");
    assert!(text.trim_end().ends_with(')'), "{text}");
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
fn explain_plan_that_cannot_be_rendered_exits_one() {
    let case = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/diff/0006-filter");
    let output = v12n(
        &[
            "explain",
            &format!("{case}/mapping.ttl"),
            &format!("{case}/queries/language-tag-not-equal.rq"),
        ],
        "",
    );
    assert_eq!(output.status.code(), Some(1));
    let error = stdout_json(&output);
    assert_eq!(error["error"]["kind"], json!("unsupported"));
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("under negation")
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

/// Runs against the PostgreSQL that `testenv/tls.sh up` starts; skipped
/// unless `V12N_TEST_TLS_DATABASE_URL` and `V12N_TEST_TLS_CERTS` are set.
#[test]
fn query_over_tls_checks_the_certificate_and_fails_with_exit_three() {
    let (Ok(url), Ok(certs)) = (
        std::env::var("V12N_TEST_TLS_DATABASE_URL"),
        std::env::var("V12N_TEST_TLS_CERTS"),
    ) else {
        eprintln!("skipped: V12N_TEST_TLS_DATABASE_URL or V12N_TEST_TLS_CERTS is not set");
        return;
    };
    let db = format!("{url}&sslmode=verify-full&sslrootcert={certs}/ca.pem");
    let output = v12n(&["query", SHOP, VIP_ORDERS, "--db", &db], "");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(bindings(&output), 3);

    let db = format!("{url}&sslmode=verify-full&sslrootcert={certs}/wrong-ca.pem");
    let output = v12n(&["query", SHOP, VIP_ORDERS, "--db", &db], "");
    assert_eq!(output.status.code(), Some(3));
    let error = &stdout_json(&output)["error"];
    assert_eq!(error["kind"], json!("database"));
    let message = error["message"].as_str().unwrap();
    assert!(message.contains("TLS handshake"), "{message}");
    // The URL holds a password; the message must not repeat it.
    let credentials = url.split_once("://").unwrap().1.split_once('@').unwrap().0;
    let password = credentials.split_once(':').unwrap().1;
    assert!(!message.contains(password), "{message}");
}

/// A query `LIMIT` equal to the row limit is a complete result (exit 0);
/// one above it is truncation when the rows exist (exit 5).
#[test]
fn query_limit_at_the_row_limit_exits_zero_and_above_it_five() {
    let Ok(url) = std::env::var("V12N_TEST_DATABASE_URL") else {
        eprintln!("skipped: V12N_TEST_DATABASE_URL is not set");
        return;
    };
    // vip-orders has three solutions.
    let vip = std::fs::read_to_string(VIP_ORDERS).unwrap();
    for (limit, row_limit, code, rows) in [
        (2, "2", 0, 2),
        (3, "2", 5, 2),
        (3, "3", 0, 3),
        (4, "3", 0, 3),
    ] {
        let query = format!("{vip} ORDER BY ?order LIMIT {limit}");
        let output = v12n(
            &[
                "query",
                SHOP,
                "--query",
                &query,
                "--db",
                &url,
                "--row-limit",
                row_limit,
            ],
            "",
        );
        assert_eq!(
            output.status.code(),
            Some(code),
            "LIMIT {limit}, row limit {row_limit}"
        );
        assert_eq!(
            bindings(&output),
            rows,
            "LIMIT {limit}, row limit {row_limit}"
        );
        assert_eq!(output.stderr.is_empty(), code == 0, "{:?}", output.stderr);
    }
}

#[test]
fn query_writes_xml_and_tsv_results() {
    let Ok(url) = std::env::var("V12N_TEST_DATABASE_URL") else {
        eprintln!("skipped: V12N_TEST_DATABASE_URL is not set");
        return;
    };
    let query = "SELECT ?c ?name WHERE { ?c <http://example.org/name> ?name } ORDER BY ?name";
    let run = |format: &str| {
        let output = v12n(
            &[
                "query", "--format", format, "--db", &url, SHOP, "--query", query,
            ],
            "",
        );
        assert_eq!(output.status.code(), Some(0), "{format}");
        assert!(output.stderr.is_empty(), "{format}");
        String::from_utf8(output.stdout).unwrap()
    };
    let customer = |id: u8, name: &str| {
        format!(
            "<result><binding name=\"c\"><uri>http://example.org/customer/{id}</uri></binding>\
             <binding name=\"name\"><literal>{name}</literal></binding></result>"
        )
    };
    assert_eq!(
        run("xml"),
        format!(
            "<?xml version=\"1.0\"?>\n\
             <sparql xmlns=\"http://www.w3.org/2005/sparql-results#\"><head>\
             <variable name=\"c\"/><variable name=\"name\"/></head><results>\n\
             {}\n{}\n{}\n</results></sparql>\n",
            customer(7, "Kim"),
            customer(8, "Lee"),
            customer(9, "Park")
        )
    );
    assert_eq!(
        run("tsv"),
        "?c\t?name\n\
         <http://example.org/customer/7>\t\"Kim\"\n\
         <http://example.org/customer/8>\t\"Lee\"\n\
         <http://example.org/customer/9>\t\"Park\"\n"
    );
}

/// Every streamed format holds a complete result of `--row-limit`
/// solutions when the row limit cuts the answer off, and exits with 5.
#[test]
fn query_over_the_row_limit_exits_five_in_every_format() {
    let Ok(url) = std::env::var("V12N_TEST_DATABASE_URL") else {
        eprintln!("skipped: V12N_TEST_DATABASE_URL is not set");
        return;
    };
    for (format, complete) in [
        ("json", "\n]}}\n"),
        ("xml", "</result>\n</results></sparql>\n"),
        ("csv", "\r\n"),
        ("tsv", "\n"),
    ] {
        let output = v12n(
            &[
                "query",
                "--format",
                format,
                SHOP,
                VIP_ORDERS,
                "--db",
                &url,
                "--row-limit",
                "2",
            ],
            "",
        );
        assert_eq!(output.status.code(), Some(5), "{format}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.ends_with(complete), "{format}: {stdout}");
        let solutions = match format {
            "json" => stdout.matches("\n{").count(),
            "xml" => stdout.matches("<result>").count(),
            // The header line and one line per solution.
            _ => stdout.lines().count() - 1,
        };
        assert_eq!(solutions, 2, "{format}: {stdout}");
        assert!(
            String::from_utf8_lossy(&output.stderr).starts_with("warning: result truncated"),
            "{format}"
        );
    }
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
    for format in ["json", "xml"] {
        let output = v12n(
            &[
                "query",
                "--format",
                format,
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
        assert_eq!(output.status.code(), Some(4), "{format}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let start = &stdout[..stdout.len().min(80)];
        // Neither document is closed, so neither passes for a result.
        if format == "json" {
            assert!(stdout.starts_with("{\"head\""), "{start}");
            assert!(serde_json::from_slice::<Value>(&output.stdout).is_err());
        } else {
            assert!(stdout.starts_with("<?xml "), "{start}");
            assert!(!stdout.contains("</results>"), "{start}");
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(stderr.lines().count(), 1, "{format}: {stderr}");
        assert!(
            stderr.starts_with("error: statement timeout of 500ms reached after ")
                && stderr.contains(" rows;"),
            "{stderr}"
        );
    }
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

const WORKSPACE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

/// Asserts the JSON error shape on stdout: `ok`, `kind`, `message` and
/// an `exit_code` equal to the process exit code.
fn assert_json_error(output: &Output, code: i32, kind: &str) -> Value {
    assert_eq!(
        output.status.code(),
        Some(code),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let error = stdout_json(output);
    assert_eq!(error["ok"], json!(false), "{error}");
    assert_eq!(error["error"]["kind"], json!(kind), "{error}");
    assert_eq!(error["error"]["exit_code"], json!(code), "{error}");
    assert!(error["error"]["message"].is_string(), "{error}");
    error["error"].clone()
}

#[test]
fn explain_query_text_option_matches_the_query_file() {
    let text = std::fs::read_to_string(VIP_ORDERS).unwrap();
    let from_file = v12n(&["explain", SHOP, VIP_ORDERS], "");
    let from_text = v12n(&["explain", SHOP, "--query", &text], "");
    assert_eq!(from_text.status.code(), Some(0));
    assert_eq!(stdout_json(&from_text), stdout_json(&from_file));
}

#[test]
fn explain_query_file_and_text_together_exit_one() {
    let output = v12n(&["explain", SHOP, VIP_ORDERS, "--query", "SELECT * {}"], "");
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--query"));
}

#[test]
fn mapping_and_query_both_from_stdin_exit_one() {
    let output = v12n(&["explain", "-", "-"], "");
    assert_json_error(&output, 1, "input");
}

#[test]
fn json_error_for_an_unreadable_file_names_the_path() {
    let output = v12n(&["explain", SHOP, "does-not-exist.rq"], "");
    let error = assert_json_error(&output, 1, "input");
    assert_eq!(error["details"], json!({ "path": "does-not-exist.rq" }));
    assert!(output.stderr.is_empty());
}

#[test]
fn csv_format_errors_go_to_stderr() {
    let output = v12n(&["terms", "--format", "csv", "-"], "not turtle");
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).starts_with("error: mapping syntax error"));
}

#[test]
fn results_only_formats_are_refused_outside_query() {
    for (command, format) in [("check", "xml"), ("terms", "tsv"), ("explain", "xml")] {
        let mut args = vec![command, "--format", format, SHOP];
        if command == "explain" {
            args.push(VIP_ORDERS);
        }
        let output = v12n(&args, "");
        assert_eq!(output.status.code(), Some(1), "{command}");
        assert!(output.stdout.is_empty(), "{command}");
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            format!("error: --format {format} is for `query` only; use json, csv or table\n")
        );
    }
}

#[test]
fn check_csv_is_one_record_with_a_header() {
    let output = v12n(&["check", "--format", "csv", SHOP], "");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "triples_maps,classes,properties\r\n2,2,4\r\n"
    );
}

#[test]
fn terms_csv_has_one_record_per_term() {
    let output = v12n(&["terms", "--format", "csv", SHOP], "");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    let records: Vec<&str> = stdout.split_terminator("\r\n").collect();
    assert_eq!(records.len(), 7, "{stdout}");
    assert_eq!(records[0], "kind,iri,prefixed_name,triples_maps");
    assert_eq!(
        records[1],
        "class,http://example.org/Customer,ex:Customer,<#CustomerMap>"
    );
    assert_eq!(
        records[6],
        "property,http://example.org/placedBy,ex:placedBy,<#OrderMap>"
    );
}

#[test]
fn explain_csv_puts_sql_and_params_in_one_record() {
    let output = v12n(&["explain", "--format", "csv", SHOP, VIP_ORDERS], "");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json = stdout_json(&v12n(&["explain", SHOP, VIP_ORDERS], ""));
    let sql = json["sql"].as_str().unwrap();
    // The SQL holds quotes and line breaks, so it is quoted (RFC 4180).
    assert_eq!(
        stdout,
        format!("sql,$1\r\n\"{}\",VIP\r\n", sql.replace('"', "\"\""))
    );
}

#[test]
fn help_lists_two_or_three_examples_for_every_command() {
    for command in ["check", "terms", "explain", "query", "serve"] {
        let help = String::from_utf8(v12n(&[command, "--help"], "").stdout).unwrap();
        let examples = help
            .lines()
            .skip_while(|l| !l.starts_with("Examples"))
            .skip(1)
            .take_while(|l| l.starts_with("  "))
            .count();
        assert!((2..=3).contains(&examples), "{command}: {examples}\n{help}");
    }
    let help = String::from_utf8(v12n(&["--help"], "").stdout).unwrap();
    for needle in ["NO_COLOR", "\"exit_code\"", "  5  row limit reached"] {
        assert!(help.contains(needle), "{needle}\n{help}");
    }
}

/// The help examples of the commands that need no database run as they
/// are from the repository root.
#[test]
fn help_examples_without_a_database_run() {
    let bin = std::path::Path::new(env!("CARGO_BIN_EXE_v12n"))
        .parent()
        .unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut ran = 0;
    for command in ["check", "terms", "explain"] {
        let help = String::from_utf8(v12n(&[command, "--help"], "").stdout).unwrap();
        for example in help
            .lines()
            .skip_while(|l| !l.starts_with("Examples"))
            .skip(1)
            .take_while(|l| l.starts_with("  "))
        {
            let output = Command::new("sh")
                .args(["-c", example.trim()])
                .current_dir(WORKSPACE)
                .env("PATH", &path)
                .env_remove("V12N_BASE_IRI")
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(0),
                "{example}\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!output.stdout.is_empty(), "{example}");
            ran += 1;
        }
    }
    assert_eq!(ran, 8);
}

/// The loop an agent runs, all in JSON: `terms` to find the IRIs, `explain`
/// to check the query, `query` to run it; then one case per exit code.
/// Runs against the `testenv` PostgreSQL; skipped unless
/// `V12N_TEST_DATABASE_URL` is set.
#[test]
fn agent_loop_terms_explain_query_in_json_and_every_exit_code() {
    let Ok(url) = std::env::var("V12N_TEST_DATABASE_URL") else {
        eprintln!("skipped: V12N_TEST_DATABASE_URL is not set");
        return;
    };

    // terms: pick the IRIs by their prefixed names.
    let terms = v12n(&["terms", "--format", "json", SHOP], "");
    assert_eq!(terms.status.code(), Some(0));
    let terms = stdout_json(&terms);
    let iri = |list: &str, name: &str| -> String {
        terms[list]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["prefixed_name"] == json!(name))
            .and_then(|t| t["iri"].as_str())
            .unwrap_or_else(|| panic!("{name} in {list}"))
            .to_owned()
    };
    let sparql = format!(
        "SELECT ?customer ?name WHERE {{ ?customer a <{}> ; <{}> ?name ; <{}> \"VIP\" }}",
        iri("classes", "ex:Customer"),
        iri("properties", "ex:name"),
        iri("properties", "ex:tier"),
    );

    // explain: the SQL binds the constant and projects both variables.
    let plan = v12n(
        &["explain", "--format", "json", SHOP, "--query", &sparql],
        "",
    );
    assert_eq!(plan.status.code(), Some(0));
    let plan = stdout_json(&plan);
    assert_eq!(plan["params"], json!(["VIP"]));
    let names: Vec<&Value> = plan["variables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| &v["name"])
        .collect();
    assert_eq!(names, [&json!("customer"), &json!("name")]);

    // query, exit 0: SPARQL 1.1 Query Results JSON, nothing on stderr.
    let run = |args: &[&str], stdin: &str| {
        let mut all = vec!["query", "--format", "json", "--db", &url];
        all.extend_from_slice(args);
        v12n(&all, stdin)
    };
    let output = run(&[SHOP, "--query", &sparql], "");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    let results = stdout_json(&output);
    assert_eq!(results["head"], json!({ "vars": ["customer", "name"] }));
    let mut found: Vec<&str> = results["results"]["bindings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["name"]["value"].as_str().unwrap())
        .collect();
    found.sort_unstable();
    assert_eq!(found, ["Kim", "Park"]);

    // The same query in CSV: lexical forms, CRLF line ends.
    let csv = v12n(
        &[
            "query", "--format", "csv", "--db", &url, SHOP, "--query", &sparql,
        ],
        "",
    );
    assert_eq!(csv.status.code(), Some(0));
    let csv = String::from_utf8(csv.stdout).unwrap();
    let mut records: Vec<&str> = csv.split_terminator("\r\n").collect();
    assert_eq!(records.remove(0), "customer,name");
    records.sort_unstable();
    assert_eq!(
        records,
        [
            "http://example.org/customer/7,Kim",
            "http://example.org/customer/9,Park"
        ]
    );

    // 1: query syntax.
    assert_json_error(
        &run(&[SHOP, "--query", "SELECT ?x WHERE {"], ""),
        1,
        "query",
    );
    // 2: invalid mapping, read from stdin.
    assert_json_error(&run(&["-", "--query", &sparql], "not turtle"), 2, "mapping");
    // 3: the database rejects the SQL: the mapping names a missing table.
    let missing = std::fs::read_to_string(SHOP)
        .unwrap()
        .replace("\"customers\"", "\"no_such_table\"");
    let error = assert_json_error(&run(&["-", "--query", &sparql], &missing), 3, "database");
    assert!(
        error["message"].as_str().unwrap().contains("no_such_table"),
        "{error}"
    );
    // 4: statement timeout before the first row: sorting a cross join of
    // 13 scans of the 3 customers (1.6 million rows) takes far longer.
    let patterns: String = (1..=13)
        .map(|i| format!("?c{i} <http://example.org/name> ?n{i} . "))
        .collect();
    let slow = format!("SELECT ?n1 WHERE {{ {patterns}}} ORDER BY ?n1");
    let output = run(
        &[
            SHOP,
            "--query",
            &slow,
            "--timeout",
            "100ms",
            "--row-limit",
            "0",
        ],
        "",
    );
    let error = assert_json_error(&output, 4, "timeout");
    assert_eq!(error["details"], json!({ "timeout_ms": 100, "rows": 0 }));
    // 5: row limit: a complete result of one solution, a warning on stderr.
    let output = run(&[SHOP, "--query", &sparql, "--row-limit", "1"], "");
    assert_eq!(output.status.code(), Some(5));
    assert_eq!(bindings(&output), 1);
    assert!(String::from_utf8_lossy(&output.stderr).starts_with("warning: result truncated"));
}
