use std::{
    collections::HashMap,
    error::Error,
    sync::{Arc, Mutex, atomic::AtomicU64},
    time::Duration,
};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval};

use crate::{
    config::Config, exporter::exporter_worker, hooks::expire_sessions, http::router,
    spans::SpanDraft,
};

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct Inner {
    pub(crate) sessions: Mutex<HashMap<String, crate::state::SessionState>>,
    pub(crate) tx: mpsc::Sender<SpanDraft>,
    pub(crate) otlp_endpoint: String,
    pub(crate) otlp_headers_configured: bool,
    pub(crate) langfuse: bool,
    pub(crate) service_name: String,
    pub(crate) recent_events: Mutex<HashMap<String, u64>>,
    pub(crate) dropped_total: AtomicU64,
}

pub async fn run(config: Config) -> Result<(), Box<dyn Error>> {
    let (tx, rx) = mpsc::channel(config.queue_capacity);
    let client = reqwest::Client::builder()
        .default_headers(config.otlp_headers.clone())
        .build()?;
    let state = Arc::new(AppState {
        inner: Arc::new(Inner {
            sessions: Mutex::new(HashMap::new()),
            tx,
            otlp_endpoint: config.otlp_endpoint.clone(),
            otlp_headers_configured: config.otlp_headers_configured,
            langfuse: config.langfuse,
            service_name: config.service_name.clone(),
            recent_events: Mutex::new(HashMap::new()),
            dropped_total: AtomicU64::new(0),
        }),
    });

    let exporter = tokio::spawn(exporter_worker(
        rx,
        client,
        config.otlp_endpoint.clone(),
        config.service_name.clone(),
    ));
    let ttl = if config.ttl_seconds > 0 {
        Some(spawn_ttl(
            state.clone(),
            Duration::from_secs(config.ttl_seconds),
            config.ttl_sweep_seconds,
        ))
    } else {
        None
    };

    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind(&config.listen_addr).await?;
    tracing::info!(
        listen_addr = %config.listen_addr,
        otlp_endpoint = %config.otlp_endpoint,
        queue_capacity = config.queue_capacity,
        ttl_seconds = config.ttl_seconds,
        "claude hooks collector listening"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    if let Some(handle) = ttl {
        handle.abort();
    }
    // Closing the last sender lets the exporter drain its queue before exiting.
    drop(state);
    match tokio::time::timeout(Duration::from_secs(10), exporter).await {
        Ok(Ok(())) => tracing::info!("exporter drained"),
        Ok(Err(error)) => tracing::warn!(error = %error, "exporter task failed"),
        Err(_) => tracing::warn!("exporter shutdown timed out"),
    }

    Ok(())
}

fn spawn_ttl(state: Arc<AppState>, ttl: Duration, sweep_seconds: u64) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(sweep_seconds.max(1)));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            expire_sessions(&state, ttl.as_nanos() as u64);
        }
    })
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::warn!(error = %error, "cannot install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
