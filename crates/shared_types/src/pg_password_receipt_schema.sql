-- Only schema/ACL bootstrap uses this short transaction-scoped lock.
-- Password writes commit later and rely on their unique receipt/CAS, not this lock.
BEGIN;
SELECT pg_advisory_xact_lock(7845993687098996451);
CREATE SCHEMA IF NOT EXISTS rcoder_management;
REVOKE ALL ON SCHEMA rcoder_management FROM PUBLIC;
CREATE TABLE IF NOT EXISTS rcoder_management.password_receipts (
    app_id text NOT NULL,
    lifecycle_id text NOT NULL,
    scope text NOT NULL CHECK (scope IN ('dev', 'prod')),
    operation_id text NOT NULL,
    request_fingerprint text NOT NULL,
    target_username text NOT NULL,
    writer_xid text NOT NULL,
    outcome text NOT NULL DEFAULT 'committed' CHECK (outcome IN ('committed', 'cancelled')),
    PRIMARY KEY (app_id, lifecycle_id, scope, operation_id)
);
-- Existing receipts were exclusively committed writes. Preserve that evidence.
ALTER TABLE rcoder_management.password_receipts ADD COLUMN IF NOT EXISTS
    outcome text NOT NULL DEFAULT 'committed' CHECK (outcome IN ('committed', 'cancelled'));
REVOKE ALL ON rcoder_management.password_receipts FROM PUBLIC;
COMMIT;
