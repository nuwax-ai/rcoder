-- RCoder Project baseline v1. Generation authorizes replacement, time never does.
CREATE TABLE containers (
 container_name TEXT NOT NULL PRIMARY KEY CHECK(length(container_name)>0),
 container_generation TEXT NOT NULL CHECK(length(container_generation)>0),
 container_id TEXT CHECK(length(container_id)>0),
 logical_id TEXT NOT NULL CHECK(length(logical_id)>0), service_type TEXT NOT NULL,
 container_ip TEXT NOT NULL, internal_port BIGINT NOT NULL CHECK(internal_port BETWEEN 0 AND 65535),
 external_port BIGINT NOT NULL CHECK(external_port BETWEEN 0 AND 65535),
 status TEXT NOT NULL, service_url TEXT NOT NULL,
 last_activity_at_us BIGINT NOT NULL, created_at_us BIGINT NOT NULL,
 row_revision BIGINT NOT NULL CHECK(row_revision>=1),
 UNIQUE(container_name,container_generation)
);
CREATE INDEX containers_logical ON containers(service_type,logical_id);
CREATE INDEX containers_physical ON containers(container_id) WHERE container_id IS NOT NULL;
CREATE TABLE projects (
 project_id TEXT NOT NULL PRIMARY KEY CHECK(length(project_id)>0),
 generation TEXT NOT NULL CHECK(length(generation)>0),
 user_id TEXT, pod_id TEXT, tenant_id TEXT, space_id TEXT, isolation_type TEXT,
 container_name TEXT, container_generation TEXT,
 latest_session TEXT, model_provider_json TEXT, request_id TEXT, agent_status_json TEXT, service_type TEXT,
 payload_version BIGINT NOT NULL CHECK(payload_version=1),
 last_activity_at_us BIGINT NOT NULL, created_at_us BIGINT NOT NULL,
 row_revision BIGINT NOT NULL CHECK(row_revision>=1),
 UNIQUE(project_id,generation),
 FOREIGN KEY(container_name,container_generation) REFERENCES containers(container_name,container_generation),
 CONSTRAINT project_container_pair CHECK((container_name IS NULL AND container_generation IS NULL) OR (container_name IS NOT NULL AND container_generation IS NOT NULL))
);
CREATE INDEX projects_user ON projects(user_id) WHERE user_id IS NOT NULL;
CREATE INDEX projects_pod ON projects(pod_id) WHERE pod_id IS NOT NULL;
CREATE INDEX projects_tenant ON projects(tenant_id,space_id) WHERE tenant_id IS NOT NULL;
CREATE TABLE sessions (
 session_id TEXT NOT NULL PRIMARY KEY CHECK(length(session_id)>0),
 generation TEXT NOT NULL CHECK(length(generation)>0),
 project_id TEXT NOT NULL, project_generation TEXT NOT NULL,
 created_at_us BIGINT NOT NULL, last_seen_at_us BIGINT NOT NULL,
 FOREIGN KEY(project_id,project_generation) REFERENCES projects(project_id,generation)
);
CREATE INDEX sessions_project ON sessions(project_id,project_generation);
CREATE TABLE project_tombstones (
 project_id TEXT NOT NULL, generation TEXT NOT NULL, retired_at_us BIGINT NOT NULL,
 PRIMARY KEY(project_id,generation)
);
CREATE TABLE session_tombstones (
 session_id TEXT NOT NULL, generation TEXT NOT NULL, retired_at_us BIGINT NOT NULL,
 PRIMARY KEY(session_id,generation)
);
CREATE TABLE container_tombstones (
 container_name TEXT NOT NULL, container_generation TEXT NOT NULL, physical_uid TEXT,
 retired_at_us BIGINT NOT NULL, PRIMARY KEY(container_name,container_generation)
);
CREATE INDEX container_tombstones_physical ON container_tombstones(physical_uid) WHERE physical_uid IS NOT NULL;

-- Receipt and registration commit atomically. Keep receipts through generation
-- retirement so a lost COMMIT response cannot authorize replaying a stale write.
CREATE TABLE project_write_receipts (
 request_id TEXT NOT NULL PRIMARY KEY CHECK(length(request_id)>0),
 fingerprint TEXT NOT NULL CHECK(length(fingerprint)=64),
 outcome TEXT NOT NULL CHECK(outcome IN ('committed','superseded')),
 recorded_at_us BIGINT NOT NULL
);
