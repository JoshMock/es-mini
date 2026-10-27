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
    match std::env::var("ES_MINI_DATA") {
        Ok(dir) => es_mini::serve_persisted(listener, dir.as_ref(), shutdown())
            .await
            .expect("serve and flush ES_MINI_DATA"),
        Err(_) => axum::serve(listener, es_mini::app())
            .with_graceful_shutdown(shutdown())
            .await
            .unwrap(),
    }
}

/// Completes on SIGINT (Ctrl-C) or SIGTERM.
async fn shutdown() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
}
