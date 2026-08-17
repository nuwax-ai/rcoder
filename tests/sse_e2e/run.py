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
RCODER = os.environ.get("RCODER_URL", CFG.get("RCODER_URL", "http://127.0.0.1:8090"))
API_KEY = os.environ.get("LLM_API_KEY", CFG.get("LLM_API_KEY", ""))
BASE_URL = os.environ.get("LLM_BASE_URL", CFG.get("LLM_BASE_URL", ""))
MODEL = os.environ.get("LLM_MODEL", CFG.get("LLM_MODEL", ""))

RUN_TAG = datetime.datetime.now().strftime("%Y%m%d_%H%M%S")
OUT_DIR = RESULTS_ROOT / RUN_TAG
# 每场景独立 user → 独立 agent 容器/进程：场景间彻底隔离
# （同一 user 的 agent 是 prompt 串行的，前一场景未完结的 turn 会被下一场景的 chat cancel）
USER = f"user-pytest-{RUN_TAG}"


def scoped_user(name: str) -> str:
    return f"{USER}-{name}"


def die(msg: str):
    print(f"❌ {msg}")
    sys.exit(1)


def base_payload(prompt: str, request_id: str, user: str = "") -> dict:
    return {
        "user_id": user or USER,
        "prompt": prompt,
        "request_id": request_id,
        "model_provider": {
            "id": MODEL, "name": MODEL,
            "base_url": BASE_URL, "api_key": API_KEY,
            "default_model": MODEL,
            "requires_openai_auth": True, "api_protocol": "openai",
        },
        "system_prompt": "你是集成测试助手。严格按要求输出，不要解释。",
        "agent_config": {
            "agent_server": {
                "agent_id": "nuwaxcode", "command": "nuwaxcode", "args": ["acp"],
                "env": {
                    "OPENAI_API_KEY": "{MODEL_PROVIDER_API_KEY}",
                    "OPENCODE_MODEL": "openai-compatible/{MODEL_PROVIDER_DEFAULT_MODEL}",
                    "OPENAI_BASE_URL": "{MODEL_PROVIDER_BASE_URL}",
                },
            }
        },
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


def ids_of(events: list[dict]) -> list[int]:
    return [e["id"] for e in events if "id" in e]


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

def scenario_full_turn(out: dict):
    """1. chat 后立刻连 SSE：完整轮 + id 单调 + id 行存在。"""
    c = Check()
    user = scoped_user("s1")
    data = chat(base_payload("从1数到6，每行一个数字", f"{RUN_TAG}-s1", user))
    sid = data["session_id"]
    time.sleep(0.8)
    evs = sse_collect(sid, 30)
    types = [e.get("event") for e in evs]
    ids = ids_of(evs)
    c.ok("prompt_start" in types, f"含 prompt_start（事件分布 { {t: types.count(t) for t in set(types)} }）")
    c.ok(types.count("end_turn") >= 1, "含 end_turn（完整轮）")
    c.ok(types.count("agent_message_chunk") >= 1, f"含流式 chunk（{types.count('agent_message_chunk')} 个）")
    c.ok(len(ids) > 0, f"id 行存在（{len(ids)} 个）")
    c.ok(monotonic_unique(ids), f"id 单调无重复（{ids[:5]}...{ids[-3:] if len(ids) > 5 else ''}）")
    text = chunks_text(evs)
    c.ok(any(ch.isdigit() for ch in text), f"回答含数字内容（{text[:30]!r}）")
    out.update(session_id=sid, event_types=sorted(set(types)), ids=ids,
               reply_head=text[:80], turn1_last_seq=max(ids) if ids else None)
    return c


def scenario_after_terminal(out: dict):
    """2. turn 结束后连 SSE：0 事件（终端即清，无本轮残留）。"""
    c = Check()
    user = scoped_user("s2")
    data = chat(base_payload("回答一个字：好", f"{RUN_TAG}-s2", user))
    sid = data["session_id"]
    time.sleep(12)  # 等 turn 完成（终端即清已执行）
    evs = sse_collect(sid, 6)
    c.ok(len(evs) == 0, f"0 事件（实际 {len(evs)}：{[e.get('event') for e in evs][:5]}）")
    out.update(session_id=sid, received=len(evs))
    return c


def scenario_two_turn_isolation(out: dict):
    """3. 第二轮流不含第一轮（seq 隔离）。"""
    c = Check()
    user = scoped_user("s3")
    d1 = chat(base_payload("从1数到4，每行一个数字", f"{RUN_TAG}-s3a", user))
    sid = d1["session_id"]
    time.sleep(0.8)
    evs1 = sse_collect(sid, 30)
    ids1 = ids_of(evs1)
    last1 = max(ids1) if ids1 else 0
    # 第二轮（同 session）
    chat({**base_payload("从10倒数到8，每行一个数字", f"{RUN_TAG}-s3b", user), "session_id": sid})
    time.sleep(0.8)
    evs2 = sse_collect(sid, 30)
    ids2 = ids_of(evs2)
    types2 = [e.get("event") for e in evs2]
    c.ok(ids2 and min(ids2) > last1, f"第二轮 seq 全 > 第一轮最大（{min(ids2) if ids2 else '-'} > {last1}）")
    c.ok(types2.count("prompt_start") == 1, f"恰一个 prompt_start（{types2.count('prompt_start')}）")
    c.ok(types2.count("end_turn") == 1, f"恰一个 end_turn（{types2.count('end_turn')}）")
    out.update(session_id=sid, turn1_last_seq=last1, turn2_ids=ids2)
    return c


def scenario_reconnect_cursor(out: dict):
    """4. turn 进行中断开，带 Last-Event-ID 重连：只收增量。"""
    c = Check()
    user = scoped_user("s4")
    d = chat(base_payload("写一篇600字左右的散文，主题：山间的清晨。直接正文。", f"{RUN_TAG}-s4", user))
    sid = d["session_id"]
    time.sleep(1.2)
    evs1 = sse_collect(sid, 3, idle_stop=False)
    ids1 = ids_of(evs1)
    c.ok(len(ids1) > 0, f"首窗口收到事件（{len(ids1)} 个）")
    last = max(ids1) if ids1 else 0
    evs2 = sse_collect(sid, 20, last_event_id=last)
    ids2 = ids_of(evs2)
    c.ok(len(ids2) > 0, f"重连收到增量事件（{len(ids2)} 个；空=turn 已结束或异常）")
    c.ok(all(i > last for i in ids2), f"增量：重连后 id 全 > {last}（实际 min={min(ids2) if ids2 else '-'}）")
    c.ok(monotonic_unique(ids2), "重连流 id 单调")
    out.update(session_id=sid, cursor=last, after_ids=ids2)
    return c


def scenario_reconnect_no_cursor(out: dict):
    """5. turn 进行中断开，无游标重连：全量重放本轮（含本轮开头）。"""
    c = Check()
    user = scoped_user("s5")
    d = chat(base_payload("写一篇600字左右的散文，主题：海边的黄昏。直接正文。", f"{RUN_TAG}-s5", user))
    sid = d["session_id"]
    time.sleep(1.2)
    evs1 = sse_collect(sid, 3, idle_stop=False)
    ids1 = ids_of(evs1)
    first = min(ids1) if ids1 else None
    c.ok(first is not None, f"首窗口收到事件（{len(ids1)} 个；空=turn 未开始产出，检查 agent 状态）")
    evs2 = sse_collect(sid, 20)
    ids2 = ids_of(evs2)
    # 无游标 = 本轮全量重放：重连流应包含本轮开头（存在 id <= 首窗口最小 id）
    c.ok(any(i <= first for i in ids2), f"重放本轮开头（重连 min={min(ids2) if ids2 else '-'} <= 首窗口 min={first}）")
    c.ok(monotonic_unique(ids2), "重放流 id 单调无重复")
    out.update(session_id=sid, first_window_min=first, replay_ids=ids2)
    return c


SCENARIOS = [
    ("full_turn_delivery", scenario_full_turn),
    ("after_terminal_empty", scenario_after_terminal),
    ("two_turn_isolation", scenario_two_turn_isolation),
    ("reconnect_with_cursor", scenario_reconnect_cursor),
    ("reconnect_no_cursor", scenario_reconnect_no_cursor),
]


def main():
    if not (API_KEY and BASE_URL and MODEL):
        die(f"缺少 LLM 配置：请在 {ENV_LOCAL} 写 LLM_API_KEY/LLM_BASE_URL/LLM_MODEL")
    # 健康检查
    try:
        requests.get(f"{RCODER}/health", timeout=5).raise_for_status()
    except Exception as e:
        die(f"rcoder 不可达（{RCODER}）：{e}")
    OUT_DIR.mkdir(parents=True, exist_ok=True)

    filter_ = sys.argv[1] if len(sys.argv) > 1 else ""
    rows = []
    for name, fn in SCENARIOS:
        if filter_ and filter_ not in name:
            continue
        print(f"\n▶ {name} ...")
        detail = {"scenario": name, "user": USER, "ts": datetime.datetime.now().isoformat()}
        t0 = time.time()
        try:
            check = fn(detail)
        except Exception as e:  # noqa: BLE001
            detail["error"] = str(e)
            print(f"  💥 异常: {e}")
            rows.append((name, False, f"exception: {e}"))
            (OUT_DIR / f"{name}.json").write_text(
                json.dumps(detail, ensure_ascii=False, indent=2, default=str))
            continue
        detail["checks"] = [{"ok": p, "desc": d} for p, d in check.items]
        detail["duration_s"] = round(time.time() - t0, 1)
        (OUT_DIR / f"{name}.json").write_text(
            json.dumps(detail, ensure_ascii=False, indent=2, default=str))
        for ln in check.lines():
            print(ln)
        rows.append((name, check.passed, f"{detail['duration_s']}s"))
        print(f"  {'✅ PASS' if check.passed else '❌ FAIL'}")

    # 汇总
    lines = [f"SSE E2E 集成测试  {RUN_TAG}", f"target: {RCODER}  model: {MODEL}  user: {USER}", ""]
    npass = 0
    for name, ok, extra in rows:
        npass += ok
        lines.append(f"  {'✅' if ok else '❌'} {name:<28} {extra}")
    lines.append("")
    lines.append(f"结果: {npass}/{len(rows)} 场景通过；明细见 {OUT_DIR}")
    summary = "\n".join(lines)
    (OUT_DIR / "summary.txt").write_text(summary)
    print("\n" + summary)
    sys.exit(0 if npass == len(rows) and rows else 1)


if __name__ == "__main__":
    main()
