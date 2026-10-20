use es_mini::app_with;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let port = std::env::var("ES_MINI_PORT").unwrap_or_else(|_| "9200".into());
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .expect("bind 127.0.0.1");
    tracing::info!("listening on {}", listener.local_addr().unwrap());
    // ponytail: a data directory is served as a read-only snapshot, loaded once at startup
    let indices = match std::env::var("ES_MINI_DATA") {
        Ok(dir) => es_index::disk::open(dir.as_ref()).expect("open ES_MINI_DATA"),
        Err(_) => Default::default(),
    };
    axum::serve(listener, app_with(indices)).await.unwrap();
}
