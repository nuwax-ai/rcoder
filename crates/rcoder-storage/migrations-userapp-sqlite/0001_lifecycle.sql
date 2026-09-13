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
