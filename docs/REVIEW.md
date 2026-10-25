# Project Review

Review date: 2026-10-25
Project: `/Users/simon/dev/claude-hooks-collector`
Scope: local single-user deployment

## 1. Summary

The project is a local **Claude Code / Qoder Hook → OTLP** collector. It builds
session/turn/tool span trees, preserves full Hook payloads, derives token usage
from Claude transcripts, and exports OTLP/HTTP to one backend at a time.

The runtime target is intentionally small:

```text
Claude Code / Qoder
        │ HTTP Hook
        ▼
hooks-collector（单进程）
        │ OTLP/HTTP
        ▼
Jaeger 或 Langfuse（二选一）
```

There is no independent OTel Collector, no dual write, no shared state, and no
HA requirement.

## 2. Current verified state

```text
Collector PID: 48070
Backend: http://127.0.0.1:4318/v1/traces (Jaeger)
Jaeger: running
Langfuse: stopped
```

Checks:

```text
cargo check                  PASS
cargo build                  PASS
cargo test                   9 passed
python3 simulate.py          PASS, 35 spans
cargo clippy --all-targets   PASS with warnings, no errors
```

Latest full simulation trace:

<http://127.0.0.1:16686/trace/4fde2f0dd43399d7166918819ceac3c5>

Health no longer exposes a spool directory:

```json
{
  "ok": true,
  "otlp_endpoint": "http://127.0.0.1:4318/v1/traces",
  "otlp_headers_configured": false,
  "sessions": 0,
  "queue_capacity": 4096,
  "dropped_total": 0
}
```

## 3. Current architecture

```text
src/
├── main.rs        boot only
├── lib.rs         module exports
├── config.rs      env, OTLP headers, endpoint normalization
├── app.rs         AppState, exporter/TTL tasks, graceful shutdown
├── http.rs        /hooks and /health
├── hooks.rs       Hook dispatch, session/turn/tool state machine, span assembly
├── state.rs       SessionState / MessageStream / pair types
├── spans.rs       SpanDraft, Claude/GenAI attributes, OTLP JSON
├── exporter.rs    queue worker, batching, retry
├── usage.rs       incremental transcript token parsing, turn/session jobs
└── util.rs        IDs, time, field helpers
```

## 4. Intentional simplifications

Removed in the local-single-user iteration:

| Removed | Effect |
|---|---|
| `spool.rs` and replay | Failed batches are retried 3 times, then logged and dropped |
| `SPOOL_DIR` / `SPOOL_REPLAY_SECS` | No disk buffer configuration |
| 5-minute event idempotency cache | Duplicate Hook delivery can create duplicate spans |
| `langfuse.*` attributes | Langfuse receives generic OTLP spans, not Langfuse-native observations |
| `langfuse.observation.input/output` | Prompt/tool output is only in `claude.hook.payload` and span attributes |
| `langfuse.observation.usage_details` | Token data remains in `gen_ai.usage.*` / `claude.usage.*` |

Kept deliberately:

- Hook fail-open behavior
- Full payload `claude.hook.payload`
- Parent/child span tree and Pre/Post links
- `MessageDisplay` streaming merge
- transcript token parsing
- Jaeger/Langfuse endpoint and header compatibility
- HTTPS support
- TTL session cleanup
- bounded queue + batching + retry
- module-level unit tests

## 5. Jaeger / Langfuse compatibility

Both use the same transport:

```text
OTLP/HTTP JSON → /v1/traces
```

- Jaeger: no headers
- Langfuse: `OTLP_HEADERS` with Basic Auth and ingestion version

Langfuse compatibility is **transport-level only** now. Generic spans arrive,
but Langfuse-specific grouping, observation types, input/output, and usage
details are not emitted. If richer Langfuse UI mapping is needed later, add an
optional backend mapper rather than putting Langfuse attributes back into the
core span builder.

## 6. Token behavior

Token parsing remains enabled.

- `agent.turn` parses the current turn from the transcript
- `session.summary` parses the full session transcript
- duplicate assistant `message.id` values are deduplicated
- `usage_offset` avoids rescanning the whole transcript each turn
- Stop retries tolerate transcript flush delay

Observed limitation:

- Qoder 1.1.78 currently reports `input_tokens=0` / `output_tokens=0` for
  `qfmodel`; the collector cannot invent values the source does not provide.
- Qoder has no `prompt_id`; per-turn token jobs are not created for Qoder.

## 7. Remaining findings

| ID | Severity | Finding | Recommendation |
|---|---|---|---|
| O-01 | Medium | No CI | Add GitHub Actions for `fmt`, `clippy`, `test` |
| O-02 | Medium | `/health` is liveness only | Add backend reachability / last export error if needed |
| O-03 | Medium | Failed batches are dropped after retries | Accept for local use; reintroduce spool only if needed |
| O-04 | Medium | Langfuse has no native observation mapping | Add optional Langfuse backend mapper if UI quality matters |
| O-05 | Low | Duplicate Hook requests create duplicate spans | Reintroduce idempotency only if duplicates become a problem |
| O-06 | Low | Qoder token usage is zero | Source limitation; collect credits later if useful |
| O-07 | Low | `FileChanged` has no matcher | Add a matcher if that event is needed |
| O-08 | Low | No OTLP/gRPC or Protobuf | Not needed for local OTLP/HTTP |
| O-09 | Low | Jaeger memory storage | Restarting Jaeger loses traces |

## 8. Verification

```bash
cd /Users/simon/dev/claude-hooks-collector

cargo fmt --check
cargo check
cargo test
cargo clippy --all-targets
python3 simulate.py

./status.sh
curl http://127.0.0.1:4321/health
```

## 9. Conclusion

The project is appropriately sized for a local single-user tool: one collector
process, one backend, in-memory state, and OTLP/HTTP delivery. The code is split
into focused modules, and the remaining complexity is mostly in Hook semantics
and token parsing—both core features rather than infrastructure duplication.

Do not add an independent OTel Collector, dual write, Redis, or HA unless the
deployment scope changes.
