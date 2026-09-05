PRAGMA foreign_keys = ON;

CREATE TABLE settings (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
) STRICT;

CREATE TABLE principals (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL UNIQUE COLLATE NOCASE,
    kind TEXT NOT NULL CHECK (kind IN ('human', 'service', 'worker')),
    password_hash TEXT,
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    created_at TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0)
) STRICT;

CREATE TABLE roles (
    name TEXT PRIMARY KEY NOT NULL,
    built_in INTEGER NOT NULL CHECK (built_in IN (0, 1)),
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0)
) STRICT;

CREATE TABLE role_permissions (
    role_name TEXT NOT NULL REFERENCES roles(name) ON DELETE CASCADE,
    permission TEXT NOT NULL,
    PRIMARY KEY (role_name, permission)
) STRICT;

CREATE TABLE principal_roles (
    principal_id TEXT NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
    role_name TEXT NOT NULL REFERENCES roles(name) ON DELETE RESTRICT,
    PRIMARY KEY (principal_id, role_name)
) STRICT;

CREATE TABLE credentials (
    id TEXT PRIMARY KEY NOT NULL,
    principal_id TEXT NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
    name TEXT,
    kind TEXT NOT NULL CHECK (kind IN ('session', 'api', 'worker')),
    token_hash TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL,
    expires_at TEXT,
    revoked_at TEXT
) STRICT;
CREATE INDEX credentials_principal_idx ON credentials(principal_id);
CREATE UNIQUE INDEX credentials_api_name_unique
    ON credentials(principal_id, name) WHERE kind = 'api';

CREATE TABLE audit_events (
    id TEXT PRIMARY KEY NOT NULL,
    actor_json TEXT NOT NULL,
    action TEXT NOT NULL,
    resource_kind TEXT NOT NULL,
    resource_id TEXT,
    details_json TEXT NOT NULL,
    request_id TEXT,
    occurred_at TEXT NOT NULL
) STRICT;

CREATE TABLE software (
    id TEXT PRIMARY KEY NOT NULL,
    slug TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    created_at TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0)
) STRICT;

CREATE TABLE software_installation (
    software_id TEXT PRIMARY KEY NOT NULL REFERENCES software(id) ON DELETE CASCADE,
    install_json TEXT NOT NULL,
    detection_json TEXT NOT NULL
) STRICT;

CREATE TABLE releases (
    id TEXT PRIMARY KEY NOT NULL,
    software_id TEXT NOT NULL REFERENCES software(id) ON DELETE RESTRICT,
    version TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('discovered','built','inspected','verified','candidate','testing','stable','failed','rejected')),
    created_at TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0),
    UNIQUE (software_id, version)
) STRICT;

CREATE TABLE release_lifecycle_events (
    id TEXT PRIMARY KEY NOT NULL,
    release_id TEXT NOT NULL REFERENCES releases(id) ON DELETE RESTRICT,
    from_state TEXT,
    to_state TEXT NOT NULL,
    actor_json TEXT NOT NULL,
    reason TEXT,
    occurred_at TEXT NOT NULL
) STRICT;

CREATE TABLE variants (
    id TEXT PRIMARY KEY NOT NULL,
    release_id TEXT NOT NULL REFERENCES releases(id) ON DELETE RESTRICT,
    platform TEXT NOT NULL,
    architecture TEXT NOT NULL,
    minimum_macos TEXT,
    maximum_macos TEXT,
    resolution_priority INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE TABLE artifacts (
    digest TEXT PRIMARY KEY NOT NULL,
    size INTEGER NOT NULL CHECK (size >= 0),
    media_type TEXT NOT NULL,
    created_at TEXT NOT NULL
) STRICT;

CREATE TABLE variant_artifacts (
    variant_id TEXT NOT NULL REFERENCES variants(id) ON DELETE RESTRICT,
    digest TEXT NOT NULL REFERENCES artifacts(digest) ON DELETE RESTRICT,
    role TEXT NOT NULL,
    PRIMARY KEY (variant_id, digest, role)
) STRICT;
CREATE UNIQUE INDEX one_primary_installer_per_variant
    ON variant_artifacts(variant_id) WHERE role = 'primary_installer';

CREATE TABLE stores (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL UNIQUE,
    role TEXT NOT NULL CHECK (role IN ('primary','replica','cache','read_only')),
    kind TEXT NOT NULL,
    config_json TEXT NOT NULL DEFAULT '{}',
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    created_at TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0)
) STRICT;

CREATE TABLE artifact_locations (
    id TEXT PRIMARY KEY NOT NULL,
    digest TEXT NOT NULL REFERENCES artifacts(digest) ON DELETE RESTRICT,
    store_id TEXT NOT NULL REFERENCES stores(id) ON DELETE RESTRICT,
    state TEXT NOT NULL CHECK (state IN ('pending','replicating','present','remote','missing','corrupt','failed')),
    verified_at TEXT,
    last_error TEXT,
    UNIQUE (digest, store_id)
) STRICT;

CREATE TABLE channels (
    software_id TEXT NOT NULL REFERENCES software(id) ON DELETE RESTRICT,
    name TEXT NOT NULL,
    release_id TEXT NOT NULL REFERENCES releases(id) ON DELETE RESTRICT,
    pinned_variant_id TEXT REFERENCES variants(id) ON DELETE RESTRICT,
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0),
    PRIMARY KEY (software_id, name)
) STRICT;

CREATE TABLE promotion_events (
    id TEXT PRIMARY KEY NOT NULL,
    software_id TEXT NOT NULL REFERENCES software(id) ON DELETE RESTRICT,
    channel_name TEXT NOT NULL,
    previous_release_id TEXT REFERENCES releases(id) ON DELETE RESTRICT,
    release_id TEXT NOT NULL REFERENCES releases(id) ON DELETE RESTRICT,
    pinned_variant_id TEXT REFERENCES variants(id) ON DELETE RESTRICT,
    actor_json TEXT NOT NULL,
    reason TEXT,
    occurred_at TEXT NOT NULL
) STRICT;

CREATE TABLE recipes (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0)
) STRICT;

CREATE TABLE recipe_revisions (
    id TEXT PRIMARY KEY NOT NULL,
    recipe_id TEXT NOT NULL REFERENCES recipes(id) ON DELETE RESTRICT,
    sequence INTEGER NOT NULL CHECK (sequence > 0),
    definition_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    builder TEXT NOT NULL DEFAULT 'autopkg',
    required_capabilities_json TEXT NOT NULL DEFAULT '[]',
    UNIQUE (recipe_id, sequence)
) STRICT;

CREATE TABLE runs (
    id TEXT PRIMARY KEY NOT NULL,
    recipe_revision_id TEXT REFERENCES recipe_revisions(id) ON DELETE RESTRICT,
    software_id TEXT REFERENCES software(id) ON DELETE RESTRICT,
    state TEXT NOT NULL CHECK (state IN ('queued','running','succeeded','failed','cancelled')),
    parameters_json TEXT NOT NULL DEFAULT '{}',
    result_json TEXT,
    created_at TEXT NOT NULL,
    completed_at TEXT
) STRICT;

CREATE TABLE run_logs (
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE RESTRICT,
    sequence INTEGER NOT NULL CHECK (sequence >= 0),
    stream TEXT NOT NULL CHECK (stream IN ('stdout','stderr','system')),
    message BLOB NOT NULL,
    occurred_at TEXT NOT NULL,
    attempt_id TEXT REFERENCES attempts(id) ON DELETE RESTRICT,
    PRIMARY KEY (run_id, sequence)
) STRICT;
CREATE INDEX run_logs_attempt_idx ON run_logs(attempt_id, sequence);

CREATE TABLE run_artifacts (
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE RESTRICT,
    digest TEXT NOT NULL REFERENCES artifacts(digest) ON DELETE RESTRICT,
    role TEXT NOT NULL,
    PRIMARY KEY (run_id, digest, role)
) STRICT;

CREATE TABLE run_provenance (
    run_id TEXT PRIMARY KEY NOT NULL REFERENCES runs(id) ON DELETE RESTRICT,
    provenance_json TEXT NOT NULL
) STRICT;

CREATE TABLE verification_results (
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE RESTRICT,
    check_name TEXT NOT NULL,
    required INTEGER NOT NULL CHECK (required IN (0, 1)),
    succeeded INTEGER NOT NULL CHECK (succeeded IN (0, 1)),
    detail TEXT,
    PRIMARY KEY (run_id, check_name)
) STRICT;

CREATE TABLE workers (
    id TEXT PRIMARY KEY NOT NULL,
    principal_id TEXT REFERENCES principals(id) ON DELETE RESTRICT,
    name TEXT NOT NULL UNIQUE,
    allowed_capabilities_json TEXT NOT NULL DEFAULT '[]',
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    registered_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0)
) STRICT;
CREATE UNIQUE INDEX workers_principal_idx ON workers(principal_id) WHERE principal_id IS NOT NULL;

CREATE TABLE worker_capabilities (
    worker_id TEXT NOT NULL REFERENCES workers(id) ON DELETE CASCADE,
    capability TEXT NOT NULL,
    PRIMARY KEY (worker_id, capability)
) STRICT;

CREATE TABLE recipe_catalog_snapshots (
    id TEXT PRIMARY KEY NOT NULL,
    worker_id TEXT NOT NULL REFERENCES workers(id) ON DELETE RESTRICT,
    schema_version INTEGER NOT NULL CHECK (schema_version > 0),
    producer TEXT NOT NULL,
    source_locator TEXT NOT NULL,
    source_revision TEXT NOT NULL,
    manifest_digest TEXT NOT NULL CHECK (
        length(manifest_digest) = 64 AND manifest_digest = lower(manifest_digest)
    ),
    recipe_count INTEGER NOT NULL CHECK (recipe_count >= 0),
    diagnostic_count INTEGER NOT NULL CHECK (diagnostic_count >= 0),
    manifest_json TEXT NOT NULL,
    observed_at TEXT NOT NULL,
    UNIQUE (worker_id, manifest_digest)
) STRICT;
CREATE INDEX recipe_catalog_snapshots_page_idx ON recipe_catalog_snapshots(id);
CREATE INDEX recipe_catalog_snapshots_latest_source_idx
    ON recipe_catalog_snapshots(producer, source_locator, observed_at DESC, id DESC);

CREATE TABLE jobs (
    id TEXT PRIMARY KEY NOT NULL,
    subject_kind TEXT NOT NULL CHECK (subject_kind IN ('build_run','recipe_catalog_scan')),
    subject_id TEXT NOT NULL,
    required_capabilities_json TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('queued','leased','succeeded','failed','cancelled')),
    maximum_attempts INTEGER NOT NULL CHECK (maximum_attempts > 0),
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    created_at TEXT NOT NULL,
    completed_at TEXT,
    result_json TEXT
) STRICT;
CREATE INDEX jobs_claim_idx ON jobs(state, created_at, id);
CREATE UNIQUE INDEX jobs_subject_idx ON jobs(subject_kind, subject_id);

CREATE TABLE recipe_catalog_scans (
    id TEXT PRIMARY KEY NOT NULL,
    job_id TEXT NOT NULL UNIQUE REFERENCES jobs(id) ON DELETE RESTRICT,
    producer TEXT NOT NULL,
    source_locator TEXT NOT NULL,
    source_revision TEXT NOT NULL,
    requested_at TEXT NOT NULL
) STRICT;
CREATE INDEX recipe_catalog_scans_page_idx ON recipe_catalog_scans(id);

CREATE TABLE recipe_catalog_scan_terminals (
    scan_id TEXT PRIMARY KEY NOT NULL REFERENCES recipe_catalog_scans(id) ON DELETE RESTRICT,
    snapshot_id TEXT REFERENCES recipe_catalog_snapshots(id) ON DELETE RESTRICT,
    failure_json TEXT,
    completed_at TEXT NOT NULL,
    CHECK ((snapshot_id IS NOT NULL) != (failure_json IS NOT NULL))
) STRICT;

CREATE TABLE attempts (
    id TEXT PRIMARY KEY NOT NULL,
    job_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE RESTRICT,
    worker_id TEXT NOT NULL REFERENCES workers(id) ON DELETE RESTRICT,
    state TEXT NOT NULL CHECK (state IN ('leased','succeeded','failed','expired')),
    heartbeat_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    completed_at TEXT,
    result_json TEXT
) STRICT;
CREATE INDEX attempts_active_idx ON attempts(job_id, state, expires_at);
CREATE INDEX attempts_expiry_idx ON attempts(state, expires_at, job_id);

CREATE TABLE run_releases (
    run_id TEXT PRIMARY KEY NOT NULL REFERENCES runs(id) ON DELETE RESTRICT,
    release_id TEXT NOT NULL REFERENCES releases(id) ON DELETE RESTRICT,
    disposition TEXT NOT NULL CHECK (
        disposition IN ('release_created', 'no_change', 'version_content_conflict')
    ),
    created_at TEXT NOT NULL
) STRICT;
CREATE UNIQUE INDEX one_creating_run_per_release
    ON run_releases(release_id) WHERE disposition = 'release_created';
CREATE INDEX run_releases_by_release
    ON run_releases(release_id, disposition, run_id);

CREATE TABLE build_targets (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL UNIQUE,
    software_id TEXT NOT NULL REFERENCES software(id) ON DELETE RESTRICT,
    recipe_revision_id TEXT NOT NULL REFERENCES recipe_revisions(id) ON DELETE RESTRICT,
    parameters_json TEXT NOT NULL,
    trigger_kind TEXT NOT NULL CHECK (trigger_kind IN ('manual', 'interval')),
    interval_seconds INTEGER,
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    next_run_at TEXT,
    revision INTEGER NOT NULL CHECK (revision > 0),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    CHECK (
        (trigger_kind = 'manual' AND interval_seconds IS NULL AND next_run_at IS NULL)
        OR
        (trigger_kind = 'interval' AND interval_seconds BETWEEN 60 AND 31536000
         AND next_run_at IS NOT NULL)
    )
) STRICT;

CREATE TABLE build_target_revisions (
    target_id TEXT NOT NULL REFERENCES build_targets(id) ON DELETE RESTRICT,
    revision INTEGER NOT NULL CHECK (revision > 0),
    name TEXT NOT NULL,
    software_id TEXT NOT NULL REFERENCES software(id) ON DELETE RESTRICT,
    recipe_revision_id TEXT NOT NULL REFERENCES recipe_revisions(id) ON DELETE RESTRICT,
    parameters_json TEXT NOT NULL,
    trigger_kind TEXT NOT NULL CHECK (trigger_kind IN ('manual', 'interval')),
    interval_seconds INTEGER,
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    next_run_at TEXT,
    changed_at TEXT NOT NULL,
    PRIMARY KEY (target_id, revision),
    CHECK (
        (trigger_kind = 'manual' AND interval_seconds IS NULL AND next_run_at IS NULL)
        OR
        (trigger_kind = 'interval' AND interval_seconds BETWEEN 60 AND 31536000
         AND next_run_at IS NOT NULL)
    )
) STRICT;

CREATE TABLE build_target_runs (
    run_id TEXT PRIMARY KEY NOT NULL REFERENCES runs(id) ON DELETE RESTRICT,
    target_id TEXT NOT NULL REFERENCES build_targets(id) ON DELETE RESTRICT,
    target_revision INTEGER NOT NULL CHECK (target_revision > 0),
    trigger_kind TEXT NOT NULL CHECK (trigger_kind IN ('manual', 'scheduled')),
    scheduled_for TEXT,
    next_run_at TEXT,
    created_at TEXT NOT NULL,
    CHECK (
        (trigger_kind = 'manual' AND scheduled_for IS NULL AND next_run_at IS NULL)
        OR
        (trigger_kind = 'scheduled' AND scheduled_for IS NOT NULL AND next_run_at IS NOT NULL)
    )
) STRICT;

CREATE INDEX build_targets_page_idx ON build_targets(id);
CREATE INDEX build_targets_due_idx
    ON build_targets(enabled, trigger_kind, next_run_at, id);
CREATE INDEX build_target_revisions_history_idx
    ON build_target_revisions(target_id, revision);
CREATE INDEX build_target_runs_target_idx
    ON build_target_runs(target_id, run_id);
CREATE UNIQUE INDEX one_scheduled_run_per_target_cursor
    ON build_target_runs(target_id, scheduled_for) WHERE trigger_kind = 'scheduled';

CREATE TABLE idempotency_keys (
    scope TEXT NOT NULL,
    key TEXT NOT NULL,
    response_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (scope, key)
) STRICT;

CREATE INDEX releases_software_page_idx ON releases(software_id, id);
CREATE INDEX variants_release_idx ON variants(release_id, id);
CREATE INDEX run_artifacts_run_idx ON run_artifacts(run_id, digest, role);
CREATE INDEX runs_recipe_idx ON runs(recipe_revision_id, id);
CREATE INDEX runs_software_idx ON runs(software_id, id);
CREATE INDEX workers_seen_idx ON workers(last_seen_at, id);
CREATE INDEX promotion_events_channel_idx
    ON promotion_events(software_id, channel_name, occurred_at, id);
CREATE INDEX lifecycle_events_release_idx
    ON release_lifecycle_events(release_id, occurred_at, id);
CREATE INDEX artifact_locations_digest_state_idx
    ON artifact_locations(digest, state, store_id);

CREATE TRIGGER audit_events_append_only_update
BEFORE UPDATE ON audit_events BEGIN SELECT RAISE(ABORT, 'audit events are append-only'); END;
CREATE TRIGGER audit_events_append_only_delete
BEFORE DELETE ON audit_events BEGIN SELECT RAISE(ABORT, 'audit events are append-only'); END;
CREATE TRIGGER lifecycle_events_append_only_update
BEFORE UPDATE ON release_lifecycle_events BEGIN SELECT RAISE(ABORT, 'lifecycle events are append-only'); END;
CREATE TRIGGER lifecycle_events_append_only_delete
BEFORE DELETE ON release_lifecycle_events BEGIN SELECT RAISE(ABORT, 'lifecycle events are append-only'); END;
CREATE TRIGGER promotion_events_append_only_update
BEFORE UPDATE ON promotion_events BEGIN SELECT RAISE(ABORT, 'promotion events are append-only'); END;
CREATE TRIGGER promotion_events_append_only_delete
BEFORE DELETE ON promotion_events BEGIN SELECT RAISE(ABORT, 'promotion events are append-only'); END;
CREATE TRIGGER recipe_revisions_immutable_update
BEFORE UPDATE ON recipe_revisions BEGIN SELECT RAISE(ABORT, 'recipe revisions are immutable'); END;
CREATE TRIGGER recipe_revisions_immutable_delete
BEFORE DELETE ON recipe_revisions BEGIN SELECT RAISE(ABORT, 'recipe revisions are immutable'); END;
CREATE TRIGGER recipe_catalog_snapshots_immutable_update
BEFORE UPDATE ON recipe_catalog_snapshots BEGIN SELECT RAISE(ABORT, 'recipe catalog snapshots are immutable'); END;
CREATE TRIGGER recipe_catalog_snapshots_immutable_delete
BEFORE DELETE ON recipe_catalog_snapshots BEGIN SELECT RAISE(ABORT, 'recipe catalog snapshots are immutable'); END;
CREATE TRIGGER run_logs_append_only_update
BEFORE UPDATE ON run_logs BEGIN SELECT RAISE(ABORT, 'run logs are append-only'); END;
CREATE TRIGGER run_logs_append_only_delete
BEFORE DELETE ON run_logs BEGIN SELECT RAISE(ABORT, 'run logs are append-only'); END;
