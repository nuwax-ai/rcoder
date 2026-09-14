-- Custom Page（WebAgentRunner 开发阶段 Vite 预览）权威注册表。
-- 独立迁移表 _sqlx_preview_migrations（见 preview_lifecycle/postgres.rs），
-- 不与 UserApp lifecycle 表族混用。所有状态迁移由应用层 CAS
-- （WHERE instance_id/operation_id/revision + state 前置）保证。

CREATE TABLE preview_instances (
    preview_key       TEXT PRIMARY KEY,
    project_id        TEXT NOT NULL,
    project_path      TEXT NOT NULL,
    instance_id       TEXT NOT NULL,
    revision          BIGINT NOT NULL,
    operation_id      TEXT NOT NULL,
    host_id           TEXT NOT NULL,
    pod_name          TEXT,
    pod_ip            TEXT,
    pid               BIGINT,
    port              INTEGER,
    base_path         TEXT,
    state             TEXT NOT NULL CHECK (state IN ('starting','ready','stopping','stopped','failed','unknown')),
    last_heartbeat_at TIMESTAMPTZ,
    last_activity_at  TIMESTAMPTZ NOT NULL,
    detail            TEXT,
    updated_at        TIMESTAMPTZ NOT NULL
);

-- 入口 URL 只有 port：活跃实例的端口全局唯一是路由无歧义的前提（协调器在
-- accept_start 事务内分配），部分索引即端口占用记账面。
CREATE INDEX preview_instances_port ON preview_instances(port)
    WHERE state IN ('starting','ready','stopping','unknown');
CREATE INDEX preview_instances_host ON preview_instances(host_id)
    WHERE state IN ('starting','ready','stopping','unknown');

CREATE TABLE preview_operations (
    operation_id   TEXT PRIMARY KEY,
    preview_key    TEXT NOT NULL,
    kind           TEXT NOT NULL CHECK (kind IN ('start','stop')),
    state          TEXT NOT NULL CHECK (state IN ('accepted','running','succeeded','failed','uncertain')),
    host_id        TEXT NOT NULL,
    requested_port INTEGER,
    allocated_port INTEGER,
    result         TEXT,
    created_at     TIMESTAMPTZ NOT NULL,
    updated_at     TIMESTAMPTZ NOT NULL
);
CREATE INDEX preview_operations_key ON preview_operations(preview_key, created_at);
