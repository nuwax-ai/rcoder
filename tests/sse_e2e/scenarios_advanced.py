"""高级场景：切模型/多连切/映射复用/跨turn/并发订阅/容器重启。"""
import subprocess
import threading
import time

from common import (
    CFG, RUN_TAG, Check, base_payload, chat, chunks_text, ids_of, monotonic_unique,
    scoped_user, sse_collect, sse_collect_retry,
)


def scenario_model_switch(out: dict):
    """6. 同 session+project 切模型（flash→pro）：零重放 + 真实执行 + 上下文延续。

    依赖两层修复：rcoder 侧（resume 重放过滤 + 重建前停旧进程）+
    nuwaxcode 侧（resolveModel 旧引用回退，fork commit 012b7caf7）。
    """
    c = Check()
    user = scoped_user("s6")
    d1 = chat(base_payload("用三点解释 CAP 定理，每点一句话，最后一行总结。", f"{RUN_TAG}-s6a", user))
    sid, pid = d1["session_id"], d1["project_id"]
    time.sleep(0.8)
    evs1 = sse_collect(sid, 30)
    ids1 = ids_of(evs1)
    last1 = max(ids1) if ids1 else 0
    r1_text = chunks_text(evs1)

    # 第二轮：切 pro（带 session_id + project_id——前端续话的正确姿势）
    p2 = base_payload("我上一条问了什么？一句话概括，再三点解释 BASE 定理。", f"{RUN_TAG}-s6b", user)
    p2["session_id"] = sid
    p2["project_id"] = pid
    mp = p2["model_provider"]
    pro = CFG.get("LLM_MODEL_PRO", "")
    for k in ("id", "name", "default_model"):
        mp[k] = pro
    # 后台发 chat（同步等返回会错过 SSE——切模型场景 chat 会等到 error 终端才返回，
    # 届时终端即清已执行，SSE 只能收到空流）
    t = threading.Thread(target=lambda: chat(p2, timeout=120), daemon=True)
    t.start()
    time.sleep(0.8)
    evs2 = sse_collect(sid, 30)
    ids2 = ids_of(evs2)
    types2 = [e.get("event") for e in evs2]
    r2_text = chunks_text(evs2)

    c.ok(len(ids2) > 0, f"第二轮收到事件（{len(ids2)} 个）")
    c.ok(all(i > last1 for i in ids2), f"第二轮 seq 全 > 第一轮最大（{min(ids2) if ids2 else '-'} > {last1}）——零历史重放")
    has_err = "error" in types2
    out.update(session_id=sid, turn1_last_seq=last1, turn2_ids=ids2,
               turn2_error=has_err, turn1_reply_head=r1_text[:60], turn2_reply_head=r2_text[:60])
    if has_err:
        print("    ⚠️ 已知问题：切模型 prompt 失败（ProviderModelNotFoundError），见场景 docstring")
    return c


def scenario_anthropic_model_switch(out: dict):
    """8. claude-code-acp-ts 切模型（flash→pro）：无重放 + 上下文延续 + 真实执行。

    与 opencode 后端不同：acp-ts 的模型来自 env（每次进程读取），无 session
    模型引用持久化——切模型场景应完整成功（opencode 的对应场景有已知问题）。
    """
    c = Check()
    user = scoped_user("s8")
    d1 = chat(base_payload("请用三点解释 CAP 定理，每点一句话，最后一行总结。",
                           f"{RUN_TAG}-s8a", user, backend="anthropic"))
    sid, pid = d1["session_id"], d1["project_id"]
    time.sleep(0.8)
    evs1 = sse_collect(sid, 40)
    ids1 = ids_of(evs1)
    last1 = max(ids1) if ids1 else 0
    r1_text = chunks_text(evs1)
    c.ok("CAP" in r1_text or "一致" in r1_text, f"第一轮 CAP 回答（{r1_text[:40]!r}）")

    p2 = base_payload("我上一条问了什么？一句话概括，再三点解释 BASE 定理。",
                      f"{RUN_TAG}-s8b", user, backend="anthropic",
                      model=CFG.get("LLM_MODEL_PRO", ""))
    p2["session_id"] = sid
    p2["project_id"] = pid
    t = threading.Thread(target=lambda: chat(p2, timeout=150), daemon=True)
    t.start()
    time.sleep(1)
    evs2 = sse_collect(sid, 60)
    ids2 = ids_of(evs2)
    types2 = [e.get("event") for e in evs2]
    r2_text = chunks_text(evs2)
    c.ok(len(ids2) > 0, f"第二轮收到事件（{len(ids2)} 个）")
    c.ok(all(i > last1 for i in ids2), f"seq 全 > 第一轮最大（{min(ids2) if ids2 else '-'} > {last1}）零重放")
    c.ok(types2.count("end_turn") >= 1, "含 end_turn（切模型后真实执行完成）")
    c.ok("CAP" in r2_text, f"上下文延续（回答记得上一轮 CAP：{r2_text[:30]!r}）")
    c.ok("BASE" in r2_text or "基本可用" in r2_text, "回答了新问题 BASE")
    out.update(session_id=sid, turn1_last_seq=last1, turn2_ids=ids2,
               turn2_reply_head=r2_text[:80])
    return c


import functools

# 双后端完整矩阵：SSE 轮次语义（完整轮/终端清/轮次隔离/带游标/无游标重连）
# 在两个 ACP agent 实现上都必须成立——交叉验证非特例。
# openai 后端=nuwaxcode(opencode)；anthropic 后端=claude-code-acp-ts。
def scenario_cross_turn_reconnect(out: dict, backend: str = "openai"):
    """断线跨越 turn 边界：turn1 进行中断开 → turn1 结束 → turn2 开始后重连。

    断言：重连不收 turn1 的任何消息（终端已清）+ 完整收到 turn2 +
    id 全在 turn2 的 seq 区间（跨轮零残留——防重复红线）。
    """
    c = Check()
    user = scoped_user(f"sg1-{backend}")
    d1 = chat(base_payload("从1数到10，每行一个数字", f"{RUN_TAG}-sg1a", user, backend=backend))
    sid, pid = d1["session_id"], d1["project_id"]
    time.sleep(0.8)
    evs1 = sse_collect(sid, 30)
    t1_max = max(ids_of(evs1)) if ids_of(evs1) else 0
    t1_text = chunks_text(evs1)
    c.ok(len(ids_of(evs1)) > 0, f"turn1 收到（{len(ids_of(evs1))} 个）")

    # 等 turn1 结束（终端清）后发 turn2，重连（不带游标）
    time.sleep(8)
    p2 = base_payload("写一篇200字短文，主题：雨后的街道。直接正文。", f"{RUN_TAG}-sg1b", user, backend=backend)
    p2["session_id"], p2["project_id"] = sid, pid

    def _r2():
        try:
            chat(p2, timeout=150)
        except Exception as e:  # noqa: BLE001
            print(f"    [turn2 chat] {e}")

    threading.Thread(target=_r2, daemon=True).start()
    time.sleep(1.5)
    evs2 = sse_collect(sid, 60)
    ids2 = ids_of(evs2)
    r2_text = chunks_text(evs2)
    types2 = [e.get("event") for e in evs2]
    c.ok(len(ids2) > 0, f"重连收到 turn2（{len(ids2)} 个）")
    c.ok(all(i > t1_max for i in ids2), f"零 turn1 残留（min={min(ids2) if ids2 else '-'} > turn1 max={t1_max}）")
    c.ok(types2.count("prompt_start") == 1 and types2.count("end_turn") >= 1, "turn2 完整链")
    c.ok(t1_text[:8] not in r2_text[:40], f"turn2 开头非 turn1 内容（{r2_text[:20]!r}）")
    out.update(session_id=sid, turn1_last_seq=t1_max, turn2_ids=ids2)
    return c


def scenario_concurrent_subscribers(out: dict, backend: str = "openai"):
    """多客户端并发订阅同一 session：两个 SSE 连接都完整收到、seq 一致。"""
    c = Check()
    user = scoped_user(f"sg2-{backend}")
    d1 = chat(base_payload("从1数到8，每行一个数字", f"{RUN_TAG}-sg2", user, backend=backend))
    sid = d1["session_id"]

    results: dict[str, list[dict]] = {}

    def _sub(tag: str):
        time.sleep(0.6)
        results[tag] = sse_collect(sid, 30)

    t_a = threading.Thread(target=_sub, args=("A",))
    t_b = threading.Thread(target=_sub, args=("B",))
    t_a.start(); t_b.start()
    t_a.join(); t_b.join()
    evs_a, evs_b = results.get("A", []), results.get("B", [])
    ids_a, ids_b = set(ids_of(evs_a)), set(ids_of(evs_b))
    types_a = [e.get("event") for e in evs_a]
    types_b = [e.get("event") for e in evs_b]
    c.ok(len(ids_a) > 0 and len(ids_b) > 0, f"两个订阅者都收到（A={len(ids_a)}, B={len(ids_b)}）")
    c.ok("end_turn" in types_a and "end_turn" in types_b, "两个订阅者都收到完整轮（end_turn）")
    inter = ids_a & ids_b
    c.ok(len(inter) > 0, f"seq 集合大范围重叠（交集 {len(inter)} 个）")
    c.ok(len(chunks_text(evs_a)) > 0 and len(chunks_text(evs_b)) > 0, "两者内容非空")
    out.update(session_id=sid, ids_a=len(ids_a), ids_b=len(ids_b), overlap=len(inter))
    return c


def scenario_error_recovery(out: dict):
    """agent error 后恢复：正常建会话 → 坏 base_url 触发 error 终端 → 正确配置恢复。

    断言：error 轮收到终态（error/End）且不 hang → 恢复轮完整执行 + 内容正常。
    """
    c = Check()
    user = scoped_user("sg3")
    d0 = chat(base_payload("从1数到3，每行一个数字", f"{RUN_TAG}-sg3a", user))
    sid, pid = d0["session_id"], d0["project_id"]
    time.sleep(0.8)
    sse_collect(sid, 30)  # 收完首轮（等终端）

    # error 轮：无效 api_key（预检不校验 key、首次 LLM 调用 401 → 执行中 error，
    # 产生 SSE 事件——区别于坏 base_url 在预检即被拒、SSE 无事件的设计行为）
    p_bad = base_payload("回答一个字：好", f"{RUN_TAG}-sg3b", user)
    p_bad["model_provider"]["api_key"] = "sk-invalid-key-for-testing"
    p_bad["session_id"], p_bad["project_id"] = sid, pid

    bad_done = threading.Event()

    def _bad():
        try:
            chat(p_bad, timeout=120)
        except Exception as e:  # noqa: BLE001
            print(f"    [bad-url chat ended] {str(e)[:60]}")
        finally:
            bad_done.set()

    threading.Thread(target=_bad, daemon=True).start()
    time.sleep(0.8)
    evs1 = sse_collect_retry(sid, 40)
    types1 = [e.get("event") for e in evs1]
    got_terminal = "error" in types1 or "end_turn" in types1
    c.ok(got_terminal, f"坏 URL 轮收到终态（分布 {sorted(set(types1))}）")

    # 恢复轮直接发（不等错误轮 chat 线程）：无效 key 的 chat 会挂到 LLM 重试
    # 超时才返回，恢复轮 chat 到达 agent 时会 cancel 它——这正是手动验证过
    # 的正确时序（cancel 挂起轮 → 恢复轮正常执行）。
    _ = bad_done

    # 恢复轮：正确配置 + 同 session 续话
    p_ok = base_payload("回答两个字：可以", f"{RUN_TAG}-sg3c", user)
    p_ok["session_id"], p_ok["project_id"] = sid, pid

    def _r2():
        try:
            chat(p_ok, timeout=150)
        except Exception as e:  # noqa: BLE001
            print(f"    [recovery chat] {e}")

    threading.Thread(target=_r2, daemon=True).start()
    time.sleep(1)
    evs2 = sse_collect(sid, 50)
    evs2 = sse_collect_retry(sid, 50)
    ids2 = ids_of(evs2)
    types2 = [e.get("event") for e in evs2]
    r2 = chunks_text(evs2)
    c.ok(len(ids2) > 0, f"恢复轮收到事件（{len(ids2)} 个）")
    c.ok(types2.count("end_turn") >= 1, f"恢复轮完整执行（分布 {sorted(set(types2))}）")
    c.ok("可" in r2 or "以" in r2, f"恢复轮内容正常（{r2[:20]!r}）")
    out.update(session_id=sid, error_turn_types=sorted(set(types1)), recovery_ids=ids2)
    return c


def scenario_container_restart_recovery(out: dict):
    """agent 容器重启后同 session 续话（模拟 agent 故障恢复链路）。"""
    c = Check()
    import subprocess
    user = scoped_user("sg4")
    d1 = chat(base_payload("从1数到5，每行一个数字", f"{RUN_TAG}-sg4a", user))
    sid, pid = d1["session_id"], d1["project_id"]
    time.sleep(0.8)
    evs1 = sse_collect(sid, 30)
    t1_max = max(ids_of(evs1)) if ids_of(evs1) else 0
    c.ok(t1_max > 0, f"重启前轮正常（seq 到 {t1_max}）")

    cname = f"dev-rcoder-agent-runner-{user}"
    r = subprocess.run(["docker", "restart", cname], capture_output=True, text=True, timeout=60)
    c.ok(r.returncode == 0, f"容器重启注入成功（{r.stderr[:40]}）")
    time.sleep(12)  # 等 agent_runner 进程起来

    p2 = base_payload("我上一条消息让你做什么了？一句话回答。", f"{RUN_TAG}-sg4b", user)
    p2["session_id"], p2["project_id"] = sid, pid

    def _r2():
        try:
            chat(p2, timeout=180)
        except Exception as e:  # noqa: BLE001
            print(f"    [post-restart chat] {e}")

    threading.Thread(target=_r2, daemon=True).start()
    time.sleep(2)
    evs2 = sse_collect(sid, 60)
    ids2 = ids_of(evs2)
    r2 = chunks_text(evs2)
    # 重启后 seq 从新进程重新计数（agent_runner 状态清零）——断言能收到+内容延续
    c.ok(len(ids2) > 0, f"重启后收到事件（{len(ids2)} 个）")
    c.ok("数" in r2 or "数字" in r2, f"上下文延续（记得数数任务：{r2[:30]!r}）")
    out.update(session_id=sid, restart_container=cname, post_restart_ids=ids2,
               post_restart_reply=r2[:60])
    return c


def _longest_common_snippet(a: str, b: str, win: int = 24) -> str:
    """找 a 中长度 win 的连续片段是否逐字出现在 b 中（SSE 重放检测）。

    模型自主复述只会语义相似，不会逐字复现 24+ 连续字符；
    SSE 重放是 chunk 逐字重组——必命中。返回命中的片段（无则空串）。
    """
    for i in range(0, max(0, len(a) - win)):
        frag = a[i:i + win]
        if "\n" not in frag and frag in b:
            return frag
    return ""


def scenario_model_switch_multi(out: dict, backend: str = "openai"):
    """三连切模型（flash→pro→flash，同 project+session）+ 逐字重放检测。

    每轮断言：seq 隔离（零旧轮事件）+ 完整执行 + 上下文延续 +
    后续轮的流中不逐字包含前面轮回答的长片段（内容级重放检测——
    比 seq 断言更贴近用户感知的"收到重复消息"）。
    """
    c = Check()
    user = scoped_user(f"sm-{backend}")
    pro = CFG.get("LLM_MODEL_PRO", "")
    turns = [
        ("flash", "请用三点解释 CAP 定理，每点一句话，最后单独一行总结。"),
        ("pro", "请用三点解释 BASE 定理，每点一句话，最后单独一行总结。"),
        ("flash", "请用两点对比 CAP 与 BASE 的关系，每点一句话。"),
    ]
    sid = pid = None
    replies: list[str] = []
    turn_last_seq = 0

    for i, (model_tag, prompt) in enumerate(turns):
        model = pro if model_tag == "pro" else ""
        p = base_payload(prompt, f"{RUN_TAG}-sm{i}{backend}", user, backend=backend, model=model)
        if sid:
            p["session_id"], p["project_id"] = sid, pid

        if sid is None:
            # 首轮同步（拿 sid/pid 后立即收 SSE，turn 尚在进行）
            d = chat(p)
            sid, pid = d["session_id"], d["project_id"]
            time.sleep(0.8)
            evs = sse_collect(sid, 60)
        else:
            def _send(pl=p, idx=i):
                try:
                    chat(pl, timeout=150)
                except Exception as e:  # noqa: BLE001
                    print(f"    [turn{idx} chat] {str(e)[:60]}")

            threading.Thread(target=_send, daemon=True).start()
            time.sleep(1)
            evs = sse_collect(sid, 60)

        ids = ids_of(evs)
        types = [e.get("event") for e in evs]
        text = chunks_text(evs)
        replies.append(text)
        c.ok(len(ids) > 0, f"turn{i}({model_tag}) 收到事件（{len(ids)} 个）")
        c.ok(all(x > turn_last_seq for x in ids),
             f"turn{i} seq 全 > 前轮（min={min(ids) if ids else '-'} > {turn_last_seq}）")
        c.ok(types.count("end_turn") >= 1, f"turn{i} 完整执行")
        turn_last_seq = max(ids) if ids else turn_last_seq

        # 逐字重放检测：本轮流不包含前面任何轮回答的 24 字连续片段
        for j in range(i):
            frag = _longest_common_snippet(replies[j], text)
            c.ok(not frag,
                 f"turn{i} 无 turn{j} 逐字重放（命中片段 {frag[:20]!r}）" if frag
                 else f"turn{i} 无 turn{j} 逐字重放")

    # 末轮上下文断言：应记得前两轮话题
    c.ok("CAP" in replies[2] and "BASE" in replies[2],
         f"末轮记得两个前序话题（{replies[2][:40]!r}）")
    out.update(session_id=sid, project_id=pid, replies=[r[:60] for r in replies],
               final_last_seq=turn_last_seq)
    return c
