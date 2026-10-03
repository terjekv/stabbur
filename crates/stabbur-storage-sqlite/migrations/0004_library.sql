-- Index bounded library summaries without copying mutable publication state.
CREATE INDEX software_library_name ON software(name, id);
CREATE INDEX runs_software_recent ON runs(software_id, created_at DESC, id DESC);
CREATE INDEX runs_software_state ON runs(software_id, state, completed_at);
CREATE INDEX targets_software_enabled ON build_targets(software_id, enabled, next_run_at);
CREATE INDEX releases_software_review ON releases(software_id, state, created_at DESC, id DESC);
CREATE INDEX attempts_worker_active ON attempts(worker_id, state, expires_at);
