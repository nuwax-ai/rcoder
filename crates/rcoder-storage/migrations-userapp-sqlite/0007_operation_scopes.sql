-- Operation scopes and per-scope active-operation slots (dev/prod/application).
-- One transaction: an unknown kind or a dangling current_operation_id aborts the
-- whole migration, leaving the database on the previous schema for diagnosis.
-- SQLite can only RAISE from triggers, so guards are temporary triggers.

UPDATE userapp_operations
SET record = json_set(record, '$.scope',
    CASE json_extract(record, '$.kind')
        WHEN 'EnsureBuilder' THEN 'Dev'
        WHEN 'AdoptBuilder' THEN 'Dev'
        WHEN 'StopBuilder' THEN 'Dev'
        WHEN 'RestartBuilder' THEN 'Dev'
        WHEN 'DestroyDevStorage' THEN 'Dev'
        WHEN 'ClearDevStorage' THEN 'Dev'
        WHEN 'Create' THEN 'Prod'
        WHEN 'Update' THEN 'Prod'
        WHEN 'StartDeployment' THEN 'Prod'
        WHEN 'RestartDeployment' THEN 'Prod'
        WHEN 'Start' THEN 'Prod'
        WHEN 'Restart' THEN 'Prod'
        WHEN 'Stop' THEN 'Prod'
        WHEN 'SetRecyclePolicy' THEN 'Prod'
        WHEN 'HotDeploy' THEN 'Prod'
        WHEN 'DeleteCompute' THEN 'Prod'
        WHEN 'DestroyProdStorage' THEN 'Prod'
        WHEN 'ClearProdStorage' THEN 'Prod'
        WHEN 'PurgeResources' THEN 'Application'
        WHEN 'DeleteApplication' THEN 'Application'
    END)
WHERE json_extract(record, '$.scope') IS NULL;

CREATE TRIGGER userapp_scope_migration_guard
BEFORE UPDATE ON userapp_operations
WHEN json_extract(NEW.record, '$.scope') IS NULL
BEGIN
    SELECT RAISE(ABORT, 'userapp operation kind has no scope mapping');
END;
UPDATE userapp_operations SET record = record WHERE json_extract(record, '$.scope') IS NULL;
DROP TRIGGER userapp_scope_migration_guard;

CREATE TRIGGER userapp_pointer_migration_guard
BEFORE UPDATE ON userapp_lifecycles
WHEN json_extract(OLD.record, '$.current_operation_id') IS NOT NULL
     AND NOT EXISTS (
        SELECT 1 FROM userapp_operations o
        WHERE o.operation_id = json_extract(OLD.record, '$.current_operation_id')
          AND o.app_id = OLD.app_id)
BEGIN
    SELECT RAISE(ABORT, 'userapp lifecycle current_operation_id is dangling');
END;

-- Terminal pointed operations never occupied a slot: their ids stay historical.
UPDATE userapp_lifecycles
SET record = (
    SELECT json_set(
               json_remove(l2.record, '$.current_operation_id'),
               '$.active_operations',
               COALESCE((
                   SELECT json_object(
                       CASE json_extract(o.record, '$.scope')
                           WHEN 'Dev' THEN 'dev'
                           WHEN 'Prod' THEN 'prod'
                           ELSE 'application' END,
                       json_extract(l2.record, '$.current_operation_id'))
                   FROM userapp_operations o
                   WHERE o.operation_id = json_extract(l2.record, '$.current_operation_id')
                     AND o.app_id = l2.app_id
                     AND o.terminal = 0
               ), json('{}')))
    FROM userapp_lifecycles l2
    WHERE l2.app_id = userapp_lifecycles.app_id)
WHERE json_type(record, '$.current_operation_id') IS NOT NULL;

DROP TRIGGER userapp_pointer_migration_guard;
