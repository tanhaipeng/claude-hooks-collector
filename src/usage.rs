use std::{
    collections::HashSet,
    fs::File,
    io::{BufRead, BufReader, Seek, SeekFrom},
    sync::Arc,
    time::Duration,
};

use serde_json::{Value, json};

use crate::app::AppState;
use crate::spans::SpanDraft;
use crate::util::unique_span_id;

#[derive(Default)]
pub(crate) struct UsageStats {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cache_read_input_tokens: u64,
    pub(crate) cache_creation_input_tokens: u64,
    pub(crate) models: Vec<String>,
}

pub(crate) struct TurnUsageJob {
    pub(crate) span: SpanDraft,
    pub(crate) session_id: String,
    pub(crate) transcript_path: Option<String>,
    pub(crate) prompt_id: Option<String>,
    pub(crate) start_offset: u64,
}

pub(crate) struct SummaryJob {
    pub(crate) trace_id: String,
    pub(crate) root_id: String,
    pub(crate) session_id: String,
    pub(crate) start_ns: u64,
    pub(crate) end_ns: u64,
    pub(crate) turn_count: u64,
    pub(crate) tool_count: u64,
    pub(crate) subagent_count: u64,
    pub(crate) model: Option<String>,
    pub(crate) transcript_path: Option<String>,
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

pub(crate) async fn run_turn_usage(state: Arc<AppState>, job: TurnUsageJob) {
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

pub(crate) async fn run_summary(state: Arc<AppState>, job: SummaryJob) {
    let path = job.transcript_path.clone();
    let usage = tokio::task::spawn_blocking(move || parse_usage(path.as_deref()))
        .await
        .unwrap_or_default();
    let span = build_summary_span(&job, &usage);

    if let Err(error) = state.inner.tx.send(span).await {
        tracing::warn!(error = %error, "failed to enqueue session summary");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "claude-hooks-test-{name}-{}",
            crate::util::now_ns()
        ))
    }

    #[test]
    fn parses_only_new_turn_range() {
        let path = temp_path("turns.jsonl");
        let content = [
            r#"{"type":"user","promptId":"p1"}"#,
            r#"{"type":"assistant","message":{"id":"m1","usage":{"input_tokens":10,"output_tokens":20}}}"#,
            r#"{"type":"user","promptId":"p2"}"#,
            r#"{"type":"assistant","message":{"id":"m2","usage":{"input_tokens":5,"output_tokens":7}}}"#,
        ]
        .join("\n")
            + "\n";
        fs::write(&path, content).unwrap();

        let (first, offset) = parse_turn_usage(path.to_str().unwrap(), "p1", 0);
        assert_eq!((first.input_tokens, first.output_tokens), (10, 20));
        let (second, _) = parse_turn_usage(path.to_str().unwrap(), "p2", offset);
        assert_eq!((second.input_tokens, second.output_tokens), (5, 7));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn dedupes_duplicate_assistant_message_ids() {
        let path = temp_path("usage-dedupe.jsonl");
        let content = [
            r#"{"type":"assistant","message":{"id":"same","model":"test","usage":{"input_tokens":10,"output_tokens":20}}}"#,
            r#"{"type":"assistant","message":{"id":"same","model":"test","usage":{"input_tokens":10,"output_tokens":20}}}"#,
        ]
        .join("\n")
            + "\n";
        fs::write(&path, content).unwrap();

        let usage = parse_usage(Some(path.to_str().unwrap()));
        assert_eq!((usage.input_tokens, usage.output_tokens), (10, 20));

        let _ = fs::remove_file(path);
    }
}
