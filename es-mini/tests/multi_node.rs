//! Multi-node guardrails, run against real es-mini processes.
//!
//! `multi_node_guard_*` tests pin today's single-node behavior that multi-node support will
//! touch (shard routing, multi-shard merging, `_bulk`, refresh, index lifecycle, persistence).
//! The `#[ignore]`d `multi_node_*` tests state the target: up to 3 es-mini processes, given a
//! fixed node list at startup (`ES_MINI_NODES`, comma-separated `host:port` including the node's
//! own), coordinate over es-mini's own HTTP protocol, without replicas, failover, or master
//! election, and answer like one node holding the same data.

use es_index::routing::shard_id;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// N es-mini processes on free localhost ports, each with its own data directory. Dropping it
/// kills every process and removes the directories, even when a test panics.
struct Cluster {
    ports: Vec<u16>,
    children: Vec<Child>,
    dir: PathBuf,
}

impl Cluster {
    fn start(n: usize, name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("esm-multi-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Hold every listener until all ports are picked, so no two nodes get the same port.
        let listeners: Vec<TcpListener> = (0..n)
            .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
            .collect();
        let ports = listeners
            .iter()
            .map(|l| l.local_addr().unwrap().port())
            .collect();
        drop(listeners);
        let mut cluster = Cluster {
            ports,
            children: Vec::new(),
            dir,
        };
        cluster.spawn_all();
        cluster
    }

    fn spawn_all(&mut self) {
        let nodes = self
            .ports
            .iter()
            .map(|p| format!("127.0.0.1:{p}"))
            .collect::<Vec<_>>()
            .join(",");
        for (i, port) in self.ports.iter().enumerate() {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_es-mini"));
            cmd.env("ES_MINI_PORT", port.to_string())
                .env("ES_MINI_DATA", self.dir.join(format!("node{i}")))
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if self.ports.len() > 1 {
                cmd.env("ES_MINI_NODES", &nodes);
            }
            self.children.push(cmd.spawn().unwrap());
        }
        for node in 0..self.ports.len() {
            let deadline = Instant::now() + Duration::from_secs(30);
            while self.try_raw(node, "GET", "/", "").is_err() {
                assert!(Instant::now() < deadline, "node {node} did not start");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    fn kill_all(&mut self) {
        for mut child in self.children.drain(..) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Kills every node without a graceful shutdown and starts them again on the same ports
    /// and data directories.
    fn crash_and_restart(&mut self) {
        self.kill_all();
        self.spawn_all();
    }

    fn try_raw(
        &self,
        node: usize,
        method: &str,
        path: &str,
        body: &str,
    ) -> std::io::Result<(u16, String)> {
        // HTTP/1.0 so the server closes the connection and never chunks the body.
        let mut s = TcpStream::connect(("127.0.0.1", self.ports[node]))?;
        write!(
            s,
            "{method} {path} HTTP/1.0\r\nHost: localhost\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        )?;
        let mut res = String::new();
        s.read_to_string(&mut res)?;
        let (head, body) = res.split_once("\r\n\r\n").unwrap_or((&res, ""));
        let status = head
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| std::io::Error::other(format!("bad response: {res}")))?;
        Ok((status, body.to_owned()))
    }

    /// Sends a request to one node and returns the status and JSON body.
    fn call(&self, node: usize, method: &str, path: &str, body: &str) -> (u16, Value) {
        let (status, text) = self.try_raw(node, method, path, body).unwrap();
        let value = serde_json::from_str(&text).unwrap_or_else(|_| panic!("{path}: {text}"));
        (status, value)
    }

    /// Like `call`, asserting a 2xx status.
    fn ok(&self, node: usize, method: &str, path: &str, body: &str) -> Value {
        let (status, value) = self.call(node, method, path, body);
        assert!(
            (200..300).contains(&status),
            "{method} {path}: {status} {value}"
        );
        value
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        self.kill_all();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

const SHARDS: u32 = 3;
const DOCS: usize = 30;

fn create_index(c: &Cluster, node: usize) {
    let body = json!({
        "settings": {"number_of_shards": SHARDS},
        "mappings": {"properties": {
            "g": {"type": "keyword"}, "n": {"type": "long"}, "t": {"type": "text"}
        }}
    });
    c.ok(node, "PUT", "/i", &body.to_string());
}

fn doc(i: usize) -> Value {
    let g = ["a", "b", "c"][i % 3];
    json!({"g": g, "n": i, "t": format!("w{} common", i % 5)})
}

/// Creates index `i` and indexes `DOCS` documents, document `k` through node `node_for(k)`,
/// returning each write's response.
fn load(c: &Cluster, node_for: impl Fn(usize) -> usize) -> Vec<Value> {
    create_index(c, node_for(0));
    (0..DOCS)
        .map(|k| {
            c.ok(
                node_for(k),
                "PUT",
                &format!("/i/_doc/{k}"),
                &doc(k).to_string(),
            )
        })
        .collect()
}

/// Drops the fields that legitimately differ between two runs of the same request.
fn stable(mut v: Value) -> Value {
    if let Some(o) = v.as_object_mut() {
        o.remove("took");
    }
    v
}

fn shards_ok() -> Value {
    json!({"total": 3, "successful": 3, "skipped": 0, "failed": 0})
}

/// The reads every node of a cluster must answer exactly as one node with the same data does.
fn reads() -> Vec<(&'static str, &'static str, String)> {
    let q = |v: Value| v.to_string();
    vec![
        ("POST", "/i/_search", q(json!({"size": 100, "sort": ["n"]}))),
        (
            "POST",
            "/i/_search",
            q(json!({"from": 5, "size": 10, "sort": [{"n": "desc"}]})),
        ),
        (
            "POST",
            "/i/_search",
            q(json!({"size": 100, "query": {"match": {"t": "w1 common"}}})),
        ),
        (
            "POST",
            "/i/_count",
            q(json!({"query": {"term": {"g": "b"}}})),
        ),
        ("POST", "/i/_count", String::new()),
        (
            "POST",
            "/i/_search",
            q(json!({"size": 0, "aggs": {
                "g": {"terms": {"field": "g"}, "aggs": {"s": {"sum": {"field": "n"}}}},
                "avg": {"avg": {"field": "n"}}, "max": {"max": {"field": "n"}}
            }})),
        ),
        (
            "POST",
            "/_query",
            q(json!({"query": "FROM i | STATS c = COUNT(*), s = SUM(n) BY g | SORT g"})),
        ),
        (
            "POST",
            "/_query",
            q(json!({"query": "FROM i | SORT n DESC | LIMIT 5 | KEEP n, g"})),
        ),
    ]
}

#[test]
fn multi_node_guard_routing_by_id() {
    let c = Cluster::start(1, "guard-routing-id");
    let writes = load(&c, |_| 0);
    // Each shard numbers its own writes, so `_seq_no` reveals the shard routing picked.
    let mut next = [0u64; SHARDS as usize];
    for (k, w) in writes.iter().enumerate() {
        let shard = shard_id(&k.to_string(), None, SHARDS) as usize;
        assert_eq!(w["_seq_no"], next[shard], "doc {k}: {w}");
        next[shard] += 1;
    }
    let cat = c.ok(
        0,
        "GET",
        "/_cat/shards/i?format=json&h=shard,prirep,docs",
        "",
    );
    for row in cat
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["prirep"] == "p")
    {
        let shard: usize = row["shard"].as_str().unwrap().parse().unwrap();
        assert_eq!(row["docs"], next[shard].to_string(), "{cat}");
    }
    for k in 0..DOCS {
        let got = c.ok(0, "GET", &format!("/i/_doc/{k}"), "");
        assert_eq!(got["_source"], doc(k), "{got}");
    }
}

#[test]
fn multi_node_guard_routing_param() {
    let c = Cluster::start(1, "guard-routing-param");
    create_index(&c, 0);
    let mut next = [0u64; SHARDS as usize];
    for k in 0..DOCS {
        let routing = format!("r{}", k % 4);
        let w = c.ok(
            0,
            "PUT",
            &format!("/i/_doc/{k}?routing={routing}"),
            &doc(k).to_string(),
        );
        let shard = shard_id(&k.to_string(), Some(&routing), SHARDS) as usize;
        assert_eq!(w["_seq_no"], next[shard], "doc {k}: {w}");
        next[shard] += 1;
        let got = c.ok(0, "GET", &format!("/i/_doc/{k}?routing={routing}"), "");
        assert_eq!(
            (&got["found"], &got["_routing"]),
            (&json!(true), &json!(routing))
        );
    }
    let r = c.ok(0, "POST", "/i/_search?size=100&routing=r1", "");
    let hits = r["hits"]["hits"].as_array().unwrap();
    assert!(hits.iter().all(|h| h["_routing"].is_string()), "{r}");
    assert!(hits.iter().any(|h| h["_routing"] == "r1"), "{r}");
}

#[test]
fn multi_node_guard_search_merges_shards() {
    let c = Cluster::start(1, "guard-search");
    load(&c, |_| 0);
    let r = c.ok(
        0,
        "POST",
        "/i/_search",
        &json!({"from": 5, "size": 10, "sort": ["n"]}).to_string(),
    );
    assert_eq!(r["_shards"], shards_ok());
    assert_eq!(r["hits"]["total"], json!({"value": DOCS, "relation": "eq"}));
    let ns: Vec<_> = r["hits"]["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["_source"]["n"].clone())
        .collect();
    assert_eq!(ns, (5..15).map(|n| json!(n)).collect::<Vec<_>>());
    let r = c.ok(
        0,
        "POST",
        "/i/_search?size=100",
        &json!({"query": {"match": {"t": "w1"}}}).to_string(),
    );
    let hits = r["hits"]["hits"].as_array().unwrap();
    assert_eq!(hits.len(), DOCS / 5, "{r}");
    let scores: Vec<f64> = hits.iter().map(|h| h["_score"].as_f64().unwrap()).collect();
    assert!(scores.windows(2).all(|w| w[0] >= w[1]), "{scores:?}");
    assert_eq!(r["hits"]["max_score"].as_f64(), Some(scores[0]));
}

#[test]
fn multi_node_guard_count() {
    let c = Cluster::start(1, "guard-count");
    load(&c, |_| 0);
    let r = c.ok(0, "POST", "/i/_count", "");
    assert_eq!(r, json!({"count": DOCS, "_shards": shards_ok()}));
    let r = c.ok(
        0,
        "POST",
        "/i/_count",
        &json!({"query": {"term": {"g": "b"}}}).to_string(),
    );
    assert_eq!(r["count"], DOCS / 3);
}

#[test]
fn multi_node_guard_aggs() {
    let c = Cluster::start(1, "guard-aggs");
    load(&c, |_| 0);
    let body = json!({"size": 0, "aggs": {
        "g": {"terms": {"field": "g"}, "aggs": {"s": {"sum": {"field": "n"}}}},
        "min": {"min": {"field": "n"}}, "max": {"max": {"field": "n"}}, "avg": {"avg": {"field": "n"}}
    }});
    let r = c.ok(0, "POST", "/i/_search", &body.to_string());
    assert_eq!(r["_shards"], shards_ok());
    let sum = |g: usize| (0..DOCS).filter(|k| k % 3 == g).sum::<usize>() as f64;
    assert_eq!(
        r["aggregations"]["g"]["buckets"],
        json!([
            {"key": "a", "doc_count": 10, "s": {"value": sum(0)}},
            {"key": "b", "doc_count": 10, "s": {"value": sum(1)}},
            {"key": "c", "doc_count": 10, "s": {"value": sum(2)}},
        ])
    );
    assert_eq!(r["aggregations"]["min"]["value"], 0.0);
    assert_eq!(r["aggregations"]["max"]["value"], (DOCS - 1) as f64);
    assert_eq!(r["aggregations"]["avg"]["value"], (DOCS - 1) as f64 / 2.0);
}

#[test]
fn multi_node_guard_esql() {
    let c = Cluster::start(1, "guard-esql");
    load(&c, |_| 0);
    let q = |query: &str| c.ok(0, "POST", "/_query", &json!({"query": query}).to_string());
    let r = q("FROM i | STATS c = COUNT(*), s = SUM(n) BY g | SORT g");
    assert_eq!(
        r["values"],
        json!([[10, 135, "a"], [10, 145, "b"], [10, 155, "c"]]),
        "{r}"
    );
    let r = q("FROM i | SORT n DESC | LIMIT 3 | KEEP n");
    assert_eq!(r["values"], json!([[29], [28], [27]]), "{r}");
}

#[test]
fn multi_node_guard_bulk_across_shards() {
    let c = Cluster::start(1, "guard-bulk");
    create_index(&c, 0);
    let body: String = (0..12)
        .map(|k| {
            format!(
                "{}\n{}\n",
                json!({"index": {"_index": "i", "_id": k.to_string()}}),
                doc(k)
            )
        })
        .collect();
    let r = c.ok(0, "POST", "/_bulk", &body);
    assert_eq!(r["errors"], false, "{r}");
    let mut next = [0u64; SHARDS as usize];
    for (k, item) in r["items"].as_array().unwrap().iter().enumerate() {
        let item = &item["index"];
        let shard = shard_id(&k.to_string(), None, SHARDS) as usize;
        assert_eq!(item["status"], 201, "{item}");
        assert_eq!(item["_seq_no"], next[shard], "{item}");
        assert_eq!(
            item["_shards"],
            json!({"total": 2, "successful": 1, "failed": 0})
        );
        next[shard] += 1;
    }
    assert_eq!(c.ok(0, "POST", "/i/_count", "")["count"], 12);
}

#[test]
fn multi_node_guard_refresh() {
    let c = Cluster::start(1, "guard-refresh");
    create_index(&c, 0);
    let w = c.ok(0, "PUT", "/i/_doc/1?refresh=true", &doc(1).to_string());
    assert_eq!(w["forced_refresh"], true, "{w}");
    c.ok(0, "PUT", "/i/_doc/2", &doc(2).to_string());
    let r = c.ok(0, "POST", "/i/_refresh", "");
    // Elasticsearch counts the unassigned replica of each shard as not refreshed.
    assert_eq!(
        r,
        json!({"_shards": {"total": 6, "successful": 3, "failed": 0}})
    );
    assert_eq!(c.ok(0, "POST", "/i/_count", "")["count"], 2);
}

#[test]
fn multi_node_guard_index_create_delete() {
    let c = Cluster::start(1, "guard-index");
    let health = c.ok(0, "GET", "/_cluster/health", "");
    assert_eq!(
        (&health["number_of_nodes"], &health["number_of_data_nodes"]),
        (&json!(1), &json!(1))
    );
    let body = json!({"settings": {"number_of_shards": SHARDS}}).to_string();
    let r = c.ok(0, "PUT", "/i", &body);
    assert_eq!(
        r,
        json!({"acknowledged": true, "shards_acknowledged": true, "index": "i"})
    );
    assert_eq!(
        c.ok(0, "GET", "/i", "")["i"]["settings"]["index"]["number_of_shards"],
        "3"
    );
    assert_eq!(
        c.ok(0, "GET", "/_cluster/health", "")["active_primary_shards"],
        3
    );
    assert_eq!(c.ok(0, "DELETE", "/i", ""), json!({"acknowledged": true}));
    let (status, r) = c.call(0, "GET", "/i", "");
    assert_eq!(
        (status, &r["error"]["type"]),
        (404, &json!("index_not_found_exception"))
    );
}

#[test]
fn multi_node_guard_data_survives_restart() {
    let mut c = Cluster::start(1, "guard-restart");
    load(&c, |_| 0);
    let before: Vec<Value> = reads()
        .iter()
        .map(|(m, p, b)| stable(c.ok(0, m, p, b)))
        .collect();
    c.crash_and_restart();
    let after: Vec<Value> = reads()
        .iter()
        .map(|(m, p, b)| stable(c.ok(0, m, p, b)))
        .collect();
    assert_eq!(before, after);
    // Each shard keeps numbering where it left off.
    let w = c.ok(0, "PUT", "/i/_doc/new", &doc(0).to_string());
    let shard = shard_id("new", None, SHARDS);
    let on_shard = (0..DOCS)
        .filter(|k| shard_id(&k.to_string(), None, SHARDS) == shard)
        .count();
    assert_eq!(w["_seq_no"], on_shard, "{w}");
}

#[test]
fn multi_node_three_shards_spread_over_three_nodes() {
    let c = Cluster::start(3, "spread");
    create_index(&c, 0);
    for node in 0..3 {
        let cat = c.ok(
            node,
            "GET",
            "/_cat/shards/i?format=json&h=shard,prirep,state,node",
            "",
        );
        let mut owners: Vec<_> = cat
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["prirep"] == "p")
            .map(|r| {
                assert_eq!(r["state"], "STARTED", "{cat}");
                r["node"].as_str().unwrap().to_owned()
            })
            .collect();
        owners.sort();
        owners.dedup();
        assert_eq!(owners.len(), 3, "node {node}: {cat}");
    }
}

#[test]
fn multi_node_single_doc_requests_reach_the_owner() {
    let one = Cluster::start(1, "reference-docs");
    let three = Cluster::start(3, "docs");
    // Each shard numbers its own writes, so equal `_seq_no`s mean each shard lives on one node.
    assert_eq!(load(&three, |k| k % 3), load(&one, |_| 0));
    let update = json!({"doc": {"n": 100}}).to_string();
    for k in 0..DOCS {
        let (node, path) = (k % 3, format!("/i/_update/{k}"));
        assert_eq!(
            three.ok(node, "POST", &path, &update),
            one.ok(0, "POST", &path, &update)
        );
    }
    for node in 0..3 {
        for k in 0..DOCS {
            for path in [format!("/i/_doc/{k}"), format!("/i/_source/{k}")] {
                assert_eq!(
                    three.call(node, "GET", &path, ""),
                    one.call(0, "GET", &path, "")
                );
            }
        }
    }
    for k in 0..DOCS {
        let path = format!("/i/_doc/{k}");
        assert_eq!(
            three.call(k % 3, "DELETE", &path, ""),
            one.call(0, "DELETE", &path, "")
        );
        assert_eq!(three.call((k + 1) % 3, "GET", &path, "").0, 404);
    }
}

#[test]
fn multi_node_bulk_reaches_the_owners() {
    let one = Cluster::start(1, "reference-bulk");
    let three = Cluster::start(3, "bulk");
    create_index(&one, 0);
    create_index(&three, 0);
    let mut body: String = (0..DOCS)
        .map(|k| {
            format!(
                "{}\n{}\n",
                json!({"index": {"_index": "i", "_id": k.to_string()}}),
                doc(k)
            )
        })
        .collect();
    body += &format!("{}\n", json!({"delete": {"_id": "3"}}));
    body += &format!(
        "{}\n{}\n",
        json!({"update": {"_id": "4"}}),
        json!({"doc": {"n": 100}})
    );
    let bulk = |c: &Cluster, node| stable(c.ok(node, "POST", "/i/_bulk", &body));
    // Each shard numbers its own writes, so equal `_seq_no`s mean each shard lives on one node.
    assert_eq!(bulk(&three, 1), bulk(&one, 0));
    for node in 0..3 {
        for k in 0..DOCS {
            let path = format!("/i/_doc/{k}");
            assert_eq!(
                three.call(node, "GET", &path, ""),
                one.call(0, "GET", &path, ""),
                "node {node}"
            );
        }
    }
}

#[test]
fn multi_node_search_reads_every_node() {
    let one = Cluster::start(1, "reference-search");
    let three = Cluster::start(3, "search");
    load(&one, |_| 0);
    load(&three, |k| k % 3);
    for node in 0..3 {
        for (m, p, b) in reads().into_iter().filter(|r| r.1 == "/i/_search") {
            assert_eq!(
                stable(three.ok(node, m, p, &b)),
                stable(one.ok(0, m, p, &b)),
                "node {node}: {b}"
            );
        }
    }
}

#[test]
fn multi_node_count_reads_every_node() {
    let one = Cluster::start(1, "reference-count");
    let three = Cluster::start(3, "count");
    load(&one, |_| 0);
    load(&three, |k| k % 3);
    for node in 0..3 {
        for (m, p, b) in reads().into_iter().filter(|r| r.1 == "/i/_count") {
            assert_eq!(
                three.ok(node, m, p, &b),
                one.ok(0, m, p, &b),
                "node {node}: {b}"
            );
        }
    }
}

#[test]
fn multi_node_esql_reads_every_node() {
    let one = Cluster::start(1, "reference-esql");
    let three = Cluster::start(3, "esql");
    load(&one, |_| 0);
    load(&three, |k| k % 3);
    for node in 0..3 {
        for (m, p, b) in reads().into_iter().filter(|r| r.1 == "/_query") {
            assert_eq!(
                stable(three.ok(node, m, p, &b)),
                stable(one.ok(0, m, p, &b)),
                "node {node}: {b}"
            );
        }
    }
}

#[test]
#[ignore = "target: multi-node"]
fn multi_node_any_node_serves_all_docs_like_one_node() {
    let one = Cluster::start(1, "reference");
    let three = Cluster::start(3, "serve");
    load(&one, |_| 0);
    load(&three, |k| k % 3);
    let body: String = (DOCS..DOCS + 6)
        .map(|k| {
            format!(
                "{}\n{}\n",
                json!({"index": {"_index": "i", "_id": k.to_string()}}),
                doc(k)
            )
        })
        .collect();
    let bulk = |c: &Cluster, node| {
        let r = c.ok(node, "POST", "/_bulk", &body);
        stable(r)
    };
    assert_eq!(bulk(&three, 1), bulk(&one, 0));
    for node in 0..3 {
        for k in 0..DOCS + 6 {
            let path = format!("/i/_doc/{k}");
            assert_eq!(
                three.ok(node, "GET", &path, ""),
                one.ok(0, "GET", &path, ""),
                "node {node}"
            );
        }
        for (m, p, b) in reads() {
            assert_eq!(
                stable(three.ok(node, m, p, &b)),
                stable(one.ok(0, m, p, &b)),
                "node {node}: {p} {b}"
            );
        }
    }
}

#[test]
fn multi_node_cluster_reports_three_nodes() {
    let c = Cluster::start(3, "nodes");
    for node in 0..3 {
        let health = c.ok(node, "GET", "/_cluster/health", "");
        assert_eq!(health["number_of_nodes"], 3, "{health}");
        assert_eq!(health["number_of_data_nodes"], 3, "{health}");
        let cat = c.ok(node, "GET", "/_cat/nodes?format=json", "");
        assert_eq!(cat.as_array().map(Vec::len), Some(3), "{cat}");
    }
}

#[test]
fn multi_node_index_create_delete_seen_on_all() {
    let c = Cluster::start(3, "lifecycle");
    create_index(&c, 1);
    for node in 0..3 {
        let r = c.ok(node, "GET", "/i", "");
        assert_eq!(
            r["i"]["settings"]["index"]["number_of_shards"], "3",
            "node {node}"
        );
    }
    c.ok(2, "DELETE", "/i", "");
    for node in 0..3 {
        assert_eq!(c.call(node, "GET", "/i", "").0, 404, "node {node}");
    }
}
