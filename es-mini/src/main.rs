use es_mini::app;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let port = std::env::var("ES_MINI_PORT").unwrap_or_else(|_| "9200".into());
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .expect("bind 127.0.0.1");
    tracing::info!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app()).await.unwrap();
}
