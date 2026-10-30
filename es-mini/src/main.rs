#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let port = std::env::var("ES_MINI_PORT").unwrap_or_else(|_| "9200".into());
    // Local-only by default; ES_MINI_HOST=0.0.0.0 inside a container so other containers can connect.
    let host = std::env::var("ES_MINI_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let listener = tokio::net::TcpListener::bind(format!("{host}:{port}"))
        .await
        .expect("bind ES_MINI_HOST:ES_MINI_PORT");
    // Node settings, as Elasticsearch's Docker image takes them: environment variables named
    // like settings (`xpack.security.enabled`), and ELASTIC_PASSWORD as `bootstrap.password`.
    let mut settings: std::collections::HashMap<String, String> =
        std::env::vars().filter(|(k, _)| k.contains('.')).collect();
    if let Ok(p) = std::env::var("ELASTIC_PASSWORD") {
        settings.insert("bootstrap.password".into(), p);
    }
    tracing::info!("listening on {}", listener.local_addr().unwrap());
    match std::env::var("ES_MINI_DATA") {
        Ok(dir) => es_mini::serve_persisted(listener, dir.as_ref(), &settings, shutdown())
            .await
            .expect("serve and flush ES_MINI_DATA"),
        Err(_) => axum::serve(
            listener,
            es_mini::app_with_settings(&settings).expect("settings"),
        )
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
