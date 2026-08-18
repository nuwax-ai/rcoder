#!/usr/bin/env python3
"""SSE 消息链路集成测试（黑盒，走真实 HTTP 接口 + 真实 LLM）。

覆盖接口：
  POST /computer/chat                对话（异步创建，返回 session_id）
  GET  /computer/progress/{sid}      SSE 消息流

场景语义矩阵（对应 SSE 轮次语义设计）：
  1. full_turn_delivery      chat 后立刻连 SSE（turn 进行中）→ 完整轮 + id 单调
  2. after_terminal_empty    turn 结束后连 SSE → 0 事件（终端即清）
  3. two_turn_isolation      第二轮流不含第一轮（seq 隔离）
  4. reconnect_with_cursor   断线带 Last-Event-ID 重连 → 只收增量
  5. reconnect_no_cursor     断线无游标重连 → 全量重放本轮（含本轮开头）

用法：
  python3 tests/sse_e2e/run.py                 # 全场景
  python3 tests/sse_e2e/run.py full_turn       # 按名过滤（子串匹配）

配置（.env.local，gitignore 保护）：
  LLM_API_KEY / LLM_BASE_URL / LLM_MODEL
  可选 RCODER_URL（默认 http://127.0.0.1:8090）

结果输出：tests/sse_e2e/results/<时间戳>/（summary.txt + 每场景 json）
"""

import json
import os
import re
import sys
import time
import datetime
import threading
from pathlib import Path

import requests

HERE = Path(__file__).parent
RESULTS_ROOT = HERE / "results"
ENV_LOCAL = HERE.parent.parent / ".env.local"


def load_env_local():
    cfg = {}
    if ENV_LOCAL.exists():
        for line in ENV_LOCAL.read_text().splitlines():
            line = line.strip()
            if line and not line.startswith("#") and "=" in line:
                k, v = line.split("=", 1)
                cfg[k.strip()] = v.strip()
    return cfg


CFG = load_env_local()
RCODER_URL_ENV = os.environ.get("RCODER_URL", "")
RCODER = RCODER_URL_ENV or CFG.get("RCODER_URL", "http://127.0.0.1:8090")
API_KEY = os.environ.get("LLM_API_KEY", CFG.get("LLM_API_KEY", ""))
BASE_URL = os.environ.get("LLM_BASE_URL", CFG.get("LLM_BASE_URL", ""))
MODEL = os.environ.get("LLM_MODEL", CFG.get("LLM_MODEL", ""))
# K8s 模式（远端 rcoder）：TEST_K8S_SSH=user@host 时清理走远程 kubectl 删 STS/svc/PVC
K8S_SSH = os.environ.get("TEST_K8S_SSH", "")
K8S_NS = os.environ.get("TEST_K8S_NS", "nuwax-k8s-test")

RUN_TAG = datetime.datetime.now().strftime("%Y%m%d_%H%M%S")
OUT_DIR = RESULTS_ROOT / RUN_TAG
# 每场景独立 user → 独立 agent 容器/进程：场景间彻底隔离
# （同一 user 的 agent 是 prompt 串行的，前一场景未完结的 turn 会被下一场景的 chat cancel）
# user 名长度 ≤23：K8s 模式资源名 = rcoder-computer-agent-runner-<user>[-headless] 须 ≤63 字符
# （Docker 模式无此限制，统一短名不影响）；HHMMSS 保证同日多次运行不撞名
USER = f"ue{RUN_TAG.split('_')[-1]}"


def scoped_user(name: str) -> str:
    return f"{USER}-{name}"


def die(msg: str):
    print(f"❌ {msg}")
    sys.exit(1)


def base_payload(prompt: str, request_id: str, user: str = "", backend: str = "openai",
                 model: str = "") -> dict:
    """构造 chat payload。backend: openai=nuwaxcode(opencode) | anthropic=claude-code-acp-ts。"""
    m = model or MODEL
    if backend == "anthropic":
        provider = {
            "id": m, "name": m,
            "base_url": CFG.get("LLM_BASE_URL_ANTHROPIC", ""),
            "api_key": API_KEY, "default_model": m,
            "requires_openai_auth": True, "api_protocol": "anthropic",
        }
        server = {
            "agent_id": "claude-code-acp-ts", "command": "claude-code-acp-ts", "args": [],
            "env": {
                "ANTHROPIC_API_KEY": "{MODEL_PROVIDER_API_KEY}",
                "ANTHROPIC_MODEL": "{MODEL_PROVIDER_DEFAULT_MODEL}",
                "ANTHROPIC_BASE_URL": "{MODEL_PROVIDER_BASE_URL}",
            },
        }
    else:
        provider = {
            "id": m, "name": m,
            "base_url": BASE_URL, "api_key": API_KEY, "default_model": m,
            "requires_openai_auth": True, "api_protocol": "openai",
        }
        server = {
            "agent_id": "nuwaxcode", "command": "nuwaxcode", "args": ["acp"],
            "env": {
                "OPENAI_API_KEY": "{MODEL_PROVIDER_API_KEY}",
                "OPENCODE_MODEL": "openai-compatible/{MODEL_PROVIDER_DEFAULT_MODEL}",
                "OPENAI_BASE_URL": "{MODEL_PROVIDER_BASE_URL}",
            },
        }
    return {
        "user_id": user or USER,
        "prompt": prompt,
        "request_id": request_id,
        "model_provider": provider,
        "system_prompt": "你是集成测试助手。严格按要求输出，不要解释。",
        "agent_config": {"agent_server": server},
    }


def chat(payload: dict, timeout: float = 180.0) -> dict:
    r = requests.post(f"{RCODER}/computer/chat", json=payload, timeout=timeout)
    r.raise_for_status()
    body = r.json()
    if not body.get("success"):
        raise RuntimeError(f"chat failed: {body}")
    return body["data"]


def sse_collect(session_id: str, duration: float, last_event_id: str | None = None,
                idle_stop: bool = True) -> list[dict]:
    """收 SSE 流：解析 event:/id:/data: 三类行；keep-alive 忽略。

    idle_stop=True 时收到 end_turn 事件后再多收 1s 即提前返回（加速场景）。
    """
    headers = {"Accept": "text/event-stream"}
    if last_event_id is not None:
        headers["Last-Event-ID"] = str(last_event_id)
    events: list[dict] = []
    cur = {}
    end_seen_at = None
    deadline = time.time() + duration
    try:
        with requests.get(f"{RCODER}/computer/progress/{session_id}",
                          headers=headers, stream=True, timeout=(5, duration + 5)) as r:
            if r.status_code != 200:
                raise RuntimeError(f"SSE HTTP {r.status_code}: {r.text[:200]}")
            # 响应头无 charset 时 requests 默认 iso-8859-1 解码 → 中文 mojibake；
            # SSE data 是 UTF-8 JSON，强制按 UTF-8 解。
            r.encoding = "utf-8"
            for raw in r.iter_lines(decode_unicode=True):
                if time.time() > deadline:
                    break
                if raw is None:
                    continue
                line = raw.strip()
                if line.startswith("event:"):
                    cur = {"event": line[6:].strip()}
                elif line.startswith("id:"):
                    cur["id"] = int(line[3:].strip())
                elif line.startswith("data:"):
                    cur["data"] = line[5:].strip()
                    if cur.get("event"):
                        events.append(cur)
                    if cur.get("event") == "end_turn" and idle_stop:
                        if end_seen_at is None:
                            end_seen_at = time.time()
                        if time.time() - end_seen_at > 1:
                            break
                elif line.startswith(":") or not line:
                    continue
    except requests.exceptions.RequestException as e:
        print(f"    [sse] connection ended: {type(e).__name__}: {str(e)[:80]}")
    return events


# 连接元事件（非对话消息）：acp-ts 后端在 SSE 建立时会话信息通知。
# "终端清空"断言的对象是对话消息，元事件不计入。
META_EVENTS = {"session_info_update"}


def sse_collect_retry(sid: str, primary: float, retry: float = 30.0) -> list[dict]:
    """sse_collect + 空结果重试一次。全量套跑靠后的场景受容器创建销毁累积
    负载影响，agent 冷启动可能超过单窗口——空流重试覆盖（单跑/手动不受影响）。"""
    evs = sse_collect(sid, primary)
    if not ids_of(evs) and not any(e.get("event") not in META_EVENTS | {"ping"} for e in evs):
        print(f"    [sse] empty window ({primary}s), retrying with {retry}s...")
        evs = sse_collect(sid, retry)
    return evs


def ids_of(events: list[dict]) -> list[int]:
    return [e["id"] for e in events if "id" in e]


def message_events(events: list[dict]) -> list[dict]:
    return [e for e in events if e.get("event") not in META_EVENTS]


def chunks_text(events: list[dict]) -> str:
    out = []
    for e in events:
        if e.get("event") == "agent_message_chunk" and e.get("data"):
            try:
                out.append(json.loads(e["data"]).get("data", {}).get("content", {}).get("text", ""))
            except json.JSONDecodeError:
                pass
    return "".join(out)


# ---------------- 断言辅助 ----------------

class Check:
    def __init__(self):
        self.items: list[tuple[bool, str]] = []

    def ok(self, cond, desc):
        self.items.append((bool(cond), desc))
        return bool(cond)

    @property
    def passed(self):
        return all(p for p, _ in self.items)

    def lines(self):
        return [("  ✅ " if p else "  ❌ ") + d for p, d in self.items]


def monotonic_unique(ids: list[int]) -> bool:
    return all(b > a for a, b in zip(ids, ids[1:])) and len(set(ids)) == len(ids)


# ---------------- 场景 ----------------
