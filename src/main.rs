use std::{
    collections::{HashMap, HashSet},
    env,
    fs::File,
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    routing::{get, post},
};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval};
use tracing_subscriber::EnvFilter;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

static SPAN_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    sessions: Mutex<HashMap<String, SessionState>>,
    tx: mpsc::Sender<SpanDraft>,
    otlp_endpoint: String,
    spool_dir: PathBuf,
    otlp_headers_configured: bool,
    recent_events: Mutex<HashMap<String, u64>>,
    dropped_total: AtomicU64,
}

struct SessionState {
    session_id: String,
    trace_id: String,
    root_id: String,
    turn_id: Option<String>,
    turn_start_ns: Option<u64>,
    pairs: HashMap<String, StartState>,
    messages: HashMap<String, MessageStream>,
    turn_prompt: Option<String>,
    usage_offset: u64,
    start_ns: u64,
    last_seen_ns: u64,
    turn_count: u64,
    tool_count: u64,
    subagent_count: u64,
    model: Option<String>,
}

struct StartState {
    span_id: String,
    start_ns: u64,
}

struct MessageStream {
    start_ns: u64,
    chunks: Vec<MessageChunk>,
}

#[derive(Clone)]
struct MessageChunk {
    index: u64,
    time_ns: u64,
    final_chunk: bool,
    delta: String,
}

struct SpanDraft {
    trace_id: String,
    span_id: String,
    parent_id: Option<String>,
    name: String,
    start_ns: u64,
    end_ns: u64,
    attributes: Vec<(String, String)>,
    error: bool,
}

struct EventOutput {
    spans: Vec<SpanDraft>,
    summary: Option<SummaryJob>,
    turn_usage: Option<TurnUsageJob>,
}

struct TurnUsageJob {
    span: SpanDraft,
    session_id: String,
    transcript_path: Option<String>,
    prompt_id: Option<String>,
    start_offset: u64,
}

struct SummaryJob {
    trace_id: String,
    root_id: String,
    session_id: String,
    start_ns: u64,
    end_ns: u64,
    turn_count: u64,
    tool_count: u64,
    subagent_count: u64,
    model: Option<String>,
    transcript_path: Option<String>,
}

#[derive(Default)]
struct UsageStats {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
    models: Vec<String>,
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

fn hex_id(input: &str, bytes: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(bytes * 2);
    for byte in &digest[..bytes] {
        out.push_str(&format!("{byte:02x}"));
    }
    if out.bytes().all(|byte| byte == b'0') {
        out.replace_range(0..1, "1");
    }
    out
}

fn unique_span_id(seed: &str) -> String {
    let counter = SPAN_COUNTER.fetch_add(1, Ordering::Relaxed);
    hex_id(&format!("{seed}:{counter}:{}", now_ns()), 8)
}

fn string_field(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn scalar_field(payload: &Value, key: &str) -> Option<String> {
    match payload.get(key)? {
        Value::String(value) => Some(value.clone()),
        Value::Bool(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn u64_field(payload: &Value, key: &str) -> Option<u64> {
    payload.get(key).and_then(Value::as_u64)
}

fn bool_field(payload: &Value, key: &str) -> Option<bool> {
    payload.get(key).and_then(Value::as_bool)
}

fn set_attr(attributes: &mut Vec<(String, String)>, key: &str, value: String) {
    if let Some(existing) = attributes.iter_mut().find(|(known, _)| known == key) {
        existing.1 = value;
    } else {
        attributes.push((key.to_string(), value));
    }
}

fn add_scalar(attributes: &mut Vec<(String, String)>, payload: &Value, key: &str, attr: &str) {
    if let Some(value) = scalar_field(payload, key) {
        attributes.push((attr.to_string(), value));
    }
}

fn base_attributes(
    payload: &Value,
    raw: &str,
    event: &str,
    session_id: &str,
) -> Vec<(String, String)> {
    let mut attributes = vec![
        ("claude.hook.event".to_string(), event.to_string()),
        ("claude.session_id".to_string(), session_id.to_string()),
        ("claude.hook.payload".to_string(), raw.to_string()),
        (
            "claude.hook.payload_bytes".to_string(),
            raw.len().to_string(),
        ),
    ];

    for (key, attr) in [
        ("prompt_id", "claude.prompt_id"),
        ("turn_id", "claude.turn_id"),
        ("cwd", "claude.cwd"),
        ("permission_mode", "claude.permission_mode"),
        ("agent_id", "claude.agent_id"),
        ("agent_type", "claude.agent_type"),
        ("tool_name", "claude.tool_name"),
        ("tool_use_id", "claude.tool_use_id"),
        ("model", "claude.model"),
        ("source", "claude.source"),
        ("to_model", "claude.to_model"),
        ("trigger", "claude.trigger"),
        ("task_id", "claude.task_id"),
        ("task_name", "claude.task_name"),
        ("notification_type", "claude.notification_type"),
        ("file_path", "claude.file_path"),
        ("reason", "claude.reason"),
        ("server_name", "claude.server_name"),
        ("elicitation_id", "claude.elicitation_id"),
        ("stop_hook_active", "claude.stop_hook_active"),
    ] {
        add_scalar(&mut attributes, payload, key, attr);
    }

    if let Some(prompt) = payload.get("prompt").and_then(Value::as_str) {
        attributes.push(("claude.prompt_length".to_string(), prompt.len().to_string()));
        set_attr(
            &mut attributes,
            "langfuse.observation.input",
            json!({ "prompt": prompt }).to_string(),
        );
    }

    if let Some(expanded) = payload.get("expanded_prompt").and_then(Value::as_str) {
        set_attr(
            &mut attributes,
            "langfuse.observation.input",
            json!({ "prompt": expanded }).to_string(),
        );
    }

    if let Some(tool_input) = payload.get("tool_input") {
        set_attr(
            &mut attributes,
            "langfuse.observation.input",
            json!({ "tool_input": tool_input }).to_string(),
        );
    }

    for output_key in ["tool_response", "tool_output"] {
        if let Some(output) = payload.get(output_key) {
            set_attr(
                &mut attributes,
                "langfuse.observation.output",
                json!({ "tool_output": output }).to_string(),
            );
        }
    }

    if let Some(last_message) = payload
        .get("last_assistant_message")
        .and_then(Value::as_str)
    {
        set_attr(
            &mut attributes,
            "langfuse.observation.output",
            json!({ "text": last_message }).to_string(),
        );
    }

    if let Some(error) = payload.get("error").and_then(Value::as_str) {
        set_attr(
            &mut attributes,
            "langfuse.observation.output",
            json!({ "error": error }).to_string(),
        );
    }

    // OTel GenAI semantic conventions, in addition to the raw Claude attributes.
    attributes.push(("gen_ai.conversation.id".to_string(), session_id.to_string()));

    let tool_name = string_field(payload, "tool_name");
    let tool_use_id = string_field(payload, "tool_use_id");
    let agent_type = string_field(payload, "agent_type");
    let agent_id = string_field(payload, "agent_id");
    let model = string_field(payload, "model").or_else(|| string_field(payload, "to_model"));

    match event {
        "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => {
            attributes.push((
                "gen_ai.operation.name".to_string(),
                "execute_tool".to_string(),
            ));
            if let Some(tool_name) = &tool_name {
                attributes.push(("gen_ai.tool.name".to_string(), tool_name.clone()));
            }
            if let Some(tool_use_id) = &tool_use_id {
                attributes.push(("gen_ai.tool.call.id".to_string(), tool_use_id.clone()));
            }
        }
        "Stop" | "StopFailure" => {
            attributes.push(("gen_ai.operation.name".to_string(), "chat".to_string()));
        }
        "UserPromptSubmit" | "SubagentStart" | "SubagentStop" | "SessionStart" | "SessionEnd" => {
            attributes.push((
                "gen_ai.operation.name".to_string(),
                "invoke_agent".to_string(),
            ));
            if let Some(agent_type) = &agent_type {
                attributes.push(("gen_ai.agent.name".to_string(), agent_type.clone()));
            }
            if let Some(agent_id) = &agent_id {
                attributes.push(("gen_ai.agent.id".to_string(), agent_id.clone()));
            }
        }
        _ => {}
    }

    if let Some(model) = &model {
        attributes.push(("gen_ai.request.model".to_string(), model.clone()));
    }

    let langfuse_type = match event {
        "Stop" | "StopFailure" => "generation",
        "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => "tool",
        "MessageDisplay" => "event",
        "SubagentStart" | "SubagentStop" => "agent",
        _ => "span",
    };
    attributes.push((
        "langfuse.observation.type".to_string(),
        langfuse_type.to_string(),
    ));
    attributes.push(("langfuse.session.id".to_string(), session_id.to_string()));

    attributes.push((
        "claude.timing.source".to_string(),
        "hook_receipt".to_string(),
    ));

    attributes
}

fn event_span(
    session: &SessionState,
    parent_id: &str,
    name: impl Into<String>,
    start_ns: u64,
    end_ns: u64,
    attributes: Vec<(String, String)>,
    error: bool,
) -> SpanDraft {
    let name = name.into();
    SpanDraft {
        trace_id: session.trace_id.clone(),
        span_id: unique_span_id(&name),
        parent_id: Some(parent_id.to_string()),
        name,
        start_ns,
        end_ns,
        attributes,
        error,
    }
}

fn build_message_span(
    trace_id: &str,
    parent_id: &str,
    session_id: &str,
    message_id: &str,
    stream: &MessageStream,
    end_ns: u64,
    name: &str,
    mut attributes: Vec<(String, String)>,
    error: bool,
) -> SpanDraft {
    let mut chunks = stream.chunks.clone();
    chunks.sort_by_key(|chunk| chunk.index);

    let total_chars: usize = chunks.iter().map(|chunk| chunk.delta.chars().count()).sum();
    let first_index = chunks.first().map(|chunk| chunk.index).unwrap_or(0);
    let last_index = chunks.last().map(|chunk| chunk.index).unwrap_or(0);
    let duration_ms = end_ns.saturating_sub(stream.start_ns) / 1_000_000;
    let chunks_json: Vec<Value> = chunks
        .iter()
        .map(|chunk| {
            json!({
                "index": chunk.index,
                "time_ns": chunk.time_ns,
                "final": chunk.final_chunk,
                "delta": chunk.delta,
            })
        })
        .collect();
    let full_text: String = chunks.iter().map(|chunk| chunk.delta.as_str()).collect();
    set_attr(
        &mut attributes,
        "langfuse.observation.output",
        json!({ "text": full_text }).to_string(),
    );

    attributes.push(("claude.message.id".to_string(), message_id.to_string()));
    attributes.push((
        "claude.message.chunk_count".to_string(),
        chunks.len().to_string(),
    ));
    attributes.push((
        "claude.message.total_chars".to_string(),
        total_chars.to_string(),
    ));
    attributes.push((
        "claude.message.first_index".to_string(),
        first_index.to_string(),
    ));
    attributes.push((
        "claude.message.last_index".to_string(),
        last_index.to_string(),
    ));
    attributes.push(("claude.message.final".to_string(), (!error).to_string()));
    attributes.push((
        "claude.message.duration_ms".to_string(),
        duration_ms.to_string(),
    ));
    attributes.push(("gen_ai.conversation.id".to_string(), session_id.to_string()));
    attributes.push((
        "claude.message.chunks_json".to_string(),
        serde_json::to_string(&chunks_json).unwrap_or_default(),
    ));

    SpanDraft {
        trace_id: trace_id.to_string(),
        span_id: unique_span_id(name),
        parent_id: Some(parent_id.to_string()),
        name: name.to_string(),
        start_ns: stream.start_ns,
        end_ns,
        attributes,
        error,
    }
}

fn drain_messages(
    session: &mut SessionState,
    parent_id: &str,
    end_ns: u64,
    name: &str,
    error: bool,
) -> Vec<SpanDraft> {
    let streams = std::mem::take(&mut session.messages);
    streams
        .into_iter()
        .map(|(message_id, stream)| {
            build_message_span(
                &session.trace_id,
                parent_id,
                &session.session_id,
                &message_id,
                &stream,
                end_ns,
                name,
                vec![
                    (
                        "claude.hook.event".to_string(),
                        "MessageDisplay".to_string(),
                    ),
                    ("claude.session_id".to_string(), session.session_id.clone()),
                ],
                error,
            )
        })
        .collect()
}

fn close_active_turn(session: &mut SessionState, end_ns: u64, reason: &str) -> Option<SpanDraft> {
    let turn_id = session.turn_id.take()?;
    let start_ns = session.turn_start_ns.take().unwrap_or(end_ns);
    let mut attributes = vec![
        (
            "claude.hook.event".to_string(),
            "SessionIncomplete".to_string(),
        ),
        ("claude.session_id".to_string(), session.session_id.clone()),
        ("claude.turn.incomplete".to_string(), "true".to_string()),
        ("claude.turn.reason".to_string(), reason.to_string()),
        (
            "gen_ai.conversation.id".to_string(),
            session.session_id.clone(),
        ),
        (
            "gen_ai.operation.name".to_string(),
            "invoke_agent".to_string(),
        ),
        (
            "claude.timing.source".to_string(),
            "hook_receipt".to_string(),
        ),
    ];
    if let Some(model) = &session.model {
        attributes.push(("claude.model".to_string(), model.clone()));
        attributes.push(("gen_ai.request.model".to_string(), model.clone()));
    }

    Some(SpanDraft {
        trace_id: session.trace_id.clone(),
        span_id: turn_id,
        parent_id: Some(session.root_id.clone()),
        name: "agent.turn.incomplete".to_string(),
        start_ns,
        end_ns,
        attributes,
        error: true,
    })
}

fn handle_event(state: &AppState, payload: &Value) -> EventOutput {
    let session_id = string_field(payload, "session_id").unwrap_or_else(|| "unknown".to_string());
    let event = string_field(payload, "hook_event_name").unwrap_or_else(|| "unknown".to_string());
    let now = now_ns();
    let mut spans = Vec::new();
    let mut summary = None;
    let mut turn_usage = None;
    let raw_payload = payload.to_string();
    let event_key = hex_id(&format!("{event}:{raw_payload}"), 16);
    let base_attrs = base_attributes(payload, &raw_payload, &event, &session_id);

    {
        let mut recent = state
            .inner
            .recent_events
            .lock()
            .expect("recent events lock");
        let cutoff = now.saturating_sub(300 * 1_000_000_000);
        recent.retain(|_, timestamp| *timestamp >= cutoff);
        if recent.contains_key(&event_key) {
            tracing::info!(
                session_id = %session_id,
                event = %event,
                "duplicate hook event ignored"
            );
            return EventOutput {
                spans: Vec::new(),
                summary: None,
                turn_usage: None,
            };
        }
        recent.insert(event_key, now);
    }

    if event == "SessionEnd"
        && !state
            .inner
            .sessions
            .lock()
            .expect("sessions lock")
            .contains_key(&session_id)
    {
        tracing::info!(session_id = %session_id, "ignoring SessionEnd without an active session");
        return EventOutput {
            spans: Vec::new(),
            summary: None,
            turn_usage: None,
        };
    }

    let mut sessions = state.inner.sessions.lock().expect("sessions lock");
    let existed = sessions.contains_key(&session_id);
    let session = sessions
        .entry(session_id.clone())
        .or_insert_with(|| SessionState {
            session_id: session_id.clone(),
            trace_id: hex_id(&format!("trace:{session_id}"), 16),
            root_id: unique_span_id("session"),
            turn_id: None,
            turn_start_ns: None,
            pairs: HashMap::new(),
            messages: HashMap::new(),
            turn_prompt: None,
            usage_offset: 0,
            start_ns: now,
            last_seen_ns: now,
            turn_count: 0,
            tool_count: 0,
            subagent_count: 0,
            model: None,
        });

    session.last_seen_ns = now;
    if let Some(model) =
        string_field(payload, "model").or_else(|| string_field(payload, "to_model"))
    {
        session.model = Some(model);
    }

    if !existed {
        spans.push(SpanDraft {
            trace_id: session.trace_id.clone(),
            span_id: session.root_id.clone(),
            parent_id: None,
            name: "session".to_string(),
            start_ns: now,
            end_ns: now + 1_000_000,
            attributes: base_attrs.clone(),
            error: false,
        });
    }

    if event == "SessionStart" {
        if existed {
            spans.push(event_span(
                session,
                &session.root_id,
                "session.resume",
                now,
                now,
                base_attrs.clone(),
                false,
            ));
        }
        return EventOutput {
            spans,
            summary: None,
            turn_usage: None,
        };
    }

    let parent = session
        .turn_id
        .clone()
        .unwrap_or_else(|| session.root_id.clone());
    let attrs = base_attrs;

    match event.as_str() {
        "UserPromptSubmit" => {
            session.turn_count += 1;
            session.turn_prompt = payload
                .get("prompt")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let turn_id = string_field(payload, "prompt_id")
                .map(|prompt_id| hex_id(&format!("turn:{}:{prompt_id}", session.root_id), 8))
                .unwrap_or_else(|| unique_span_id("turn"));
            session.turn_id = Some(turn_id.clone());
            session.turn_start_ns = Some(now);
            spans.push(SpanDraft {
                trace_id: session.trace_id.clone(),
                span_id: unique_span_id("prompt"),
                parent_id: Some(turn_id),
                name: "prompt".to_string(),
                start_ns: now,
                end_ns: now,
                attributes: attrs,
                error: false,
            });
        }

        "Stop" | "StopFailure" => {
            let mut pending =
                drain_messages(session, &parent, now, "message.display.incomplete", true);
            spans.append(&mut pending);

            let turn_id = session
                .turn_id
                .clone()
                .unwrap_or_else(|| unique_span_id("turn"));
            let start_ns = session.turn_start_ns.unwrap_or(now);
            let turn_prompt = session.turn_prompt.take();
            session.turn_id = None;
            session.turn_start_ns = None;
            let mut turn_attrs = attrs;
            if let Some(prompt) = turn_prompt {
                set_attr(
                    &mut turn_attrs,
                    "langfuse.observation.input",
                    json!({ "prompt": prompt }).to_string(),
                );
            }
            let turn_span = SpanDraft {
                trace_id: session.trace_id.clone(),
                span_id: turn_id,
                parent_id: Some(session.root_id.clone()),
                name: "agent.turn".to_string(),
                start_ns,
                end_ns: now,
                attributes: turn_attrs,
                error: event == "StopFailure",
            };
            let transcript_path = string_field(payload, "transcript_path");
            let prompt_id = string_field(payload, "prompt_id");
            let start_offset = session.usage_offset;
            if let Some(path) = &transcript_path {
                if let Ok(metadata) = std::fs::metadata(path) {
                    session.usage_offset = session.usage_offset.max(metadata.len());
                }
            }
            if transcript_path.is_some() && prompt_id.is_some() {
                turn_usage = Some(TurnUsageJob {
                    span: turn_span,
                    session_id: session.session_id.clone(),
                    transcript_path,
                    prompt_id,
                    start_offset,
                });
            } else {
                spans.push(turn_span);
            }
        }

        "PreToolUse" => {
            session.tool_count += 1;
            let key = string_field(payload, "tool_use_id")
                .or_else(|| string_field(payload, "tool_name"))
                .unwrap_or_else(|| "tool".to_string());
            let tool_name =
                string_field(payload, "tool_name").unwrap_or_else(|| "tool".to_string());
            let pair_key = format!("tool:{key}");
            let span_id = hex_id(&format!("tool:{}:{key}:start", session.root_id), 8);
            session.pairs.insert(
                pair_key,
                StartState {
                    span_id: span_id.clone(),
                    start_ns: now,
                },
            );
            spans.push(SpanDraft {
                trace_id: session.trace_id.clone(),
                span_id,
                parent_id: Some(parent.clone()),
                name: format!("tool.{tool_name}"),
                start_ns: now,
                end_ns: now,
                attributes: attrs,
                error: false,
            });
        }

        "PostToolUse" | "PostToolUseFailure" => {
            let key = string_field(payload, "tool_use_id")
                .or_else(|| string_field(payload, "tool_name"))
                .unwrap_or_else(|| "tool".to_string());
            let tool_name =
                string_field(payload, "tool_name").unwrap_or_else(|| "tool".to_string());
            let pair_key = format!("tool:{key}");
            let start = session.pairs.remove(&pair_key);
            let start_ns = start.as_ref().map(|item| item.start_ns).unwrap_or(now);
            let mut result_attrs = attrs;
            if let Some(start) = &start {
                result_attrs.push(("claude.paired_span_id".to_string(), start.span_id.clone()));
            }
            spans.push(SpanDraft {
                trace_id: session.trace_id.clone(),
                span_id: hex_id(&format!("tool:{}:{key}:result", session.root_id), 8),
                parent_id: Some(parent.clone()),
                name: format!("tool.{tool_name}.result"),
                start_ns,
                end_ns: now,
                attributes: result_attrs,
                error: event == "PostToolUseFailure",
            });
        }

        "SubagentStart" => {
            session.subagent_count += 1;
            let key = string_field(payload, "agent_id")
                .or_else(|| string_field(payload, "agent_type"))
                .unwrap_or_else(|| "subagent".to_string());
            let agent_type =
                string_field(payload, "agent_type").unwrap_or_else(|| "subagent".to_string());
            let pair_key = format!("subagent:{key}");
            let span_id = hex_id(&format!("subagent:{}:{key}:start", session.root_id), 8);
            session.pairs.insert(
                pair_key,
                StartState {
                    span_id: span_id.clone(),
                    start_ns: now,
                },
            );
            spans.push(SpanDraft {
                trace_id: session.trace_id.clone(),
                span_id,
                parent_id: Some(parent.clone()),
                name: format!("subagent.{agent_type}"),
                start_ns: now,
                end_ns: now,
                attributes: attrs,
                error: false,
            });
        }

        "SubagentStop" => {
            let key = string_field(payload, "agent_id")
                .or_else(|| string_field(payload, "agent_type"))
                .unwrap_or_else(|| "subagent".to_string());
            let agent_type =
                string_field(payload, "agent_type").unwrap_or_else(|| "subagent".to_string());
            let pair_key = format!("subagent:{key}");
            let start = session.pairs.remove(&pair_key);
            let start_ns = start.as_ref().map(|item| item.start_ns).unwrap_or(now);
            let mut result_attrs = attrs;
            if let Some(start) = &start {
                result_attrs.push(("claude.paired_span_id".to_string(), start.span_id.clone()));
            }
            spans.push(SpanDraft {
                trace_id: session.trace_id.clone(),
                span_id: hex_id(&format!("subagent:{}:{key}:stop", session.root_id), 8),
                parent_id: Some(parent.clone()),
                name: format!("subagent.{agent_type}.stop"),
                start_ns,
                end_ns: now,
                attributes: result_attrs,
                error: false,
            });
        }

        "PreCompact" | "PostCompact" => {
            if event == "PreCompact" {
                let span_id = hex_id(&format!("compact:{}:pre", session.root_id), 8);
                session.pairs.insert(
                    "compact".to_string(),
                    StartState {
                        span_id: span_id.clone(),
                        start_ns: now,
                    },
                );
                spans.push(SpanDraft {
                    trace_id: session.trace_id.clone(),
                    span_id,
                    parent_id: Some(parent.clone()),
                    name: "context.compact".to_string(),
                    start_ns: now,
                    end_ns: now,
                    attributes: attrs,
                    error: false,
                });
            } else {
                let start = session.pairs.remove("compact");
                let start_ns = start.as_ref().map(|item| item.start_ns).unwrap_or(now);
                let mut result_attrs = attrs;
                if let Some(start) = &start {
                    result_attrs.push(("claude.paired_span_id".to_string(), start.span_id.clone()));
                }
                spans.push(SpanDraft {
                    trace_id: session.trace_id.clone(),
                    span_id: hex_id(&format!("compact:{}:post", session.root_id), 8),
                    parent_id: Some(parent.clone()),
                    name: "context.compact.result".to_string(),
                    start_ns,
                    end_ns: now,
                    attributes: result_attrs,
                    error: false,
                });
            }
        }

        "PreModelSwitch" | "PostModelSwitch" => {
            if event == "PreModelSwitch" {
                let span_id = hex_id(&format!("model:{}:pre", session.root_id), 8);
                session.pairs.insert(
                    "model".to_string(),
                    StartState {
                        span_id: span_id.clone(),
                        start_ns: now,
                    },
                );
                spans.push(SpanDraft {
                    trace_id: session.trace_id.clone(),
                    span_id,
                    parent_id: Some(parent.clone()),
                    name: "model.switch".to_string(),
                    start_ns: now,
                    end_ns: now,
                    attributes: attrs,
                    error: false,
                });
            } else {
                let start = session.pairs.remove("model");
                let start_ns = start.as_ref().map(|item| item.start_ns).unwrap_or(now);
                let mut result_attrs = attrs;
                if let Some(start) = &start {
                    result_attrs.push(("claude.paired_span_id".to_string(), start.span_id.clone()));
                }
                spans.push(SpanDraft {
                    trace_id: session.trace_id.clone(),
                    span_id: hex_id(&format!("model:{}:post", session.root_id), 8),
                    parent_id: Some(parent.clone()),
                    name: "model.switch.result".to_string(),
                    start_ns,
                    end_ns: now,
                    attributes: result_attrs,
                    error: false,
                });
            }
        }

        "TaskCreated" | "TaskCompleted" => {
            let key = string_field(payload, "task_id")
                .or_else(|| string_field(payload, "task_name"))
                .unwrap_or_else(|| "task".to_string());
            let pair_key = format!("task:{key}");
            if event == "TaskCreated" {
                let span_id = hex_id(&format!("task:{}:{key}:created", session.root_id), 8);
                session.pairs.insert(
                    pair_key,
                    StartState {
                        span_id: span_id.clone(),
                        start_ns: now,
                    },
                );
                spans.push(SpanDraft {
                    trace_id: session.trace_id.clone(),
                    span_id,
                    parent_id: Some(parent.clone()),
                    name: "task".to_string(),
                    start_ns: now,
                    end_ns: now,
                    attributes: attrs,
                    error: false,
                });
            } else {
                let start = session.pairs.remove(&pair_key);
                let start_ns = start.as_ref().map(|item| item.start_ns).unwrap_or(now);
                let mut result_attrs = attrs;
                if let Some(start) = &start {
                    result_attrs.push(("claude.paired_span_id".to_string(), start.span_id.clone()));
                }
                spans.push(SpanDraft {
                    trace_id: session.trace_id.clone(),
                    span_id: hex_id(&format!("task:{}:{key}:completed", session.root_id), 8),
                    parent_id: Some(parent.clone()),
                    name: "task.result".to_string(),
                    start_ns,
                    end_ns: now,
                    attributes: result_attrs,
                    error: false,
                });
            }
        }

        "Elicitation" | "ElicitationResult" => {
            let key = string_field(payload, "elicitation_id")
                .or_else(|| string_field(payload, "server_name"))
                .unwrap_or_else(|| "elicitation".to_string());
            let pair_key = format!("elicitation:{key}");
            if event == "Elicitation" {
                let span_id = hex_id(&format!("elicitation:{}:{key}:start", session.root_id), 8);
                session.pairs.insert(
                    pair_key,
                    StartState {
                        span_id: span_id.clone(),
                        start_ns: now,
                    },
                );
                spans.push(SpanDraft {
                    trace_id: session.trace_id.clone(),
                    span_id,
                    parent_id: Some(parent.clone()),
                    name: "mcp.elicitation".to_string(),
                    start_ns: now,
                    end_ns: now,
                    attributes: attrs,
                    error: false,
                });
            } else {
                let start = session.pairs.remove(&pair_key);
                let start_ns = start.as_ref().map(|item| item.start_ns).unwrap_or(now);
                let mut result_attrs = attrs;
                if let Some(start) = &start {
                    result_attrs.push(("claude.paired_span_id".to_string(), start.span_id.clone()));
                }
                spans.push(SpanDraft {
                    trace_id: session.trace_id.clone(),
                    span_id: hex_id(&format!("elicitation:{}:{key}:result", session.root_id), 8),
                    parent_id: Some(parent.clone()),
                    name: "mcp.elicitation.result".to_string(),
                    start_ns,
                    end_ns: now,
                    attributes: result_attrs,
                    error: false,
                });
            }
        }

        "MessageDisplay" => {
            let message_id =
                string_field(payload, "message_id").unwrap_or_else(|| unique_span_id("message"));
            let index = u64_field(payload, "index").unwrap_or(0);
            let final_chunk = bool_field(payload, "final").unwrap_or(false);
            let delta = payload
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();

            {
                let stream = session
                    .messages
                    .entry(message_id.clone())
                    .or_insert_with(|| MessageStream {
                        start_ns: now,
                        chunks: Vec::new(),
                    });
                if !stream.chunks.iter().any(|chunk| chunk.index == index) {
                    stream.chunks.push(MessageChunk {
                        index,
                        time_ns: now,
                        final_chunk,
                        delta,
                    });
                }
            }

            if final_chunk {
                if let Some(stream) = session.messages.remove(&message_id) {
                    let mut message_attrs = attrs;
                    message_attrs.push(("claude.message.id".to_string(), message_id.clone()));
                    spans.push(build_message_span(
                        &session.trace_id,
                        &parent,
                        &session.session_id,
                        &message_id,
                        &stream,
                        now,
                        "message.display",
                        message_attrs,
                        false,
                    ));
                }
            }
        }

        "SessionEnd" => {
            let root_id = session.root_id.clone();
            let turn_parent = session.turn_id.clone().unwrap_or_else(|| root_id.clone());
            let mut pending = drain_messages(
                session,
                &turn_parent,
                now,
                "message.display.incomplete",
                true,
            );
            spans.append(&mut pending);
            if let Some(turn) = close_active_turn(session, now, "session_end_without_stop") {
                spans.push(turn);
            }

            let span = event_span(
                session,
                &session.root_id,
                "session.end",
                now,
                now,
                attrs,
                false,
            );
            spans.push(span);
            summary = Some(SummaryJob {
                trace_id: session.trace_id.clone(),
                root_id: session.root_id.clone(),
                session_id: session_id.clone(),
                start_ns: session.start_ns,
                end_ns: now,
                turn_count: session.turn_count,
                tool_count: session.tool_count,
                subagent_count: session.subagent_count,
                model: session.model.clone(),
                transcript_path: string_field(payload, "transcript_path"),
            });
            sessions.remove(&session_id);
        }

        _ => {
            let name = match event.as_str() {
                "PermissionRequest" => "permission.request".to_string(),
                "PermissionDenied" => "permission.denied".to_string(),
                "Notification" => "notification".to_string(),
                "MessageDisplay" => "message.display".to_string(),
                "PostToolBatch" => "tool.batch".to_string(),
                "UserPromptExpansion" => "prompt.expansion".to_string(),
                "TeammateIdle" => "teammate.idle".to_string(),
                "Setup" => "setup".to_string(),
                "InstructionsLoaded" => "instructions.loaded".to_string(),
                "ConfigChange" => "config.change".to_string(),
                "CwdChanged" => "cwd.changed".to_string(),
                "DirectoryAdded" => "directory.added".to_string(),
                "FileChanged" => "file.changed".to_string(),
                "WorktreeCreate" => "worktree.create".to_string(),
                "WorktreeRemove" => "worktree.remove".to_string(),
                other => format!("event.{other}"),
            };
            spans.push(event_span(session, &parent, name, now, now, attrs, false));
        }
    }

    EventOutput {
        spans,
        summary,
        turn_usage,
    }
}

fn is_integer_attribute(key: &str) -> bool {
    key.starts_with("gen_ai.usage.")
        || key.ends_with("_tokens")
        || key.ends_with("_count")
        || key.ends_with("_ms")
        || key.ends_with("_length")
        || key.ends_with("_bytes")
        || key.ends_with("_index")
}

fn is_bool_attribute(key: &str) -> bool {
    matches!(
        key,
        "claude.message.final"
            | "claude.stop_hook_active"
            | "claude.turn.incomplete"
            | "claude.usage.available"
    )
}

fn otel_attribute(key: &str, value: &str) -> Value {
    let typed_value = if is_bool_attribute(key) {
        json!({ "boolValue": value == "true" })
    } else if is_integer_attribute(key) {
        match value.parse::<i64>() {
            Ok(number) => json!({ "intValue": number.to_string() }),
            Err(_) => json!({ "stringValue": value }),
        }
    } else {
        json!({ "stringValue": value })
    };

    json!({
        "key": key,
        "value": typed_value
    })
}

fn dedupe_attributes(attributes: &[(String, String)]) -> Vec<(String, String)> {
    let mut deduped: Vec<(String, String)> = Vec::with_capacity(attributes.len());
    for (key, value) in attributes {
        if key == "claude.message.chunks_json" {
            continue;
        }
        if let Some(existing) = deduped.iter_mut().find(|(known, _)| known == key) {
            existing.1 = value.clone();
        } else {
            deduped.push((key.clone(), value.clone()));
        }
    }
    deduped
}

fn span_to_otel(span: &SpanDraft) -> Value {
    let attributes: Vec<Value> = dedupe_attributes(&span.attributes)
        .iter()
        .map(|(key, value)| otel_attribute(key, value))
        .collect();

    let mut otel_span = json!({
        "traceId": span.trace_id,
        "spanId": span.span_id,
        "name": span.name,
        "kind": 1,
        "startTimeUnixNano": span.start_ns.to_string(),
        "endTimeUnixNano": span.end_ns.max(span.start_ns).to_string(),
        "attributes": attributes,
    });

    if let Some(parent_id) = &span.parent_id {
        otel_span["parentSpanId"] = json!(parent_id);
    }
    if span.error {
        otel_span["status"] = json!({
            "code": 2,
            "message": "hook reported a failure"
        });
    }
    if let Some((_, paired_span_id)) = span
        .attributes
        .iter()
        .find(|(key, _)| key == "claude.paired_span_id")
    {
        otel_span["links"] = json!([{
            "traceId": span.trace_id,
            "spanId": paired_span_id,
        }]);
    }
    if let Some((_, chunks_json)) = span
        .attributes
        .iter()
        .find(|(key, _)| key == "claude.message.chunks_json")
    {
        if let Ok(chunks) = serde_json::from_str::<Vec<Value>>(chunks_json) {
            let events: Vec<Value> = chunks
                .iter()
                .map(|chunk| {
                    let index = chunk.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let time_ns = chunk
                        .get("time_ns")
                        .and_then(Value::as_u64)
                        .unwrap_or(span.start_ns);
                    let final_chunk = chunk
                        .get("final")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let delta = chunk
                        .get("delta")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    json!({
                        "name": "message.delta",
                        "timeUnixNano": time_ns.to_string(),
                        "attributes": [
                            otel_attribute("claude.message.index", &index.to_string()),
                            otel_attribute("claude.message.final", &final_chunk.to_string()),
                            otel_attribute("claude.message.delta_length", &delta.chars().count().to_string()),
                            otel_attribute("claude.message.delta", delta)
                        ]
                    })
                })
                .collect();
            otel_span["events"] = json!(events);
        }
    }

    otel_span
}

fn otlp_body(spans: &[SpanDraft]) -> Value {
    let otel_spans: Vec<Value> = spans.iter().map(span_to_otel).collect();
    json!({
        "resourceSpans": [{
            "resource": {
                "attributes": [
                    {"key": "service.name", "value": {"stringValue": "claude-code-hooks"}},
                    {"key": "service.namespace", "value": {"stringValue": "claude-code"}},
                    {"key": "service.version", "value": {"stringValue": env!("CARGO_PKG_VERSION")}},
                    {"key": "telemetry.sdk.name", "value": {"stringValue": "claude-hooks-collector"}},
                    {"key": "telemetry.sdk.language", "value": {"stringValue": "rust"}},
                    {"key": "telemetry.source", "value": {"stringValue": "hooks"}}
                ]
            },
            "scopeSpans": [{
                "scope": {
                    "name": "claude-hooks-collector",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "spans": otel_spans
            }]
        }]
    })
}

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

async fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    #[cfg(unix)]
    tokio::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).await?;
    Ok(())
}

async fn secure_file(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    Ok(())
}

async fn spool_batch(spool_dir: &Path, body: &Value) -> Result<(), String> {
    ensure_private_dir(spool_dir)
        .await
        .map_err(|error| error.to_string())?;

    let sequence = SPAN_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = format!("batch-{}-{sequence}.json", now_ns());
    let path = spool_dir.join(&name);
    let tmp_path = spool_dir.join(format!(".{name}.tmp"));

    tokio::fs::write(&tmp_path, body.to_string())
        .await
        .map_err(|error| error.to_string())?;
    secure_file(&tmp_path)
        .await
        .map_err(|error| error.to_string())?;
    if let Err(error) = tokio::fs::rename(&tmp_path, &path).await {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(error.to_string());
    }
    Ok(())
}

async fn flush_batch(
    client: &reqwest::Client,
    endpoint: &str,
    spool_dir: &Path,
    spans: &[SpanDraft],
) {
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

    match spool_batch(spool_dir, &body).await {
        Ok(()) => tracing::error!(
            spans = spans.len(),
            spool = %spool_dir.display(),
            "OTLP batch failed after retries; spooled for replay"
        ),
        Err(error) => tracing::error!(
            spans = spans.len(),
            error = %error,
            "OTLP batch failed and could not be spooled"
        ),
    }
}

async fn exporter_worker(
    mut rx: mpsc::Receiver<SpanDraft>,
    client: reqwest::Client,
    endpoint: String,
    spool_dir: PathBuf,
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
                            flush_batch(&client, &endpoint, &spool_dir, &batch).await;
                            batch.clear();
                        }
                    }
                    None => {
                        if !batch.is_empty() {
                            flush_batch(&client, &endpoint, &spool_dir, &batch).await;
                        }
                        break;
                    }
                }
            }
            _ = ticker.tick() => {
                if !batch.is_empty() {
                    flush_batch(&client, &endpoint, &spool_dir, &batch).await;
                    batch.clear();
                }
            }
        }
    }
}

async fn replay_loop(
    client: reqwest::Client,
    endpoint: String,
    spool_dir: PathBuf,
    replay_seconds: u64,
) {
    if let Err(error) = replay_once(&client, &endpoint, &spool_dir).await {
        tracing::warn!(error = %error, "initial spool replay failed");
    }

    let mut ticker = interval(Duration::from_secs(replay_seconds.max(1)));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;
        if let Err(error) = replay_once(&client, &endpoint, &spool_dir).await {
            tracing::warn!(error = %error, "spool replay failed");
        }
    }
}

async fn replay_once(
    client: &reqwest::Client,
    endpoint: &str,
    spool_dir: &Path,
) -> Result<(), String> {
    let mut entries = match tokio::fs::read_dir(spool_dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };

    let mut paths = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|error| error.to_string())?
    {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) == Some("json") {
            paths.push(path);
        }
    }
    paths.sort();

    for path in paths {
        let body = match tokio::fs::read_to_string(&path).await {
            Ok(body) => body,
            Err(error) => {
                tracing::warn!(path = %path.display(), error = %error, "cannot read spool file");
                continue;
            }
        };

        let response = client
            .post(endpoint)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await;

        match response {
            Ok(response) if response.status().is_success() => {
                if let Err(error) = tokio::fs::remove_file(&path).await {
                    tracing::warn!(path = %path.display(), error = %error, "cannot remove replayed spool file");
                } else {
                    tracing::info!(path = %path.display(), "replayed spool batch");
                }
            }
            Ok(response) if matches!(response.status().as_u16(), 400 | 413 | 415 | 422) => {
                let bad_path = path.with_extension(format!("bad-{}", now_ns()));
                if let Err(error) = tokio::fs::rename(&path, &bad_path).await {
                    tracing::warn!(path = %path.display(), error = %error, "cannot quarantine bad spool file");
                } else {
                    tracing::warn!(
                        path = %path.display(),
                        bad_path = %bad_path.display(),
                        status = %response.status(),
                        "quarantined bad spool file"
                    );
                }
            }
            Ok(response) => {
                tracing::warn!(
                    path = %path.display(),
                    status = %response.status(),
                    "spool replay rejected; will retry"
                );
                break;
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), error = %error, "spool replay failed");
                break;
            }
        }
    }

    Ok(())
}

fn add_usage(map: &serde_json::Map<String, Value>, usage: &mut UsageStats) {
    if let Some(value) = map.get("input_tokens").and_then(Value::as_u64) {
        usage.input_tokens += value;
    }
    if let Some(value) = map.get("output_tokens").and_then(Value::as_u64) {
        usage.output_tokens += value;
    }
    if let Some(value) = map.get("cache_read_input_tokens").and_then(Value::as_u64) {
        usage.cache_read_input_tokens += value;
    }
    if let Some(value) = map
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64)
    {
        usage.cache_creation_input_tokens += value;
    }
}

fn add_assistant_usage(
    value: &Value,
    usage: &mut UsageStats,
    seen_message_ids: &mut HashSet<String>,
) {
    let Some(message) = value.get("message").and_then(Value::as_object) else {
        return;
    };
    if let Some(message_id) = message.get("id").and_then(Value::as_str) {
        if !seen_message_ids.insert(message_id.to_string()) {
            return;
        }
    }
    if let Some(item) = message.get("usage").and_then(Value::as_object) {
        add_usage(item, usage);
    }
    if let Some(model) = message.get("model").and_then(Value::as_str) {
        if !usage.models.iter().any(|known| known.as_str() == model) {
            usage.models.push(model.to_string());
        }
    }
}

fn walk_usage(value: &Value, usage: &mut UsageStats) {
    match value {
        Value::Object(map) => {
            if let Some(item) = map.get("usage").and_then(Value::as_object) {
                add_usage(item, usage);
            }
            if map.contains_key("input_tokens") || map.contains_key("output_tokens") {
                add_usage(map, usage);
            }
            if let Some(model) = map.get("model").and_then(Value::as_str) {
                if !usage.models.iter().any(|known| known.as_str() == model) {
                    usage.models.push(model.to_string());
                }
            }
            for (key, item) in map {
                if key != "usage" {
                    walk_usage(item, usage);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                walk_usage(item, usage);
            }
        }
        _ => {}
    }
}

fn parse_turn_usage(path: &str, prompt_id: &str, start_offset: u64) -> (UsageStats, u64) {
    let mut usage = UsageStats::default();
    let Ok(file) = File::open(path) else {
        return (usage, start_offset);
    };
    let mut reader = BufReader::new(file);
    if reader.seek(SeekFrom::Start(start_offset)).is_err() {
        return (usage, start_offset);
    }

    let mut active = false;
    let mut saw_prompt = false;
    let mut seen_message_ids = HashSet::new();
    let mut line = String::new();
    let mut end_offset = start_offset;

    loop {
        let before = match reader.stream_position() {
            Ok(position) => position,
            Err(_) => break,
        };
        line.clear();
        let bytes_read = match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(bytes_read) => bytes_read,
            Err(_) => break,
        };
        if bytes_read == 0 {
            break;
        }
        let after = reader
            .stream_position()
            .unwrap_or(before + bytes_read as u64);

        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            end_offset = after;
            continue;
        };
        let line_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        let line_prompt = value
            .get("promptId")
            .or_else(|| value.get("prompt_id"))
            .and_then(Value::as_str);

        if line_type == "user" {
            if let Some(line_prompt) = line_prompt {
                if active && line_prompt != prompt_id {
                    end_offset = before;
                    break;
                }
                if line_prompt == prompt_id {
                    active = true;
                    saw_prompt = true;
                }
            }
            end_offset = after;
            continue;
        }

        if active && line_type == "assistant" {
            add_assistant_usage(&value, &mut usage, &mut seen_message_ids);
        }
        end_offset = after;
    }

    if !saw_prompt {
        (UsageStats::default(), start_offset)
    } else {
        (usage, end_offset)
    }
}

fn parse_usage(path: Option<&str>) -> UsageStats {
    let mut usage = UsageStats::default();
    let mut seen_message_ids = HashSet::new();
    let Some(path) = path else {
        return usage;
    };
    let Ok(file) = File::open(path) else {
        return usage;
    };

    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) == Some("assistant") {
            add_assistant_usage(&value, &mut usage, &mut seen_message_ids);
        } else {
            walk_usage(&value, &mut usage);
        }
    }

    usage
}

fn build_summary_span(job: &SummaryJob, usage: &UsageStats) -> SpanDraft {
    let duration_ms = job.end_ns.saturating_sub(job.start_ns) / 1_000_000;
    let total_tokens = usage.input_tokens + usage.output_tokens;
    let model = job.model.clone().or_else(|| usage.models.first().cloned());

    let mut attributes = vec![
        (
            "claude.hook.event".to_string(),
            "SessionSummary".to_string(),
        ),
        ("claude.session_id".to_string(), job.session_id.clone()),
        (
            "claude.session.duration_ms".to_string(),
            duration_ms.to_string(),
        ),
        ("claude.turn_count".to_string(), job.turn_count.to_string()),
        ("claude.tool_count".to_string(), job.tool_count.to_string()),
        (
            "claude.subagent_count".to_string(),
            job.subagent_count.to_string(),
        ),
        ("gen_ai.operation.name".to_string(), "chat".to_string()),
        ("gen_ai.conversation.id".to_string(), job.session_id.clone()),
        (
            "gen_ai.usage.input_tokens".to_string(),
            usage.input_tokens.to_string(),
        ),
        (
            "gen_ai.usage.output_tokens".to_string(),
            usage.output_tokens.to_string(),
        ),
        (
            "claude.usage.cache_read_input_tokens".to_string(),
            usage.cache_read_input_tokens.to_string(),
        ),
        (
            "claude.usage.cache_creation_input_tokens".to_string(),
            usage.cache_creation_input_tokens.to_string(),
        ),
        (
            "claude.usage.total_tokens".to_string(),
            total_tokens.to_string(),
        ),
        (
            "claude.timing.source".to_string(),
            "session_summary".to_string(),
        ),
        (
            "langfuse.observation.type".to_string(),
            "generation".to_string(),
        ),
        ("langfuse.session.id".to_string(), job.session_id.clone()),
        (
            "langfuse.observation.usage_details".to_string(),
            json!({
                "input": usage.input_tokens,
                "output": usage.output_tokens,
                "total": total_tokens,
                "cache_read_input_tokens": usage.cache_read_input_tokens,
                "cache_creation_input_tokens": usage.cache_creation_input_tokens,
            })
            .to_string(),
        ),
    ];

    if let Some(model) = &model {
        attributes.push(("claude.model".to_string(), model.clone()));
        attributes.push(("gen_ai.request.model".to_string(), model.clone()));
        attributes.push(("langfuse.observation.model.name".to_string(), model.clone()));
    }

    SpanDraft {
        trace_id: job.trace_id.clone(),
        span_id: unique_span_id("session.summary"),
        parent_id: Some(job.root_id.clone()),
        name: "session.summary".to_string(),
        start_ns: job.start_ns,
        end_ns: job.end_ns,
        attributes,
        error: false,
    }
}

async fn run_turn_usage(state: Arc<AppState>, job: TurnUsageJob) {
    let path = job.transcript_path.clone();
    let prompt_id = job.prompt_id.clone();
    let mut usage = UsageStats::default();
    let mut new_offset = job.start_offset;

    // Stop can fire before Claude has flushed assistant usage into the transcript.
    // Only scan the new transcript range for this session, not the whole file again.
    for attempt in 0..6 {
        let path = path.clone();
        let prompt_id = prompt_id.clone();
        let start_offset = job.start_offset;
        let (parsed, offset) =
            tokio::task::spawn_blocking(move || match (path.as_deref(), prompt_id.as_deref()) {
                (Some(path), Some(prompt_id)) => parse_turn_usage(path, prompt_id, start_offset),
                _ => (UsageStats::default(), start_offset),
            })
            .await
            .unwrap_or_default();

        usage = parsed;
        new_offset = offset;
        if usage.input_tokens > 0 || usage.output_tokens > 0 {
            break;
        }
        if attempt < 5 {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    {
        let mut sessions = state.inner.sessions.lock().expect("sessions lock");
        if let Some(session) = sessions.get_mut(&job.session_id) {
            session.usage_offset = session.usage_offset.max(new_offset);
        }
    }

    let mut span = job.span;
    let total_tokens = usage.input_tokens + usage.output_tokens;
    span.attributes.push((
        "gen_ai.usage.input_tokens".to_string(),
        usage.input_tokens.to_string(),
    ));
    span.attributes.push((
        "gen_ai.usage.output_tokens".to_string(),
        usage.output_tokens.to_string(),
    ));
    span.attributes.push((
        "claude.usage.cache_read_input_tokens".to_string(),
        usage.cache_read_input_tokens.to_string(),
    ));
    span.attributes.push((
        "claude.usage.cache_creation_input_tokens".to_string(),
        usage.cache_creation_input_tokens.to_string(),
    ));
    span.attributes.push((
        "claude.usage.total_tokens".to_string(),
        total_tokens.to_string(),
    ));
    span.attributes
        .push(("claude.usage.scope".to_string(), "turn".to_string()));
    span.attributes
        .push(("claude.usage.source".to_string(), "transcript".to_string()));
    if let Some(prompt_id) = &job.prompt_id {
        span.attributes
            .push(("claude.usage.prompt_id".to_string(), prompt_id.clone()));
    }
    span.attributes.push((
        "langfuse.observation.usage_details".to_string(),
        json!({
            "input": usage.input_tokens,
            "output": usage.output_tokens,
            "total": total_tokens,
            "cache_read_input_tokens": usage.cache_read_input_tokens,
            "cache_creation_input_tokens": usage.cache_creation_input_tokens,
        })
        .to_string(),
    ));
    if let Some(model) = usage.models.first() {
        span.attributes
            .push(("gen_ai.request.model".to_string(), model.clone()));
        span.attributes
            .push(("langfuse.observation.model.name".to_string(), model.clone()));
    }

    if let Err(error) = state.inner.tx.send(span).await {
        tracing::warn!(error = %error, "failed to enqueue turn usage span");
    }
}

async fn run_summary(state: Arc<AppState>, job: SummaryJob) {
    let path = job.transcript_path.clone();
    let usage = tokio::task::spawn_blocking(move || parse_usage(path.as_deref()))
        .await
        .unwrap_or_default();
    let span = build_summary_span(&job, &usage);

    if let Err(error) = state.inner.tx.send(span).await {
        tracing::warn!(error = %error, "failed to enqueue session summary");
    }
}

fn expire_sessions(state: &AppState, ttl_ns: u64) {
    let now = now_ns();
    let mut sessions = state.inner.sessions.lock().expect("sessions lock");
    let expired: Vec<String> = sessions
        .iter()
        .filter(|(_, session)| now.saturating_sub(session.last_seen_ns) > ttl_ns)
        .map(|(session_id, _)| session_id.clone())
        .collect();

    for session_id in expired {
        if let Some(mut session) = sessions.remove(&session_id) {
            let root_id = session.root_id.clone();
            let turn_parent = session.turn_id.clone().unwrap_or_else(|| root_id.clone());
            for span in drain_messages(
                &mut session,
                &turn_parent,
                now,
                "message.display.incomplete",
                true,
            ) {
                if let Err(error) = state.inner.tx.try_send(span) {
                    tracing::warn!(error = %error, "failed to enqueue incomplete message span");
                }
            }
            if let Some(turn) = close_active_turn(&mut session, now, "session_timeout") {
                if let Err(error) = state.inner.tx.try_send(turn) {
                    tracing::warn!(error = %error, "failed to enqueue incomplete turn span");
                }
            }

            let duration_ms = now.saturating_sub(session.start_ns) / 1_000_000;
            let mut attributes = vec![
                (
                    "claude.hook.event".to_string(),
                    "SessionTimeout".to_string(),
                ),
                ("claude.session_id".to_string(), session_id.clone()),
                (
                    "claude.session.duration_ms".to_string(),
                    duration_ms.to_string(),
                ),
                (
                    "claude.turn_count".to_string(),
                    session.turn_count.to_string(),
                ),
                (
                    "claude.tool_count".to_string(),
                    session.tool_count.to_string(),
                ),
                (
                    "claude.subagent_count".to_string(),
                    session.subagent_count.to_string(),
                ),
                (
                    "gen_ai.operation.name".to_string(),
                    "invoke_agent".to_string(),
                ),
                ("gen_ai.conversation.id".to_string(), session_id.clone()),
            ];
            if let Some(model) = &session.model {
                attributes.push(("claude.model".to_string(), model.clone()));
                attributes.push(("gen_ai.request.model".to_string(), model.clone()));
            }

            let span = SpanDraft {
                trace_id: session.trace_id.clone(),
                span_id: unique_span_id("session.timeout"),
                parent_id: Some(session.root_id.clone()),
                name: "session.timeout".to_string(),
                start_ns: session.start_ns,
                end_ns: now,
                attributes,
                error: true,
            };
            if let Err(error) = state.inner.tx.try_send(span) {
                tracing::warn!(error = %error, "failed to enqueue session timeout span");
            }
            tracing::warn!(session_id = %session_id, "expired stale session");
        }
    }
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

fn parse_otlp_headers(value: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for part in value.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let Some((name, value)) = part.split_once('=') else {
            tracing::warn!("ignoring malformed OTLP_HEADERS entry without '='");
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        match (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            (Ok(name), Ok(value)) => {
                headers.insert(name, value);
            }
            _ => tracing::warn!(header = %name, "ignoring invalid OTLP_HEADERS entry"),
        }
    }
    headers
}

fn normalize_otlp_endpoint(value: &str) -> String {
    let trimmed = value.trim_end_matches('/');
    if trimmed.ends_with("/v1/traces") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1/traces")
    }
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let listen_addr = env::var("LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:4321".to_string());
    let otlp_endpoint = normalize_otlp_endpoint(
        &env::var("OTLP_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:4318".to_string()),
    );
    let spool_dir = PathBuf::from(env::var("SPOOL_DIR").unwrap_or_else(|_| "spool".to_string()));
    let otlp_headers = parse_otlp_headers(&env::var("OTLP_HEADERS").unwrap_or_default());
    let otlp_headers_configured = !otlp_headers.is_empty();
    let queue_capacity = env::var("QUEUE_CAPACITY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(4096)
        .max(1);
    let ttl_seconds = env::var("SESSION_TTL_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(3600);
    let ttl_sweep_seconds = env::var("TTL_SWEEP_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(60);
    let spool_replay_seconds = env::var("SPOOL_REPLAY_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(30);

    ensure_private_dir(&spool_dir).await?;

    let (tx, rx) = mpsc::channel(queue_capacity);
    let client = reqwest::Client::builder()
        .default_headers(otlp_headers)
        .build()?;
    let state = Arc::new(AppState {
        inner: Arc::new(Inner {
            sessions: Mutex::new(HashMap::new()),
            tx,
            otlp_endpoint: otlp_endpoint.clone(),
            spool_dir: spool_dir.clone(),
            otlp_headers_configured,
            recent_events: Mutex::new(HashMap::new()),
            dropped_total: AtomicU64::new(0),
        }),
    });

    let exporter = tokio::spawn(exporter_worker(
        rx,
        client.clone(),
        otlp_endpoint.clone(),
        spool_dir.clone(),
    ));
    let replay = tokio::spawn(replay_loop(
        client,
        otlp_endpoint.clone(),
        spool_dir.clone(),
        spool_replay_seconds,
    ));
    let ttl = if ttl_seconds > 0 {
        Some(spawn_ttl(
            state.clone(),
            Duration::from_secs(ttl_seconds),
            ttl_sweep_seconds,
        ))
    } else {
        None
    };

    let app = Router::new()
        .route("/hooks", post(hooks))
        .route("/health", get(health))
        .with_state(state.clone())
        .layer(DefaultBodyLimit::max(32 * 1024 * 1024));

    let listener = tokio::net::TcpListener::bind(&listen_addr).await?;
    tracing::info!(
        %listen_addr,
        %otlp_endpoint,
        spool = %spool_dir.display(),
        queue_capacity,
        ttl_seconds,
        "claude hooks collector listening"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    if let Some(handle) = ttl {
        handle.abort();
    }
    replay.abort();

    // Closing the last sender lets the exporter drain its queue before exiting.
    drop(state);
    match tokio::time::timeout(Duration::from_secs(10), exporter).await {
        Ok(Ok(())) => tracing::info!("exporter drained"),
        Ok(Err(error)) => tracing::warn!(error = %error, "exporter task failed"),
        Err(_) => tracing::warn!("exporter shutdown timed out"),
    }

    Ok(())
}
