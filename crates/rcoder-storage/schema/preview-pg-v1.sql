-- RCoder Preview baseline v1. Active port ownership is global per database.
CREATE TABLE preview_instances (
 preview_key TEXT NOT NULL PRIMARY KEY CHECK(length(preview_key)>0),
 project_id TEXT NOT NULL, project_path TEXT NOT NULL,
 instance_id TEXT NOT NULL UNIQUE CHECK(length(instance_id)>0),
 revision BIGINT NOT NULL CHECK(revision>=1),
 operation_id TEXT, host_id TEXT NOT NULL,
 pod_name TEXT, pod_ip TEXT, pid BIGINT CHECK(pid>0),
 port BIGINT CHECK(port BETWEEN 1 AND 65535), base_path TEXT,
 state TEXT NOT NULL CHECK(state IN ('starting','ready','stopping','stopped','failed','unknown')),
 last_heartbeat_at_us BIGINT, last_activity_at_us BIGINT NOT NULL,
 detail TEXT, updated_at_us BIGINT NOT NULL,
 CONSTRAINT preview_active_port CHECK(state NOT IN ('starting','ready','stopping','unknown') OR port IS NOT NULL)
);
CREATE UNIQUE INDEX preview_active_port_unique ON preview_instances(port) WHERE state IN ('starting','ready','stopping','unknown');
CREATE INDEX preview_active_host ON preview_instances(host_id) WHERE state IN ('starting','ready','stopping','unknown');
CREATE TABLE preview_operations (
 operation_id TEXT NOT NULL PRIMARY KEY CHECK(length(operation_id)>0),
 preview_key TEXT NOT NULL REFERENCES preview_instances(preview_key),
 instance_id TEXT NOT NULL CHECK(length(instance_id)>0),
 request_fingerprint TEXT NOT NULL CHECK(length(request_fingerprint)=64),
 kind TEXT NOT NULL CHECK(kind IN ('start','stop')),
 state TEXT NOT NULL CHECK(state IN ('accepted','running','succeeded','failed','uncertain')),
 host_id TEXT NOT NULL,
 requested_port BIGINT CHECK(requested_port BETWEEN 1 AND 65535),
 allocated_port BIGINT CHECK(allocated_port BETWEEN 1 AND 65535),
 payload_version BIGINT NOT NULL CHECK(payload_version=1),
 request_json TEXT NOT NULL, result_json TEXT,
 created_at_us BIGINT NOT NULL, updated_at_us BIGINT NOT NULL
);
CREATE INDEX preview_operations_key ON preview_operations(preview_key,created_at_us,operation_id);
