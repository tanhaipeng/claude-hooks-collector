# Claude Code Hooks -> OTLP Collector

用 Rust + Axum 接收 **Claude Code Hook**，把 Hook 事件转换成 OpenTelemetry span，
通过 **OTLP/HTTP** 发给 Langfuse、Jaeger 或其他兼容后端。

> 项目名/包名/二进制名：`claude-hooks-collector`
> OTel `service.name`：`claude-code-hooks`
> OTel `service.namespace`：`claude-code`

当前后端由 [`collector.env`](collector.env.example) 决定。默认代码配置是 Jaeger
（`http://127.0.0.1:4318`），也支持 Langfuse 和 HTTPS OTLP 后端。

## 特性

- Axum `POST /hooks`，直接接收 Claude Code HTTP Hook 的完整 JSON
- Claude Code 当前 **33 种 Hook 事件**：collector 全部能处理，默认配置启用 **31 种**
- `MessageDisplay` 流式 chunk 按 `message_id` 合并成一个 span，delta 作为 span events
- 保留完整事件内容：`claude.hook.payload` 是原始 JSON，不截断
- 有状态的父子关系：`session → agent.turn → prompt/tool/subagent/task/...`
- 每个事件独立采集；Pre/Post 用两个 span + OTel links 表达配对
- 同一套 OTLP/HTTP 输出兼容 Jaeger 和 Langfuse（通过 endpoint/headers 切换）
- **Langfuse UI 适配**：检测到 Langfuse endpoint 时自动补 `langfuse.observation.*`；Jaeger 模式不写这些字段
- GenAI semconv：`gen_ai.operation.name`、`gen_ai.tool.*`、`gen_ai.usage.*` 等
- `agent.turn` 每轮 token；`session.summary` 累计 token
- 有界队列、批量导出、失败重试、TTL 清理、SIGINT/SIGTERM 优雅退出
- **重复 Hook 去重**：相同 event + 完整 payload 在 5 分钟内只处理一次
- transcript 增量解析
- 支持 HTTPS（reqwest + rustls）

## 快速开始

```bash
cd /Users/simon/dev/claude-hooks-collector

# 可选：配置后端
cp collector.env.example collector.env
# 编辑 collector.env

./start.sh
./status.sh
curl http://127.0.0.1:4321/health
```

停止：

```bash
./stop.sh
```

默认监听：`127.0.0.1:4321`
Hook 端点：`http://127.0.0.1:4321/hooks`

## 架构

```text
Claude Code
  │ HTTP POST /hooks（完整事件 JSON）
  ▼
Axum collector :4321
  │ session / turn / tool / subagent 状态机
  │ span + parentSpanId + links
  ▼
有界队列（默认 4096）
  │ 最多 64 个 / 250ms 一批
  ▼
OTLP/HTTP
  │ 失败重试 3 次
  │ 仍失败 → 记录错误并丢弃该批
  ▼
Jaeger :4318
  或 Langfuse :3000/api/public/otel/v1/traces
  或任意 OTLP/HTTP HTTPS 后端
```

一次只发一个后端，不做双写。切换后端只需要改 `collector.env` 并重启 collector。

## 工程结构

`main.rs` 现在只负责 tracing 初始化和启动，业务逻辑按职责拆分：

| 文件 | 职责 |
|---|---|
| `src/main.rs` | 进程入口，只调用 `app::run` |
| `src/lib.rs` | 模块导出 |
| `src/config.rs` | `collector.env`/环境变量解析、OTLP header、endpoint |
| `src/app.rs` | AppState、启动、exporter/TTL task、优雅退出 |
| `src/http.rs` | Axum 路由、`/hooks`、`/health` |
| `src/hooks.rs` | Hook 事件分发、session/turn/tool 状态机、span 组装、TTL 清理 |
| `src/state.rs` | `SessionState`、pair、MessageStream 状态类型 |
| `src/spans.rs` | SpanDraft、Claude/GenAI 属性、OTLP JSON 转换 |
| `src/exporter.rs` | 队列 worker、批量、重试 |
| `src/usage.rs` | transcript token 增量解析、turn/session token job |
| `src/util.rs` | ID、时间、字段提取等纯工具 |

单元测试直接放在模块内：

```bash
cargo test
```

当前有 12 个测试，覆盖 config、util、attributes、transcript 增量解析、token 去重、Qoder request_set_id、客户端标记。

## OTel 资源命名

| 属性 | 值 |
|---|---|
| `service.name` | `claude-code-hooks` |
| `service.namespace` | `claude-code` |
| `service.version` | `0.1.0` |
| `telemetry.sdk.name` | `claude-hooks-collector` |
| `telemetry.sdk.language` | `rust` |
| `telemetry.source` | `hooks` |

`service.name` 表示“Hook 遥测的生产者”，不是 Claude Code 本体。可通过
`SERVICE_NAME` 覆盖；Qoder 单独实例可设为 `qoder-cli`。

## 配置

`start.sh` 会 source 项目根目录的 `collector.env`（该文件已 gitignore）。
示例见 [`collector.env.example`](collector.env.example)。

```bash
# Jaeger
OTLP_ENDPOINT=http://127.0.0.1:4318

# Langfuse
OTLP_ENDPOINT=http://127.0.0.1:3000/api/public/otel
OTLP_HEADERS='Authorization=Basic <base64(pk:sk)>,x-langfuse-ingestion-version=4'
LANGFUSE_HOST=http://127.0.0.1:3000
LANGFUSE_AUTH=<base64(pk:sk)>
```

环境变量：

| 变量 | 默认值 | 说明 |
|---|---|---|
| `LISTEN_ADDR` | `127.0.0.1:4321` | collector 监听地址 |
| `OTLP_ENDPOINT` | `http://127.0.0.1:4318` | 自动补 `/v1/traces`；支持 http/https |
| `OTLP_HEADERS` | 空 | `key=value,key=value`，用于 Langfuse Basic Auth 等 |
| `OTEL_BACKEND` | 自动 | `langfuse` / `jaeger` / `generic`；留空按 endpoint 自动判断 |
| `SERVICE_NAME` | `claude-code-hooks` | OTel `service.name`；单独跑 Qoder 实例时可设为 `qoder-cli` |
| `QUEUE_CAPACITY` | `4096` | 有界队列；满了丢 span 并计数 |
| `SESSION_TTL_SECS` | `3600` | 无活动 session 清理时间；0 禁用 |
| `TTL_SWEEP_SECS` | `60` | TTL 扫描间隔 |

健康检查：

```bash
curl http://127.0.0.1:4321/health
```

返回包含：

```text
otlp_endpoint
otlp_headers_configured
langfuse
sessions
queue_capacity / queue_available
dropped_total
```

`/health` 目前是进程存活检查，不探测后端是否可达。

## Langfuse 接入

Langfuse 自托管（Docker Compose）示例：

```bash
cd /Users/simon/dev/langfuse
docker-compose up -d
docker-compose ps
```

UI：

```text
http://localhost:3000
```

Headless 初始化会使用 `.env` 里的：

```text
LANGFUSE_INIT_USER_EMAIL
LANGFUSE_INIT_USER_PASSWORD
LANGFUSE_INIT_ORG_ID
LANGFUSE_INIT_PROJECT_ID
LANGFUSE_INIT_PROJECT_PUBLIC_KEY
LANGFUSE_INIT_PROJECT_SECRET_KEY
```

当前项目：

```text
Organization: claude-code
Project:      claude-hooks
```

Collector 配置：

```bash
OTLP_ENDPOINT=http://127.0.0.1:3000/api/public/otel
OTLP_HEADERS='Authorization=Basic <base64(public:secret)>,x-langfuse-ingestion-version=4'
OTEL_BACKEND=langfuse
```

`x-langfuse-ingestion-version: 4` 用于 Langfuse v4 实时 ingestion。

### Langfuse 字段映射

collector 会根据 endpoint 或 `OTEL_BACKEND` 自动启用 Langfuse mapper：

```text
OTEL_BACKEND=langfuse
```

启用后写入：

- `langfuse.observation.type`
- `langfuse.session.id`
- `langfuse.observation.input/output`
- `langfuse.observation.usage_details`
- `langfuse.observation.model.name`

Jaeger 模式（默认 `http://127.0.0.1:4318`）不会写这些字段。

注意：

- 如果只设置 Langfuse endpoint 但使用非标准 host/port，显式设置
  `OTEL_BACKEND=langfuse` 更可靠
- Qoder 使用 `request_set_id` fallback；turn token job 会创建，但 Qoder 当前返回 0

如果 UI 报 **Project Not Found**：

1. 确认登录的是有 `claude-code` 组织权限的账号
2. 确认该用户在 `projects`/`project_memberships` 中有 `claude-hooks`
3. 退出登录后重新登录，或使用无痕窗口

这是 Langfuse 的项目成员权限问题，不是 collector 上报失败。

### 停止 Langfuse

```bash
cd /Users/simon/dev/langfuse
docker-compose down
```

数据卷会保留。不要用 `docker-compose down -v`，否则 Langfuse 的 ClickHouse/MinIO/Postgres 数据会被删除。

## Jaeger 接入

Jaeger 不需要额外 OTel Collector：

```bash
cd /Users/simon/Documents/deepseek-harness/default-workspace/jaeger
./start.sh
```

端点：

```text
UI:        http://localhost:16686
OTLP HTTP: http://localhost:4318
OTLP gRPC: localhost:4317
```

配置：

```bash
OTLP_ENDPOINT=http://127.0.0.1:4318
```

注意：当前 Jaeger 是 **memory storage**，停止后 trace 丢失。需要持久化时给 Jaeger 配 Badger/ClickHouse 等 storage。

## 配置 Claude Code

把 [`claude-hooks.json`](claude-hooks.json) 的 `hooks` 合并进
`~/.claude/settings.json`，修改后重启 Claude Code。

先备份：

```bash
cp ~/.claude/settings.json ~/.claude/settings.json.bak
```

合并示例：

```python
import json
from pathlib import Path

settings_path = Path.home() / ".claude" / "settings.json"
fragment = json.loads(Path("claude-hooks.json").read_text())

data = json.loads(settings_path.read_text()) if settings_path.exists() else {}
hooks = data.setdefault("hooks", {})

for event, groups in fragment["hooks"].items():
    current = hooks.setdefault(event, [])
    for group in groups:
        if group not in current:
            current.append(group)

settings_path.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n")
```

如果设置了 `allowedHttpHookUrls`，加入：

```json
"allowedHttpHookUrls": ["http://127.0.0.1:4321/hooks"]
```

### 默认启用的 31 个事件

```text
SessionStart, Setup, UserPromptSubmit, UserPromptExpansion,
PreToolUse, PermissionRequest, PermissionDenied,
PostToolUse, PostToolUseFailure, PostToolBatch,
Notification, MessageDisplay,
SubagentStart, SubagentStop,
TaskCreated, TaskCompleted,
Stop, StopFailure, TeammateIdle,
InstructionsLoaded, ConfigChange, CwdChanged, DirectoryAdded,
FileChanged, PreCompact, PostCompact,
PreModelSwitch, PostModelSwitch,
Elicitation, ElicitationResult, SessionEnd
```

### 为什么没有 WorktreeCreate / WorktreeRemove

这两个事件有特殊失败契约：

- `WorktreeCreate` Hook 会替换 Claude 默认的 `git worktree` 行为
- HTTP Hook 需要返回 `hookSpecificOutput.worktreePath`
- 返回空 204 可能导致 worktree 创建失败

默认配置不挂这两个事件，避免遥测影响 Claude 的 worktree 流程。Collector 仍能处理它们，但只建议在明确实现自定义 worktree 时启用。

### 为什么 SessionStart 用 command + curl

Claude Code 的 `SessionStart` 只支持 `type: "command"` 和 `type: "mcp_tool"`，不支持 HTTP Hook。

所以：

```bash
/usr/bin/curl -s --max-time 2 --connect-timeout 1 \
  -X POST http://127.0.0.1:4321/hooks \
  -H 'Content-Type: application/json' \
  --data-binary @- >/dev/null 2>&1 || true
```

失败也退出 0，不会阻塞 Claude 启动。

## Hook 失败是否影响 Claude

默认配置是 fail-open：

- HTTP Hook 连接失败、非 2xx、timeout：非阻塞错误
- `SessionStart` 的 curl 失败被 `|| true` 吞掉
- 没有配置 `onFailure: "block"`
- 默认 timeout 为 2 秒

实测 collector 停止时，`claude -p` 仍以 exit 0 完成；输出可能出现一条
`hook ... failed` 提示，但不会中断 prompt 或工具调用。

## Qoder CLI 接入

本机 Qoder CLI `1.1.78` 已验证可以直接把 Hooks 发到当前 collector。

Qoder 配置：`~/.qoder/settings.json`

- 除 `WorktreeCreate` / `WorktreeRemove` 外，Qoder 文档中的 25 个事件已配置
- `SessionStart` 使用 `command + curl` 转发到 `4321`
- 其余事件使用 Qoder 原生 `type: "http"` 指向 `http://127.0.0.1:4321/hooks`
- Hook `timeout` 为 2 秒
- 旧 `qoder-otel-bridge`（4320）不再是 Qoder 的接收端

实测 Qoder 会话 trace：

<http://127.0.0.1:16686/trace/a688f42fbb316f4323d93fa90faa9078>

包含：

```text
session
prompt
tool.Read
tool.Read.result
agent.turn
session.end
session.summary
```

### Qoder 兼容性限制

- Qoder 的 `UserPromptSubmit` 使用 `request_set_id`，没有 Claude 的 `prompt_id`
  - collector 现在会用 `request_set_id` / `parent_request_set_id` 作为 turn ID 和 token job 的 fallback
  - turn/tool span 正常
  - per-turn token job 会创建，但 Qoder 当前返回的 token 仍为 0
- Qoder 的模型名有两层：
  - Hook payload 的 `model`（如 `Qwen3.8-Flash`）→ `claude.model` / `gen_ai.request.model`
  - transcript 的 provider model（如 `qfmodel`）→ `gen_ai.response.model` / `claude.usage.model`
  - 不会再用 transcript model 覆盖用户看到的请求模型
- Qoder transcript 的 `usage` 目前为 0（Qwen3.8-Flash），所以 `session.summary` token 也是 0
  - 这是 Qoder 数据源限制，不是 collector 丢失
- Qoder 与 Claude Code 默认共用 `service.name=claude-code-hooks`
  - 需要严格隔离时，给 Qoder 单独跑一个 collector 实例并设置 `SERVICE_NAME=qoder-cli`
  - span 上有 `claude.client=qoder` 可用于过滤
- Qoder 的 `WorktreeCreate` / `WorktreeRemove` 未配置，原因与 Claude 相同：特殊失败契约可能影响 worktree 流程

## 事件映射

| Claude Hook 事件 | 生成的 span | 说明 |
|---|---|---|
| `SessionStart` | `session` | 根 span |
| `Setup` | `setup` | 初始化 |
| `UserPromptSubmit` | `prompt` | 创建 turn |
| `UserPromptExpansion` | `prompt.expansion` | slash command 展开 |
| `PreToolUse` | `tool.<name>` | 工具开始，零时长 |
| `PostToolUse` | `tool.<name>.result` | 工具结束，带 duration |
| `PostToolUseFailure` | `tool.<name>.result` | status=ERROR |
| `PostToolBatch` | `tool.batch` | 批量工具结束 |
| `PermissionRequest` | `permission.request` | 权限请求 |
| `PermissionDenied` | `permission.denied` | 权限拒绝 |
| `SubagentStart` | `subagent.<type>` | 子 agent 开始 |
| `SubagentStop` | `subagent.<type>.stop` | 子 agent 结束 |
| `TaskCreated` | `task` | 任务创建 |
| `TaskCompleted` | `task.result` | 任务完成 |
| `Stop` | `agent.turn` | 一轮结束 |
| `StopFailure` | `agent.turn` | status=ERROR |
| `PreCompact` | `context.compact` | 压缩开始 |
| `PostCompact` | `context.compact.result` | 压缩结束 |
| `PreModelSwitch` | `model.switch` | 模型切换开始 |
| `PostModelSwitch` | `model.switch.result` | 模型切换结束 |
| `Elicitation` | `mcp.elicitation` | MCP 询问开始 |
| `ElicitationResult` | `mcp.elicitation.result` | MCP 询问结束 |
| `Notification` | `notification` | 通知 |
| `MessageDisplay` | `message.display` | 流式消息合并 |
| `TeammateIdle` | `teammate.idle` | teammate 空闲 |
| `InstructionsLoaded` | `instructions.loaded` | 指令加载 |
| `ConfigChange` | `config.change` | 配置变更 |
| `CwdChanged` | `cwd.changed` | 工作目录变更 |
| `DirectoryAdded` | `directory.added` | 添加目录 |
| `FileChanged` | `file.changed` | 文件变更 |
| `WorktreeCreate` | `worktree.create` | 默认未启用 |
| `WorktreeRemove` | `worktree.remove` | 默认未启用 |
| `SessionEnd` | `session.end` | 会话结束 |
| 其他/未知事件 | `event.<name>` | 兜底 |

### trace 树

```text
session
├── session.summary
├── agent.turn
│   ├── prompt
│   ├── prompt.expansion
│   ├── tool.Read
│   ├── tool.Read.result
│   ├── tool.Bash
│   ├── tool.Bash.result
│   ├── subagent.Explore
│   ├── subagent.Explore.stop
│   ├── task
│   ├── task.result
│   ├── context.compact
│   ├── context.compact.result
│   ├── model.switch
│   ├── model.switch.result
│   ├── mcp.elicitation
│   └── mcp.elicitation.result
├── agent.turn.incomplete   # SessionEnd/TTL 时若仍有 active turn
├── setup
├── permission.request
└── session.end
```

Pre/Post、Start/Stop 是兄弟节点，通过 `claude.paired_span_id` 和 OTel links 关联。

## 属性

每个事件 span 至少包含：

```text
claude.hook.event
claude.hook.payload          # 完整原始 JSON
claude.hook.payload_bytes
claude.session_id
claude.prompt_id
claude.request_set_id
claude.parent_request_set_id
claude.client
claude.cwd
claude.permission_mode
claude.tool_name / claude.tool_use_id
claude.agent_id / claude.agent_type
claude.prompt_length
claude.paired_span_id
claude.timing.source
gen_ai.conversation.id
```

GenAI：

```text
gen_ai.operation.name
gen_ai.tool.name / gen_ai.tool.call.id
gen_ai.agent.name / gen_ai.agent.id
gen_ai.request.model
gen_ai.response.model
gen_ai.usage.input_tokens / output_tokens
claude.usage.model
```

Langfuse 模式额外包含：

```text
langfuse.observation.type
langfuse.session.id
langfuse.observation.input / output
langfuse.observation.usage_details
langfuse.observation.model.name
```

数值/布尔属性会导出为 `intValue` / `boolValue`；重复 key 会在导出前合并。

## Token

### agent.turn

`Stop` / `StopFailure` 后从 transcript 解析当前 turn：

```text
gen_ai.usage.input_tokens
gen_ai.usage.output_tokens
claude.usage.total_tokens
claude.usage.cache_read_input_tokens
claude.usage.cache_creation_input_tokens
claude.usage.scope = turn
claude.usage.source = transcript
claude.usage.prompt_id
```

按 assistant `message.id` 去重，避免同一响应多个 content block 重复计数。

transcript 按 session 维护 `usage_offset`，Stop 只解析新增区间，不每轮重扫全文件。
`Stop` 可能早于 transcript flush，最多重试 6 次，每次 300ms。

如果 SessionEnd/TTL 时还有未关闭 turn，会补发：

```text
agent.turn.incomplete
  claude.turn.incomplete = true
  claude.turn.reason = session_end_without_stop / session_timeout
```

### session.summary

`SessionEnd` 后生成累计 token 汇总：

```text
gen_ai.usage.input_tokens
gen_ai.usage.output_tokens
claude.usage.total_tokens
claude.usage.cache_read_input_tokens
claude.usage.cache_creation_input_tokens
claude.session.duration_ms
claude.turn_count
claude.tool_count
claude.subagent_count
```

### message.display

流式 delta 合并成一个 span，每个 delta 是 span event：

```text
message.delta
  claude.message.index
  claude.message.final
  claude.message.delta_length
  claude.message.delta
```

## 可靠性

- 有界队列：`QUEUE_CAPACITY`，满了丢 span 并计 `dropped_total`
- 批量：最多 64 个或 250ms 一批
- 失败重试：3 次
- 重试仍失败：记录错误并丢弃该批（不落盘、不重放）
- TTL：超时清理 session，补发 `session.timeout` / `agent.turn.incomplete`
- SIGINT/SIGTERM：优雅停止，等待 exporter drain
- ID：session root 每次进程唯一；turn/tool 等 ID 含 root 前缀，降低重启重复 ID 风险
- 重复 Hook 去重：相同 event + 完整 payload 在 5 分钟内只处理一次

## 验证

```bash
cargo check
cargo build
cargo test
python3 simulate.py
```

`simulate.py` 会：

1. 发送 36 条 Hook（33 种事件 + 额外 PreToolUse + 2 个额外 MessageDisplay chunk）
2. 自动检测 collector 的 OTLP endpoint
3. Jaeger 模式查询 `/api/v3/traces`
4. Langfuse 模式查询 `/api/public/v2/observations`
5. 校验 35 个 span/observation、payload、token、message events
6. Jaeger 模式不写 `langfuse.*`；Langfuse 模式由 `OTEL_BACKEND`/endpoint 自动启用

Jaeger 当前示例 trace：

<http://127.0.0.1:16686>

## 安全与限制

- **隐私**：`claude.hook.payload` 包含 prompt、工具输入输出、文件内容；只应发本地或可信后端
- **默认监听 localhost**：不要把 collector 的 `/hooks` 暴露到公网
- **Langfuse**：自托管时建议把 3000/9090 绑定 `127.0.0.1`，当前 compose 默认监听所有网卡
- **无鉴权**：`/hooks` 没有 token，适合本地；远程部署需要加认证或反向代理
- **单后端**：一次只发 Jaeger 或 Langfuse，不做双写
- **不支持 gRPC/Protobuf**：当前只支持 OTLP/HTTP JSON
- **Jaeger memory storage**：重启丢 trace
- **一轮一个 turn**：同 session 当前只维护一个 active turn；Claude 正常串行
- **transcript schema**：依赖 Claude JSONL 的 `promptId` / `message.id` / `usage` 字段，Claude 改格式时需要适配
- **CI/metrics 未完成**：已有 9 个 Rust 单测，但还没有 CI 和 Prometheus metrics

## 排查

### collector 没数据

```bash
cd /Users/simon/dev/claude-hooks-collector
./status.sh
tail -50 collector.log
curl http://127.0.0.1:4321/health
```

确认：

- Hook 是否请求了 `127.0.0.1:4321`
- `collector.env` 的 `OTLP_ENDPOINT` 是否正确
- 后端是否启动

### Langfuse Project Not Found

检查当前登录账号是否属于 `claude-code` 组织，并且在 `claude-hooks` 项目有 membership。修复后退出登录重新进入。

### 切换后端

```bash
# 切 Jaeger
printf 'OTLP_ENDPOINT=http://127.0.0.1:4318\n' > collector.env
./stop.sh && ./start.sh

# 切 Langfuse（保留 collector.env.langfuse 备份时）
cp collector.env.langfuse collector.env
./stop.sh && ./start.sh
```

## Review 与后续改进

完整 review 记录见 [docs/REVIEW.md](docs/REVIEW.md)。

当前已知可改进项：

1. Langfuse/MinIO 端口绑定 127.0.0.1
2. health 增加后端可达性、last export error
3. 如确实需要，再做多后端双写
4. CI + mock OTLP 集成测试
5. 支持 OTLP/gRPC / Protobuf
6. FileChanged matcher
7. 更精确的工具耗时（transcript 时间戳）
8. metrics（Prometheus）
9. 需要 Langfuse 完整 UI 映射时，加可选 Langfuse backend mapper
