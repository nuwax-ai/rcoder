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


def resolve_replica_ips() -> list[str]:
    """解析 rcoder 副本 Pod IP（确定性直连路由用）"""
    out = subprocess.run(
        ["ssh", K8S_SSH, "kubectl", "get", "pods", "-n", K8S_NS,
         "-l", "app.kubernetes.io/component=rcoder-main", "-o", "wide"],
        capture_output=True, text=True, timeout=30,
    )
    ips = []
    for line in out.stdout.splitlines()[1:]:
        cols = line.split()
        if len(cols) >= 6 and cols[5].startswith("10."):
            ips.append(cols[5])
    return ips



def bg_chat(url: str, payload: dict):
    """后台 chat 线程：异常必须可见（线程内异常默认被吞）"""
    try:
        chat_at(url, payload)
        print(f"    [chat@{url}] ok")
    except Exception as e:  # noqa: BLE001
        print(f"    [chat@{url}] FAILED: {type(e).__name__}: {str(e)[:120]}")


def open_tunnels(ips: list[str], base_port: int = 18086) -> list[subprocess.Popen]:
    """每副本一条 ssh 本地端口转发隧道（Pod IP 是集群内网，本机不可达）"""
    procs = []
    for i, ip in enumerate(ips):
        local = base_port + i
        proc = subprocess.Popen(
            ["ssh", "-N", "-L", f"127.0.0.1:{local}:{ip}:8086", K8S_SSH],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        procs.append(proc)
    time.sleep(2)
    return procs


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
    replicas = resolve_replica_ips()
    entries = [f"http://{h}:{NODEPORT}" for h in ENTRY_HOSTS]
    print(f"副本 Pod IP: {replicas}（直连 :8086）")
    print(f"节点入口: {entries}")
    if len(replicas) < 2:
        print("❌ 副本数 < 2，无法验证跨副本")
        sys.exit(1)

    user = scoped_user("lb")
    c = Check()
    tunnels = open_tunnels(replicas)
    rep_url = [f"http://127.0.0.1:{18086 + i}" for i in range(len(replicas))]

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

    # ===== 场景 3（辅助）：确定性跨副本（Pod 直连隧道：chat@A 订阅@B）=====
    print("\n▶ lb_cross_replica_sse（确定性：chat@副本A SSE@副本B）")
    d2 = chat_at(rep_url[0], base_payload("用一句话说明什么是分布式系统", f"{RUN_TAG}-lx", user))
    sid2 = d2["session_id"]
    p2 = base_payload("用三点解释 ACID 特性", f"{RUN_TAG}-lx2", user)
    p2["session_id"], p2["project_id"] = sid2, d2["project_id"]
    threading.Thread(target=bg_chat, args=(rep_url[0], p2), daemon=True).start()
    time.sleep(4)  # write-behind 落 PG + async 复制追平窗口（新会话跨副本首读）
    evs2 = sse_collect_at(rep_url[1], sid2, 40)
    c.ok(len(ids_of(evs2)) > 0, f"跨副本 SSE 收到 {len(ids_of(evs2))} 事件（chat@A 订阅@B）")
    c.ok("ACID" in chunks_text(evs2) or "原子" in chunks_text(evs2), "跨副本 SSE 内容正确")

    # ===== 汇总 =====
    print()
    for ln in c.lines():
        print(ln)
    print(f"\n结果: {'✅ 全过' if c.passed else '❌ 存在失败'}")
    cleanup(user)
    for t in tunnels:
        t.terminate()
    sys.exit(0 if c.passed else 1)


if __name__ == "__main__":
    main()
