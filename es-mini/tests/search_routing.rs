//! Searches, counts and by-query writes search only the shards their `?routing=` and the
//! search_routing of the aliases they go through select, as Elasticsearch 9.5.5's
//! IndexNameExpressionResolver.resolveSearchRouting does. Every expected value here is what
//! Elasticsearch 9.5.5 answered to the same requests.

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn send(app: &axum::Router, method: &str, uri: &str, body: String) -> Value {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

async fn call(app: &axum::Router, method: &str, uri: &str, body: Value) -> Value {
    send(app, method, uri, body.to_string()).await
}

/// `r` (3 shards: routing values `a` and `b` share one, `c` and `zzz` others) and `o` (2
/// shards), with aliases `ra` (search_routing a), `rab` (a,b), `rplain` (none) on `r` and `oa`
/// (routing a) on `o`.
async fn setup() -> axum::Router {
    let app = es_mini::app();
    for (index, shards) in [("r", 3), ("o", 2)] {
        let settings = json!({"settings": {"number_of_shards": shards}});
        call(&app, "PUT", &format!("/{index}"), settings).await;
    }
    let docs = [
        ("r", 1, Some("a")),
        ("r", 2, Some("b")),
        ("r", 3, Some("c")),
        ("r", 4, None),
        ("r", 5, None),
        ("r", 6, None),
        ("r", 7, None),
        ("o", 11, None),
        ("o", 12, Some("a")),
        ("o", 13, None),
    ];
    for (index, n, routing) in docs {
        let id = n % 10;
        let routing = routing.map_or(String::new(), |r| format!("&routing={r}"));
        let uri = format!("/{index}/_doc/{id}?refresh=true{routing}");
        call(&app, "PUT", &uri, json!({"n": n, "k": "x"})).await;
    }
    let add = |index: &str, alias: &str, routing: Value| {
        let mut add = json!({"index": index, "alias": alias});
        add.as_object_mut()
            .unwrap()
            .extend(routing.as_object().unwrap().clone());
        json!({"add": add})
    };
    let actions = [
        add("r", "ra", json!({"search_routing": "a"})),
        add("r", "rab", json!({"search_routing": "a,b"})),
        add("r", "rplain", json!({})),
        add("o", "oa", json!({"routing": "a"})),
    ];
    call(&app, "POST", "/_aliases", json!({ "actions": actions })).await;
    app
}

/// The `_shards.total` and the `n` of each hit, of a search sorted by `n`.
async fn search(app: &axum::Router, target: &str, params: &str) -> (Value, Vec<u64>) {
    let uri = format!("/{target}/_search?size=20{params}");
    let r = call(app, "GET", &uri, json!({"sort": ["n"]})).await;
    let hits = r["hits"]["hits"].as_array().expect("hits");
    let ns = hits.iter().map(|h| h["_source"]["n"].as_u64().unwrap());
    (r["_shards"]["total"].clone(), ns.collect())
}

#[tokio::test]
async fn routing_parameter_searches_only_the_routed_shards() {
    let app = setup().await;
    let ab = (json!(1), vec![1, 2]);
    assert_eq!(search(&app, "r", "&routing=a").await, ab);
    assert_eq!(search(&app, "r", "&routing=a,b").await, ab);
    assert_eq!(search(&app, "r", "&routing=zzz").await, (json!(1), vec![]));
    let both = (json!(2), vec![1, 2, 12, 13]);
    assert_eq!(search(&app, "r,o", "&routing=a").await, both);
    assert_eq!(search(&app, "_all", "&routing=a").await, both);
}

#[tokio::test]
async fn alias_search_routing_narrows_the_shards() {
    let app = setup().await;
    let all = (json!(3), vec![1, 2, 3, 4, 5, 6, 7]);
    assert_eq!(search(&app, "ra", "").await, (json!(1), vec![1, 2]));
    // the request's routing intersected with the alias's; none in common searches every shard
    assert_eq!(search(&app, "ra", "&routing=b").await, all);
    assert_eq!(
        search(&app, "rab", "&routing=b").await,
        (json!(1), vec![1, 2])
    );
    // reaching the index directly or through an alias without routing searches every shard
    assert_eq!(search(&app, "r,ra", "").await, all);
    assert_eq!(search(&app, "ra,rplain", "").await, all);
    assert_eq!(search(&app, "r*", "").await, all);
    let (total, ns) = search(&app, "ra,o", "").await;
    assert_eq!((total, ns), (json!(3), vec![1, 2, 11, 12, 13]));
    let (total, ns) = search(&app, "ra,oa", "").await;
    assert_eq!((total, ns), (json!(2), vec![1, 2, 12, 13]));
}

#[tokio::test]
async fn count_scroll_and_msearch_are_routed() {
    let app = setup().await;
    let routed =
        json!({"count": 2, "_shards": {"total": 1, "successful": 1, "skipped": 0, "failed": 0}});
    assert_eq!(
        call(&app, "GET", "/r/_count?routing=a", json!({})).await,
        routed
    );
    assert_eq!(call(&app, "GET", "/ra/_count", json!({})).await, routed);
    let r = call(
        &app,
        "GET",
        "/r/_search?routing=b&scroll=1m&size=1",
        json!({}),
    )
    .await;
    assert_eq!(r["_shards"]["total"], 1);
    assert_eq!(r["hits"]["total"]["value"], 2);
    let body = "{\"index\":\"r\",\"routing\":\"a\"}\n{}\n{\"index\":\"ra\"}\n{}\n".to_owned();
    let r = send(&app, "POST", "/_msearch", body).await;
    for resp in r["responses"].as_array().unwrap() {
        assert_eq!(resp["_shards"]["total"], 1);
        assert_eq!(resp["hits"]["total"]["value"], 2);
    }
}

#[tokio::test]
async fn by_query_writes_only_the_routed_shards() {
    let app = setup().await;
    let all = json!({"query": {"match_all": {}}});
    for uri in [
        "/r/_update_by_query?routing=b&refresh=true",
        "/ra/_update_by_query?refresh=true",
    ] {
        let r = call(&app, "POST", uri, all.clone()).await;
        assert_eq!((&r["total"], &r["updated"]), (&json!(2), &json!(2)));
    }
    let uri = "/r/_delete_by_query?routing=a&refresh=true";
    let r = call(&app, "POST", uri, all.clone()).await;
    assert_eq!((&r["total"], &r["deleted"]), (&json!(2), &json!(2)));
    let r = call(&app, "POST", "/rab/_delete_by_query?refresh=true", all).await;
    assert_eq!((&r["total"], &r["deleted"]), (&json!(0), &json!(0)));
    assert_eq!(search(&app, "r", "").await, (json!(3), vec![3, 4, 5, 6, 7]));
}
