-- Every coalesced request remains idempotent after the shared operation finishes.
CREATE TABLE userapp_operation_requests (
    app_id TEXT NOT NULL REFERENCES userapp_lifecycles(app_id),
    request_id TEXT NOT NULL,
    operation_id TEXT NOT NULL REFERENCES userapp_operations(operation_id),
    PRIMARY KEY(app_id, request_id)
);
INSERT INTO userapp_operation_requests(app_id, request_id, operation_id)
SELECT app_id, request_id, operation_id FROM userapp_operations WHERE request_id IS NOT NULL;
