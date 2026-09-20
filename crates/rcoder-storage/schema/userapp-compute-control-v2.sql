-- Additive compute-control state. No existing baseline table is rewritten.
CREATE TABLE userapp_compute_intents (
    app_id TEXT NOT NULL,
    lifecycle_id TEXT NOT NULL,
    scope TEXT NOT NULL CHECK(scope IN ('dev','prod')),
    generation BIGINT NOT NULL CHECK(generation >= 1),
    revision BIGINT NOT NULL CHECK(revision >= 1),
    desired_state TEXT NOT NULL CHECK(desired_state IN ('running','stopped')),
    control_operation_id TEXT,
    updated_at_us BIGINT NOT NULL,
    PRIMARY KEY(app_id,scope)
);
CREATE TABLE userapp_compute_controls (
    operation_id TEXT PRIMARY KEY,
    app_id TEXT NOT NULL,
    lifecycle_id TEXT NOT NULL,
    scope TEXT NOT NULL CHECK(scope IN ('dev','prod')),
    generation BIGINT NOT NULL CHECK(generation >= 1),
    revision BIGINT NOT NULL CHECK(revision >= 1),
    action TEXT NOT NULL CHECK(action IN ('stop','restart')),
    state TEXT NOT NULL CHECK(state IN ('pending','running','recovery_required','succeeded','failed','superseded')),
    request_id TEXT NOT NULL,
    request_fingerprint TEXT NOT NULL,
    executor_id TEXT,
    stage TEXT NOT NULL,
    evidence_json TEXT NOT NULL,
    checkpoint_json TEXT NOT NULL DEFAULT 'null',
    lease_json TEXT,
    error_code TEXT,
    error_message TEXT,
    created_at_us BIGINT NOT NULL,
    updated_at_us BIGINT NOT NULL,
    terminal_at_us BIGINT,
    UNIQUE(app_id,scope,request_id),
    UNIQUE(app_id,scope,generation),
    CHECK((state IN ('succeeded','failed','superseded') AND terminal_at_us IS NOT NULL)
       OR (state NOT IN ('succeeded','failed','superseded') AND terminal_at_us IS NULL)),
    CHECK(state <> 'running' OR (executor_id IS NOT NULL AND length(executor_id)>0))
);
CREATE INDEX userapp_compute_controls_pending ON userapp_compute_controls(state,updated_at_us,operation_id);

