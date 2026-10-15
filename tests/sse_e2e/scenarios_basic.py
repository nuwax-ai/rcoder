"""基础 SSE 语义场景（双后端参数化）：完整轮/终端清空/轮次隔离/游标重连。"""
import threading
import time

from common import (
    META_EVENTS, RUN_TAG, Check, base_payload, chat, chunks_text, ids_of, message_events,
    monotonic_unique, scoped_user, sse_collect,
)


def scenario_full_turn(out: dict, backend: str = "openai"):
    """1. chat 后立刻连 SSE：完整轮 + id 单调 + id 行存在。"""
    c = Check()
    user = scoped_user(f"s1-{backend}")
    data = chat(base_payload("从1数到6，每行一个数字", f"{RUN_TAG}-s1", user, backend=backend))
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


def scenario_after_terminal(out: dict, backend: str = "openai"):
    """2. turn 结束后连 SSE：0 事件（终端即清，无本轮残留）。"""
    c = Check()
    user = scoped_user(f"s2-{backend}")
    data = chat(base_payload("回答一个字：好", f"{RUN_TAG}-s2", user, backend=backend))
    sid = data["session_id"]
    time.sleep(12)  # 等 turn 完成（终端即清已执行）
    evs = sse_collect(sid, 6)
    msgs = message_events(evs)
    c.ok(len(msgs) == 0, f"0 消息事件（实际 {len(msgs)}：{[e.get('event') for e in msgs][:5]}；"
        f"元事件 {[e.get('event') for e in evs if e.get('event') in META_EVENTS]} 不计）")
    out.update(session_id=sid, received=len(evs))
    return c


def scenario_two_turn_isolation(out: dict, backend: str = "openai"):
    """3. 第二轮流不含第一轮（seq 隔离）。"""
    c = Check()
    user = scoped_user(f"s3-{backend}")
    d1 = chat(base_payload("从1数到4，每行一个数字", f"{RUN_TAG}-s3a", user, backend=backend))
    sid, pid = d1["session_id"], d1["project_id"]
    time.sleep(0.8)
    evs1 = sse_collect(sid, 30)
    ids1 = ids_of(evs1)
    last1 = max(ids1) if ids1 else 0
    # 第二轮（同 session，后台发——同步等待会错过 SSE 窗口；任务要够长，
    # acp-ts 后端短任务（倒数 3 个数）在 0.8s 连接前就终结清空了）
    def _round2():
        try:
            # 续话必须同时带 session_id + project_id：acp-ts(claude-code) 的 session
            # 存储按 cwd（project 目录）——不带 project_id 会生成新目录，跨目录
            # resume 必然 Resource not found → 裸新建（丢上下文+换 session id）。
            chat({**base_payload("写一篇300字左右的短文，主题：城市的夜晚。直接正文。",
                                f"{RUN_TAG}-s3b", user, backend=backend),
                  "session_id": sid, "project_id": pid})
        except Exception as e:  # noqa: BLE001
            print(f"    [round2 chat] {e}")
    threading.Thread(target=_round2, daemon=True).start()
    time.sleep(0.8)
    evs2 = sse_collect(sid, 30)
    ids2 = ids_of(evs2)
    types2 = [e.get("event") for e in evs2]
    c.ok(ids2 and min(ids2) > last1, f"第二轮 seq 全 > 第一轮最大（{min(ids2) if ids2 else '-'} > {last1}）")
    c.ok(types2.count("prompt_start") == 1, f"恰一个 prompt_start（{types2.count('prompt_start')}）")
    c.ok(types2.count("end_turn") == 1, f"恰一个 end_turn（{types2.count('end_turn')}）")
    out.update(session_id=sid, turn1_last_seq=last1, turn2_ids=ids2)
    return c


def scenario_reconnect_cursor(out: dict, backend: str = "openai"):
    """4. turn 进行中断开，带 Last-Event-ID 重连：只收增量。"""
    c = Check()
    user = scoped_user(f"s4-{backend}")
    d = chat(base_payload("写一篇600字左右的散文，主题：山间的清晨。直接正文。", f"{RUN_TAG}-s4", user, backend=backend))
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


def scenario_reconnect_no_cursor(out: dict, backend: str = "openai"):
    """5. turn 进行中断开，无游标重连：纯实时（零已收消息重放——防重复红线）。

    首连已消费（首窗口 seq 1..N）→ 无游标重连不得再收到 <= N 的任何事件
    （首连资格已被占：重连只收实时流）。区别于带游标场景的增量补齐。"""
    c = Check()
    user = scoped_user(f"s5-{backend}")
    d = chat(base_payload("写一篇600字左右的散文，主题：海边的黄昏。直接正文。", f"{RUN_TAG}-s5", user, backend=backend))
    sid = d["session_id"]
    time.sleep(1.2)
    evs1 = sse_collect(sid, 3, idle_stop=False)
    ids1 = ids_of(evs1)
    last1 = max(ids1) if ids1 else None
    c.ok(last1 is not None, f"首窗口收到事件（{len(ids1)} 个，到 seq={last1}）")
    evs2 = sse_collect(sid, 20)
    ids2 = ids_of(evs2)
    # 无游标重连 = 纯实时：不得出现首窗口已收的任何 seq
    c.ok(all(i > last1 for i in ids2),
         f"零已收消息重放（重连收到 {len(ids2)} 个全 > {last1}，min={min(ids2) if ids2 else '-'}）")
    c.ok(monotonic_unique(ids2), "重连流 id 单调无重复")
    out.update(session_id=sid, first_window_last=last1, reconnect_ids=ids2)
    return c


def scenario_no_session_reuse(out: dict, backend: str = "openai"):
    """前端标准姿势：第二轮不带 session_id，只带 project_id——rcoder 内部
    按 project→session 映射自动复用（resolve_forward_request）。

    断言三件事：①响应返回的 session_id 与第一轮相同（复用非新建）
    ②SSE 正常收到第二轮 ③上下文延续（模型记得第一轮）。
    """
    c = Check()
    user = scoped_user(f"s9-{backend}")
    d1 = chat(base_payload("请用三点解释 CAP 定理，每点一句话。", f"{RUN_TAG}-s9a", user, backend=backend))
    sid, pid = d1["session_id"], d1["project_id"]
    time.sleep(0.8)
    evs1 = sse_collect(sid, 40)
    last1 = max(ids_of(evs1)) if ids_of(evs1) else 0

    # 第二轮：只带 user_id + project_id（不带 session_id —— 前端标准续话姿势）
    p2 = base_payload("我上一条消息问了什么？一句话概括，再写一行总结。", f"{RUN_TAG}-s9b", user, backend=backend)
    p2["project_id"] = pid
    assert "session_id" not in p2, "second round must NOT carry session_id"
    r2_holder = {}

    def _round2():
        try:
            r2_holder["data"] = chat(p2, timeout=150)
        except Exception as e:  # noqa: BLE001
            r2_holder["error"] = str(e)

    threading.Thread(target=_round2, daemon=True).start()
    time.sleep(1)
    evs2 = sse_collect(sid, 60)
    ids2 = ids_of(evs2)
    r2_text = chunks_text(evs2)
    types2 = [e.get("event") for e in evs2]

    d2 = r2_holder.get("data")
    c.ok(d2 is not None, f"第二轮 chat 成功（{r2_holder.get('error', 'ok')[:60]}）")
    if d2:
        c.ok(d2["session_id"] == sid,
             f"session_id 复用（响应 {d2['session_id'][:18]}.. == 第一轮 {sid[:18]}..）")
    c.ok(len(ids2) > 0, f"SSE 收到第二轮事件（{len(ids2)} 个）")
    c.ok(all(i > last1 for i in ids2), f"seq 延续全 > 第一轮（{min(ids2) if ids2 else '-'} > {last1}）")
    c.ok(types2.count("end_turn") >= 1, "第二轮完整执行")
    c.ok("CAP" in r2_text, f"上下文延续（记得 CAP：{r2_text[:30]!r}）")
    out.update(session_id=sid, project_id=pid, turn1_last_seq=last1,
               turn2_ids=ids2, reused_sid=(d2 or {}).get("session_id"),
               turn2_reply_head=r2_text[:60])
    return c
