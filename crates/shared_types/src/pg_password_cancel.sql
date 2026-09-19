-- Competes with the original writer on exactly the same unique key.
-- An existing committed receipt is immutable; cancellation cannot undo it.
INSERT INTO rcoder_management.password_receipts AS receipt
    (app_id, lifecycle_id, scope, operation_id, request_fingerprint, target_username, writer_xid, outcome)
VALUES (
    current_setting('rcoder.app_id'), current_setting('rcoder.lifecycle_id'),
    current_setting('rcoder.scope'), current_setting('rcoder.operation_id'),
    current_setting('rcoder.fingerprint'), current_setting('rcoder.username'),
    pg_current_xact_id()::text, 'cancelled'
)
ON CONFLICT (app_id, lifecycle_id, scope, operation_id) DO UPDATE
    SET operation_id = EXCLUDED.operation_id
    WHERE receipt.request_fingerprint = EXCLUDED.request_fingerprint
      AND receipt.target_username = EXCLUDED.target_username
RETURNING receipt.outcome;
