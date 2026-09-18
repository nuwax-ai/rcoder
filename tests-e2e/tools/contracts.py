"""Reviewed acceptance steps. Changing implementation does not change this catalog."""
import json
from pathlib import Path

FAULTS = ('missing-url', 'truncated', 'sha', 'zip', 'manifest', 'idle', 'link-escape', 'link-cycle')
HOT = {
    'Docker contract process completed',
    'cold A operation configured', 'A identity', 'A serves content',
    'broken B accepted', 'broken B operation failed',
    'switched failure does not restore old readiness', 'migration reversal never claimed',
    'failed switch does not rerun old migrations', 'manual redeploy accepted', 'manual redeploy restores A',
    'restart retains B operation identity', 'restart retains B content despite cold A env',
    'restart preserves container identity', 'slow B accepted',
    'concurrent deployment rejected', 'A serves during prepare',
    'former capacity and entry settings do not reject B', 'B identity', 'B serves content', 'container unchanged', 'owned resource cleanup',
} | {name + suffix for name in FAULTS for suffix in (' accepted', ' fails correct operation', ' old content healthy', ' no temporary residue')}
REQUIRED = {
    'userapp_hot_deployment_builtin_contract': HOT,
    'userapp_hot_deployment_supervisord_contract': HOT,
    'userapp_deploy_full_chain': {
        'builder/runtime artifact toolchains are compatible',
        'create-workspace（ensure builder + owner 注册）',
        'template-cli 全量模板初始化（init --next + 6×add）',
        'workspace 根含 workspace.manifest.toml + 7 个子项目目录',
        'build 终态 = completed（7 服务全量构建成功）',
        'completed 快照含 release_id/sha256/size_bytes/file_name',
        'tasks cancel 幂等（已终态 → already_terminal=true）',
        'tasks SSE 回放（终态后连流 → 全量事件 + completed + 自然关流）',
        'static 取包 + sha256 与任务快照一致',
        'start(url) 部署受理（200 + Running）', 'prod health 就绪（200 + 0000）',
        'prod logs/sources/query 声明源非空', 'prod logs/stream SSE 通道（200 + text/event-stream）',
        '热部署受理（deploy_mode=hot + 新 release_id → 200）', '热部署后流量仍可达',
        'prod resource registered at creation', 'prod diagnostics captured before deletion',
        'stop → stopped', 'stop 后 health 探测自动唤醒 → running', 'prod delete purge 回收',
    },
}

REQUIRED['userapp_scope_isolation_during_deploy'] = {
    'builder/runtime artifact toolchains are compatible',
    'create-workspace（ensure builder + owner 注册）',
    'template-cli 全量模板初始化（init --next + 6×add）',
    'workspace 根含 workspace.manifest.toml + 7 个子项目目录',
    'build 受理（200 + task_id + artifact_path 预生成）',
    'build 终态 = completed（7 服务全量构建成功）',
    'completed 快照含 release_id/sha256/size_bytes/file_name',
    'tasks cancel 幂等（已终态 → already_terminal=true）',
    'tasks SSE 回放（终态后连流 → 全量事件 + completed + 自然关流）',
    'static 取包 + sha256 与任务快照一致',
    '部署在途窗口捕获（current 数组含非终态 StartDeployment）',
    'dev restart 与 prod 部署并发 → 独立受理（无 conflicting 409）',
    'current 数组双 scope 同现（Dev RestartBuilder + Prod 部署族）',
    '隔离场景部署终态 running（并发不破坏部署）',
    '同域并发 restart → 恰一胜者 + 败者 409 带 blocker.scope=Dev',
    'prod image and container identity recorded before cleanup',
    'prod diagnostics captured before deletion',
    'prod delete purge 回收',
    '删除后流量转 502（backend 已注销）',
}

REQUIRED['userapp_deploy_full_chain'] |= {
    f'流量七路[{name}]就绪' for name in (
        'next /', 'react /react/', 'vue /vue/', 'go /api/go/ready',
        'java readiness', 'python /api/python/ready', 'rust /api/rust/ready',
    )
}

# Frozen acceptance steps from reviewed baseline runs; never inferred at runtime.
for _scenario, _steps in json.loads(Path(__file__).with_name('acceptance_steps.json').read_text()).items():
    REQUIRED.setdefault(_scenario, set()).update(_steps)
HOT.update({
    name + suffix for name in ('static-A', 'static-B', 'static-C')
    for suffix in (' accepted', ' identity', ' actual content')
})
HOT.update({
    'static old port released', 'static failure accepted',
    'static failed deployment stays failed',
    'static failed generation port released', 'static removal accepted',
    'static removal serves replacement', 'removed static listener closed',
})

REQUIRED['pg_storage_lifecycle_contract'] = {
    'PG17 isolated environment ready', 'PG owned project cleanup', 'PG contract process completed',
    *('PG ' + case for case in (
        'old_remove_preserves_replacement_and_no_resurrection',
        'delayed_clear_and_remove_preserve_reused_session',
        'reload_and_cross_replica_sync_preserve_identity',
        'container_delete_preserves_changed_association',
        'legacy_schema_backfill_is_stable',
        'flush_failure_shared_between_concurrent_callers',
        'cancelled_durable_write_is_queued_and_shutdown_waits',
    )),
}

REQUIRED['userapp_devbuild_skip_and_fallback_source_mode'].update({
    'Q10 failing build fixture installed',
    'Q10 failed build cannot become already-running success',
    'Q10 previous process preserved after build failure',
    'Q10 previous content remains healthy after build failure',
})

REQUIRED['userapp_devbuild_no_lockfile_pnpm_install'] = {
    'create-workspace（ensure 开发容器）',
    'init 模板 zip（manifests + dist fixture 直投源码目录）',
    'dev/start 受理（task_id）',
    '无 lockfile dev/start completed（--no-frozen-lockfile 安装成功）',
    'devbuild 生成 pnpm-lock.yaml（修复前此处 ERR_PNPM_NO_LOCKFILE 失败）',
    'dev/list → pid>0（devrun 存活）',
    'devrun 服务内容经代理可达（node server.js）',
    '事故反例 fixture 写入（frozen + 过期 lockfile）',
    'frozen + 过期 lockfile → 任务 failed（错误如实传播）',
    'dev/stop → Stopped',
}

REQUIRED['lb_entry_rotation'] = {'跨入口上下文延续（CAP/BASE 关键词）'} | {
    f'turn{i} {step}' for i in range(4) for step in (
        '跨入口收到事件', 'seq 全 > 前轮（跨入口 seq 单源连续）', '完整执行（end_turn）',
    )
} | {f'turn{i} 无 turn{j} 逐字重放' for i in range(4) for j in range(i)}
REQUIRED['lb_cross_entry_cursor_reconnect'] = {
    '首段经入口 A 收到事件', '续传事件全 > 游标（无重复）', '跨入口续传符合轮次边界',
}
REQUIRED['lb_new_session_cross_entry'] = {
    f'轮{i} {step}' for i, keyword in enumerate(('分布式', '微服务', '负载均衡'))
    for step in ('新会话跨入口收到事件', f'内容正确（含 {keyword}）')
}

REQUIRED['docker_deletion_identity_contract'] |= {
    'Docker lifecycle process completed', 'Q01 real Docker test executed',
    'Q01 Docker replacement and volume identity proved',
}

# New userApp persistence contracts are required independently of Agent PG tests.
from storage_contract_cases import SQLITE_TARGETS
REQUIRED['pg_storage_lifecycle_contract'].add('PG userApp transactions and restart')
REQUIRED['sqlite_storage_lifecycle_contract'] = {
    'SQLite contract process completed', 'SQLite frozen cases present',
    *('SQLite ' + case for case in SQLITE_TARGETS),
}

REQUIRED['sqlite_compose_recreation_contract'] = {'SQLite contract process completed'} | {
    f'SQLite Compose {index} {step}' for index in range(3) for step in (
        'configuration', 'first-open convergence', 'one builder identity', 'HTTP persisted', 'recreated identity', 'invalid startup rejected', 'owned cleanup',
    )
}

from concurrency_contract import CASES as CONCURRENCY_CASES
REQUIRED['userapp_concurrency_component_contract'] = {'Concurrency contract process completed'} | {
    'Concurrency ' + case for cases in CONCURRENCY_CASES.values() for case in cases
}

REQUIRED['native_terminal_release_crash_contract'] = {
    'Native contract process completed', 'Native terminal committed before release',
    'Native worker terminated by SIGKILL', 'Native terminal retry not executed again',
    'Native unreceipted marker not reclaimed', 'Native owned process cleanup',
}

REQUIRED['docker_runtime_crash_recovery_contract'] = {'Docker crash contract process completed'} | {
    'Docker ' + mode + ' ' + step for mode in ('before_create', 'after_start') for step in (
        'exact barrier established', 'SIGKILL observed', 'restart quarantines without replay', 'owned cleanup',
    )
}

REQUIRED['userapp_dev_app_proxy_lazy_start'].update({
    'lazy recreation removes only captured builder',
    'lazy recreation records same-lifecycle replacement',
})
