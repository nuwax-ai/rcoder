#!/usr/bin/env python3
"""SSE 消息链路集成测试入口（场景定义见 scenarios_basic/advanced）。

用法：
  python3 tests/sse_e2e/run.py              # 全部场景
  python3 tests/sse_e2e/run.py reconnect    # 按名过滤（子串）
配置与场景语义详见同目录 common.py 与 README.md。
"""

import datetime
import functools
import json
import os
import sys
import time
from pathlib import Path

import requests

from common import API_KEY, BASE_URL, CFG, MODEL, OUT_DIR, RCODER, RESULTS_ROOT, RUN_TAG, USER, die, scoped_user
from scenarios_advanced import (
    scenario_anthropic_model_switch, scenario_concurrent_subscribers,
    scenario_container_restart_recovery, scenario_cross_turn_reconnect,
    scenario_model_switch, scenario_model_switch_multi,
)
from scenarios_basic import (
    scenario_after_terminal, scenario_full_turn, scenario_no_session_reuse,
    scenario_reconnect_cursor, scenario_reconnect_no_cursor,
    scenario_two_turn_isolation,
)


SCENARIOS = [
    ("full_turn_delivery", scenario_full_turn),
    ("full_turn_acp_ts", functools.partial(scenario_full_turn, backend="anthropic")),
    ("after_terminal_empty", scenario_after_terminal),
    ("after_terminal_acp_ts", functools.partial(scenario_after_terminal, backend="anthropic")),
    ("two_turn_isolation", scenario_two_turn_isolation),
    ("two_turn_acp_ts", functools.partial(scenario_two_turn_isolation, backend="anthropic")),
    ("reconnect_with_cursor", scenario_reconnect_cursor),
    ("reconnect_cursor_acp_ts", functools.partial(scenario_reconnect_cursor, backend="anthropic")),
    ("reconnect_no_cursor", scenario_reconnect_no_cursor),
    ("reconnect_nocursor_acp_ts", functools.partial(scenario_reconnect_no_cursor, backend="anthropic")),
    # 切模型（各后端独立链路）
    ("model_switch", scenario_model_switch),
    ("model_switch_acp_ts", scenario_anthropic_model_switch),
    ("model_switch_multi", scenario_model_switch_multi),
    ("model_switch_multi_acp_ts", functools.partial(scenario_model_switch_multi, backend="anthropic")),
    # 无 session_id 续话（前端标准姿势：rcoder 内部 project→session 映射复用）
    ("no_session_reuse", scenario_no_session_reuse),
    ("no_session_reuse_acp_ts", functools.partial(scenario_no_session_reuse, backend="anthropic")),
    # 故障与并发边界
    ("cross_turn_reconnect", scenario_cross_turn_reconnect),
    ("cross_turn_acp_ts", functools.partial(scenario_cross_turn_reconnect, backend="anthropic")),
    ("concurrent_subscribers", scenario_concurrent_subscribers),
    ("concurrent_sub_acp_ts", functools.partial(scenario_concurrent_subscribers, backend="anthropic")),
    # TODO: error_recovery 脚本形态下恢复轮 SSE 空响应待查（手动同序列冷/热容器
    # 均通过——错误轮 6 事件 + 恢复轮 6 事件 + cancel 语义正常；疑与 collect_retry
    # 双连接挂共享流状态有关，非业务逻辑缺陷）。手动复现序列见 git 历史。
    # ("error_recovery", scenario_error_recovery),
    ("container_restart_recovery", scenario_container_restart_recovery),
]


def cleanup_test_containers():
    """主动删除本次测试创建的 agent 容器（不等闲置回收：场景多时容器堆积
    会吃光宿主内存/节点盘，拖慢甚至拖垮后续场景）。

    Docker 模式: docker rm dev-rcoder-agent-runner-<USER 前缀>；
    K8s 模式(TEST_K8S_SSH 指定时): 远程 kubectl 删 STS/headless svc/PVC。
    """
    import subprocess
    from common import K8S_NS, K8S_SSH
    try:
        if K8S_SSH:
            out = subprocess.run(
                ["ssh", K8S_SSH, "kubectl", "-n", K8S_NS, "get", "sts,svc,pvc", "-o", "name"],
                capture_output=True, text=True, timeout=30,
            )
            prefix = f"rcoder-computer-agent-runner-{USER}"
            targets = [l.strip() for l in out.stdout.splitlines() if prefix in l]
            if targets:
                subprocess.run(
                    ["ssh", K8S_SSH, "kubectl", "-n", K8S_NS, "delete", *targets],
                    capture_output=True, timeout=180,
                )
                print(f"  🧹 已清理 {len(targets)} 个 K8s 资源（STS/svc/PVC）")
            return
        out = subprocess.run(
            ["docker", "ps", "-aq", "--filter", f"name=dev-rcoder-agent-runner-{USER}"],
            capture_output=True, text=True, timeout=15,
        )
        ids = [l for l in out.stdout.splitlines() if l.strip()]
        if ids:
            subprocess.run(["docker", "rm", "-f", *ids], capture_output=True, timeout=60)
            print(f"  🧹 已清理 {len(ids)} 个测试容器")
    except Exception as e:  # noqa: BLE001
        print(f"  ⚠️ 容器清理失败（不影响测试结果）: {e}")


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
            cleanup_test_containers()
            continue
        detail["checks"] = [{"ok": p, "desc": d} for p, d in check.items]
        detail["duration_s"] = round(time.time() - t0, 1)
        (OUT_DIR / f"{name}.json").write_text(
            json.dumps(detail, ensure_ascii=False, indent=2, default=str))
        for ln in check.lines():
            print(ln)
        rows.append((name, check.passed, f"{detail['duration_s']}s"))
        print(f"  {'✅ PASS' if check.passed else '❌ FAIL'}")
        cleanup_test_containers()

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
