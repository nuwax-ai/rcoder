"""Reviewed acceptance steps. Changing implementation does not change this catalog."""
import json
from pathlib import Path

FAULTS = ('missing-url', 'truncated', 'sha', 'zip', 'manifest', 'idle', 'link-escape', 'link-cycle')
HOT = {
    'A accepted', 'A identity', 'A serves content',
    'broken B accepted', 'broken B operation failed',
    'old code and orchestration restored', 'migration reversal never claimed',
    'recovery does not rerun old migrations', 'slow B accepted',
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
    'static failed deployment restores serving configuration',
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
