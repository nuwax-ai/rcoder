-- Internal execution inputs are never selected by operation query endpoints.
CREATE TABLE userapp_operation_inputs (
    operation_id TEXT PRIMARY KEY NOT NULL REFERENCES userapp_operations(operation_id),
    app_id TEXT NOT NULL REFERENCES userapp_lifecycles(app_id),
    lifecycle_id TEXT NOT NULL,
    payload TEXT NOT NULL
);
