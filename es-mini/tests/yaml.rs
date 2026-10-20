//! Runs the vendored Elasticsearch YAML REST tests (tests/yaml) against es-mini in-process.
//!
//! Known failures live in tests/yaml-xfail.txt; the run fails on any other failure and on any
//! xfail that now passes (remove it from the list). Environment:
//! - `YAML_FILTER=substr`: only run tests whose `file/name` contains `substr`.
//! - `YAML_VERBOSE=1`: print every failure and skip reason.
//! - `YAML_WRITE_XFAIL=1`: rewrite the xfail list from this run's failures.

// The per-test report on stdout is this runner's output.
#![allow(clippy::print_stdout)]

use axum::{
    Router,
    body::Body,
    http::{Method, Request},
};
use http_body_util::BodyExt;
use regex::Regex;
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
};
use tower::ServiceExt;

/// Runner features we implement; tests requiring others are skipped.
const FEATURES: &[&str] = &[
    "headers",
    "warnings",
    "allowed_warnings",
    "allowed_warnings_regex",
    "contains",
    "close_to",
    "stash_in_path",
    "stash_in_key",
];

/// `do:` keys that are not the API name.
const DO_OPTIONS: &[&str] = &[
    "catch",
    "headers",
    "warnings",
    "allowed_warnings",
    "allowed_warnings_regex",
    "warnings_regex",
    "node_selector",
];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests")
}

struct SpecPath {
    path: String,
    methods: Vec<String>,
    parts: Vec<String>,
}

type Specs = HashMap<String, Vec<SpecPath>>;

fn load_specs() -> Specs {
    let mut specs = HashMap::new();
    for entry in fs::read_dir(root().join("yaml/api")).unwrap() {
        let v: Value = serde_json::from_slice(&fs::read(entry.unwrap().path()).unwrap()).unwrap();
        for (name, api) in v.as_object().unwrap() {
            let strings = |v: &Value| -> Vec<String> {
                v.as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|s| s.as_str().map(str::to_owned))
                    .collect()
            };
            let paths = api["url"]["paths"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| SpecPath {
                    path: p["path"].as_str().unwrap().to_owned(),
                    methods: strings(&p["methods"]),
                    parts: p["parts"]
                        .as_object()
                        .map(|o| o.keys().cloned().collect())
                        .unwrap_or_default(),
                });
            specs.insert(name.clone(), paths.collect());
        }
    }
    specs
}

struct Response {
    body: Value,
    warnings: Vec<String>,
}

struct Ctx<'a> {
    app: Router,
    specs: &'a Specs,
    stash: HashMap<String, Value>,
    last: Option<Response>,
}

/// Err carries the failure reason.
type Outcome = Result<(), String>;

fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(scalar).collect::<Vec<_>>().join(","),
        v => v.to_string(),
    }
}

fn num(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str()?.parse().ok())
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b',' | b'*' => {
                (b as char).to_string()
            }
            b => format!("%{b:02X}"),
        })
        .collect()
}

/// ES regexes are `/.../` compiled with Java's COMMENTS flag.
fn regex_match(pattern: &str, actual: &str) -> bool {
    let inner = pattern.trim().trim_start_matches('/').trim_end_matches('/');
    Regex::new(&format!("(?xs){inner}")).is_ok_and(|re| re.is_match(actual))
}

fn is_regex(s: &str) -> bool {
    let s = s.trim();
    s.len() > 1 && s.starts_with('/') && s.ends_with('/')
}

fn matches(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
        (Value::String(e), a) if is_regex(e) => regex_match(e, &scalar(a)),
        (Value::Number(_), _) | (_, Value::Number(_)) => match (num(expected), num(actual)) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        },
        (Value::Array(e), Value::Array(a)) => {
            e.len() == a.len() && e.iter().zip(a).all(|(x, y)| matches(x, y))
        }
        (Value::Object(e), Value::Object(a)) => {
            e.len() == a.len()
                && e.iter()
                    .all(|(k, x)| a.get(k).is_some_and(|y| matches(x, y)))
        }
        (e, a) => e == a,
    }
}

fn subset(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
        (Value::Object(e), Value::Object(a)) => e
            .iter()
            .all(|(k, x)| a.get(k).is_some_and(|y| subset(x, y))),
        (e, a) => matches(e, a),
    }
}

fn catch_ok(catch: &str, status: u16, body: &Value) -> bool {
    match catch {
        "missing" => status == 404,
        "conflict" => status == 409,
        "bad_request" => status == 400,
        "forbidden" => status == 403,
        "unauthorized" => status == 401,
        "request_timeout" => status == 408,
        "unavailable" => status == 503,
        "request" => status >= 400 && ![400, 401, 403, 404, 408, 409].contains(&status),
        // Unlike `match`, catch patterns are not in extended mode: spaces are literal.
        c if is_regex(c) => {
            let inner = c.trim().trim_start_matches('/').trim_end_matches('/');
            status >= 400
                && Regex::new(&format!("(?s){inner}"))
                    .is_ok_and(|re| re.is_match(&body.to_string()))
        }
        _ => false,
    }
}

impl Ctx<'_> {
    /// Replaces `$name` values and `${name}` substrings with stashed values.
    fn unstash(&self, v: &Value) -> Value {
        match v {
            Value::String(s) if s == "$body" => {
                self.last.as_ref().map_or(Value::Null, |r| r.body.clone())
            }
            Value::String(s) if s.starts_with('$') && !s.starts_with("${") => self
                .stash
                .get(&s[1..])
                .cloned()
                .unwrap_or_else(|| v.clone()),
            Value::String(s) if s.contains("${") => {
                let re = Regex::new(r"\$\{(\w+)\}").unwrap();
                let out = re.replace_all(s, |c: &regex::Captures| {
                    self.stash.get(&c[1]).map(scalar).unwrap_or_default()
                });
                Value::String(out.into())
            }
            Value::Array(a) => Value::Array(a.iter().map(|x| self.unstash(x)).collect()),
            Value::Object(o) => Value::Object(
                o.iter()
                    .map(|(k, x)| (scalar(&self.unstash(&json!(k))), self.unstash(x)))
                    .collect(),
            ),
            v => v.clone(),
        }
    }

    /// Resolves a dotted path (`\.` escapes a dot) in the last response body.
    fn lookup(&self, path: &str) -> Option<Value> {
        let mut cur = self.last.as_ref()?.body.clone();
        if path.is_empty() || path == "$body" {
            return Some(cur);
        }
        let mut parts = vec![String::new()];
        let mut chars = path.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' if chars.peek() == Some(&'.') => parts.last_mut()?.push(chars.next()?),
                '.' => parts.push(String::new()),
                c => parts.last_mut()?.push(c),
            }
        }
        for p in parts {
            let key = scalar(&self.unstash(&Value::String(p)));
            cur = match &cur {
                Value::Object(o) => o.get(&key)?.clone(),
                Value::Array(a) => a.get(key.parse::<usize>().ok()?)?.clone(),
                _ => return None,
            };
        }
        Some(cur)
    }

    async fn call(&mut self, step: &Map<String, Value>) -> Outcome {
        let (api, params) = step
            .iter()
            .find(|(k, _)| !DO_OPTIONS.contains(&k.as_str()))
            .ok_or("do: without an API")?;
        let spec = self
            .specs
            .get(api)
            .ok_or(format!("no spec for API [{api}]"))?;
        let params = self
            .unstash(params)
            .as_object()
            .cloned()
            .unwrap_or_default();
        let path = spec
            .iter()
            .filter(|p| p.parts.iter().all(|x| params.contains_key(x)))
            .max_by_key(|p| p.parts.len())
            .ok_or(format!("no [{api}] path for the given params"));
        // "param" means the client rejects the request before sending it.
        let path = match (path, step.get("catch").and_then(Value::as_str)) {
            (Err(_), Some("param")) => return Ok(()),
            (p, _) => p?,
        };
        let body = params.get("body");
        let has = |m: &str| path.methods.iter().any(|x| x == m);
        let method = match body {
            Some(_) if has("POST") => "POST",
            Some(_) if has("PUT") => "PUT",
            _ => path.methods[0].as_str(),
        };
        let mut url = path.path.clone();
        for part in &path.parts {
            url = url.replace(&format!("{{{part}}}"), &urlencode(&scalar(&params[part])));
        }
        let ignore: Vec<u64> = match params.get("ignore") {
            Some(Value::Array(a)) => a.iter().filter_map(Value::as_u64).collect(),
            Some(v) => v.as_u64().into_iter().collect(),
            None => vec![],
        };
        let query: Vec<String> = params
            .iter()
            .filter(|(k, _)| !path.parts.contains(k) && *k != "body" && *k != "ignore")
            .map(|(k, v)| format!("{k}={}", urlencode(&scalar(v))))
            .collect();
        if !query.is_empty() {
            url = format!("{url}?{}", query.join("&"));
        }
        let ndjson = api.ends_with("bulk");
        let body_bytes = match body {
            None => vec![],
            Some(Value::String(s)) => s.clone().into_bytes(),
            Some(Value::Array(lines)) if ndjson => lines
                .iter()
                .map(|l| format!("{}\n", scalar_or_json(l)))
                .collect::<String>()
                .into_bytes(),
            Some(v) => serde_json::to_vec(v).unwrap(),
        };
        let content_type = if ndjson {
            "application/x-ndjson"
        } else {
            "application/json"
        };
        let mut req = Request::builder()
            .method(Method::from_bytes(method.as_bytes()).unwrap())
            .uri(&url)
            .header("content-type", content_type);
        if let Some(Value::Object(h)) = step.get("headers") {
            for (k, v) in h {
                req = req.header(k, scalar(&self.unstash(v)));
            }
        }
        let req = req
            .body(Body::from(body_bytes))
            .map_err(|e| e.to_string())?;
        let resp = self
            .app
            .clone()
            .oneshot(req)
            .await
            .map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let warnings = resp
            .headers()
            .get_all("warning")
            .iter()
            .filter_map(|v| v.to_str().ok().map(str::to_owned))
            .collect();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body = match method {
            "HEAD" => Value::Bool((200..300).contains(&status)),
            _ if bytes.is_empty() => Value::Null,
            _ => serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into())),
        };
        let summary = format!(
            "{method} {url} -> {status} {}",
            body.to_string().chars().take(300).collect::<String>()
        );
        self.last = Some(Response { body, warnings });
        let r = self.last.as_ref().unwrap();

        for w in step
            .get("warnings")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let w = scalar(w);
            if !r.warnings.iter().any(|x| x.contains(&w)) {
                return Err(format!("missing warning [{w}]: {summary}"));
            }
        }
        match step.get("catch").and_then(Value::as_str) {
            Some(c) if catch_ok(c, status, &r.body) => Ok(()),
            Some(c) => Err(format!("expected catch [{c}]: {summary}")),
            None if status < 400
                || (method == "HEAD" && status == 404)
                || ignore.contains(&(status as u64)) =>
            {
                Ok(())
            }
            None => Err(format!("unexpected error: {summary}")),
        }
    }

    async fn step(&mut self, step: &Value) -> Outcome {
        let (kind, arg) = step
            .as_object()
            .and_then(|o| o.iter().next())
            .ok_or("malformed step")?;
        let pairs = || arg.as_object().into_iter().flatten();
        match kind.as_str() {
            "do" => self.call(arg.as_object().ok_or("malformed do")?).await,
            "skip" | "requires" => Ok(()),
            "set" => {
                for (path, name) in pairs() {
                    let v = self
                        .lookup(path)
                        .ok_or(format!("set: no value at [{path}]"))?;
                    self.stash.insert(scalar(name), v);
                }
                Ok(())
            }
            "match" | "contains" => {
                for (path, expected) in pairs() {
                    let expected = self.unstash(expected);
                    let actual = self.lookup(path);
                    let ok = match (kind.as_str(), &actual) {
                        ("match", Some(a)) => matches(&expected, a),
                        ("match", None) => expected.is_null(),
                        (_, Some(Value::Array(items))) => {
                            items.iter().any(|i| subset(&expected, i))
                        }
                        _ => false,
                    };
                    if !ok {
                        let got = actual.map_or("nothing".into(), |a| a.to_string());
                        return Err(format!("{kind} [{path}]: expected {expected}, got {got}"));
                    }
                }
                Ok(())
            }
            "length" => {
                for (path, n) in pairs() {
                    let len = match self.lookup(path) {
                        Some(Value::Array(a)) => Some(a.len()),
                        Some(Value::Object(o)) => Some(o.len()),
                        Some(Value::String(s)) => Some(s.chars().count()),
                        _ => None,
                    };
                    let n = self.unstash(n).as_u64().map(|n| n as usize);
                    if len != n {
                        return Err(format!("length [{path}]: expected {n:?}, got {len:?}"));
                    }
                }
                Ok(())
            }
            "is_true" | "is_false" => {
                let path = scalar(arg);
                let v = self.lookup(&path);
                let truthy = match &v {
                    None | Some(Value::Null) | Some(Value::Bool(false)) => false,
                    Some(x) => !matches!(scalar(x).as_str(), "" | "false" | "0"),
                };
                match truthy == (kind == "is_true") {
                    true => Ok(()),
                    false => Err(format!("{kind} [{path}]: got {v:?}")),
                }
            }
            "gt" | "gte" | "lt" | "lte" => {
                for (path, bound) in pairs() {
                    let a = self
                        .lookup(path)
                        .and_then(|v| num(&v))
                        .ok_or(format!("{kind} [{path}]: not a number"))?;
                    let b =
                        num(&self.unstash(bound)).ok_or(format!("{kind} [{path}]: bad bound"))?;
                    let ok = match kind.as_str() {
                        "gt" => a > b,
                        "gte" => a >= b,
                        "lt" => a < b,
                        _ => a <= b,
                    };
                    if !ok {
                        return Err(format!("{kind} [{path}]: {a} vs {b}"));
                    }
                }
                Ok(())
            }
            "close_to" => {
                for (path, spec) in pairs() {
                    let a = self
                        .lookup(path)
                        .and_then(|v| num(&v))
                        .ok_or(format!("close_to [{path}]: not a number"))?;
                    let (v, e) = (
                        num(&spec["value"]).unwrap_or(f64::NAN),
                        num(&spec["error"]).unwrap_or(0.0),
                    );
                    if (a - v).abs() > e {
                        return Err(format!("close_to [{path}]: {a} vs {v}±{e}"));
                    }
                }
                Ok(())
            }
            other => Err(format!("unsupported step [{other}]")),
        }
    }
}

fn scalar_or_json(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

/// Why a section should be skipped, if it should.
fn skip_reason(steps: &[Value]) -> Option<String> {
    let unsupported = |arg: &Value, key: &str| -> Option<String> {
        let list = match arg.get(key) {
            Some(Value::Array(a)) => a.iter().map(scalar).collect(),
            Some(v) => vec![scalar(v)],
            None => vec![],
        };
        list.into_iter()
            .find(|f| !FEATURES.contains(&f.as_str()))
            .map(|f| format!("runner feature [{f}]"))
    };
    for (kind, arg) in steps.iter().filter_map(Value::as_object).flatten() {
        match kind.as_str() {
            "skip" if arg.get("awaits_fix").is_some() || arg.get("known_issues").is_some() => {
                return Some("awaits_fix".into());
            }
            "skip"
                if arg
                    .get("version")
                    .is_some_and(|v| scalar(v).trim() == "all") =>
            {
                return Some("version: all".into());
            }
            // We report every 9.5.5 cluster feature, so `skip: cluster_features` always applies.
            "skip" if arg.get("cluster_features").is_some() => {
                return Some("cluster feature present".into());
            }
            "skip" => {
                if let Some(r) = unsupported(arg, "features") {
                    return Some(r);
                }
            }
            "requires" => {
                if let Some(r) = unsupported(arg, "test_runner_features") {
                    return Some(r);
                }
            }
            _ => {}
        }
    }
    None
}

struct Section {
    name: String,
    steps: Vec<Value>,
}

/// Returns (setup steps, test sections). Teardown is unnecessary: each test gets a fresh server.
fn load_file(path: &Path) -> (Vec<Value>, Vec<Section>) {
    let text = fs::read_to_string(path).unwrap();
    let mut setup = vec![];
    let mut tests = vec![];
    for doc in serde_yaml_ng::Deserializer::from_str(&text) {
        let Ok(Value::Object(o)) = <Value as serde::Deserialize>::deserialize(doc) else {
            continue;
        };
        for (name, steps) in o {
            let steps = steps.as_array().cloned().unwrap_or_default();
            match name.as_str() {
                "setup" => setup = steps,
                "teardown" => {}
                _ => tests.push(Section { name, steps }),
            }
        }
    }
    (setup, tests)
}

async fn run_test(specs: &Specs, setup: &[Value], test: &Section) -> Outcome {
    let mut ctx = Ctx {
        app: es_mini::app(),
        specs,
        stash: HashMap::new(),
        last: None,
    };
    for s in setup {
        ctx.step(s).await.map_err(|e| format!("setup: {e}"))?;
    }
    for s in &test.steps {
        ctx.step(s).await?;
    }
    Ok(())
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else if p.extension().is_some_and(|x| x == "yml") {
            out.push(p);
        }
    }
    out
}

#[tokio::test]
async fn yaml_rest_tests() {
    let specs = load_specs();
    let filter = std::env::var("YAML_FILTER").unwrap_or_default();
    let verbose = std::env::var("YAML_VERBOSE").is_ok();
    let xfail_path = root().join("yaml-xfail.txt");
    let xfail: BTreeSet<String> = fs::read_to_string(&xfail_path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect();

    let yaml_dir = root().join("yaml");
    let mut files = walk(&yaml_dir);
    files.sort();
    let (mut passed, mut skipped) = (0, 0);
    let mut failed = BTreeSet::new();
    let mut problems = vec![];
    for file in files {
        let rel = file
            .strip_prefix(&yaml_dir)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let (setup, tests) = load_file(&file);
        for t in &tests {
            let id = format!("{rel}/{}", t.name);
            if !id.contains(&filter) {
                continue;
            }
            if let Some(why) = skip_reason(&setup).or_else(|| skip_reason(&t.steps)) {
                skipped += 1;
                if verbose {
                    println!("SKIP {id}: {why}");
                }
                continue;
            }
            match run_test(&specs, &setup, t).await {
                Ok(()) => {
                    passed += 1;
                    if xfail.contains(&id) {
                        problems.push(format!("XPASS {id} (delete it from tests/yaml-xfail.txt)"));
                    }
                }
                Err(e) => {
                    if verbose || !xfail.contains(&id) {
                        println!("FAIL {id}\n     {e}");
                    }
                    if !xfail.contains(&id) {
                        problems.push(format!("FAIL {id}"));
                    }
                    failed.insert(id);
                }
            }
        }
    }
    let known = failed.intersection(&xfail).count();
    println!(
        "yaml: {passed} passed, {} failed ({known} xfail), {skipped} skipped",
        failed.len()
    );
    if std::env::var("YAML_WRITE_XFAIL").is_ok() {
        let body: String = failed.iter().map(|id| format!("{id}\n")).collect();
        fs::write(
            &xfail_path,
            format!("# Known-failing YAML tests. Fix one, then delete its line.\n{body}"),
        )
        .unwrap();
        return;
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
