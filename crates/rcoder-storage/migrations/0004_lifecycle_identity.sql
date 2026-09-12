-- Requires all old writers to stop before rollout; old binaries do not carry generations.
-- Backfill identities once, retaining the same identities across every reload/sync.
ALTER TABLE projects ADD COLUMN generation text NOT NULL DEFAULT gen_random_uuid()::text;
ALTER TABLE sessions ADD COLUMN generation text NOT NULL DEFAULT gen_random_uuid()::text;
ALTER TABLE sessions ADD COLUMN project_generation text;
UPDATE sessions s SET project_generation = p.generation FROM projects p WHERE p.project_id = s.project_id;
ALTER TABLE sessions ALTER COLUMN project_generation SET NOT NULL;
CREATE TABLE project_tombstones (
    project_id text NOT NULL,
    generation text NOT NULL,
    PRIMARY KEY (project_id, generation)
);
CREATE TABLE session_tombstones (
    session_id text NOT NULL,
    generation text NOT NULL,
    PRIMARY KEY (session_id, generation)
);
CREATE TABLE container_tombstones (
    container_id text PRIMARY KEY
);
COMMENT ON TABLE project_tombstones IS 'Exact retired lifecycle identities; retain while old operations can replay';
