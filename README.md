# Claude Code Hooks -> OTLP Collector (Rust + Axum)

接收 Claude Code 的 **HTTP 类型 Hook**，把每个 Hook 事件转换成 OTel span，
通过 **OTLP/HTTP** 上报到 Langfuse、Jaeger 或其他兼容后端。
当前后端由 `collector.env` 决定；本机现在发送到 **Jaeger**（`http://127.0.0.1:4318/v1/traces`），Langfuse 接入方式见下文。

## 特性

- Axum `POST /hooks`，直接接收 Claude Code HTTP Hook 的 JSON body
- Collector 支持 Claude Code 当前 **33 种 Hook 事件**；默认配置启用 **31 种**（不含 WorktreeCreate/WorktreeRemove）
- **每个 Hook 事件都采集**；`MessageDisplay` 的流式 chunk 合并为一个 span，delta 作为 span events
- **完整保存事件 JSON**：`claude.hook.payload` 属性就是原始 body
- 保留常用字段：`session_id`、`prompt_id`、`cwd`、`tool_name`、`tool_input`、`tool_response` 等
- 有状态的父子关系：
  - `session` 根 span
  - `agent.turn` 一轮对话
  - `tool.<name>` / `tool.<name>.result` 工具调用
  - `subagent.<type>` / `.stop`
  - `context.compact`、`model.switch`、`task`、`mcp.elicitation`
- OTLP 导出异步执行，不阻塞 Claude Code Hook
- 32MB 请求体上限，适配大工具输出
- **P0 可靠性**：SIGINT/SIGTERM 优雅退出、有界队列、批量导出、失败重试、本地 spool 重放、TTL 清理
- **P1 语义**：GenAI 语义属性、span links、`agent.turn` 每轮 token、`session.summary` 会话汇总、int/bool 属性类型
- **Langfuse 可读性**：input/output、observation type、model、usage、session id
- **安全/稳定性**：spool 目录 0700、文件 0600；5 分钟事件幂等；增量 transcript 解析
- **HTTPS**：reqwest 启用 rustls，可发 Langfuse Cloud 或其他 HTTPS OTLP 后端

## 架构

```text
Claude Code
  │ HTTP POST /hooks（完整事件 JSON）
  ▼
Axum collector :4321
  │ 状态机：session / turn / tool / subagent ...
  │ 生成 span + parentSpanId + links
  ▼
有界队列（默认 4096）
  │ 批量 64 个 / 250ms
  ▼
OTLP/HTTP → Langfuse :3000/api/public/otel/v1/traces
  │ Basic Auth + x-langfuse-ingestion-version: 4
  │ 失败重试 3 次
  ├─ 仍失败 → spool/*.json → 定时重放
  ▼
Langfuse UI :3000
```

也可以是 Jaeger：

```text
OTLP_ENDPOINT=http://127.0.0.1:4318
```

此时 trace 只发 Jaeger，不并发双写。

## OTel 资源命名

上报的 resource attributes：

| 属性 | 值 |
|---|---|
| `service.name` | `claude-code-hooks` |
| `service.namespace` | `claude-code` |
| `service.version` | `0.1.0` |
| `telemetry.sdk.name` | `claude-hooks-collector` |
| `telemetry.source` | `hooks` |

说明：

- 项目/目录/二进制仍叫 `claude-hooks-collector`
- Langfuse 里的 **service 名**是 `claude-code-hooks`，表示“Claude Code 的 Hook 遥测”
- `service.namespace=claude-code` 保留产品归属
- 之前写入 Jaeger 的旧 trace 仍显示在 `claude-code` service 下；新 trace 发往 Langfuse

## 运行

```bash
./start.sh
./status.sh
./stop.sh
```

默认监听：`http://127.0.0.1:4321`

健康检查：

```bash
curl http://127.0.0.1:4321/health
```

环境变量：

| 变量 | 默认值 |
|---|---|
| `LISTEN_ADDR` | `127.0.0.1:4321` |
| `OTLP_ENDPOINT` | `http://127.0.0.1:3000/api/public/otel`（会自动补 `/v1/traces`，支持 http/https） |
| `OTLP_HEADERS` | 可选，`key=value,key=value`；Langfuse 需要 Basic Auth 和 `x-langfuse-ingestion-version=4` |
| `QUEUE_CAPACITY` | `4096`（有界队列；满了丢 span 并计数） |
| `SPOOL_DIR` | `spool`（导出失败后的落盘目录） |
| `SESSION_TTL_SECS` | `3600`（0 表示禁用 TTL） |
| `TTL_SWEEP_SECS` | `60`（TTL 扫描间隔） |
| `SPOOL_REPLAY_SECS` | `30`（spool 重放间隔） |

## Langfuse 本地部署

本次已安装到：

```text
/Users/simon/dev/langfuse
```

运行方式：

```bash
cd /Users/simon/dev/langfuse
docker-compose up -d
docker-compose ps
```

UI：

```text
http://localhost:3000
```

登录账号和初始密码保存在 `langfuse/.env` 的 `LANGFUSE_INIT_USER_*` 变量里。
Project：`claude-hooks`；Organization：`claude-code`。

后端运行时：

- Docker 使用 Colima；已设置 `brew services start colima` 开机自启
- Langfuse 容器使用 `restart: always`
- 停止 Langfuse：`docker-compose down`（数据卷保留）

Collector 使用 `collector.env`（已 gitignore）配置 Langfuse：

```bash
OTLP_ENDPOINT=http://127.0.0.1:3000/api/public/otel
OTLP_HEADERS='Authorization=Basic <base64(pk:sk)>,x-langfuse-ingestion-version=4'
LANGFUSE_HOST=http://127.0.0.1:3000
LANGFUSE_AUTH=<base64(pk:sk)>
```

`start.sh` 会自动 source 这个文件。示例见
[collector.env.example](collector.env.example)。

验证：

```bash
python3 simulate.py
```

`simulate.py` 会检测 collector 的 OTLP endpoint；如果指向 Langfuse，
会查询 `GET /api/public/v2/observations` 并确认 35 个 observation 已到达。

`agent.turn` / `session.summary` 会映射为 Langfuse 的 `GENERATION`，
`usage_details` 包含 `input` / `output` / `total` / cache token，模型名写入
`langfuse.observation.model.name`。

同时补充 Langfuse input/output：

- `prompt` → `langfuse.observation.input`
- `tool.*` → tool_input / tool_output
- `agent.turn` → 本轮 prompt / last_assistant_message
- `message.display` → 合并后的流式文本

spool 目录权限为 `0700`，batch 文件为 `0600`。同一个 Hook 事件在 5 分钟内重复提交会被忽略。

切换后端时不做双写：当前 `collector.env` 已切回 Jaeger，新 trace 只发 Jaeger；Langfuse 数据仍保留在其 Docker Volume。
如果要切换回 Jaeger，修改 `collector.env` 的 `OTLP_ENDPOINT` 并重启 collector。

## 不影响 Claude 流程

默认 hook 配置按“遥测失败不能阻塞 Claude”设计：

- HTTP Hook 的 connection failure / 非 2xx / timeout 默认是非阻塞错误，Claude 继续执行。
- `SessionStart` 用 `command + curl`，加了 `--max-time 2`，并且 `>/dev/null 2>&1 || true`，失败也退出 0。
- `WorktreeCreate` / `WorktreeRemove` **没有启用**：这两个事件有特殊失败契约，WorktreeCreate 的 hook 会替换 Claude 默认的 `git worktree` 行为，返回空 204 可能导致 worktree 创建失败。为避免影响 Claude，默认配置直接不挂这两个事件；collector 仍能处理它们，但只建议在你明确要自定义 worktree 时手动配置。
- 没有配置 `onFailure: "block"`。

如果 collector 没启动，Claude 仍能继续执行，只是没有 trace；原生 HTTP Hook
可能显示一条 `hook ... failed` / `hook error` 提示，但这是非阻塞错误，不会中断
prompt、工具调用或会话。实测 collector 停止时 `claude -p` 仍以 exit 0 完成。

默认 HTTP Hook 的 `timeout` 设为 2 秒，SessionStart 的 curl 也限制为 2 秒，
避免 collector 卡住时拖慢 Claude。

## 配置 Claude Code

把 [claude-hooks.json](claude-hooks.json) 里的 `hooks` 合并进
`~/.claude/settings.json`（或项目 `.claude/settings.json`）。

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

如果你设置了 `allowedHttpHookUrls`，记得把
`http://127.0.0.1:4321/hooks` 加进白名单：

```json
{
  "allowedHttpHookUrls": ["http://127.0.0.1:4321/hooks"]
}
```

配置修改后需要**重启 Claude Code**。

> **SessionStart 的特殊处理**：Claude Code 的 `SessionStart` 只支持
> `type: "command"` 和 `type: "mcp_tool"`，不支持 HTTP Hook。
> 所以 `claude-hooks.json` 里 `SessionStart` 用 `curl` 把 JSON 转发给
> 同一个 `/hooks` 端点，其他事件使用原生 `type: "http"`。

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
| `MessageDisplay` | `message.display` | 消息显示 |
| `TeammateIdle` | `teammate.idle` | teammate 空闲 |
| `InstructionsLoaded` | `instructions.loaded` | 指令加载 |
| `ConfigChange` | `config.change` | 配置变更 |
| `CwdChanged` | `cwd.changed` | 工作目录变更 |
| `DirectoryAdded` | `directory.added` | 添加目录 |
| `FileChanged` | `file.changed` | 文件变更 |
| `WorktreeCreate` | `worktree.create` | 默认配置未启用，避免影响 Claude worktree |
| `WorktreeRemove` | `worktree.remove` | 默认配置未启用 |
| `SessionEnd` | `session.end` | 会话结束 |
| 其他/未知事件 | `event.<name>` | 兜底 |

所有 span 的父节点规则：

```text
session
├── session.summary
├── agent.turn
├── agent.turn.incomplete   # 异常结束/TTL 时出现
│   ├── prompt
│   ├── tool.Read
│   ├── tool.Read.result
│   ├── tool.Bash
│   ├── tool.Bash.result
│   ├── subagent.Explore
│   ├── subagent.Explore.stop
│   └── ...
├── setup
├── permission.request
└── session.end
```

注意：为了“每个事件都可见”，`PreToolUse` 和 `PostToolUse` 是两个 span：

- `tool.Read`：PreToolUse，零时长
- `tool.Read.result`：PostToolUse，带 duration，并带 `claude.paired_span_id`

这样做的好处是：

- PreToolUse 事件本身不会丢
- 即使 PostToolUse 永远没来，PreToolUse 的完整内容也已经上报

## 完整事件内容

每个 span 都带：

| 属性 | 内容 |
|---|---|
| `claude.hook.event` | 事件名 |
| `claude.hook.payload` | **完整原始 JSON** |
| `claude.hook.payload_bytes` | 原始 JSON 字节数 |
| `claude.session_id` | 会话 ID |
| `claude.prompt_id` | prompt ID |
| `claude.cwd` | 工作目录 |
| `claude.permission_mode` | 权限模式 |
| `claude.tool_name` / `claude.tool_use_id` | 工具信息 |
| `claude.agent_id` / `claude.agent_type` | 子 agent 信息 |
| `claude.prompt_length` | prompt 长度 |
| `claude.paired_span_id` | 配对 span 的 ID |
| `claude.timing.source` | `hook_receipt` / `turn_usage` / `session_summary` |
| `claude.message.id` | 流式消息 ID |
| `claude.message.chunk_count` | 合并的 delta 数量 |
| `claude.message.total_chars` | 合并后的总字符数 |
| `claude.message.first_index` / `last_index` | delta 索引范围 |
| `claude.message.duration_ms` | 首个 delta 到 final 的时长 |
| `gen_ai.conversation.id` | 会话 ID |
| `gen_ai.operation.name` | `execute_tool` / `invoke_agent` |
| `gen_ai.tool.name` / `gen_ai.tool.call.id` | 工具语义 |
| `gen_ai.agent.name` / `gen_ai.agent.id` | 子 agent 语义 |
| `gen_ai.request.model` | 模型名（如果事件里有） |

配对结果 span 还会带 OTel **links**，指向 Pre/Start span。

### agent.turn 的 token

`Stop` / `StopFailure` 后会从 transcript 解析**当前 turn** 的 token，挂到 `agent.turn`：

| 属性 | 内容 |
|---|---|
| `gen_ai.usage.input_tokens` / `output_tokens` | 本轮 token |
| `claude.usage.cache_read_input_tokens` / `cache_creation_input_tokens` | 缓存 token |
| `claude.usage.total_tokens` | 本轮总 token |
| `claude.usage.scope` | `turn` |
| `claude.usage.source` | `transcript` |
| `claude.usage.prompt_id` | 对应 prompt |
| `gen_ai.request.model` | 模型名 |

按 assistant `message.id` 去重，避免 Claude transcript 里同一响应的多个
content block 被重复计算。`Stop` 可能早于 transcript flush，因此最多重试
6 次、每次 300ms。

如果会话在 `Stop` 之前异常结束（`SessionEnd` 或 TTL 清理），collector 会补发：

```text
agent.turn.incomplete
  claude.turn.incomplete = true
  claude.turn.reason     = session_end_without_stop / session_timeout
```

这样 `prompt` / `tool.*` 不会成为没有父节点的孤儿 span。

### session.summary 的 token

`session.summary` 仍是整个 session 的累计 token：

| 属性 | 内容 |
|---|---|
| `claude.session.duration_ms` | 整个 session 时长 |
| `claude.turn_count` / `claude.tool_count` / `claude.subagent_count` | 事件计数 |
| `gen_ai.usage.input_tokens` / `output_tokens` | 累计 token |
| `claude.usage.cache_read_input_tokens` / `cache_creation_input_tokens` | 缓存 token |
| `claude.usage.total_tokens` | 总 token |
| `gen_ai.request.model` | 模型名 |

`message.display` 的每个流式 delta 会变成该 span 的一个 **span event**：

```text
message.delta
  claude.message.index
  claude.message.final
  claude.message.delta_length
  claude.message.delta
```

因此长回复不会再产生 N 个 `message.display` span，而是一个 span + N 个事件。

`claude.hook.payload` 是完整 JSON，不截断。因此：

- `UserPromptSubmit` 的 prompt 全文在里面
- `PreToolUse` 的 `tool_input` 全文在
- `PostToolUse` 的 `tool_response` 全文在
- `PermissionRequest` / `Elicitation` 的所有字段都在

## 验证

```bash
./start.sh
python3 simulate.py
```

`simulate.py` 会发送 36 条 Hook（33 种事件 + 一次额外 PreToolUse + 两个额外
MessageDisplay chunk），期望生成 **35 个 span**（34 个事件 span + 1 个
`session.summary`；3 个流式 chunk 合并成 1 个 `message.display`），然后：

1. 等 Jaeger 索引
2. 按 `session_id` 计算 trace ID
3. 检查 span 数量、payload、prompt/tool 标记、`agent.turn` 和 `session.summary` 的 token 属性

最后输出类似：

```text
session                    event=SessionStart ...
  setup                    event=Setup ...
  agent.turn               event=Stop ...
    prompt                 event=UserPromptSubmit ...
    prompt.expansion       event=UserPromptExpansion ...
    tool.Read              event=PreToolUse ...
    tool.Read.result       event=PostToolUse ...
    ...
  session.end              event=SessionEnd ...
OK: all hook events exported with full payload
```

## 注意

1. **隐私**：prompt、工具输入、工具输出、文件内容都会进 `claude.hook.payload`。
   这套服务只应监听 `127.0.0.1`，Jaeger 也不要把端口暴露到公网。
2. **体积**：大工具输出可能让单个 span 超过 OTLP/Jaeger 的限制。
   服务端已放宽到 32MB，但 Jaeger 的 OTLP 接收端仍可能拒绝超大请求；
   必要时在前面加 OTel Collector 并调整 `max_recv_msg_size`。
3. **状态**：session/turn/tool 状态在内存里；collector 重启会丢未关闭的配对，
   已经上报的 span 不受影响。超过 `SESSION_TTL_SECS` 的 session 会生成
   `session.timeout` 和（如果有未关闭 turn）`agent.turn.incomplete`，然后清理。
4. **一轮一个 turn**：同一个 session 当前只维护一个 `turn_id`，Claude Code 正常是串行 turn；
   如果将来并行，需要改成 map/stack。
5. **HTTP Hook 返回**：collector 立即返回 `204`，导出由后台队列/worker 执行，
   不让 Jaeger 的延迟阻塞 Claude Code。
6. **token 成本**：当前统计 token，不计算美元成本；成本需要额外的模型价目表。
   `agent.turn` 是每轮 token，`session.summary` 是累计 token。
7. **关停**：`Ctrl-C` 和 `SIGTERM` 都会触发优雅退出；`stop.sh` 会等待 exporter
   drain 完成，超时才 SIGKILL。
8. **属性类型**：`gen_ai.usage.*`、`*_count`、`*_ms`、`*_tokens` 等导出为
   `intValue`；`claude.message.final`、`claude.turn.incomplete` 等导出为
   `boolValue`。重复 key 会在导出前合并。
9. **spool**：启动时立即尝试重放；4xx 的坏批次会重命名为 `.bad-*`，避免阻塞后续批次。
10. **FileChanged**：当前配置没有 matcher，Claude 可能不会触发该事件；需要时在
    `claude-hooks.json` 里补 `matcher`（例如 `.envrc|.env`）。
11. **耗时精度**：工具耗时仍是“Pre/Post Hook 收到时间差”，
   属性 `claude.timing.source=hook_receipt` 标明了这一点；更精确的耗时需要
   从 transcript 里解析工具时间戳。
12. **流式消息**：`MessageDisplay` 按 `message_id` 合并到 `final=true`；
   未收到 final 时，会在 Stop/SessionEnd/TTL 时以
   `message.display.incomplete` 补发。
