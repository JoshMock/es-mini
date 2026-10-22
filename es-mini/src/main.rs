#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let port = std::env::var("ES_MINI_PORT").unwrap_or_else(|_| "9200".into());
    // Local-only by default; ES_MINI_HOST=0.0.0.0 inside a container so other containers can connect.
    let host = std::env::var("ES_MINI_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let listener = tokio::net::TcpListener::bind(format!("{host}:{port}"))
        .await
        .expect("bind ES_MINI_HOST:ES_MINI_PORT");
    tracing::info!("listening on {}", listener.local_addr().unwrap());
    let app = match std::env::var("ES_MINI_DATA") {
        Ok(dir) => es_mini::app_persisted(dir.as_ref()).expect("open ES_MINI_DATA"),
        Err(_) => es_mini::app(),
    };
    axum::serve(listener, app).await.unwrap();
}
