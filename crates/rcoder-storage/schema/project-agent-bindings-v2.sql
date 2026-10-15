-- Logical UserApp conversations do not add builder project/container references.
CREATE TABLE userapp_agent_bindings (
 binding_generation TEXT NOT NULL PRIMARY KEY CHECK(length(binding_generation)>0),
 app_id TEXT NOT NULL CHECK(length(app_id)>0),
 lifecycle_id TEXT NOT NULL CHECK(length(lifecycle_id)>0),
 project_id TEXT NOT NULL CHECK(length(project_id)>0),
 user_id TEXT NOT NULL CHECK(length(user_id)>0),
 revision BIGINT NOT NULL CHECK(revision>=1),
 current_session_id TEXT CHECK(length(current_session_id)>0),
 created_at_us BIGINT NOT NULL, updated_at_us BIGINT NOT NULL,
 UNIQUE(app_id,lifecycle_id,project_id), UNIQUE(user_id,project_id)
);
CREATE INDEX userapp_agent_bindings_app ON userapp_agent_bindings(app_id,lifecycle_id);
CREATE INDEX userapp_agent_bindings_project ON userapp_agent_bindings(project_id);
CREATE TABLE userapp_agent_sessions (
 session_id TEXT NOT NULL PRIMARY KEY CHECK(length(session_id)>0),
 session_generation TEXT NOT NULL CHECK(length(session_generation)>0),
 binding_generation TEXT NOT NULL REFERENCES userapp_agent_bindings(binding_generation),
 created_at_us BIGINT NOT NULL,
 UNIQUE(session_id,binding_generation)
);
CREATE INDEX userapp_agent_sessions_binding ON userapp_agent_sessions(binding_generation);
ALTER TABLE userapp_agent_bindings ADD CONSTRAINT userapp_agent_current_owner
 FOREIGN KEY(current_session_id,binding_generation)
 REFERENCES userapp_agent_sessions(session_id,binding_generation)
 DEFERRABLE INITIALLY DEFERRED;
