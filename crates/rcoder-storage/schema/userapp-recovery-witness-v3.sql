-- Immutable evidence for registration reconstructed from existing compute.
-- Kept independently of operation history: discovery invents no successful task.
CREATE TABLE userapp_recovery_witnesses (
    app_id TEXT NOT NULL,
    lifecycle_id TEXT NOT NULL,
    witness_json TEXT NOT NULL,
    created_at_us BIGINT NOT NULL,
    PRIMARY KEY(app_id,lifecycle_id)
);
