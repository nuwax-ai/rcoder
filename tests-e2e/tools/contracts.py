"""Reviewed acceptance steps. Changing implementation does not change this catalog."""
FAULTS = ('missing-url', 'truncated', 'sha', 'zip', 'manifest', 'quota', 'download-limit', 'total-limit', 'entry-limit', 'idle', 'link-escape', 'link-cycle')
HOT = {
    'A accepted', 'A identity', 'A serves content',
    'broken B accepted', 'broken B operation failed',
    'old code and orchestration restored', 'migration reversal never claimed',
    'recovery does not rerun old migrations', 'slow B accepted',
    'concurrent deployment rejected', 'A serves during prepare',
    'B identity', 'B serves content', 'container unchanged', 'owned resource cleanup',
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
