//! Shared harness for the parity tests: parses fixtures/generate.sh and compares es-mini's
//! responses with the ones real Elasticsearch 9.5.5 recorded in fixtures/expected/.

// The per-case report on stdout is this harness's output.
#![allow(clippy::print_stdout)]

use axum::{
    Router,
    body::Body,
    http::{Method, Request},
};
use http_body_util::BodyExt;
use serde_json::Value;
use std::{collections::BTreeSet, fs, path::PathBuf};
use tower::ServiceExt;

/// Response fields that vary per run; generate.sh strips the same list.
const VOLATILE: &[&str] = &[
    "took",
    "start_time_in_millis",
    "completion_time_in_millis",
    "expiration_time_in_millis",
    "cpu_nanos",
    "read_nanos",
    "bytes_read",
    "rows_emitted",
];

pub fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures")
}

pub struct Req {
    pub method: String,
    pub path: String,
    pub body: Option<String>,
}

/// One `record NAME METHOD PATH ['BODY']` line.
pub struct Case {
    pub name: String,
    pub req: Req,
}

pub fn between<'a>(s: &'a str, start: &str, end: &str) -> Vec<&'a str> {
    s.split(start)
        .skip(1)
        .map(|rest| &rest[..rest.find(end).expect("unterminated block")])
        .collect()
}

/// (setup requests, recorded cases) from generate.sh.
pub fn parse_script() -> (Vec<Req>, Vec<Case>) {
    let script = fs::read_to_string(fixtures().join("generate.sh")).unwrap();
    let create = between(&script, "req PUT /fixture '", "'")[0];
    let mut setup = vec![Req {
        method: "PUT".into(),
        path: "/fixture".into(),
        body: Some(create.into()),
    }];
    for ndjson in between(&script, "<<'NDJSON'\n", "NDJSON\n") {
        let path = "/fixture/_bulk?refresh=true".into();
        setup.push(Req {
            method: "POST".into(),
            path,
            body: Some(ndjson.into()),
        });
    }
    let cases = script
        .lines()
        .filter_map(|l| {
            l.trim_start_matches("ALLOW_404=404 ")
                .strip_prefix("record ")
        })
        .map(|l| {
            let mut parts = l.splitn(4, ' ');
            let mut next = || parts.next().map(str::to_owned);
            let (name, method, path) = (next().unwrap(), next().unwrap(), next().unwrap());
            let body = next().map(|b| b.trim_matches('\'').to_owned());
            Case {
                name,
                req: Req { method, path, body },
            }
        })
        .collect();
    (setup, cases)
}

pub async fn send(app: &Router, r: &Req) -> Value {
    let content_type = if r.path.contains("_bulk") {
        "application/x-ndjson"
    } else {
        "application/json"
    };
    let req = Request::builder()
        .method(Method::from_bytes(r.method.as_bytes()).unwrap())
        .uri(&r.path)
        .header("content-type", content_type)
        .body(Body::from(r.body.clone().unwrap_or_default()))
        .unwrap();
    let bytes = app
        .clone()
        .oneshot(req)
        .await
        .unwrap()
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

/// Paths where `actual` differs from `expected`. Numbers match within 1e-5 relative error, which
/// absorbs f32/f64 formatting but not real scoring differences.
pub fn diff(path: &str, expected: &Value, actual: &Value, out: &mut Vec<String>) {
    match (expected, actual) {
        (Value::Object(e), Value::Object(a)) => {
            for k in e.keys().chain(a.keys()).collect::<BTreeSet<_>>() {
                let (x, y) = (
                    e.get(k).unwrap_or(&Value::Null),
                    a.get(k).unwrap_or(&Value::Null),
                );
                match (e.contains_key(k), a.contains_key(k)) {
                    (true, false) => out.push(format!("{path}.{k}: missing (expected {x})")),
                    (false, true) => out.push(format!("{path}.{k}: unexpected {y}")),
                    _ => diff(&format!("{path}.{k}"), x, y, out),
                }
            }
        }
        (Value::Array(e), Value::Array(a)) if e.len() == a.len() => {
            for (i, (x, y)) in e.iter().zip(a).enumerate() {
                diff(&format!("{path}[{i}]"), x, y, out);
            }
        }
        (Value::Number(x), Value::Number(y)) => {
            let (x, y) = (x.as_f64().unwrap(), y.as_f64().unwrap());
            if (x - y).abs() > 1e-5 * x.abs().max(y.abs()) {
                out.push(format!("{path}: expected {x}, got {y}"));
            }
        }
        (e, a) if e != a => out.push(format!("{path}: expected {e}, got {a}")),
        _ => {}
    }
}

/// Compares every recorded case. Fails on any difference not in `xfail` and on any listed
/// case that now matches. `PARITY_VERBOSE=1` prints every difference.
pub async fn compare(app: &Router, xfail: &BTreeSet<String>) {
    let (_, cases) = parse_script();
    assert!(
        cases.len() >= 10,
        "parsed only {} cases from generate.sh",
        cases.len()
    );
    let verbose = std::env::var("PARITY_VERBOSE").is_ok();
    let mut problems = vec![];
    for c in &cases {
        let expected: Value = serde_json::from_slice(
            &fs::read(fixtures().join(format!("expected/{}.json", c.name))).unwrap(),
        )
        .unwrap();
        let mut actual = send(app, &c.req).await;
        if let Value::Object(o) = &mut actual {
            VOLATILE.iter().for_each(|k| drop(o.remove(*k)));
        }
        let mut diffs = vec![];
        diff("", &expected, &actual, &mut diffs);
        let listed = xfail.contains(&c.name);
        match (diffs.is_empty(), listed) {
            (true, true) => {
                problems.push(format!("XPASS {} (delete it from its xfail list)", c.name))
            }
            (false, false) => problems.push(format!("FAIL {}", c.name)),
            _ => {}
        }
        if !diffs.is_empty() && (verbose || !listed) {
            println!("DIFF {}\n  {}", c.name, diffs.join("\n  "));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
