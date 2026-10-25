use std::sync::{Arc, atomic::Ordering};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    routing::{get, post},
};
use serde_json::{Value, json};

use crate::app::AppState;
use crate::hooks::handle_event;
use crate::usage::{run_summary, run_turn_usage};

pub(crate) fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/hooks", post(hooks))
        .route("/health", get(health))
        .with_state(state)
        .layer(DefaultBodyLimit::max(32 * 1024 * 1024))
}

async fn hooks(State(state): State<Arc<AppState>>, Json(payload): Json<Value>) -> StatusCode {
    let output = handle_event(&state, &payload);

    for span in output.spans {
        if let Err(error) = state.inner.tx.try_send(span) {
            state.inner.dropped_total.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(error = %error, "span queue full or closed; dropping span");
        }
    }

    if let Some(job) = output.turn_usage {
        tokio::spawn(run_turn_usage(state.clone(), job));
    }

    if let Some(job) = output.summary {
        tokio::spawn(run_summary(state.clone(), job));
    }

    StatusCode::NO_CONTENT
}

async fn health(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "service": "claude-hooks-collector",
        "otel_service_name": "claude-code-hooks",
        "otel_service_namespace": "claude-code",
        "otlp_endpoint": state.inner.otlp_endpoint,
        "spool_dir": state.inner.spool_dir.display().to_string(),
        "otlp_headers_configured": state.inner.otlp_headers_configured,
        "sessions": state.inner.sessions.lock().map(|sessions| sessions.len()).unwrap_or(0),
        "queue_capacity": state.inner.tx.max_capacity(),
        "queue_available": state.inner.tx.capacity(),
        "dropped_total": state.inner.dropped_total.load(Ordering::Relaxed),
    }))
}
