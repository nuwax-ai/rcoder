DO $rcoder_password_operation$
DECLARE
    original_writer text;
    original_outcome text;
BEGIN
    INSERT INTO rcoder_management.password_receipts AS receipt
        (app_id, lifecycle_id, scope, operation_id, request_fingerprint, target_username, writer_xid)
    VALUES (
        current_setting('rcoder.app_id'), current_setting('rcoder.lifecycle_id'),
        current_setting('rcoder.scope'), current_setting('rcoder.operation_id'),
        current_setting('rcoder.fingerprint'), current_setting('rcoder.username'),
        pg_current_xact_id()::text
    )
    ON CONFLICT (app_id, lifecycle_id, scope, operation_id) DO UPDATE
        SET operation_id = EXCLUDED.operation_id
        WHERE receipt.request_fingerprint = EXCLUDED.request_fingerprint
          AND receipt.target_username = EXCLUDED.target_username
    RETURNING receipt.writer_xid, receipt.outcome INTO original_writer, original_outcome;
    IF original_writer IS NULL THEN
        RAISE EXCEPTION 'Database password operation identity mismatch';
    END IF;
    IF original_outcome = 'cancelled' THEN
        RAISE EXCEPTION 'Database password operation was cancelled';
    END IF;
    IF original_writer = pg_current_xact_id()::text THEN
        IF current_setting('rcoder.create_role') = 'true' THEN
            EXECUTE format('CREATE ROLE %I LOGIN PASSWORD %L',
                current_setting('rcoder.username'), current_setting('rcoder.private_password'));
        ELSE
            EXECUTE format('ALTER ROLE %I PASSWORD %L',
                current_setting('rcoder.username'), current_setting('rcoder.private_password'));
        END IF;
    END IF;
END
$rcoder_password_operation$;
