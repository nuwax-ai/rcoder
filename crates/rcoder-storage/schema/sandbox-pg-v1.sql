-- RCoder E2B Sandbox baseline v1. Unreleased baseline; freeze at first release.
-- No whole-record JSON authority, no cascade deletion of lifecycle evidence.
-- Template registry: head row points at immutable revisions; instances
-- denormalize the revision spec they were created from (survive registry edits).
CREATE TABLE sandbox_templates (
 template_id TEXT NOT NULL PRIMARY KEY CHECK(length(template_id)>0),
 current_revision BIGINT NOT NULL CHECK(current_revision>=1),
 enabled BOOLEAN NOT NULL,
 display_name TEXT,
 created_at_us BIGINT NOT NULL, updated_at_us BIGINT NOT NULL
);
CREATE TABLE sandbox_template_revisions (
 template_id TEXT NOT NULL,
 revision BIGINT NOT NULL CHECK(revision>=1),
 image TEXT NOT NULL CHECK(length(image)>0),
 image_digest TEXT,
 envd_version TEXT NOT NULL CHECK(length(envd_version)>0),
 runtime_class TEXT CHECK(runtime_class IS NULL OR length(runtime_class)>0),
 cpu_millicores BIGINT NOT NULL CHECK(cpu_millicores>0),
 memory_mib BIGINT NOT NULL CHECK(memory_mib>0),
 default_user TEXT NOT NULL,
 default_workdir TEXT NOT NULL,
 default_env_json TEXT,
 ttl_max_seconds BIGINT NOT NULL CHECK(ttl_max_seconds>0),
 warm_target BIGINT NOT NULL DEFAULT 0 CHECK(warm_target>=0),
 created_at_us BIGINT NOT NULL,
 PRIMARY KEY(template_id,revision)
);
CREATE TABLE sandbox_api_keys (
 key_id TEXT NOT NULL PRIMARY KEY CHECK(length(key_id)>0),
 tenant_id TEXT NOT NULL CHECK(length(tenant_id)>0),
 public_prefix TEXT NOT NULL CHECK(length(public_prefix)>0),
 key_hash TEXT NOT NULL UNIQUE CHECK(length(key_hash)>0),
 scopes TEXT NOT NULL DEFAULT 'sandbox' CHECK(scopes='sandbox' OR scopes='sandbox,admin'),
 created_at_us BIGINT NOT NULL,
 expires_at_us BIGINT, revoked_at_us BIGINT,
 CONSTRAINT api_key_revocation CHECK(revoked_at_us IS NULL OR expires_at_us IS NULL OR revoked_at_us<=expires_at_us)
);
CREATE TABLE sandbox_instances (
 sandbox_id TEXT NOT NULL PRIMARY KEY CHECK(length(sandbox_id)>0),
 tenant_id TEXT NOT NULL CHECK(length(tenant_id)>0),
 key_id TEXT REFERENCES sandbox_api_keys(key_id),
 template_id TEXT NOT NULL CHECK(length(template_id)>0),
 template_revision BIGINT NOT NULL CHECK(template_revision>=1),
 image TEXT NOT NULL CHECK(length(image)>0),
 image_digest TEXT,
 envd_version TEXT NOT NULL CHECK(length(envd_version)>0),
 runtime_class TEXT,
 cpu_millicores BIGINT NOT NULL CHECK(cpu_millicores>0),
 memory_mib BIGINT NOT NULL CHECK(memory_mib>0),
 desired_state TEXT NOT NULL CHECK(desired_state IN ('provisioning','initializing','ready','terminating','cleanup_pending','deleted','failed')),
 observed_state TEXT,
 instance_revision BIGINT NOT NULL DEFAULT 1 CHECK(instance_revision>=1),
 operation_id TEXT,
 namespace TEXT NOT NULL CHECK(length(namespace)>0),
 pod_name TEXT NOT NULL CHECK(length(pod_name)>0),
 pod_uid TEXT,
 token_hash TEXT NOT NULL CHECK(length(token_hash)>0),
 encrypted_token TEXT,
 token_generation BIGINT NOT NULL DEFAULT 1 CHECK(token_generation>=1),
 env_json TEXT,
 metadata_json TEXT,
 idempotency_key TEXT,
 failure_reason TEXT,
 created_at_us BIGINT NOT NULL, updated_at_us BIGINT NOT NULL,
 ready_at_us BIGINT, expires_at_us BIGINT, hard_deadline_us BIGINT, deleted_at_us BIGINT,
 CONSTRAINT sandbox_deadline CHECK(hard_deadline_us IS NULL OR expires_at_us IS NULL OR expires_at_us<=hard_deadline_us),
 CONSTRAINT sandbox_terminal CHECK((desired_state IN ('deleted','failed') AND deleted_at_us IS NOT NULL) OR (desired_state NOT IN ('deleted','failed') AND deleted_at_us IS NULL))
);
CREATE UNIQUE INDEX sandbox_instances_pod ON sandbox_instances(pod_name) WHERE deleted_at_us IS NULL;
CREATE INDEX sandbox_instances_list ON sandbox_instances(tenant_id,created_at_us,sandbox_id);
CREATE INDEX sandbox_instances_expiry ON sandbox_instances(expires_at_us) WHERE deleted_at_us IS NULL;
CREATE INDEX sandbox_instances_reconcile ON sandbox_instances(desired_state) WHERE deleted_at_us IS NULL;
CREATE TABLE sandbox_volumes (
 volume_id TEXT NOT NULL PRIMARY KEY CHECK(length(volume_id)>0),
 tenant_id TEXT NOT NULL CHECK(length(tenant_id)>0),
 key_id TEXT REFERENCES sandbox_api_keys(key_id),
 name TEXT NOT NULL CHECK(length(name)>0),
 pvc_name TEXT NOT NULL CHECK(length(pvc_name)>0),
 storage_class TEXT NOT NULL,
 size_gib INTEGER NOT NULL CHECK(size_gib>0 AND size_gib<=100),
 created_at_us BIGINT NOT NULL, updated_at_us BIGINT NOT NULL,
 deleted_at_us BIGINT,
 CONSTRAINT sandbox_volume_terminal CHECK((deleted_at_us IS NOT NULL) OR (deleted_at_us IS NULL))
);
CREATE INDEX sandbox_volumes_list ON sandbox_volumes(tenant_id,created_at_us,volume_id);
CREATE UNIQUE INDEX sandbox_volumes_name ON sandbox_volumes(tenant_id,name) WHERE deleted_at_us IS NULL;
CREATE UNIQUE INDEX sandbox_instances_idem ON sandbox_instances(idempotency_key) WHERE deleted_at_us IS NULL AND idempotency_key IS NOT NULL;
