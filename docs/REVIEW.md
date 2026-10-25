# Project Review

Review date: 2026-10-25
Project: `/Users/simon/dev/claude-hooks-collector`
Review code commit: `a1d1cc0`
Docs commit: `4db4ea5`

## 1. Executive summary

The project is a working local **Claude Code Hook → OTLP bridge**. It receives
hook JSON over HTTP, reconstructs session/turn/tool hierarchy as OpenTelemetry
spans, preserves full event payloads, derives turn/session token usage from
Claude transcripts, and exports OTLP/HTTP to Jaeger or Langfuse.

The core pipeline is usable. Recent hardening work fixed the most important
data-quality and lifecycle issues:

- Langfuse input/output fields
- spool permissions
- HTTPS/TLS
- incremental transcript parsing
- smaller lock scope
- event idempotency
- SIGTERM graceful shutdown
- attribute dedupe/type correctness
- incomplete turn closure

The main remaining gaps are operational rather than functional: no dual-write,
no readiness/metrics endpoint, no automated Rust test suite, and some security
hardening around local HTTP/Langfuse ports.

## 2. Current verified state

At review time:

```text
Collector PID: 44450
Collector backend: http://127.0.0.1:4318/v1/traces (Jaeger)
Jaeger PID: 40960
Langfuse: stopped (Docker Compose stack not running)
```

Checks:

```text
cargo check                         PASS
cargo build                         PASS
python3 simulate.py                 PASS, 35 spans
cargo clippy --all-targets          PASS with ~11 warnings (no errors)
```

`/health`:

```json
{
  "ok": true,
  "otlp_endpoint": "http://127.0.0.1:4318/v1/traces",
  "otlp_headers_configured": false,
  "sessions": 0,
  "queue_capacity": 4096,
  "queue_available": 4096,
  "dropped_total": 0
}
```

Tested behavior:

- Full 33-event simulation reaches Jaeger as 35 spans
- Langfuse mode reaches 35 observations when the stack is running
- Duplicate Hook payloads are ignored (6 spans instead of 12 in the duplicate test)
- Two-turn transcript test returns 10/20 and 5/7 input/output tokens
- Forced export failure creates `spool` with mode `0700` and batch file `0600`

## 3. Architecture assessment

Strengths:

- Clear state machine: session → turn → tool/subagent/task/pair
- Deterministic trace ID based on `session_id`
- Full event JSON retained in `claude.hook.payload`
- Pair spans use stable `claude.paired_span_id` plus OTel `links`
- Streaming message chunks are collapsed into one span with span events
- Bounded queue + batching + retry + spool is a pragmatic local design
- Langfuse-specific mapping is additive; Jaeger ignores those attributes
- Hook handlers are fail-open: collector failure does not block Claude
- Worktree hooks are deliberately excluded from the default config

Tradeoffs:

- One active turn per session; Claude is normally serial
- One OTLP endpoint per collector; switch, not fan-out
- Full payloads can be large; Jaeger/Langfuse limits apply
- Token parsing depends on Claude transcript schema
- Jaeger is memory-only in the current setup

## 4. Findings

### Fixed in this iteration

| ID | Severity | Finding | Fix | Evidence |
|---|---|---|---|---|
| F-01 | High | Langfuse input/output were empty | Added `langfuse.observation.input/output` for prompt, tools, turn, stream | Jaeger span attributes show prompt/tool/answer text |
| F-02 | High | spool files were world-readable | Directory `0700`, files `0600` | Forced failure test shows `drwx------` / `-rw-------` |
| F-03 | High | HTTPS was unsupported | Enabled reqwest `rustls` | `cargo check`/`build` pass with rustls deps |
| F-04 | Medium | Every turn rescanned full transcript | Added per-session `usage_offset`; parse new range only | Two-turn test returns 10/20 and 5/7 tokens |
| F-05 | Medium | `handle_event` serialized full payload under session lock | Build base attributes before acquiring lock | Code inspection, `base_attrs` before `sessions.lock()` |
| F-06 | Medium | Duplicate Hook requests created duplicate spans | 5-minute global event-key cache; unique root IDs | Duplicate test produces 6, not 12, spans |
| F-07 | High | SIGTERM bypassed graceful shutdown | SIGINT + SIGTERM signal handler; stop waits | SIGTERM test delivered queued batch |
| F-08 | Medium | Duplicate OTel attributes and string-typed numeric fields | Dedupe attributes; int/bool value export | Raw OTLP/Jaeger checks clean |
| F-09 | Medium | `agent.turn`/`session.summary` had no Langfuse usage model | `GENERATION`, model, usage details, `chat` operation | Langfuse events show input/output/total token maps |
| F-10 | Medium | SessionEnd/TTL could orphan active turn | Emit `agent.turn.incomplete` | SessionEnd/TTL tests include incomplete span |

### Open findings

| ID | Severity | Finding | Recommendation |
|---|---|---|---|
| O-01 | High | Langfuse compose binds `3000:3000` and `9090:9000` to all interfaces | Bind `127.0.0.1:3000:3000` and `127.0.0.1:9090:9000`; firewall if remote |
| O-02 | High | `/health` only proves the collector process is alive | Add backend reachability, last export error/time, spool count, queue depth, `/metrics` |
| O-03 | Medium | Only one backend; no Jaeger + Langfuse dual-write | Implement multi-target fan-out or put an OTel Collector in front |
| O-04 | Medium | Transcript parser assumes `promptId`, `message.id`, `usage` schema | Add schema-version detection, `usage.available` flag, fixture tests |
| O-05 | Medium | Spool has no size/age cap; 5xx replay can block a batch | Add max file count/bytes/age and bounded replay backoff |
| O-06 | Medium | No Rust unit/integration tests or CI | Add mock OTLP tests for events, dedupe, tokens, TTL, spool, SIGTERM |
| O-07 | Low | Late/out-of-order `MessageDisplay final` can create a second span | Track completed `message_id`s; ignore late finals |
| O-08 | Low | `FileChanged` has no matcher in `claude-hooks.json` | Add e.g. `"matcher": ".envrc|.env"` if the event is needed |
| O-09 | Low | No OTLP/gRPC or protobuf support | Add optional gRPC/protobuf transport if performance matters |
| O-10 | Low | `start.sh` builds debug binary every start; no launchd unit | Add release build/cached binary and an optional LaunchAgent |
| O-11 | Low | Langfuse headless init did not create project membership automatically | Add an init/post-init membership script; document admin account |
| O-12 | Low | `cargo clippy` has ~11 warnings | Fix `too_many_arguments`, collapsible `if`, `while let` warnings |
| O-13 | Low | HTTPS enabled but not live-tested against a TLS backend | Add an integration test or test against Langfuse Cloud |
| O-14 | Low | Full prompt/tool output is duplicated in payload + Langfuse input/output | Consider size caps or references for very large payloads |

## 5. Recommended roadmap

### P0 — before exposing beyond localhost

1. Bind Langfuse and MinIO ports to `127.0.0.1`
2. Add readiness/metrics and last export error
3. Add auth/token or enforce localhost-only `/hooks`
4. Cap spool size and add replay backoff

### P1 — reliability and maintainability

1. Rust unit tests + CI (`cargo fmt`, `clippy`, `test`)
2. Mock OTLP integration tests for token/dedupe/spool/SIGTERM
3. Multi-target fan-out if both Langfuse and Jaeger are required
4. Improve transcript schema handling and sidechain token semantics
5. Handle late/out-of-order streaming chunks

### P2 — scale and ergonomics

1. OTLP/gRPC + Protobuf
2. Prometheus metrics and Grafana dashboard
3. Release build / launchd service
4. Langfuse project membership automation
5. FileChanged matcher and more precise tool durations

## 6. Operational notes

- Current backend is selected by `collector.env`; it is gitignored.
- `collector.env.langfuse` preserves the previous Langfuse configuration.
- `collector.env.example` shows both Jaeger and Langfuse templates.
- Langfuse is stopped; its Docker volumes remain and should not be deleted with
  `docker-compose down -v`.
- Jaeger currently uses memory storage; stopping it loses traces.
- The default Claude hook set has 31 events; WorktreeCreate/WorktreeRemove are
  intentionally excluded to avoid changing Claude worktree behavior.

## 7. Verification commands

```bash
cd /Users/simon/dev/claude-hooks-collector

cargo check
cargo build
cargo clippy --all-targets
python3 simulate.py

./status.sh
curl http://127.0.0.1:4321/health

# Jaeger
curl -s http://127.0.0.1:16686/api/v3/services

# Langfuse (when running)
curl -s -H "Authorization: Basic $LANGFUSE_AUTH" \
  http://127.0.0.1:3000/api/public/v2/observations?limit=5
```

## 8. Conclusion

The project is in good shape for a local development tool: the core event model,
full payload capture, token accounting, backend switching, and fail-open Hook
behavior are implemented and verified.

The biggest remaining improvement is not a missing feature but **operational
trust**: readiness/metrics, tests/CI, and localhost-only exposure. Those are the
next items to address before treating the collector as a long-running shared
service.
