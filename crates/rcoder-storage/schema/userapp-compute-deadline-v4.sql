-- Additive compute-control total deadlines. The operation deadline table
-- carries a foreign key into userapp_operations, so compute controls keep
-- their own table with the same shape and a foreign key into their own row.
CREATE TABLE userapp_compute_control_deadlines (
    operation_id TEXT NOT NULL PRIMARY KEY,
    app_id TEXT NOT NULL,
    lifecycle_id TEXT NOT NULL,
    deadline_ms BIGINT NOT NULL CHECK(deadline_ms>0),
    FOREIGN KEY(operation_id) REFERENCES userapp_compute_controls(operation_id)
);
