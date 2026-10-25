# Project Review

Review date: 2026-10-25
Project: `/Users/simon/dev/claude-hooks-collector`
Scope: local single-user deployment

## 1. Summary

The project is a local **Claude Code / Qoder Hook → OTLP** collector. It builds
session/turn/tool span trees, preserves full Hook payloads, derives token usage
from Claude transcripts, and exports OTLP/HTTP to one backend at a time.

Runtime shape:

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
Collector PID: 49729
Backend: http://127.0.0.1:4318/v1/traces (Jaeger)
Jaeger: running
Langfuse: stopped
```

Checks:

```text
cargo check                  PASS
cargo build                  PASS
cargo test                   12 passed
python3 simulate.py          PASS, 35 spans
cargo clippy --all-targets   PASS with warnings, no errors
```

Latest full simulation trace:

<http://127.0.0.1:16686/trace/a1042e8ae4b4a4e5710be7adef6990ec>

Duplicate Hook test:

```text
5 events sent twice → 6 spans
session, prompt, tool.Read, agent.turn, session.end, session.summary
```

Langfuse-mode mock exporter test confirmed these attributes are present when
`OTEL_BACKEND=langfuse`:

```text
langfuse.observation.input
langfuse.observation.output
langfuse.observation.type
langfuse.observation.usage_details
langfuse.observation.model.name
langfuse.session.id
```

Jaeger mode produced no `langfuse.*` attributes.

## 3. Current architecture

```text
src/
├── main.rs        boot only
├── lib.rs         module exports
├── config.rs      env, OTLP headers, endpoint normalization, backend detection
├── app.rs         AppState, exporter/TTL tasks, graceful shutdown
├── http.rs        /hooks and /health
├── hooks.rs       Hook dispatch, session/turn/tool state machine, dedupe, span assembly
├── state.rs       SessionState / MessageStream / pair types
├── spans.rs       SpanDraft, Claude/GenAI/Langfuse attributes, OTLP JSON
├── exporter.rs    queue worker, batching, retry
├── usage.rs       incremental transcript token parsing, turn/session jobs
└── util.rs        IDs, time, field helpers
```

## 4. Intentional tradeoffs

### Spool removed

Failed batches are retried 3 times, then logged and dropped.

- No `SPOOL_DIR`
- No replay loop
- No disk buffer
- If Jaeger/Langfuse is down longer than the retry window, that batch is lost

This is deliberate for local single-user use.

### Langfuse mapper is optional

`OTEL_BACKEND=langfuse` or a Langfuse-style endpoint enables:

- `langfuse.observation.type`
- `langfuse.session.id`
- `langfuse.observation.input/output`
- `langfuse.observation.usage_details`
- `langfuse.observation.model.name`

Jaeger mode does not write any `langfuse.*` attributes.

### Duplicate Hook dedupe

Identical `event + complete payload` is processed once within a 5-minute window.

- Prevents HTTP retry / duplicate configuration from creating duplicate spans
- Cache is in-memory and resets on collector restart
- Not a distributed idempotency guarantee

## 5. Jaeger / Langfuse compatibility

Both use the same transport:

```text
OTLP/HTTP JSON → /v1/traces
```

- Jaeger: no headers
- Langfuse: `OTLP_HEADERS` with Basic Auth and ingestion version
- Langfuse backend mapping: enabled by `OTEL_BACKEND=langfuse` or endpoint detection

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
- Qoder has no `prompt_id`; the collector now falls back to `request_set_id` /
  `parent_request_set_id`, so a turn token job is created, but Qoder currently
  returns zeros.

## 7. Qoder compatibility

Qoder 1.1.78 is configured with 25 non-worktree events. A real tool session
produced the expected span tree:

<http://127.0.0.1:16686/trace/b802f639c251be1cef578b4616cf9c02>

Qoder-specific handling:

- `request_set_id` / `parent_request_set_id` fallback for turn ID and token job
- transcript parser matches `requestSetId`
- `claude.client=qoder` attribute
- `SERVICE_NAME=qoder-cli` can isolate a separate Qoder collector instance

Qoder still returns zero token values for `qfmodel`, so token display is zero even
though the parse path is now active.

## 8. Remaining findings

| ID | Severity | Finding | Recommendation |
|---|---|---|---|
| O-01 | Medium | No CI | Add GitHub Actions for `fmt`, `clippy`, `test` |
| O-02 | Medium | `/health` is liveness only | Add backend reachability / last export error if needed |
| O-03 | Medium | Failed batches are dropped after retries | Accept for local use; reintroduce spool only if needed |
| O-04 | Low | Dedupe cache is per-process | Accept for single-user; use external dedupe only for multi-instance |
| O-05 | Low | Qoder token usage is zero | Source limitation; Qoder currently reports zero tokens |
| O-06 | Low | `FileChanged` has no matcher | Add a matcher if that event is needed |
| O-07 | Low | No OTLP/gRPC or Protobuf | Not needed for local OTLP/HTTP |
| O-08 | Low | Jaeger memory storage | Restarting Jaeger loses traces |

## 9. Verification

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

## 10. Conclusion

The project is appropriately sized for a local single-user tool: one collector
process, one backend, in-memory state, and OTLP/HTTP delivery. The code is split
into focused modules, and the remaining complexity is mostly in Hook semantics,
dedupe, and token parsing—core features rather than infrastructure duplication.

Do not add an independent OTel Collector, dual write, Redis, or HA unless the
deployment scope changes.
