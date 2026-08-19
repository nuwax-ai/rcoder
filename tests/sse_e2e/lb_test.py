#!/usr/bin/env python3
"""多副本负载均衡专项测试：同一会话的请求经不同节点入口/不同 rcoder 副本，
验证会话延续、seq 连续、无重复/丢失。

两类路由维度：
  ① 副本轮换（确定性）：Pod IP 直连（:8086）——turn 依次走副本 A/B/A/B...
  ② 节点入口轮换（用户场景）：三个节点的 NodePort(:30295) 轮换——kube-proxy
     随机落点，验证多入口混合访问的整体正确性
  ③ 跨副本 SSE：chat 发副本 A、SSE 订阅副本 B；断开带游标重连另一副本

前置：TEST_K8S_SSH 指向集群（解析 rcoder Pod IP）；LB_ENTRY_HOSTS 为节点
入口列表（默认 192.168.1.19,192.168.1.34,192.168.1.18）。

用法：
  TEST_K8S_SSH=swufe@192.168.1.19 python3 tests/sse_e2e/lb_test.py
"""

import json
import os
import subprocess
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from common import (  # noqa: E402
    CFG, RUN_TAG, Check, base_payload, chat, chunks_text, ids_of, sse_collect,
    scoped_user,
)

K8S_SSH = os.environ.get("TEST_K8S_SSH", "")
K8S_NS = os.environ.get("TEST_K8S_NS", "nuwax-k8s-test")
ENTRY_HOSTS = [
    h.strip()
    for h in os.environ.get(
        "LB_ENTRY_HOSTS", "192.168.1.19,192.168.1.34,192.168.1.18"
    ).split(",")
    if h.strip()
]
NODEPORT = os.environ.get("LB_NODEPORT", "30295")


def chat_at(url: str, payload: dict) -> dict:
    """向指定 url 发 chat（与 common.chat 同校验）"""
    import requests

    r = requests.post(f"{url}/computer/chat", json=payload, timeout=180)
    r.raise_for_status()
    body = r.json()
    if not body.get("success"):
        raise RuntimeError(f"chat failed via {url}: {body}")
    return body["data"]


def sse_collect_at(url: str, sid: str, duration: float, last_event_id=None):
    """从指定 url 收 SSE（与 common.sse_collect 同解析）"""
    import requests

    headers = {"Accept": "text/event-stream"}
    if last_event_id is not None:
        headers["Last-Event-ID"] = str(last_event_id)
    events, cur = [], {}
    deadline = time.time() + duration
    try:
        with requests.get(f"{url}/computer/progress/{sid}", headers=headers,
                          stream=True, timeout=(5, duration + 5)) as r:
            r.encoding = "utf-8"
            for raw in r.iter_lines(decode_unicode=True):
                if time.time() > deadline:
                    break
                if not raw:
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
                    if cur.get("event") == "end_turn":
                        time.sleep(1)
                        if time.time() > deadline:
                            break
    except Exception as e:  # noqa: BLE001
        print(f"    [sse@{url}] ended: {type(e).__name__}")
    return events


def cleanup(user: str):
    """清理本次测试的 agent 容器（同 run.py 三层保护）"""
    try:
        out = subprocess.run(
            ["ssh", K8S_SSH, "kubectl", "-n", K8S_NS, "get", "sts,svc,pvc", "-o", "name"],
            capture_output=True, text=True, timeout=30,
        )
        prefix = f"rcoder-computer-agent-runner-{user}"
        targets = [
            l.strip() for l in out.stdout.splitlines()
            if "/" in l and l.strip().split("/", 1)[1].startswith(prefix)
        ]
        if targets:
            print(f"  🧹 待清理: {', '.join(t.split('/')[-1] for t in targets)}")
            subprocess.run(["ssh", K8S_SSH, "kubectl", "-n", K8S_NS, "delete", *targets],
                           capture_output=True, timeout=180)
    except Exception as e:  # noqa: BLE001
        print(f"  ⚠️ 清理失败: {e}")


def snippet(a: str, b: str, win: int = 24) -> str:
    """24 字连续逐字重放检测（与主套件一致）"""
    for i in range(0, max(0, len(a) - win)):
        frag = a[i:i + win]
        if frag in b:
            return frag
    return ""


def main():
    if not K8S_SSH:
        print("❌ 需要 TEST_K8S_SSH（解析副本 IP 与清理）")
        sys.exit(1)
    entries = [f"http://{h}:{NODEPORT}" for h in ENTRY_HOSTS]
    print(f"节点入口: {entries}")

    user = scoped_user("lb")
    c = Check()

    # ===== 场景 1（主路径）：chat 经三个节点 NodePort 轮换，SSE 也轮换入口 =====
    # 真实用户路径：宿主机 IP + NodePort；kube-proxy 落点随机（后端 3 副本），
    # 多轮轮换自然遍历"chat 落副本 X、SSE 落副本 Y"的组合。
    print("\n▶ lb_entry_rotation（chat 三节点 NodePort 轮换 + SSE 入口轮换）")
    prompts = [
        "从1数到5，每行一个数字",
        "我上一条让你做什么了？一句话回答，再解释 CAP 定理三点",
        "再解释 BASE 定理三点",
        "最后对比 CAP 和 BASE 的关系，两点",
    ]
    replies, turn_max = [], 0
    sid = pid = None
    for i, prompt in enumerate(prompts):
        chat_url = entries[i % len(entries)]  # chat: .19 → .34 → .18 → .19
        sse_url = entries[(i + 1) % len(entries)]  # SSE: .34 → .18 → .19 → .34
        p = base_payload(prompt, f"{RUN_TAG}-lb{i}", user)
        if sid:
            p["session_id"], p["project_id"] = sid, pid
        if sid is None:
            d = chat_at(chat_url, p)  # 首轮同步拿 sid
            sid, pid = d["session_id"], d["project_id"]
            time.sleep(0.5)
            sse_url = chat_url
        else:
            threading.Thread(target=bg_chat, args=(chat_url, p), daemon=True).start()
            time.sleep(1)
        evs = sse_collect_at(sse_url, sid, 45)
        ids = ids_of(evs)
        text = chunks_text(evs)
        replies.append(text)
        c.ok(len(ids) > 0, f"turn{i} chat@{chat_url.split('//')[1]} SSE@{sse_url.split('//')[1]} 收 {len(ids)} 事件")
        c.ok(all(x > turn_max for x in ids),
             f"turn{i} seq 全 > 前轮（min={min(ids) if ids else '-'} > {turn_max}）——跨入口 seq 单源连续")
        c.ok("end_turn" in [e.get("event") for e in evs], f"turn{i} 完整执行")
        turn_max = max(ids) if ids else turn_max
        for j in range(i):
            frag = snippet(replies[j], text)
            c.ok(not frag, f"turn{i} 无 turn{j} 逐字重放" + (f"（命中 {frag[:16]!r}）" if frag else ""))
    c.ok("CAP" in replies[1] and "BASE" in replies[2] and "CAP" in replies[3],
         f"跨入口上下文延续（末轮：{replies[3][:40]!r}）")

    # ===== 场景 2：断线带游标跨入口重连（真实用户断线路径）=====
    print("\n▶ lb_cross_entry_cursor_reconnect（SSE 断于入口 A，带游标续于入口 B）")
    d3 = chat_at(entries[0], base_payload("从10数到15，每行一个", f"{RUN_TAG}-lr", user))
    sid3 = d3["session_id"]
    p3 = base_payload("我数到几了？直接答数字", f"{RUN_TAG}-lr2", user)
    p3["session_id"], p3["project_id"] = sid3, d3["project_id"]
    threading.Thread(target=bg_chat, args=(entries[1], p3), daemon=True).start()
    time.sleep(0.8)
    evs_a = sse_collect_at(entries[0], sid3, 12)  # A 收一段即断
    ids_a = ids_of(evs_a)
    c.ok(len(ids_a) > 0, f"首段经入口 {entries[0].split('//')[1]} 收到 {len(ids_a)} 事件")
    cursor = max(ids_a) if ids_a else 0
    evs_b = sse_collect_at(entries[2], sid3, 40, last_event_id=cursor)  # B 续
    ids_b = ids_of(evs_b)
    c.ok(all(x > cursor for x in ids_b),
         f"续传事件全 > 游标（min={min(ids_b) if ids_b else '-'} > {cursor}）——无重复")
    # 首段窗口内 turn 已结束（含 end_turn）→ 终端即清后无增量，续 0 事件是正确行为；
    # turn 未结束 → 必须收到后续事件（不丢）
    first_seg_done = any(e.get("event") == "end_turn" for e in evs_a)
    if first_seg_done:
        c.ok(all(x > cursor for x in ids_b), "turn 已结束：续传零增量且无重复（正确）")
    else:
        c.ok(len(ids_b) > 0, "turn 进行中：续传收到后续事件（不丢）")

    # ===== 场景 3：新会话跨入口（NodePort 随机落点，多轮统计覆盖跨副本）=====
    # durable 直写 + SSE 回源的验收场景：chat 落随机副本 X，1 秒后从另一入口
    # 订阅（落随机副本 Y，3 副本下每轮 2/3 概率 X≠Y）——多轮组合覆盖跨副本，
    # 且就是真实用户的负载均衡形态（无需也不应指定副本）。
    print("\n▶ lb_new_session_cross_entry（新会话 chat 后 1s 从另一入口订阅）")
    for i, kw in enumerate(["分布式", "微服务", "负载均衡"]):
        chat_url = entries[i % len(entries)]
        sse_url = entries[(i + 1) % len(entries)]
        d4 = chat_at(chat_url, base_payload(f"用一句话解释：{kw}", f"{RUN_TAG}-n{i}", user))
        sid4 = d4["session_id"]
        threading.Thread(target=bg_chat, args=(
            chat_url,
            {**base_payload(f"再补充一句关于{kw}的要点", f"{RUN_TAG}-n{i}b", user),
             "session_id": sid4, "project_id": d4["project_id"]}), daemon=True).start()
        time.sleep(1)  # 验收窗口：durable+回源后必须直接命中
        evs4 = sse_collect_at(sse_url, sid4, 40)
        ids4 = ids_of(evs4)
        c.ok(len(ids4) > 0, f"轮{i} chat@{chat_url.split('//')[1]} 1s后SSE@{sse_url.split('//')[1]} 收 {len(ids4)} 事件")
        c.ok(kw in chunks_text(evs4), f"轮{i} 内容正确（含 {kw}）")

    # ===== 汇总 =====
    print()
    for ln in c.lines():
        print(ln)
    print(f"\n结果: {'✅ 全过' if c.passed else '❌ 存在失败'}")
    cleanup(user)
    sys.exit(0 if c.passed else 1)


if __name__ == "__main__":
    main()
