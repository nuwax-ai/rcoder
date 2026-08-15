-- rcoder-pg 初始 schema（Phase 1：5 张表）
-- 执行方式：sqlx::migrate!() 启动自动执行（advisory lock 保护，多副本并发安全）。
-- 注释经 COMMENT ON 写入库 catalog，psql \d+ 与数据库工具直接可见。

-- ============================================================
-- 1. 容器条目元数据（运行态真源在 K8s/Docker API，本表存路由所需静态信息）
-- ============================================================
CREATE TABLE containers (
    container_name text PRIMARY KEY,
    container_id   text,
    logical_id     text NOT NULL,
    service_type   text NOT NULL,
    container_ip   text NOT NULL DEFAULT '',
    internal_port  int  NOT NULL DEFAULT 0,
    external_port  int  NOT NULL DEFAULT 0,
    status         text NOT NULL DEFAULT 'pending',
    service_url    text NOT NULL DEFAULT '',
    last_activity  timestamptz NOT NULL DEFAULT now(),
    created_at     timestamptz NOT NULL DEFAULT now(),
    version        bigint NOT NULL DEFAULT 1
);
CREATE INDEX idx_containers_logical ON containers(logical_id);

COMMENT ON TABLE  containers IS '容器条目元数据。运行态真源在 K8s/Docker API,本表存路由所需静态信息';
COMMENT ON COLUMN containers.container_name IS '主键=K8s Pod/Service 名或 Docker 容器名;无真实容器时回退 logical_id 占位';
COMMENT ON COLUMN containers.container_id IS 'K8s Pod UID/Docker 容器 ID;占位条目尚无真实容器时为 NULL';
COMMENT ON COLUMN containers.logical_id IS '逻辑标识(Computer→user_id/pod_id, Web→project_id/pod_id),容器生命周期内稳定,RAII 清理与 pod_ensure 重建按此定位';
COMMENT ON COLUMN containers.service_type IS '服务类型(web-agent-runner/computer-agent-runner 等,对齐 ServiceType)';
COMMENT ON COLUMN containers.container_ip IS 'Pod/容器 IP(Docker 直连用;K8s 走 Service FQDN)';
COMMENT ON COLUMN containers.internal_port IS 'agent_runner gRPC 端口(默认 50051)';
COMMENT ON COLUMN containers.external_port IS 'Docker 宿主端口映射(K8s 模式为 0)';
COMMENT ON COLUMN containers.status IS '容器状态快照(pending/running/...)';
COMMENT ON COLUMN containers.service_url IS '访问 URL 快照';
COMMENT ON COLUMN containers.last_activity IS '最后活跃时间(节流写,闲置回收判据)';
COMMENT ON COLUMN containers.created_at IS '创建时间';
COMMENT ON COLUMN containers.version IS '乐观锁版本号,upsert 自增(Phase 2 跨副本冲突检测用)';

-- ============================================================
-- 2. project 主记录（project↔user↔container 映射与模型配置）
-- ============================================================
CREATE TABLE projects (
    project_id     text PRIMARY KEY,
    user_id        text,
    pod_id         text,
    tenant_id      text,
    space_id       text,
    isolation_type text,
    container_name text REFERENCES containers(container_name) ON DELETE SET NULL,
    latest_session text,
    model_provider jsonb,
    request_id     text,
    agent_status   jsonb,
    service_type   text,
    last_activity  timestamptz NOT NULL DEFAULT now(),
    created_at     timestamptz NOT NULL DEFAULT now(),
    version        bigint NOT NULL DEFAULT 1
);
CREATE INDEX idx_projects_user   ON projects(user_id) WHERE user_id IS NOT NULL;
CREATE INDEX idx_projects_pod    ON projects(pod_id)  WHERE pod_id  IS NOT NULL;
CREATE INDEX idx_projects_tenant ON projects(tenant_id, space_id) WHERE tenant_id IS NOT NULL;

COMMENT ON TABLE  projects IS 'project 主记录:project↔user↔container 映射与模型配置(原内存 ProjectCoreState+ExtendedState)';
COMMENT ON COLUMN projects.project_id IS '项目唯一 ID(主键)';
COMMENT ON COLUMN projects.user_id IS '用户 ID(ComputerAgentRunner 模式专用,容器唯一标识;Web 模式 NULL)';
COMMENT ON COLUMN projects.pod_id IS '共享容器模式 Pod ID(多 project 共享同一容器)';
COMMENT ON COLUMN projects.tenant_id IS '租户 ID(多租户隔离,共享容器下反查项目归属)';
COMMENT ON COLUMN projects.space_id IS '空间 ID(租户下二级分组)';
COMMENT ON COLUMN projects.isolation_type IS '隔离类型(tenant/space/project),仅记录与日志用';
COMMENT ON COLUMN projects.container_name IS '关联容器条目(FK→containers);共享容器下多 project 指向同一行';
COMMENT ON COLUMN projects.latest_session IS '最近添加的 session_id(兼容单值读路径)';
COMMENT ON COLUMN projects.model_provider IS '模型提供商配置(ModelProviderConfig JSON,含明文 api_key——运维排查需直连上游验证)';
COMMENT ON COLUMN projects.request_id IS '当前活跃请求 ID';
COMMENT ON COLUMN projects.agent_status IS 'Agent 运行状态快照(AgentStatus JSON,可空,可从 agent_runner 回查)';
COMMENT ON COLUMN projects.service_type IS '服务类型';
COMMENT ON COLUMN projects.last_activity IS '最后活跃时间(节流写)';
COMMENT ON COLUMN projects.created_at IS '创建时间';
COMMENT ON COLUMN projects.version IS '乐观锁版本号,upsert 自增';

-- ============================================================
-- 3. session 映射（热路径：gateway resolve，启动全量加载进内存镜像）
-- ============================================================
CREATE TABLE sessions (
    session_id     text PRIMARY KEY,
    project_id     text NOT NULL REFERENCES projects(project_id) ON DELETE CASCADE,
    container_name text,
    created_at     timestamptz NOT NULL DEFAULT now(),
    last_seen_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX idx_sessions_project ON sessions(project_id);

COMMENT ON TABLE  sessions IS 'session 映射热路径表:gateway /internal/session/{id}/resolve 每消息查询,启动全量加载进内存镜像';
COMMENT ON COLUMN sessions.session_id IS '会话 ID(主键,resolve 入口)';
COMMENT ON COLUMN sessions.project_id IS '所属项目(FK→projects,删 project 级联删 session)';
COMMENT ON COLUMN sessions.container_name IS '冗余容器名:resolve 单次 PK 查询直达容器免 join;与 projects.container_name 同步维护';
COMMENT ON COLUMN sessions.created_at IS '创建时间';
COMMENT ON COLUMN sessions.last_seen_at IS '最后活跃时间(update_session_activity 节流写)';

-- ============================================================
-- 4. UserApp 活动状态（闲置自动回收 + 流量唤醒的共享判据）
-- ============================================================
CREATE TABLE userapp_activity (
    app_id        text PRIMARY KEY,
    last_accessed timestamptz,
    stopped       boolean NOT NULL DEFAULT false,
    wake_blocked  boolean NOT NULL DEFAULT false,
    updated_at    timestamptz NOT NULL DEFAULT now()
);

COMMENT ON TABLE  userapp_activity IS 'UserApp 活动状态:闲置自动回收+流量唤醒的共享判据(原 AppActivityRegistry 内存态)';
COMMENT ON COLUMN userapp_activity.app_id IS 'UserApp 应用 ID(主键)';
COMMENT ON COLUMN userapp_activity.last_accessed IS '最近真实 HTTP 访问时间(Pingora touch 5s 节流+批量 flush,闲置回收判据)';
COMMENT ON COLUMN userapp_activity.stopped IS '已 scale-to-zero,可被流量唤醒';
COMMENT ON COLUMN userapp_activity.wake_blocked IS '用户主动停止/发布切换中,禁止流量自动唤醒';
COMMENT ON COLUMN userapp_activity.updated_at IS '行更新时间';

-- ============================================================
-- 5. UserApp 构建/发布任务（任务状态跨重启/跨副本可查）
-- ============================================================
CREATE TABLE publish_tasks (
    task_id     text PRIMARY KEY,
    app_id      text NOT NULL,
    project_id  text NOT NULL,
    kind        text NOT NULL,
    state       text NOT NULL,
    stage       text,
    release_id  text,
    error       text,
    progress    jsonb,
    owner_pod   text,
    created_at  timestamptz NOT NULL DEFAULT now(),
    terminal_at timestamptz
);
-- 同 app 单活跃任务约束（跨副本；终端 409 语义与内存版 AppBusy 一致）
CREATE UNIQUE INDEX idx_publish_one_active_per_app ON publish_tasks(app_id) WHERE terminal_at IS NULL;
CREATE INDEX idx_publish_terminal ON publish_tasks(terminal_at);

COMMENT ON TABLE  publish_tasks IS 'UserApp 构建/发布任务表:任务状态跨重启/跨副本可查;进度事件流仍在内存(原 PublishTaskStore)';
COMMENT ON COLUMN publish_tasks.task_id IS '任务 ID(uuid v7,应用侧生成,主键)';
COMMENT ON COLUMN publish_tasks.app_id IS '所属应用 ID';
COMMENT ON COLUMN publish_tasks.project_id IS '来源项目 ID';
COMMENT ON COLUMN publish_tasks.kind IS '任务类型(build/publish)';
COMMENT ON COLUMN publish_tasks.state IS '任务状态(pending/running/cancelling/completed/failed/cancelled,对齐 PublishTaskStatus)';
COMMENT ON COLUMN publish_tasks.stage IS '当前阶段标识';
COMMENT ON COLUMN publish_tasks.release_id IS '产出 release ID(completed 终态回填)';
COMMENT ON COLUMN publish_tasks.error IS '失败原因文案';
COMMENT ON COLUMN publish_tasks.progress IS '最新进度摘要 JSON(当前步骤/百分比,非事件流)';
COMMENT ON COLUMN publish_tasks.owner_pod IS '执行该任务的 rcoder Pod(诊断用)';
COMMENT ON COLUMN publish_tasks.created_at IS '创建时间';
COMMENT ON COLUMN publish_tasks.terminal_at IS '终态时间戳,NULL=未终态;部分唯一索引 UNIQUE(app_id) WHERE terminal_at IS NULL = 同 app 单活跃任务';
