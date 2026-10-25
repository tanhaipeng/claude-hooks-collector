use std::collections::HashMap;

use serde_json::{Value, json};

use crate::app::AppState;
use crate::spans::{SpanDraft, base_attributes};
use crate::state::{MessageChunk, MessageStream, SessionState, StartState};
use crate::usage::{SummaryJob, TurnUsageJob};
use crate::util::{bool_field, hex_id, now_ns, string_field, u64_field, unique_span_id};

pub(crate) struct EventOutput {
    pub(crate) spans: Vec<SpanDraft>,
    pub(crate) summary: Option<SummaryJob>,
    pub(crate) turn_usage: Option<TurnUsageJob>,
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

pub(crate) fn handle_event(state: &AppState, payload: &Value) -> EventOutput {
    let session_id = string_field(payload, "session_id").unwrap_or_else(|| "unknown".to_string());
    let event = string_field(payload, "hook_event_name").unwrap_or_else(|| "unknown".to_string());
    let now = now_ns();
    let mut spans = Vec::new();
    let mut summary = None;
    let mut turn_usage = None;
    let raw_payload = payload.to_string();
    let base_attrs = base_attributes(payload, &raw_payload, &event, &session_id);

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
            session.turn_id = None;
            session.turn_start_ns = None;
            let turn_attrs = attrs;
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

pub(crate) fn expire_sessions(state: &AppState, ttl_ns: u64) {
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
