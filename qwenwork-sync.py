#!/usr/bin/env python3
"""把 Qoder CLI / Claude Code 的 hooks 配置降级成 QwenWork（opencode 引擎）可用的版本。

QwenWork 的用户级 hooks（~/.qwenworkcn/settings.json）限制：
  * 只认 8 个事件：SessionStart / UserPromptSubmit / PreToolUse / PostToolUse /
    PostToolUseFailure / Notification / Stop / SessionEnd
  * 只认 type=command 的处理器，http 会被静默丢弃
  * 处理器字段白名单：type / command / timeout / async / shell

所以本脚本做三件事：
  1. 裁掉 QwenWork 不认识的事件（并在报告里列出来）
  2. 把 http 处理器改写成等价的 curl command（保留 url / headers / timeout）
  3. 剥掉白名单之外的字段，避免整条处理器被宿主跳过

用法：
  ./qwenwork-sync.py                      # ~/.qoder/settings.json -> ~/.qwenworkcn/settings.json
  ./qwenwork-sync.py --from ~/.claude/settings.json
  ./qwenwork-sync.py --dry-run            # 只打印报告，不落盘
"""

from __future__ import annotations

import argparse
import json
import os
import shlex
import shutil
import sys
import time

SUPPORTED_EVENTS = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "Notification",
    "Stop",
    "SessionEnd",
]

# 宿主 user-hook 解析器的处理器字段白名单（多一个字段整条会被 skip）
HANDLER_FIELDS = {"type", "command", "timeout", "async", "shell"}
# 组级允许的字段
GROUP_FIELDS = {"matcher", "sequential", "hooks", "async"}

CURL = "/usr/bin/curl"


def curl_command(url: str, headers: dict[str, str] | None, timeout: int) -> str:
    parts = [
        CURL,
        "-sS",
        f"--max-time {timeout}",
        f"--connect-timeout {min(1, timeout)}",
        "-X POST",
        shlex.quote(url),
        "-H 'Content-Type: application/json'",
    ]
    for key, value in (headers or {}).items():
        parts.append(f"-H {shlex.quote(f'{key}: {value}')}")
    parts.append("--data-binary @- >/dev/null 2>&1 || true")
    return " ".join(parts)


def normalize_timeout(raw, warnings: list[str], where: str) -> int:
    """timeout 单位为秒；非正数按 2 处理，并保证比 curl 的 --max-time 多 1s 余量。"""
    try:
        value = int(raw)
    except (TypeError, ValueError):
        value = 2
    if value <= 0:
        warnings.append(f"{where}: timeout={raw!r} 非法，回退为 2s")
        value = 2
    return value


# http 处理器里由本脚本消费/已知的源字段（不是"被丢弃"，不要误报）
HTTP_SOURCE_FIELDS = {
    "type",
    "url",
    "headers",
    "timeout",
    "once",
    "allowedEnvVars",
    "async",
    "name",
    "description",
    "statusMessage",
}


def convert_handler(handler: dict, where: str, warnings: list[str]) -> dict | None:
    kind = handler.get("type")

    if kind == "command":
        extra = sorted(set(handler) - HANDLER_FIELDS)
        if extra:
            warnings.append(f"{where}: 丢弃非白名单字段 {extra}")
        command = handler.get("command")
        if not isinstance(command, str) or not command.strip():
            warnings.append(f"{where}: command 为空，跳过")
            return None
        timeout = normalize_timeout(handler.get("timeout"), warnings, where)
        result: dict = {
            "type": "command",
            "command": command,
            "timeout": max(3, timeout),
            # 显式 bash：宿主默认用 $SHELL 的登录 shell（zsh -lc），会加载 .zshrc，
            # 每次 hook 多花 3~8ms；bash -c 是非登录 shell，快且不受 rc 影响
            "shell": handler["shell"] if handler.get("shell") in ("bash", "powershell") else "bash",
        }
        if handler.get("async") is True:
            result["async"] = True
        return result

    if kind == "http":
        extra = sorted(set(handler) - HTTP_SOURCE_FIELDS)
        if extra:
            warnings.append(f"{where}: 丢弃非白名单字段 {extra}")
        url = handler.get("url")
        if not isinstance(url, str) or not url.strip():
            warnings.append(f"{where}: http.url 为空，跳过")
            return None
        timeout = normalize_timeout(handler.get("timeout"), warnings, where)
        if handler.get("once"):
            warnings.append(f"{where}: http.once 不受支持，已忽略（每个事件都会发）")
        if handler.get("allowedEnvVars"):
            warnings.append(f"{where}: http.allowedEnvVars 不受支持，已忽略")
        headers = handler.get("headers")
        if not isinstance(headers, dict):
            headers = None
        result = {
            "type": "command",
            "command": curl_command(url.strip(), headers, timeout),
            # 宿主在 timeout 秒后 kill 进程；比 curl 自身 --max-time 多留 1s
            "timeout": max(3, timeout + 1),
            "shell": "bash",
        }
        if handler.get("async") is True:
            result["async"] = True
        return result

    warnings.append(f"{where}: 处理器类型 {kind!r} 不受支持，跳过")
    return None


def convert(source: dict) -> tuple[dict, dict]:
    hooks = source.get("hooks")
    report = {
        "kept_events": [],
        "dropped_events": [],
        "handlers_total": 0,
        "http_converted": 0,
        "command_kept": 0,
        "skipped_handlers": 0,
        "warnings": [],
    }
    if not isinstance(hooks, dict):
        return {}, report

    out: dict = {}
    for event, groups in hooks.items():
        if event not in SUPPORTED_EVENTS:
            report["dropped_events"].append(event)
            continue
        if not isinstance(groups, list):
            report["warnings"].append(f"{event}: 不是数组，跳过")
            continue

        kept_groups = []
        for index, group in enumerate(groups):
            if not isinstance(group, dict):
                report["warnings"].append(f"{event}[{index}]: 不是对象，跳过")
                continue
            where = f"{event}[{index}]"
            group_extra = sorted(set(group) - GROUP_FIELDS)
            if group_extra:
                report["warnings"].append(f"{where}: 丢弃组级字段 {group_extra}")

            handlers = group.get("hooks")
            if not isinstance(handlers, list):
                report["warnings"].append(f"{where}: hooks 不是数组，跳过")
                continue

            kept_handlers = []
            for handler_index, handler in enumerate(handlers):
                if not isinstance(handler, dict):
                    report["warnings"].append(f"{where}.hooks[{handler_index}]: 不是对象，跳过")
                    continue
                report["handlers_total"] += 1
                converted = convert_handler(
                    handler, f"{where}.hooks[{handler_index}]", report["warnings"]
                )
                if converted is None:
                    report["skipped_handlers"] += 1
                    continue
                if handler.get("type") == "http":
                    report["http_converted"] += 1
                else:
                    report["command_kept"] += 1
                kept_handlers.append(converted)

            if not kept_handlers:
                continue
            kept_group: dict = {}
            if isinstance(group.get("matcher"), str) and group["matcher"].strip():
                kept_group["matcher"] = group["matcher"].strip()
            if group.get("sequential") is True:
                kept_group["sequential"] = True
            if group.get("async") is True:
                kept_group["async"] = True
            kept_group["hooks"] = kept_handlers
            kept_groups.append(kept_group)

        if kept_groups:
            out[event] = kept_groups
            report["kept_events"].append(event)

    return out, report


def load_jsonc(path: str) -> dict:
    """宿主用 jsonc-parser 读取（允许注释/尾逗号），这里做个宽松解析。"""
    with open(path, "r", encoding="utf-8-sig") as handle:
        raw = handle.read()
    try:
        return json.loads(raw)
    except json.JSONDecodeError:
        stripped = "\n".join(
            line for line in raw.splitlines() if not line.lstrip().startswith("//")
        )
        return json.loads(stripped)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--from", dest="source", default=os.path.expanduser("~/.qoder/settings.json"))
    parser.add_argument("--to", dest="target", default=os.path.expanduser("~/.qwenworkcn/settings.json"))
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()

    if not os.path.isfile(args.source):
        print(f"source not found: {args.source}", file=sys.stderr)
        return 1
    source = load_jsonc(args.source)
    hooks, report = convert(source)

    existing: dict = {}
    if os.path.isfile(args.target):
        try:
            existing = load_jsonc(args.target)
        except Exception:
            existing = {}
    merged = {**existing, "hooks": hooks}

    print(f"source     : {args.source}")
    print(f"target     : {args.target}")
    print(f"kept       : {', '.join(report['kept_events']) or '(none)'}")
    dropped = report["dropped_events"]
    print(f"dropped({len(dropped)}): {', '.join(dropped) or '(none)'}")
    print(
        "handlers   : "
        f"{report['handlers_total']} total, "
        f"{report['http_converted']} http->curl, "
        f"{report['command_kept']} command kept, "
        f"{report['skipped_handlers']} skipped"
    )
    for warning in report["warnings"]:
        print(f"  ! {warning}")

    if args.dry_run:
        print("\n--dry-run, 未写入")
        print(json.dumps({"hooks": hooks}, ensure_ascii=False, indent=2))
        return 0

    os.makedirs(os.path.dirname(args.target), exist_ok=True)
    if os.path.isfile(args.target):
        backup = f"{args.target}.bak-{time.strftime('%Y%m%d-%H%M%S')}"
        shutil.copy2(args.target, backup)
        print(f"backup     : {backup}")
    with open(args.target, "w", encoding="utf-8") as handle:
        json.dump(merged, handle, ensure_ascii=False, indent=2)
        handle.write("\n")
    print(f"written    : {args.target}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
