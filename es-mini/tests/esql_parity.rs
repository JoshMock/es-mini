//! Replays fixtures/generate_esql.sh's ingest into an in-memory es-mini, runs every query in
//! fixtures/esql-queries.txt, and compares the columns and values (or, for invalid queries, the
//! error type) with Elasticsearch 9.5.5's responses in fixtures/expected-esql/. Known differences
//! live in fixtures/esql-parity-xfail.txt, one query per line.

// The shared harness also serves fixture_parity and disk_parity; this test uses only part of it.
#[allow(dead_code)]
mod common;

use common::parity::{Req, between, diff, fixtures, report, send};
use serde_json::{Value, json};
use std::{collections::BTreeSet, fs};

/// The non-blank, non-comment lines of a fixtures file.
fn lines(file: &str) -> Vec<String> {
    fs::read_to_string(fixtures().join(file))
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn in_memory_matches_recorded_esql_responses() {
    let app = es_mini::app();
    let script = fs::read_to_string(fixtures().join("generate_esql.sh")).unwrap();
    for (path, body) in script
        .lines()
        .filter_map(|l| l.strip_prefix("req PUT ")?.split_once(' '))
    {
        let body = Some(body.trim_matches('\'').to_owned());
        let r = send(
            &app,
            &Req {
                method: "PUT".into(),
                path: path.into(),
                body,
            },
        )
        .await;
        assert_eq!(r["acknowledged"], true, "PUT {path}: {r}");
    }
    for ndjson in between(&script, "<<'NDJSON'\n", "NDJSON\n") {
        let path = "/_bulk?refresh=true".into();
        let body = Some(ndjson.into());
        let r = send(
            &app,
            &Req {
                method: "POST".into(),
                path,
                body,
            },
        )
        .await;
        assert_eq!(r["errors"], false, "bulk: {r}");
    }

    let queries = lines("esql-queries.txt");
    let recorded = fs::read_dir(fixtures().join("expected-esql"))
        .unwrap()
        .count();
    assert_eq!(recorded, queries.len(), "rerun fixtures/generate_esql.sh");
    let xfail: BTreeSet<String> = lines("esql-parity-xfail.txt").into_iter().collect();
    let mut problems = vec![];
    for (i, q) in queries.iter().enumerate() {
        let file = fixtures().join(format!("expected-esql/{:03}.json", i + 1));
        let rec: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        assert_eq!(
            rec["query"], *q,
            "{file:?} is stale: rerun fixtures/generate_esql.sh"
        );
        let body = Some(json!({ "query": q }).to_string());
        let actual = send(
            &app,
            &Req {
                method: "POST".into(),
                path: "/_query".into(),
                body,
            },
        )
        .await;
        let expected = &rec["response"];
        let mut diffs = vec![];
        if let Some(ty) = expected.pointer("/error/type") {
            let got = actual.pointer("/error/type").unwrap_or(&Value::Null);
            diff("error.type", ty, got, &mut diffs);
        } else {
            for k in ["columns", "values"] {
                diff(k, &expected[k], &actual[k], &mut diffs);
            }
        }
        report(q, &diffs, &xfail, &mut problems);
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
