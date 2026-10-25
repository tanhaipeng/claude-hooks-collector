use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval};

use crate::spans::{SpanDraft, otlp_body};

async fn export_batch(
    client: &reqwest::Client,
    endpoint: &str,
    body: &Value,
) -> Result<(), String> {
    let response = client
        .post(endpoint)
        .json(body)
        .send()
        .await
        .map_err(|error| error.to_string())?;

    if !response.status().is_success() {
        return Err(format!("OTLP endpoint returned HTTP {}", response.status()));
    }
    Ok(())
}

async fn flush_batch(client: &reqwest::Client, endpoint: &str, spans: &[SpanDraft]) {
    if spans.is_empty() {
        return;
    }

    let body = otlp_body(spans);
    for attempt in 1..=3 {
        match export_batch(client, endpoint, &body).await {
            Ok(()) => return,
            Err(error) => {
                tracing::warn!(
                    attempt,
                    spans = spans.len(),
                    error = %error,
                    "OTLP batch export failed"
                );
                tokio::time::sleep(Duration::from_millis(200 * attempt)).await;
            }
        }
    }

    tracing::error!(
        spans = spans.len(),
        "OTLP batch failed after retries; dropping batch"
    );
}

pub(crate) async fn exporter_worker(
    mut rx: mpsc::Receiver<SpanDraft>,
    client: reqwest::Client,
    endpoint: String,
) {
    let mut batch = Vec::with_capacity(64);
    let mut ticker = interval(Duration::from_millis(250));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            item = rx.recv() => {
                match item {
                    Some(span) => {
                        batch.push(span);
                        if batch.len() >= 64 {
                            flush_batch(&client, &endpoint, &batch).await;
                            batch.clear();
                        }
                    }
                    None => {
                        if !batch.is_empty() {
                            flush_batch(&client, &endpoint, &batch).await;
                        }
                        break;
                    }
                }
            }
            _ = ticker.tick() => {
                if !batch.is_empty() {
                    flush_batch(&client, &endpoint, &batch).await;
                    batch.clear();
                }
            }
        }
    }
}
