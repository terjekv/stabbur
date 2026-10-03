CREATE TABLE exports (
    id TEXT PRIMARY KEY NOT NULL,
    slug TEXT UNIQUE NOT NULL,
    definition_json TEXT NOT NULL CHECK(json_valid(definition_json)),
    revision INTEGER NOT NULL CHECK(revision > 0),
    generation INTEGER NOT NULL DEFAULT 0 CHECK(generation >= 0),
    reader_epoch INTEGER NOT NULL DEFAULT 0 CHECK(reader_epoch >= 0),
    updated_at TEXT NOT NULL
) STRICT;
CREATE TABLE export_definitions (
    export_id TEXT NOT NULL REFERENCES exports(id) ON DELETE RESTRICT,
    revision INTEGER NOT NULL CHECK(revision > 0),
    definition_json TEXT NOT NULL CHECK(json_valid(definition_json)),
    created_at TEXT NOT NULL,
    PRIMARY KEY(export_id, revision)
) STRICT;
CREATE TABLE export_snapshots (
    export_id TEXT NOT NULL REFERENCES exports(id) ON DELETE RESTRICT,
    generation INTEGER NOT NULL CHECK(generation > 0),
    snapshot_json TEXT NOT NULL CHECK(json_valid(snapshot_json)),
    PRIMARY KEY(export_id, generation)
) STRICT;
CREATE TABLE export_readers (
    token_hash TEXT PRIMARY KEY NOT NULL,
    export_id TEXT NOT NULL REFERENCES exports(id) ON DELETE RESTRICT,
    epoch INTEGER NOT NULL CHECK(epoch >= 0),
    created_at TEXT NOT NULL
) STRICT;
CREATE INDEX export_readers_by_export ON export_readers(export_id, epoch);
