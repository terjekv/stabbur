-- Publication eligibility does not erase the lifecycle stage a release attained.
ALTER TABLE releases ADD COLUMN availability_json TEXT NOT NULL
    DEFAULT '{"kind":"available"}' CHECK (json_valid(availability_json));
CREATE TABLE release_availability_events (
    id TEXT PRIMARY KEY NOT NULL,
    release_id TEXT NOT NULL REFERENCES releases(id) ON DELETE RESTRICT,
    availability_json TEXT NOT NULL CHECK (json_valid(availability_json)),
    actor_json TEXT NOT NULL,
    occurred_at TEXT NOT NULL
) STRICT;
CREATE INDEX release_availability_history ON release_availability_events(release_id, id);

ALTER TABLE workers ADD COLUMN draining INTEGER NOT NULL DEFAULT 0 CHECK (draining IN (0,1));

-- Preserve all existing evidence while widening the outcome vocabulary.
CREATE TABLE run_releases_next (
    run_id TEXT PRIMARY KEY NOT NULL REFERENCES runs(id) ON DELETE RESTRICT,
    release_id TEXT NOT NULL REFERENCES releases(id) ON DELETE RESTRICT,
    disposition TEXT NOT NULL CHECK (disposition IN
        ('release_created','no_change','evidence_changed','verification_failed','version_content_conflict')),
    created_at TEXT NOT NULL
) STRICT;
INSERT INTO run_releases_next SELECT * FROM run_releases;
DROP TABLE run_releases;
ALTER TABLE run_releases_next RENAME TO run_releases;
CREATE UNIQUE INDEX one_creating_run_per_release
    ON run_releases(release_id) WHERE disposition = 'release_created';
CREATE INDEX run_releases_by_release ON run_releases(release_id, disposition, run_id);
