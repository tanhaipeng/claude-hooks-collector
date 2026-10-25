use serde_json::{Value, json};

use crate::util::{add_scalar, set_attr, string_field};

pub(crate) struct SpanDraft {
    pub(crate) trace_id: String,
    pub(crate) span_id: String,
    pub(crate) parent_id: Option<String>,
    pub(crate) name: String,
    pub(crate) start_ns: u64,
    pub(crate) end_ns: u64,
    pub(crate) attributes: Vec<(String, String)>,
    pub(crate) error: bool,
}

pub(crate) fn base_attributes(
    payload: &Value,
    raw: &str,
    event: &str,
    session_id: &str,
    langfuse: bool,
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
    }

    if langfuse {
        add_langfuse_attributes(&mut attributes, payload, event, session_id);
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
        if langfuse {
            set_attr(
                &mut attributes,
                "langfuse.observation.model.name",
                model.clone(),
            );
        }
    }

    attributes.push((
        "claude.timing.source".to_string(),
        "hook_receipt".to_string(),
    ));

    attributes
}

fn add_langfuse_attributes(
    attributes: &mut Vec<(String, String)>,
    payload: &Value,
    event: &str,
    session_id: &str,
) {
    let observation_type = match event {
        "Stop" | "StopFailure" => "generation",
        "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => "tool",
        "MessageDisplay" => "event",
        "SubagentStart" | "SubagentStop" => "agent",
        _ => "span",
    };
    set_attr(
        attributes,
        "langfuse.observation.type",
        observation_type.to_string(),
    );
    set_attr(attributes, "langfuse.session.id", session_id.to_string());

    if let Some(prompt) = payload.get("prompt").and_then(Value::as_str) {
        set_attr(
            attributes,
            "langfuse.observation.input",
            json!({ "prompt": prompt }).to_string(),
        );
    }
    if let Some(expanded) = payload.get("expanded_prompt").and_then(Value::as_str) {
        set_attr(
            attributes,
            "langfuse.observation.input",
            json!({ "prompt": expanded }).to_string(),
        );
    }
    if let Some(tool_input) = payload.get("tool_input") {
        set_attr(
            attributes,
            "langfuse.observation.input",
            json!({ "tool_input": tool_input }).to_string(),
        );
    }
    for output_key in ["tool_response", "tool_output"] {
        if let Some(output) = payload.get(output_key) {
            set_attr(
                attributes,
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
            attributes,
            "langfuse.observation.output",
            json!({ "text": last_message }).to_string(),
        );
    }
    if let Some(error) = payload.get("error").and_then(Value::as_str) {
        set_attr(
            attributes,
            "langfuse.observation.output",
            json!({ "error": error }).to_string(),
        );
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

pub(crate) fn span_to_otel(span: &SpanDraft) -> Value {
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

pub(crate) fn otlp_body(spans: &[SpanDraft]) -> Value {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exports_integer_and_bool_values() {
        assert_eq!(
            otel_attribute("gen_ai.usage.input_tokens", "42")["value"],
            json!({"intValue": "42"})
        );
        assert_eq!(
            otel_attribute("claude.message.final", "true")["value"],
            json!({"boolValue": true})
        );
        assert_eq!(
            otel_attribute("claude.cwd", "/tmp")["value"],
            json!({"stringValue": "/tmp"})
        );
    }

    #[test]
    fn dedupes_last_attribute_value() {
        let attrs = vec![
            ("claude.message.id".to_string(), "one".to_string()),
            ("claude.message.id".to_string(), "two".to_string()),
        ];
        let deduped = dedupe_attributes(&attrs);
        assert_eq!(
            deduped,
            vec![("claude.message.id".to_string(), "two".to_string())]
        );
    }
}
