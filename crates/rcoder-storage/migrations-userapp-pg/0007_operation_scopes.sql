-- Operation scopes and per-scope active-operation slots (dev/prod/application).
-- One transaction: an unknown kind or a dangling current_operation_id aborts the
-- whole migration, leaving the database on the previous schema for diagnosis.

UPDATE userapp_operations
SET record = jsonb_set(record::jsonb, '{scope}', to_jsonb(
    CASE record::jsonb ->> 'kind'
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
    END))::text
WHERE record::jsonb ->> 'scope' IS NULL;

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM userapp_operations WHERE record::jsonb ->> 'scope' IS NULL) THEN
        RAISE EXCEPTION 'userapp operation kind has no scope mapping';
    END IF;
    IF EXISTS (
        SELECT 1 FROM userapp_lifecycles l
        WHERE l.record::jsonb ->> 'current_operation_id' IS NOT NULL
          AND NOT EXISTS (
            SELECT 1 FROM userapp_operations o
            WHERE o.operation_id = l.record::jsonb ->> 'current_operation_id'
              AND o.app_id = l.app_id)
    ) THEN
        RAISE EXCEPTION 'userapp lifecycle current_operation_id is dangling';
    END IF;
END $$;

-- Terminal pointed operations never occupied a slot: their ids stay historical.
UPDATE userapp_lifecycles l
SET record = sub.updated::text
FROM (
    SELECT l2.app_id,
           jsonb_set(
               l2.record::jsonb - 'current_operation_id',
               '{active_operations}',
               COALESCE((
                   SELECT jsonb_build_object(
                       CASE o.record::jsonb ->> 'scope'
                           WHEN 'Dev' THEN 'dev'
                           WHEN 'Prod' THEN 'prod'
                           ELSE 'application' END,
                       to_jsonb(l2.record::jsonb ->> 'current_operation_id'))
                   FROM userapp_operations o
                   WHERE o.operation_id = l2.record::jsonb ->> 'current_operation_id'
                     AND o.app_id = l2.app_id
                     AND o.terminal = 0
               ), '{}'::jsonb)
           ) AS updated
    FROM userapp_lifecycles l2
    WHERE l2.record::jsonb ? 'current_operation_id'
) AS sub
WHERE l.app_id = sub.app_id;
