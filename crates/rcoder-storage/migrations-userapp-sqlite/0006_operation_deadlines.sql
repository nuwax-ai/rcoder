CREATE TABLE userapp_operation_deadlines (
    operation_id TEXT PRIMARY KEY NOT NULL REFERENCES userapp_operations(operation_id),
    app_id TEXT NOT NULL REFERENCES userapp_lifecycles(app_id),
    lifecycle_id TEXT NOT NULL,
    deadline_ms INTEGER NOT NULL
);
