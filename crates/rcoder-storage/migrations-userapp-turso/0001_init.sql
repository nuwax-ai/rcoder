-- Turso 初始 schema（specs/userapp-turso-local-storage plan §4）。
-- 无存量数据：直接采用 7 个 SQLite 迁移的最终形态（dev/prod/application
-- 三槽位结构），不做旧单指针迁移。SQL 为 SQLite 方言（Turso 追踪
-- SQLite 3.50.4 兼容）。
CREATE TABLE userapp_lifecycles (
    app_id TEXT PRIMARY KEY NOT NULL,
    record TEXT NOT NULL
);
CREATE TABLE userapp_operations (
    operation_id TEXT PRIMARY KEY NOT NULL,
    app_id TEXT NOT NULL REFERENCES userapp_lifecycles(app_id),
    request_id TEXT,
    terminal INTEGER NOT NULL CHECK (terminal IN (0, 1)),
    record TEXT NOT NULL,
    UNIQUE (app_id, request_id)
);
CREATE INDEX userapp_operations_pending ON userapp_operations(terminal, operation_id);
CREATE TABLE userapp_recreations (
    app_id TEXT NOT NULL REFERENCES userapp_lifecycles(app_id),
    request_id TEXT NOT NULL,
    previous_lifecycle_id TEXT NOT NULL,
    new_lifecycle_id TEXT NOT NULL,
    PRIMARY KEY (app_id, request_id)
);
-- Every coalesced request remains idempotent after the shared operation finishes.
CREATE TABLE userapp_operation_requests (
    app_id TEXT NOT NULL REFERENCES userapp_lifecycles(app_id),
    request_id TEXT NOT NULL,
    operation_id TEXT NOT NULL REFERENCES userapp_operations(operation_id),
    PRIMARY KEY(app_id, request_id)
);
-- Internal execution inputs are never selected by operation query endpoints.
CREATE TABLE userapp_operation_inputs (
    operation_id TEXT PRIMARY KEY NOT NULL REFERENCES userapp_operations(operation_id),
    app_id TEXT NOT NULL REFERENCES userapp_lifecycles(app_id),
    lifecycle_id TEXT NOT NULL,
    payload TEXT NOT NULL
);
-- No cascading deletion: retained physical ownership fences prevent a replacement
-- lifecycle from claiming an older container/workload UID.
CREATE TABLE userapp_resource_bindings (
    service_type TEXT NOT NULL,
    physical_uid TEXT NOT NULL,
    app_id TEXT NOT NULL REFERENCES userapp_lifecycles(app_id),
    record TEXT NOT NULL,
    PRIMARY KEY (service_type, physical_uid)
);
CREATE INDEX userapp_resource_bindings_app ON userapp_resource_bindings(app_id);
CREATE TABLE userapp_operation_leases (
    operation_id TEXT PRIMARY KEY NOT NULL REFERENCES userapp_operations(operation_id),
    app_id TEXT NOT NULL REFERENCES userapp_lifecycles(app_id),
    lifecycle_id TEXT NOT NULL,
    record TEXT NOT NULL
);
CREATE TABLE userapp_operation_deadlines (
    operation_id TEXT PRIMARY KEY NOT NULL REFERENCES userapp_operations(operation_id),
    app_id TEXT NOT NULL REFERENCES userapp_lifecycles(app_id),
    lifecycle_id TEXT NOT NULL,
    deadline_ms INTEGER NOT NULL
);
