// SPDX-License-Identifier: Apache-2.0
//! Catalog snapshots read from the `testenv` PostgreSQL. Skipped unless
//! `V12N_TEST_DATABASE_URL` is set, e.g.
//! `postgresql://v12n:v12n-dev@127.0.0.1:55432/shop`.

use tokio_postgres::{Client, NoTls};
use v12n_core::Limits;
use v12n_core::backend::Backend;
use v12n_core::catalog::{
    CatalogRequest, CatalogSnapshot, Fingerprint, ForeignKey, Relation, TableStatus,
};
use v12n_core::mapping::{LogicalTable, Mapping};
use v12n_postgres::PostgresBackend;

const MAPPING: &str = r#"
@prefix rr: <http://www.w3.org/ns/r2rml#> .
@prefix ex: <http://example.org/> .

<#Items> rr:logicalTable [ rr:tableName "v12n_catalog.items" ] ;
  rr:subjectMap [ rr:template "item/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:code ; rr:objectMap [ rr:column "\"Code\"" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:label ; rr:objectMap [ rr:column "Label" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:at ; rr:objectMap [ rr:column "at" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:gone ; rr:objectMap [ rr:column "gone" ] ] .

<#Missing> rr:logicalTable [ rr:tableName "v12n_catalog.missing" ] ;
  rr:subjectMap [ rr:template "missing/{id}" ] .

<#Query> rr:logicalTable [ rr:sqlQuery "SELECT 1 AS Num" ] ;
  rr:subjectMap [ rr:template "num/{Num}" ] .

<#Broken> rr:logicalTable [ rr:sqlQuery "SELECT id FROM v12n_catalog.nowhere" ] ;
  rr:subjectMap [ rr:template "broken/{id}" ] .
"#;

fn database_url() -> Option<String> {
    let url = std::env::var("V12N_TEST_DATABASE_URL").ok();
    if url.is_none() {
        eprintln!("skipped: V12N_TEST_DATABASE_URL is not set");
    }
    url
}

/// A client that has created `schema` with the tables of [`MAPPING`].
async fn admin(url: &str, schema: &str) -> Client {
    let (client, connection) = tokio_postgres::connect(url, NoTls).await.unwrap();
    tokio::spawn(connection);
    let setup = "DROP SCHEMA IF EXISTS v12n_catalog CASCADE;
             CREATE SCHEMA v12n_catalog;
             CREATE TABLE v12n_catalog.owners (id int PRIMARY KEY);
             CREATE TABLE v12n_catalog.items (
               id int PRIMARY KEY,
               \"Code\" text NOT NULL UNIQUE,
               label text NOT NULL,
               at time UNIQUE,
               owner int REFERENCES v12n_catalog.owners (id),
               note text);
             ALTER TABLE v12n_catalog.items ADD CONSTRAINT unchecked
               FOREIGN KEY (note) REFERENCES v12n_catalog.items (\"Code\") NOT VALID;";
    client
        .batch_execute(&setup.replace("v12n_catalog", schema))
        .await
        .unwrap();
    client
}

/// The request of [`MAPPING`] with its tables in `schema`.
fn request(schema: &str) -> CatalogRequest {
    let mapping = MAPPING.replace("v12n_catalog", schema);
    CatalogRequest::for_mapping(&Mapping::from_turtle(&mapping).unwrap())
}

async fn load(url: &str, request: &CatalogRequest) -> CatalogSnapshot {
    let backend = PostgresBackend::connect(url, Limits::default())
        .await
        .unwrap();
    backend.load_catalog(request).await.unwrap()
}

fn table(name: &str) -> LogicalTable {
    LogicalTable::TableName(name.into())
}

fn relation<'s>(snapshot: &'s CatalogSnapshot, table: &LogicalTable) -> &'s Relation {
    match &snapshot.table(table).unwrap().status {
        TableStatus::Readable(relation) => relation,
        other => panic!("{table:?} is {other:?}"),
    }
}

fn fingerprint(snapshot: &CatalogSnapshot, name: &str) -> Fingerprint {
    snapshot
        .table(&table(name))
        .unwrap()
        .fingerprint
        .clone()
        .unwrap()
}

#[tokio::test]
async fn snapshot_reads_columns_keys_and_foreign_keys() {
    let Some(url) = database_url() else { return };
    let admin = admin(&url, "v12n_catalog").await;
    let request = request("v12n_catalog");
    let snapshot = load(&url, &request).await;

    let items = relation(&snapshot, &table("v12n_catalog.items"));
    let column = |written: &str| {
        let c = &items.columns[written];
        (c.name.as_str(), c.ty.name.as_str(), c.not_null)
    };
    assert_eq!(column("id"), ("id", "int4", true));
    assert_eq!(column("\"Code\""), ("Code", "text", true));
    assert_eq!(column("Label"), ("label", "text", true));
    assert_eq!(column("at"), ("at", "time", false));
    assert_eq!(items.missing_columns, ["gone"]);

    let keys: Vec<(Vec<String>, bool, bool)> = items
        .unique_keys
        .iter()
        .map(|k| (k.columns.clone(), k.separates_terms(), k.identifies_rows()))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    assert_eq!(
        keys,
        [
            (vec!["Code".to_owned()], true, true),
            (vec!["at".to_owned()], false, false),
            (vec!["id".to_owned()], true, true),
        ]
    );
    assert_eq!(
        items.foreign_keys,
        [ForeignKey {
            columns: vec!["owner".into()],
            referenced_table: "v12n_catalog.owners".into(),
            referenced_columns: vec!["id".into()],
        }]
    );

    let query = LogicalTable::SqlQuery("SELECT 1 AS Num".into());
    assert_eq!(relation(&snapshot, &query).columns["Num"].name, "num");
    assert_eq!(snapshot.table(&query).unwrap().fingerprint, None);

    assert_eq!(
        snapshot
            .problems(&request)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        [
            "table v12n_catalog.items has no column gone",
            "table v12n_catalog.missing does not exist",
            "an rr:sqlQuery cannot be read: relation \"v12n_catalog.nowhere\" does not exist",
        ]
    );

    admin
        .batch_execute("DROP SCHEMA v12n_catalog CASCADE")
        .await
        .unwrap();
}

#[tokio::test]
async fn fingerprint_changes_with_what_the_snapshot_reads() {
    let Some(url) = database_url() else { return };
    let admin = admin(&url, "v12n_fingerprint").await;
    let request = request("v12n_fingerprint");
    let items = "v12n_fingerprint.items";
    let missing = "v12n_fingerprint.missing";
    let before = load(&url, &request).await;

    admin
        .batch_execute("CREATE INDEX ON v12n_fingerprint.items (note)")
        .await
        .unwrap();
    let after_index = load(&url, &request).await;
    assert_eq!(
        fingerprint(&after_index, items),
        fingerprint(&before, items)
    );

    let changes = [
        "ALTER TABLE v12n_fingerprint.items DROP CONSTRAINT \"items_Code_key\" CASCADE",
        "ALTER TABLE v12n_fingerprint.items ALTER COLUMN label DROP NOT NULL",
        "ALTER TABLE v12n_fingerprint.items ALTER COLUMN at TYPE timetz",
        "ALTER TABLE v12n_fingerprint.items RENAME COLUMN note TO remark",
        "ALTER TABLE v12n_fingerprint.items DROP CONSTRAINT items_owner_fkey",
        "CREATE TABLE v12n_fingerprint.child () INHERITS (v12n_fingerprint.items)",
    ];
    let mut previous = fingerprint(&after_index, items);
    for change in changes {
        admin.batch_execute(change).await.unwrap();
        let current = fingerprint(&load(&url, &request).await, items);
        assert_ne!(current, previous, "{change}");
        previous = current;
    }

    let missing_before = fingerprint(&before, missing);
    admin
        .batch_execute("CREATE TABLE v12n_fingerprint.missing (id int)")
        .await
        .unwrap();
    let created = load(&url, &request).await;
    assert_ne!(fingerprint(&created, missing), missing_before);
    assert!(matches!(
        created.table(&table(missing)).unwrap().status,
        TableStatus::Readable(_)
    ));

    admin
        .batch_execute("DROP SCHEMA v12n_fingerprint CASCADE")
        .await
        .unwrap();
}
