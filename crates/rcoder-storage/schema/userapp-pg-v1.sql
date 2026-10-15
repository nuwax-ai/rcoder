-- RCoder UserApp baseline v1. Unreleased baseline; freeze at first release.
-- No whole-record JSON authority, no cascade deletion of lifecycle evidence.
CREATE TABLE userapps (
 app_id TEXT NOT NULL PRIMARY KEY CHECK(length(app_id)>0),
 lifecycle_id TEXT NOT NULL CHECK(length(lifecycle_id)>0),
 lifecycle_epoch BIGINT NOT NULL CHECK(lifecycle_epoch>=1),
 lifecycle_state TEXT NOT NULL CHECK(lifecycle_state IN ('active','deleting','deleted')),
 metadata_revision BIGINT NOT NULL CHECK(metadata_revision>=1),
 control_revision BIGINT NOT NULL DEFAULT 1 CHECK(control_revision>=1),
 name TEXT, tenant_id TEXT, space_id TEXT,
 recycle_enabled BOOLEAN, wake_on_traffic BOOLEAN,
 idle_timeout_seconds BIGINT CHECK(idle_timeout_seconds>=0),
 created_at_us BIGINT NOT NULL, updated_at_us BIGINT NOT NULL,
 UNIQUE(app_id,lifecycle_id)
);
CREATE TABLE userapp_operations (
 operation_id TEXT NOT NULL PRIMARY KEY CHECK(length(operation_id)>0),
 app_id TEXT NOT NULL REFERENCES userapps(app_id),
 lifecycle_id TEXT NOT NULL CHECK(length(lifecycle_id)>0),
 kind TEXT NOT NULL, scope TEXT NOT NULL,
 state TEXT NOT NULL CHECK(state IN ('pending','running','waiting_retry','recovery_required','succeeded','failed')),
 revision BIGINT NOT NULL CHECK(revision>=1),
 origin_request_id TEXT, request_fingerprint TEXT NOT NULL CHECK(length(request_fingerprint)=64),
 executor_id TEXT, step TEXT NOT NULL, error_code TEXT, error_message TEXT,
 payload_version BIGINT NOT NULL CHECK(payload_version=1),
 command_json TEXT, admitted_metadata_json TEXT, runtime_policy_on_success_json TEXT,
 checkpoint_json TEXT NOT NULL,
 created_at_us BIGINT NOT NULL, updated_at_us BIGINT NOT NULL, terminal_at_us BIGINT,
 UNIQUE(operation_id,app_id,lifecycle_id),
 UNIQUE(operation_id,app_id,lifecycle_id,scope),
 CONSTRAINT operation_scope CHECK(
  (scope='dev' AND kind IN ('ensure_builder','adopt_builder','stop_builder','restart_builder','destroy_dev_storage','clear_dev_storage','reset_dev_database_password')) OR
  (scope='prod' AND kind IN ('create','start_deployment','restart_deployment','update','start','restart','stop','set_recycle_policy','hot_deploy','delete_compute','destroy_prod_storage','clear_prod_storage','reset_prod_database_password','prepare_prod_database')) OR
  (scope='application' AND kind IN ('purge_resources','delete_application'))),
 CONSTRAINT operation_terminal CHECK((state IN ('succeeded','failed') AND terminal_at_us IS NOT NULL) OR (state NOT IN ('succeeded','failed') AND terminal_at_us IS NULL)),
 CONSTRAINT operation_executor CHECK(state<>'running' OR (executor_id IS NOT NULL AND length(executor_id)>0))
);
CREATE INDEX userapp_operations_recovery ON userapp_operations(state,app_id,operation_id);
CREATE INDEX userapp_operations_unfinished ON userapp_operations(operation_id) WHERE terminal_at_us IS NULL;
CREATE INDEX userapp_operations_history ON userapp_operations(app_id,lifecycle_id,created_at_us);
CREATE TABLE userapp_active_operations (
 app_id TEXT NOT NULL PRIMARY KEY, lifecycle_id TEXT NOT NULL,
 dev_operation_id TEXT, prod_operation_id TEXT, application_operation_id TEXT,
 FOREIGN KEY(app_id,lifecycle_id) REFERENCES userapps(app_id,lifecycle_id),
 FOREIGN KEY(dev_operation_id,app_id,lifecycle_id) REFERENCES userapp_operations(operation_id,app_id,lifecycle_id),
 FOREIGN KEY(prod_operation_id,app_id,lifecycle_id) REFERENCES userapp_operations(operation_id,app_id,lifecycle_id),
 FOREIGN KEY(application_operation_id,app_id,lifecycle_id) REFERENCES userapp_operations(operation_id,app_id,lifecycle_id),
 CONSTRAINT application_slot_exclusive CHECK(application_operation_id IS NULL OR (dev_operation_id IS NULL AND prod_operation_id IS NULL)),
 CONSTRAINT environment_slots_distinct CHECK(dev_operation_id IS NULL OR prod_operation_id IS NULL OR dev_operation_id<>prod_operation_id)
);
CREATE TABLE userapp_requests (
 app_id TEXT NOT NULL REFERENCES userapps(app_id), request_id TEXT NOT NULL CHECK(length(request_id)>0),
 target_kind TEXT NOT NULL, operation_id TEXT, lifecycle_id TEXT,
 previous_lifecycle_id TEXT, new_lifecycle_id TEXT, created_at_us BIGINT NOT NULL,
 PRIMARY KEY(app_id,request_id),
 FOREIGN KEY(operation_id,app_id,lifecycle_id) REFERENCES userapp_operations(operation_id,app_id,lifecycle_id),
 CONSTRAINT request_shape CHECK(
  (target_kind='control' AND operation_id IS NOT NULL AND lifecycle_id IS NOT NULL AND previous_lifecycle_id IS NULL AND new_lifecycle_id IS NULL) OR
  (target_kind='recreate' AND operation_id IS NULL AND lifecycle_id IS NULL AND previous_lifecycle_id IS NOT NULL AND new_lifecycle_id IS NOT NULL AND length(previous_lifecycle_id)>0 AND length(new_lifecycle_id)>0 AND previous_lifecycle_id<>new_lifecycle_id))
);
CREATE TABLE userapp_operation_inputs (
 operation_id TEXT NOT NULL PRIMARY KEY, app_id TEXT NOT NULL, lifecycle_id TEXT NOT NULL,
 payload_version BIGINT NOT NULL CHECK(payload_version=1), payload TEXT NOT NULL,
 payload_digest TEXT NOT NULL CHECK(length(payload_digest)=64), created_at_us BIGINT NOT NULL,
 FOREIGN KEY(operation_id,app_id,lifecycle_id) REFERENCES userapp_operations(operation_id,app_id,lifecycle_id)
);
CREATE TABLE userapp_operation_leases (
 operation_id TEXT NOT NULL PRIMARY KEY, app_id TEXT NOT NULL, lifecycle_id TEXT NOT NULL,
 executor_id TEXT NOT NULL CHECK(length(executor_id)>0), request_fingerprint TEXT NOT NULL CHECK(length(request_fingerprint)=64),
 receipt_version BIGINT NOT NULL CHECK(receipt_version=1), receipt_json TEXT NOT NULL, created_at_us BIGINT NOT NULL,
 FOREIGN KEY(operation_id,app_id,lifecycle_id) REFERENCES userapp_operations(operation_id,app_id,lifecycle_id)
);
CREATE TABLE userapp_operation_deadlines (
 operation_id TEXT NOT NULL PRIMARY KEY, app_id TEXT NOT NULL, lifecycle_id TEXT NOT NULL,
 deadline_ms BIGINT NOT NULL CHECK(deadline_ms>0),
 FOREIGN KEY(operation_id,app_id,lifecycle_id) REFERENCES userapp_operations(operation_id,app_id,lifecycle_id)
);
CREATE TABLE userapp_resource_bindings (
 service_type TEXT NOT NULL CHECK(service_type='user-app-builder'), physical_uid TEXT NOT NULL CHECK(length(physical_uid)>0),
 app_id TEXT NOT NULL, lifecycle_id TEXT NOT NULL, adopted_by_operation TEXT NOT NULL,
 created_at_us BIGINT NOT NULL, PRIMARY KEY(service_type,physical_uid),
 FOREIGN KEY(adopted_by_operation,app_id,lifecycle_id) REFERENCES userapp_operations(operation_id,app_id,lifecycle_id)
);
CREATE TABLE userapp_activity (
 app_id TEXT NOT NULL, lifecycle_id TEXT NOT NULL, scope TEXT NOT NULL CHECK(scope IN ('dev','prod')),
 last_accessed_at_us BIGINT NOT NULL, updated_at_us BIGINT NOT NULL,
 PRIMARY KEY(app_id,lifecycle_id,scope),
 FOREIGN KEY(app_id,lifecycle_id) REFERENCES userapps(app_id,lifecycle_id)
);
-- Immutable payload versions remain available for an operation captured before
-- a concurrent save. Recreate cannot overwrite an old lifecycle's credentials.
CREATE TABLE userapp_runtime_config_versions (
 app_id TEXT NOT NULL REFERENCES userapps(app_id), lifecycle_id TEXT NOT NULL,
 scope TEXT NOT NULL CHECK(scope IN ('dev','prod')), version BIGINT NOT NULL CHECK(version>=1),
 request_id TEXT NOT NULL CHECK(length(request_id)>0), expected_revision BIGINT NOT NULL CHECK(expected_revision>=0),
 payload_version BIGINT NOT NULL CHECK(payload_version=1),
 pg_username TEXT NOT NULL CHECK(length(pg_username)>0), pg_password TEXT NOT NULL CHECK(length(pg_password)>0),
 created_at_us BIGINT NOT NULL,
 PRIMARY KEY(app_id,lifecycle_id,scope,version),
 UNIQUE(app_id,lifecycle_id,scope,request_id)
);
CREATE TABLE userapp_runtime_configs (
 app_id TEXT NOT NULL, lifecycle_id TEXT NOT NULL, scope TEXT NOT NULL CHECK(scope IN ('dev','prod')),
 revision BIGINT NOT NULL CHECK(revision>=1), saved_version BIGINT NOT NULL,
 applied_version BIGINT, applying_version BIGINT, applying_operation_id TEXT,
 updated_at_us BIGINT NOT NULL, PRIMARY KEY(app_id,lifecycle_id,scope),
 FOREIGN KEY(app_id,lifecycle_id,scope,saved_version) REFERENCES userapp_runtime_config_versions(app_id,lifecycle_id,scope,version),
 FOREIGN KEY(app_id,lifecycle_id,scope,applied_version) REFERENCES userapp_runtime_config_versions(app_id,lifecycle_id,scope,version),
 FOREIGN KEY(app_id,lifecycle_id,scope,applying_version) REFERENCES userapp_runtime_config_versions(app_id,lifecycle_id,scope,version),
 FOREIGN KEY(applying_operation_id,app_id,lifecycle_id,scope) REFERENCES userapp_operations(operation_id,app_id,lifecycle_id,scope),
 CONSTRAINT runtime_applying_pair CHECK((applying_version IS NULL AND applying_operation_id IS NULL) OR (applying_version IS NOT NULL AND applying_operation_id IS NOT NULL))
);
CREATE TABLE userapp_operation_configs (
 operation_id TEXT NOT NULL PRIMARY KEY, app_id TEXT NOT NULL, lifecycle_id TEXT NOT NULL,
 scope TEXT NOT NULL CHECK(scope IN ('dev','prod')), config_version BIGINT NOT NULL,
 physical_uid TEXT, deployment_generation TEXT,
 credential_state TEXT NOT NULL CHECK(credential_state IN ('captured','applying','applied','failed','unknown')),
 business_state TEXT NOT NULL CHECK(business_state IN ('not_started','starting','ready','failed','unknown')),
 created_at_us BIGINT NOT NULL, updated_at_us BIGINT NOT NULL,
 FOREIGN KEY(operation_id,app_id,lifecycle_id,scope) REFERENCES userapp_operations(operation_id,app_id,lifecycle_id,scope),
 FOREIGN KEY(app_id,lifecycle_id,scope,config_version) REFERENCES userapp_runtime_config_versions(app_id,lifecycle_id,scope,version),
 CONSTRAINT runtime_physical_pair CHECK((physical_uid IS NULL AND deployment_generation IS NULL) OR (physical_uid IS NOT NULL AND length(physical_uid)>0 AND deployment_generation IS NOT NULL AND length(deployment_generation)>0)),
 CONSTRAINT runtime_application_identity CHECK(credential_state='captured' OR (physical_uid IS NOT NULL AND deployment_generation IS NOT NULL))
);
