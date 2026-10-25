#!/usr/bin/env python3
"""Send a full Claude Code hook event sequence and verify the resulting trace."""

import hashlib
import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime, timedelta, timezone

COLLECTOR = os.environ.get("COLLECTOR_URL", "http://127.0.0.1:4321/hooks")
JAEGER = os.environ.get("JAEGER_API", "http://127.0.0.1:16686/api/v3/traces")
SERVICE = "claude-code-hooks"


def post(event):
    data = json.dumps(event).encode()
    req = urllib.request.Request(
        COLLECTOR,
        data=data,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            if resp.status not in (200, 204):
                raise RuntimeError(f"collector returned {resp.status}")
            resp.read()
    except urllib.error.HTTPError as exc:
        raise RuntimeError(f"collector returned {exc.code}: {exc.read()[:500]!r}") from exc


def main():
    run = str(int(time.time()))
    session_id = f"sim-session-{run}"
    prompt_id = f"sim-prompt-{run}"
    transcript_path = f"/tmp/{session_id}.jsonl"
    with open(transcript_path, "w", encoding="utf-8") as handle:
        handle.write(json.dumps({
            "type": "user",
            "promptId": prompt_id,
            "message": {"role": "user", "content": "simulated prompt"},
        }) + "\n")
        handle.write(json.dumps({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "model": "claude-demo",
                "usage": {"input_tokens": 10, "output_tokens": 20},
            }
        }) + "\n")
    base = {
        "session_id": session_id,
        "transcript_path": transcript_path,
        "cwd": "/Users/simon/demo",
        "permission_mode": "default",
        "prompt_id": prompt_id,
    }

    def send(name, **fields):
        payload = dict(base)
        payload["hook_event_name"] = name
        payload.update(fields)
        post(payload)

    send("SessionStart", source="startup", model="claude-demo")
    send("Setup", source="init")
    send(
        "UserPromptSubmit",
        prompt="FULL_PROMPT_CONTENT: inspect the repository",
    )
    send("UserPromptExpansion", command_name="/review", expanded_prompt="please review")
    send(
        "PreToolUse",
        tool_name="Read",
        tool_use_id="tool-read-1",
        tool_input={"file_path": "/Users/simon/demo/README.md", "limit": 20, "marker": "FULL_TOOL_INPUT"},
    )
    send(
        "PermissionRequest",
        tool_name="Read",
        tool_use_id="tool-read-1",
        tool_input={"file_path": "/Users/simon/demo/README.md"},
    )
    send(
        "PermissionDenied",
        tool_name="Write",
        tool_use_id="tool-write-1",
        reason="write not allowed",
    )
    send(
        "PostToolUse",
        tool_name="Read",
        tool_use_id="tool-read-1",
        tool_input={"file_path": "/Users/simon/demo/README.md"},
        tool_response={"content": "FULL_TOOL_OUTPUT: hello from README"},
    )
    send(
        "PreToolUse",
        tool_name="Bash",
        tool_use_id="tool-bash-1",
        tool_input={"command": "exit 1"},
    )
    send(
        "PostToolUseFailure",
        tool_name="Bash",
        tool_use_id="tool-bash-1",
        tool_input={"command": "exit 1"},
        error="command failed with exit code 1",
    )
    send("PostToolBatch", tool_use_ids=["tool-read-1", "tool-bash-1"])
    send("Notification", notification_type="idle_prompt", message="needs attention")
    send("MessageDisplay", message_id="msg-stream-1", index=0, final=False, delta="FULL_STREAM_A")
    send("MessageDisplay", message_id="msg-stream-1", index=1, final=False, delta="FULL_STREAM_B")
    send("MessageDisplay", message_id="msg-stream-1", index=2, final=True, delta="FULL_STREAM_C")
    send("SubagentStart", agent_id="agent-1", agent_type="Explore")
    send("SubagentStop", agent_id="agent-1", agent_type="Explore", last_assistant_message="done")
    send("TaskCreated", task_id="task-1", task_name="inspect repo")
    send("TaskCompleted", task_id="task-1", task_name="inspect repo")
    send("PreCompact", trigger="auto")
    send("PostCompact", trigger="auto")
    send("PreModelSwitch", to_model="claude-opus-5")
    send("PostModelSwitch", to_model="claude-opus-5")
    send("Elicitation", server_name="demo-mcp", elicitation_id="elic-1")
    send("ElicitationResult", server_name="demo-mcp", elicitation_id="elic-1")
    send("TeammateIdle", agent_id="agent-1")
    send("InstructionsLoaded", file_path="CLAUDE.md", reason="session_start")
    send("ConfigChange", source="project_settings")
    send("CwdChanged", cwd="/Users/simon/demo/subdir")
    send("DirectoryAdded", directory="/Users/simon/extra")
    send("FileChanged", file_path=".envrc")
    send("WorktreeCreate", worktree_path="/tmp/worktree-1")
    send("WorktreeRemove", worktree_path="/tmp/worktree-1")
    send("Stop", prompt_id=prompt_id, last_assistant_message="all done")
    send("StopFailure", prompt_id=prompt_id, reason="rate_limit")
    send("SessionEnd", reason="other")

    expected_trace = hashlib.sha256(f"trace:{session_id}".encode()).hexdigest()[:32]
    print(f"session: {session_id}")
    print(f"trace:   {expected_trace}")

    now = datetime.now(timezone.utc)
    params = {
        "query.service_name": SERVICE,
        "query.start_time_min": (now - timedelta(minutes=10)).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "query.start_time_max": (now + timedelta(minutes=1)).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "query.limit": "20",
    }
    url = JAEGER + "?" + urllib.parse.urlencode(params)

    spans = []
    for attempt in range(1, 11):
        try:
            with urllib.request.urlopen(url, timeout=10) as resp:
                data = json.load(resp)
        except urllib.error.HTTPError as exc:
            if exc.code == 404:
                print(f"waiting for trace index... attempt {attempt}")
                time.sleep(2)
                continue
            raise
        spans = []
        for resource in (data.get("result") or {}).get("resourceSpans") or []:
            for scope in resource.get("scopeSpans") or []:
                for span in scope.get("spans") or []:
                    if span.get("traceId") == expected_trace:
                        spans.append(span)
        if len(spans) >= 35:
            break
        print(f"waiting for spans... {len(spans)}/35 (attempt {attempt})")
        time.sleep(2)

    if not spans:
        print("NO TRACE FOUND")
        return 1

    by_id = {span["spanId"]: span for span in spans}
    children = {}
    for span in spans:
        parent = span.get("parentSpanId")
        if parent not in by_id:
            parent = None
        children.setdefault(parent, []).append(span)

    print(f"spans: {len(spans)}")

    def walk(parent, depth):
        for span in sorted(children.get(parent, []), key=lambda item: int(item["startTimeUnixNano"])):
            attrs = {
                a["key"]: list(a["value"].values())[0]
                for a in span.get("attributes", [])
            }
            event = attrs.get("claude.hook.event", "?")
            payload = attrs.get("claude.hook.payload", "")
            events = span.get("events", [])
            print(
                "  " * depth
                + f"{span['name']:<26} event={event:<22} payload={len(payload)}B events={len(events)}"
            )
            walk(span["spanId"], depth + 1)

    walk(None, 0)

    problems = []
    if len(spans) != 35:
        problems.append(f"expected exactly 35 spans, got {len(spans)}")
    for span in spans:
        keys = [a["key"] for a in span.get("attributes", [])]
        if len(keys) != len(set(keys)):
            problems.append(f"duplicate attributes on {span['name']}: {keys}")
        attrs = {a["key"]: list(a["value"].values())[0] for a in span.get("attributes", [])}
        if span["name"] not in ("session.summary", "session.timeout") and not attrs.get("claude.hook.payload"):
            problems.append(f"missing payload on {span['name']}")
    joined = "\n".join(
        a["value"].get("stringValue", "")
        for span in spans
        for a in span.get("attributes", [])
        if a["key"] == "claude.hook.payload"
    )
    for marker in ("FULL_PROMPT_CONTENT", "FULL_TOOL_INPUT", "FULL_TOOL_OUTPUT"):
        if marker not in joined:
            problems.append(f"payload missing marker {marker}")

    message = next((span for span in spans if span["name"] == "message.display"), None)
    if not message:
        problems.append("missing merged message.display span")
    else:
        message_attrs = {
            a["key"]: list(a["value"].values())[0]
            for a in message.get("attributes", [])
        }
        raw_message_attrs = {a["key"]: a["value"] for a in message.get("attributes", [])}
        if "intValue" not in raw_message_attrs.get("claude.message.chunk_count", {}):
            problems.append("chunk_count is not exported as intValue")
        if "boolValue" not in raw_message_attrs.get("claude.message.final", {}):
            problems.append("message.final is not exported as boolValue")
        if message_attrs.get("claude.message.chunk_count") != "3":
            problems.append(
                f"message chunk_count={message_attrs.get('claude.message.chunk_count')} (expected 3)"
            )
        events = message.get("events", [])
        if len(events) != 3:
            problems.append(f"message events={len(events)} (expected 3)")
        deltas = []
        for event in events:
            event_attrs = {
                a["key"]: list(a["value"].values())[0]
                for a in event.get("attributes", [])
            }
            deltas.append(event_attrs.get("claude.message.delta", ""))
        if "".join(deltas) != "FULL_STREAM_A" + "FULL_STREAM_B" + "FULL_STREAM_C":
            problems.append(f"message deltas mismatch: {deltas}")

    turn = next((span for span in spans if span["name"] == "agent.turn"), None)
    if not turn:
        problems.append("missing agent.turn span")
    else:
        turn_attrs = {
            a["key"]: list(a["value"].values())[0]
            for a in turn.get("attributes", [])
        }
        raw_turn_attrs = {a["key"]: a["value"] for a in turn.get("attributes", [])}
        if "intValue" not in raw_turn_attrs.get("gen_ai.usage.input_tokens", {}):
            problems.append("turn input_tokens is not exported as intValue")
        if turn_attrs.get("gen_ai.usage.input_tokens") != "10":
            problems.append(
                f"turn input_tokens={turn_attrs.get('gen_ai.usage.input_tokens')} (expected 10)"
            )
        if turn_attrs.get("gen_ai.usage.output_tokens") != "20":
            problems.append(
                f"turn output_tokens={turn_attrs.get('gen_ai.usage.output_tokens')} (expected 20)"
            )
        if turn_attrs.get("claude.usage.scope") != "turn":
            problems.append(f"turn usage scope={turn_attrs.get('claude.usage.scope')}")

    summary = [
        span for span in spans if span["name"] == "session.summary"
    ]
    if not summary:
        problems.append("missing session.summary span")
    else:
        attrs = {
            a["key"]: list(a["value"].values())[0]
            for a in summary[0].get("attributes", [])
        }
        for key in ("claude.session.duration_ms", "gen_ai.usage.input_tokens", "gen_ai.usage.output_tokens"):
            if key not in attrs:
                problems.append(f"summary missing {key}")
        if attrs.get("gen_ai.usage.input_tokens") != "10":
            problems.append(f"summary input_tokens={attrs.get('gen_ai.usage.input_tokens')} (expected 10)")
        if attrs.get("gen_ai.usage.output_tokens") != "20":
            problems.append(f"summary output_tokens={attrs.get('gen_ai.usage.output_tokens')} (expected 20)")

    if problems:
        print("FAILED:")
        for problem in problems:
            print(" -", problem)
        return 1

    print(f"view: http://127.0.0.1:16686/trace/{expected_trace}")
    print("OK: all hook events exported with full payload")
    return 0


if __name__ == "__main__":
    sys.exit(main())
