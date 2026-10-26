-- Additive compute-control total deadlines. The operation deadline table
-- carries a foreign key into userapp_operations, so compute controls keep
-- their own table with the same shape and a foreign key into their own row.
-- unheld_observed_ms anchors the settlement quiet window: the first durable
-- observation that the physical lease stopped being renewed. Settlement waits
-- a window longer than the kube client's default write timeout (295s) after
-- that anchor, so no write the dead holder had already submitted can still
-- land after the control is terminalized and the scope admits new work.
CREATE TABLE userapp_compute_control_deadlines (
    operation_id TEXT NOT NULL PRIMARY KEY,
    app_id TEXT NOT NULL,
    lifecycle_id TEXT NOT NULL,
    deadline_ms BIGINT NOT NULL CHECK(deadline_ms>0),
    unheld_observed_ms BIGINT,
    FOREIGN KEY(operation_id) REFERENCES userapp_compute_controls(operation_id)
);
