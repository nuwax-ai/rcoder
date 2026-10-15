#!/usr/bin/env python3
"""Docker Compose 部署模式下的 UserApp / app_manager 回归测试。

**已废弃（2026-08-27）**：本脚本打面的 publish 编排 / POST /api/v1/userapp 创建
等接口已随发布链路重构删除（b6e7029 起），运行必 404。权威回归面已迁移到
Rust e2e 套件 `tests-e2e/tests/compose_userapp*.rs`；保留本文件仅作历史参照，
勿再维护。

覆盖 compose 模式可直接验证的面（K8s 专属链路除外）：
  1. tasks/query 列表接口（compose = 内存任务表遍历，分页结构）
  2. app 创建约束：UserApp 语义下必须先有 active release lock（发布流水线独占创建）
  3. publish/build 接口标识校验快速失败（不挂起——activate 死锁修复的行为侧面）
  4. publish 全链路终态可达（agent 缺失 → 有限时间内 failed，不挂死）

用法: python3 tests/userapp_compose_regression.py [--base http://127.0.0.1:8090]
"""

import argparse
import sys
import time
import uuid

import requests

BASE = "http://127.0.0.1:8090"
TIMEOUT = 60

PASS, FAIL = [], []


def check(name, cond, detail=""):
    mark = "✅" if cond else "❌"
    print(f"  {mark} {name}" + (f"（{detail}）" if detail else ""))
    (PASS if cond else FAIL).append(name)
    return cond


def test_tasks_query():
    print("▶ tasks_query 接口（compose 内存模式）")
    r = requests.post(f"{BASE}/api/v1/userapp/publish/tasks/query", json={}, timeout=TIMEOUT)
    check("空查询返回 200", r.status_code == 200, f"status={r.status_code}")
    if r.status_code != 200:
        return
    data = r.json().get("data") or {}
    pagination = data.get("pagination") or {}
    check("返回合法分页结构（items + pagination）",
          isinstance(data.get("items"), list) and "total" in pagination,
          f"total={pagination.get('total')}, items={len(data.get('items') or [])}")
    r2 = requests.post(f"{BASE}/api/v1/userapp/publish/tasks/query",
                       json={"filters": {"active_only": True}}, timeout=TIMEOUT)
    check("active_only 过滤查询返回 200", r2.status_code == 200, f"status={r2.status_code}")


def test_publish_identifiers():
    print("▶ publish/build 标识校验（快速失败，不挂起）")
    for kind in ("publish", "build"):
        t0 = time.time()
        try:
            r = requests.post(f"{BASE}/api/v1/userapp/BAD_ID/{kind}",
                              json={"project_id": "also-bad"}, timeout=10)
            elapsed = time.time() - t0
            check(f"{kind} 非法 app_id 返回 4xx 而非挂起",
                  400 <= r.status_code < 500 and elapsed < 5,
                  f"status={r.status_code}, {elapsed:.1f}s")
        except requests.Timeout:
            check(f"{kind} 非法 app_id 快速失败", False, "10s 超时（疑似挂起！）")
    # project_id != app_id → 400（UserAppBuilder 一 app 一 workspace 契约）
    app_id = f"app-e2e-mismatch-{uuid.uuid4().hex[:6]}"
    r = requests.post(f"{BASE}/api/v1/userapp/{app_id}/publish",
                      json={"project_id": f"proj-{uuid.uuid4().hex[:8]}"}, timeout=10)
    check("project_id != app_id 被拒（400 契约校验）",
          r.status_code == 400 and "must equal" in r.text,
          f"status={r.status_code}")


def test_publish_reaches_terminal():
    print("▶ publish 合法请求 → 有限时间内到达终态（死锁修复行为面）")
    # project_id == app_id；agent 会话不存在 → ensure_agent_addr 失败 → failed
    ident = f"app-e2e-term-{uuid.uuid4().hex[:6]}"
    t0 = time.time()
    r = requests.post(f"{BASE}/api/v1/userapp/{ident}/publish",
                      json={"project_id": ident}, timeout=15)
    check("publish 受理（200 任务创建）", r.status_code == 200,
          f"status={r.status_code}, body={r.text[:120]}")
    if r.status_code != 200:
        return
    task_id = (r.json().get("data") or {}).get("task_id")
    deadline = time.time() + 180
    status = "unknown"
    while time.time() < deadline:
        g = requests.get(f"{BASE}/api/v1/userapp/publish/tasks/{task_id}", timeout=10)
        if g.status_code == 200:
            data = g.json().get("data") or {}
            task = data.get("task") or {}
            status = (task.get("status") or "unknown").lower()
            if status in ("failed", "cancelled", "completed"):
                break
        time.sleep(3)
    elapsed = time.time() - t0
    check("publish 任务 180s 内到达终态（不挂死）",
          status in ("failed", "cancelled", "completed"),
          f"status={status}, {elapsed:.0f}s")
    # tasks/query 按 appIds 过滤应能看到该任务
    q = requests.post(f"{BASE}/api/v1/userapp/publish/tasks/query",
                      json={"filters": {"app_ids": [ident]}}, timeout=TIMEOUT)
    items = ((q.json().get("data") or {}).get("items")) if q.status_code == 200 else []
    check("tasks/query 可按 app_ids 过滤到该任务",
          q.status_code == 200 and len(items) >= 1,
          f"status={q.status_code}, 命中={len(items or [])}")


def test_app_create_requires_release_lock():
    print("▶ 直接 create_app 被发布流水线约束拦截（UserApp 语义）")
    app_id = f"app-e2e-nolock-{uuid.uuid4().hex[:6]}"
    payload = {
        "appId": app_id,
        "name": "e2e-nolock-test",
        "image": "alpine:3.19",
        "command": ["sleep", "3600"],
    }
    r = requests.post(f"{BASE}/api/v1/userapp", json=payload, timeout=TIMEOUT)
    check("无 release lock 创建被拒（ERR_INVALID_STATE）",
          r.status_code in (400, 409, 500) and "release lock" in r.text,
          f"status={r.status_code}, body={r.text[:150]}")
    # 确认没有留下半成品 app（GET 404 / 或不存在）
    g = requests.get(f"{BASE}/api/v1/userapp/{app_id}", timeout=TIMEOUT)
    check("失败后无残留 app", g.status_code == 404, f"status={g.status_code}")


def main():
    global BASE
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default=BASE)
    args = ap.parse_args()
    BASE = args.base.rstrip("/")

    tag = time.strftime("%Y%m%d_%H%M%S")
    print(f"Compose UserApp/app_manager 回归  {tag}")
    print(f"target: {BASE}\n")

    test_tasks_query()
    print()
    test_publish_identifiers()
    print()
    test_publish_reaches_terminal()
    print()
    test_app_create_requires_release_lock()

    print(f"\n结果: {len(PASS)}/{len(PASS) + len(FAIL)} 断言通过")
    if FAIL:
        print("失败断言:")
        for f in FAIL:
            print(f"  - {f}")
        sys.exit(1)


if __name__ == "__main__":
    main()
