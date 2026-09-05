//! SQLite implementation of the recipes ports.
use super::{
    AuditEvent, CompletionOutcome, DateTime, Job, JobState, JobSubject, Lease, NewRecipeRevision,
    RECIPE_CATALOG_SCAN_SELECT, RecipeCatalogMatch, RecipeCatalogPublication,
    RecipeCatalogPublishOutcome, RecipeCatalogScanCompletion, RecipeCatalogScanCreation,
    RecipeCatalogScanExecutionFailure, RecipeCatalogScanExecutionResult, RecipeCatalogScanId,
    RecipeCatalogScanJob, RecipeCatalogScanRecord, RecipeCatalogScanStorage,
    RecipeCatalogScanSummaryRecord, RecipeCatalogSnapshotId, RecipeCatalogSnapshotRecord,
    RecipeCatalogSnapshotSummaryRecord, RecipeCatalogStorage, RecipeId, RecipeRecord,
    RecipeRevisionId, RecipeRevisionRecord, RecipeStorage, Row, SqliteStorage, StorageError, Utc,
    WorkerId, async_trait, backend, insert_audit, invalid_data, job_completion_scope,
    job_state_name, job_subject_fields, map_write, persist_recipe_catalog_snapshot, query,
    query_scalar, recipe_catalog_scan_from_row, recipe_catalog_scan_summary_from_row,
    recipe_catalog_snapshot_from_row, recipe_catalog_summary_from_row, recipe_from_row,
    recipe_revision_from_row,
};

#[async_trait]
impl RecipeStorage for SqliteStorage {
    async fn create_recipe(
        &self,
        recipe: &RecipeRecord,
        audit: &AuditEvent,
    ) -> Result<(), StorageError> {
        if recipe.name.trim() != recipe.name || !(1..=128).contains(&recipe.name.len()) {
            return Err(invalid_data("recipe name must contain 1-128 bytes"));
        }
        let revision = i64::try_from(recipe.revision)
            .map_err(|_| invalid_data("recipe revision exceeds SQLite range"))?;
        let mut connection = self.acquire_write().await?;
        query("INSERT INTO recipes (id, name, created_at, revision) VALUES (?, ?, ?, ?)")
            .bind(recipe.id.to_string())
            .bind(&recipe.name)
            .bind(recipe.created_at)
            .bind(revision)
            .execute(&mut *connection)
            .await
            .map_err(map_write)?;
        insert_audit(&mut connection, audit).await?;
        connection.commit("committing recipe creation").await?;
        Ok(())
    }

    async fn recipe(&self, identity: &str) -> Result<Option<RecipeRecord>, StorageError> {
        let row =
            query("SELECT id, name, created_at, revision FROM recipes WHERE id = ? OR name = ?")
                .bind(identity)
                .bind(identity)
                .fetch_optional(&self.pool)
                .await
                .map_err(|error| backend("loading recipe", &error))?;
        row.as_ref().map(recipe_from_row).transpose()
    }

    async fn create_recipe_revision(
        &self,
        revision: &NewRecipeRevision,
        audit: &AuditEvent,
    ) -> Result<RecipeRevisionRecord, StorageError> {
        if revision.builder.trim() != revision.builder
            || !(1..=64).contains(&revision.builder.len())
        {
            return Err(invalid_data("builder name must contain 1-64 bytes"));
        }
        let definition_json = serde_json::to_string(&revision.definition)
            .map_err(|error| invalid_data(format!("serializing recipe definition: {error}")))?;
        let required_json = serde_json::to_string(&revision.required_capabilities)
            .map_err(|error| invalid_data(format!("serializing recipe capabilities: {error}")))?;
        let mut connection = self.acquire_write().await?;
        let next_sequence: i64 = query_scalar(
            "SELECT COALESCE(MAX(sequence), 0) + 1 FROM recipe_revisions WHERE recipe_id = ?",
        )
        .bind(revision.recipe_id.to_string())
        .fetch_one(&mut *connection)
        .await
        .map_err(|error| backend("allocating recipe revision sequence", &error))?;
        if revision
            .expected_sequence
            .is_some_and(|expected| u64::try_from(next_sequence) != Ok(expected.get()))
        {
            return Err(StorageError::StaleRevision);
        }
        query(
            "INSERT INTO recipe_revisions
             (id, recipe_id, sequence, definition_json, created_at, builder,
              required_capabilities_json)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(revision.id.to_string())
        .bind(revision.recipe_id.to_string())
        .bind(next_sequence)
        .bind(&definition_json)
        .bind(revision.created_at)
        .bind(&revision.builder)
        .bind(&required_json)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        insert_audit(&mut connection, audit).await?;
        connection.commit("committing recipe revision").await?;
        Ok(RecipeRevisionRecord {
            id: revision.id,
            recipe_id: revision.recipe_id,
            sequence: u64::try_from(next_sequence)
                .map_err(|_| invalid_data("negative recipe sequence"))?,
            builder: revision.builder.clone(),
            definition: revision.definition.clone(),
            required_capabilities: revision.required_capabilities.clone(),
            created_at: revision.created_at,
        })
    }

    async fn recipe_revision(
        &self,
        revision_id: RecipeRevisionId,
    ) -> Result<Option<RecipeRevisionRecord>, StorageError> {
        let row = query(
            "SELECT id, recipe_id, sequence, definition_json, created_at, builder,
                    required_capabilities_json
             FROM recipe_revisions WHERE id = ?",
        )
        .bind(revision_id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| backend("loading recipe revision", &error))?;
        row.as_ref().map(recipe_revision_from_row).transpose()
    }

    async fn list_recipes(
        &self,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<RecipeRecord>, StorageError> {
        if !(1..=200).contains(&limit) {
            return Err(invalid_data("recipe page limit must be between 1 and 200"));
        }
        let rows = query(
            "SELECT id, name, created_at, revision FROM recipes
             WHERE id > ? ORDER BY id LIMIT ?",
        )
        .bind(after.unwrap_or(""))
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(|error| backend("listing recipes", &error))?;
        rows.iter().map(recipe_from_row).collect()
    }

    async fn recipe_revisions(
        &self,
        recipe_id: RecipeId,
    ) -> Result<Vec<RecipeRevisionRecord>, StorageError> {
        let rows = query(
            "SELECT id, recipe_id, sequence, definition_json, created_at, builder,
                    required_capabilities_json FROM recipe_revisions
             WHERE recipe_id = ? ORDER BY sequence",
        )
        .bind(recipe_id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(|error| backend("listing recipe revisions", &error))?;
        rows.iter().map(recipe_revision_from_row).collect()
    }
}

#[async_trait]
impl RecipeCatalogStorage for SqliteStorage {
    async fn publish_recipe_catalog(
        &self,
        snapshot: &RecipeCatalogSnapshotRecord,
        audit: &AuditEvent,
    ) -> Result<RecipeCatalogPublication, StorageError> {
        let mut connection = self.acquire_write().await?;
        let publication = persist_recipe_catalog_snapshot(&mut connection, snapshot).await?;
        if publication.outcome == RecipeCatalogPublishOutcome::Published {
            insert_audit(&mut connection, audit).await?;
        }
        connection
            .commit("committing recipe catalog publication")
            .await?;
        Ok(publication)
    }

    async fn recipe_catalog_snapshot(
        &self,
        snapshot_id: RecipeCatalogSnapshotId,
    ) -> Result<Option<RecipeCatalogSnapshotRecord>, StorageError> {
        let row = query(
            "SELECT id, worker_id, schema_version, producer, source_locator, source_revision,
                    manifest_digest, recipe_count, diagnostic_count, manifest_json, observed_at
             FROM recipe_catalog_snapshots WHERE id = ?",
        )
        .bind(snapshot_id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| backend("loading recipe catalog snapshot", &error))?;
        row.as_ref()
            .map(recipe_catalog_snapshot_from_row)
            .transpose()
    }

    async fn list_recipe_catalog_snapshots(
        &self,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<RecipeCatalogSnapshotSummaryRecord>, StorageError> {
        if !(1..=200).contains(&limit) {
            return Err(invalid_data(
                "recipe catalog page limit must be between 1 and 200",
            ));
        }
        let rows = query(
            "SELECT id, worker_id, producer, source_locator, source_revision,
                    manifest_digest, recipe_count, diagnostic_count, observed_at
             FROM recipe_catalog_snapshots WHERE id > ? ORDER BY id LIMIT ?",
        )
        .bind(after.unwrap_or(""))
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(|error| backend("listing recipe catalog snapshots", &error))?;
        rows.iter().map(recipe_catalog_summary_from_row).collect()
    }

    async fn latest_recipe_catalog_matches(
        &self,
        identifier: &str,
    ) -> Result<Vec<RecipeCatalogMatch>, StorageError> {
        if identifier.trim() != identifier || !(1..=2_048).contains(&identifier.len()) {
            return Err(invalid_data("recipe catalog identifier is invalid"));
        }
        let rows = query(
            "SELECT s.id, s.worker_id, s.schema_version, s.producer, s.source_locator,
                    s.source_revision, s.manifest_digest, s.recipe_count, s.diagnostic_count,
                    s.manifest_json, s.observed_at,
                    entry.value AS recipe_json
             FROM recipe_catalog_snapshots s
             JOIN json_each(s.manifest_json, '$.recipes') entry
             WHERE json_extract(entry.value, '$.identifier') = ?
               AND s.id = (
                   SELECT latest.id FROM recipe_catalog_snapshots latest
                   WHERE latest.producer = s.producer
                     AND latest.source_locator = s.source_locator
                   ORDER BY latest.observed_at DESC, latest.id DESC LIMIT 1
               )
             ORDER BY s.producer, s.source_locator, s.id",
        )
        .bind(identifier)
        .fetch_all(&self.pool)
        .await
        .map_err(|error| backend("resolving recipe catalog identifier", &error))?;
        rows.iter()
            .map(|row| {
                let snapshot = recipe_catalog_snapshot_from_row(row)?;
                let recipe_json: String = row
                    .try_get("recipe_json")
                    .map_err(|error| backend("decoding recipe catalog entry", &error))?;
                let recipe = serde_json::from_str(&recipe_json).map_err(|error| {
                    invalid_data(format!("invalid recipe catalog entry: {error}"))
                })?;
                Ok(RecipeCatalogMatch {
                    snapshot_id: snapshot.id,
                    worker_id: snapshot.worker_id,
                    producer: snapshot.manifest.producer,
                    source_locator: snapshot.manifest.source.locator,
                    source_revision: snapshot.manifest.source.revision,
                    recipe,
                    observed_at: snapshot.observed_at,
                })
            })
            .collect()
    }
}

#[async_trait]
impl RecipeCatalogScanStorage for SqliteStorage {
    #[allow(clippy::too_many_lines)] // Scan, job, idempotency, and audit creation must be atomic.
    async fn create_recipe_catalog_scan(
        &self,
        scan: &RecipeCatalogScanRecord,
        job: &Job,
        idempotency_scope: &str,
        idempotency_key: &str,
        audit: &AuditEvent,
    ) -> Result<RecipeCatalogScanCreation, StorageError> {
        if idempotency_scope.is_empty()
            || idempotency_key.is_empty()
            || idempotency_key.len() > 255
            || scan.state != JobState::Queued
            || scan.snapshot_id.is_some()
            || scan.failure.is_some()
            || scan.completed_at.is_some()
            || scan.job_id != job.id
            || job.subject != (JobSubject::RecipeCatalogScan { scan_id: scan.id })
            || job.state != JobState::Queued
        {
            return Err(invalid_data("catalog scan creation is invalid"));
        }
        let envelope: RecipeCatalogScanJob = serde_json::from_value(job.payload.clone())
            .map_err(|error| invalid_data(format!("invalid catalog scan job: {error}")))?;
        envelope
            .validate()
            .map_err(|error| invalid_data(format!("invalid catalog scan request: {error}")))?;
        if envelope.request.scan_id != scan.id
            || envelope.request.producer != scan.producer
            || envelope.request.source != scan.source
            || envelope.request.required_capabilities != job.required_capabilities
        {
            return Err(invalid_data(
                "catalog scan job differs from its durable request",
            ));
        }
        let mut connection = self.acquire_write().await?;
        let replay: Option<String> =
            query_scalar("SELECT response_json FROM idempotency_keys WHERE scope = ? AND key = ?")
                .bind(idempotency_scope)
                .bind(idempotency_key)
                .fetch_optional(&mut *connection)
                .await
                .map_err(|error| backend("checking catalog scan idempotency key", &error))?;
        if let Some(replay) = replay {
            let mut creation: RecipeCatalogScanCreation = serde_json::from_str(&replay)
                .map_err(|error| invalid_data(format!("invalid catalog scan replay: {error}")))?;
            creation.outcome = CompletionOutcome::Replayed;
            connection.commit("committing catalog scan replay").await?;
            return Ok(creation);
        }
        let required = serde_json::to_string(&job.required_capabilities)
            .map_err(|error| invalid_data(format!("serializing job capabilities: {error}")))?;
        let payload = serde_json::to_string(&job.payload)
            .map_err(|error| invalid_data(format!("serializing job payload: {error}")))?;
        let (subject_kind, subject_id) = job_subject_fields(job.subject);
        query(
            "INSERT INTO jobs
             (id, subject_kind, subject_id, required_capabilities_json, payload_json, state,
              maximum_attempts, attempt_count, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(job.id.to_string())
        .bind(subject_kind)
        .bind(subject_id)
        .bind(required)
        .bind(payload)
        .bind(job_state_name(job.state))
        .bind(i64::from(job.maximum_attempts))
        .bind(i64::from(job.attempt_count))
        .bind(job.created_at)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        query(
            "INSERT INTO recipe_catalog_scans
             (id, job_id, producer, source_locator, source_revision, requested_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(scan.id.to_string())
        .bind(scan.job_id.to_string())
        .bind(&scan.producer)
        .bind(&scan.source.locator)
        .bind(&scan.source.revision)
        .bind(scan.requested_at)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        let creation = RecipeCatalogScanCreation {
            scan: scan.clone(),
            outcome: CompletionOutcome::Completed,
        };
        let response = serde_json::to_string(&creation)
            .map_err(|error| invalid_data(format!("serializing catalog scan creation: {error}")))?;
        query(
            "INSERT INTO idempotency_keys (scope, key, response_json, created_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(idempotency_scope)
        .bind(idempotency_key)
        .bind(response)
        .bind(scan.requested_at)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        insert_audit(&mut connection, audit).await?;
        connection
            .commit("committing catalog scan creation")
            .await?;
        Ok(creation)
    }

    async fn recipe_catalog_scan(
        &self,
        scan_id: RecipeCatalogScanId,
    ) -> Result<Option<RecipeCatalogScanRecord>, StorageError> {
        let row = query(&format!("{RECIPE_CATALOG_SCAN_SELECT} WHERE s.id = ?"))
            .bind(scan_id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| backend("loading recipe catalog scan", &error))?;
        row.as_ref().map(recipe_catalog_scan_from_row).transpose()
    }

    async fn list_recipe_catalog_scans(
        &self,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<RecipeCatalogScanSummaryRecord>, StorageError> {
        if !(1..=200).contains(&limit) {
            return Err(invalid_data(
                "catalog scan page limit must be between 1 and 200",
            ));
        }
        let rows = query(&format!(
            "{RECIPE_CATALOG_SCAN_SELECT} WHERE s.id > ? ORDER BY s.id LIMIT ?"
        ))
        .bind(after.unwrap_or(""))
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(|error| backend("listing recipe catalog scans", &error))?;
        rows.iter()
            .map(recipe_catalog_scan_summary_from_row)
            .collect()
    }

    async fn cancel_recipe_catalog_scan(
        &self,
        scan_id: RecipeCatalogScanId,
        idempotency_key: &str,
        audit: &AuditEvent,
        now: DateTime<Utc>,
    ) -> Result<RecipeCatalogScanRecord, StorageError> {
        if idempotency_key.is_empty() || idempotency_key.len() > 255 {
            return Err(invalid_data("idempotency key must contain 1-255 bytes"));
        }
        let scope = format!("recipe_catalog_scan:{scan_id}:cancel");
        let mut connection = self.acquire_write().await?;
        let replay: Option<String> =
            query_scalar("SELECT response_json FROM idempotency_keys WHERE scope = ? AND key = ?")
                .bind(&scope)
                .bind(idempotency_key)
                .fetch_optional(&mut *connection)
                .await
                .map_err(|error| backend("checking catalog scan cancellation replay", &error))?;
        if let Some(replay) = replay {
            let scan = serde_json::from_str(&replay).map_err(|error| {
                invalid_data(format!("invalid catalog scan cancellation replay: {error}"))
            })?;
            connection
                .commit("committing catalog scan cancellation replay")
                .await?;
            return Ok(scan);
        }
        let row = query(&format!("{RECIPE_CATALOG_SCAN_SELECT} WHERE s.id = ?"))
            .bind(scan_id.to_string())
            .fetch_optional(&mut *connection)
            .await
            .map_err(|error| backend("loading catalog scan for cancellation", &error))?
            .ok_or(StorageError::NotFound)?;
        let mut scan = recipe_catalog_scan_from_row(&row)?;
        if !matches!(scan.state, JobState::Queued | JobState::Leased) {
            return Err(StorageError::Conflict);
        }
        let failure = RecipeCatalogScanExecutionFailure {
            schema_version: RecipeCatalogScanJob::SCHEMA_VERSION,
            scan_id,
            producer: Some(scan.producer.clone()),
            code: "cancelled".to_owned(),
            detail: "Catalog scan was cancelled by an authenticated operator.".to_owned(),
            failed_at: now,
        };
        let failure_json = serde_json::to_string(&failure)
            .map_err(|error| invalid_data(format!("serializing catalog cancellation: {error}")))?;
        query(
            "UPDATE attempts SET state = 'expired', completed_at = ?, result_json = ?
             WHERE job_id = ? AND state = 'leased'",
        )
        .bind(now)
        .bind(&failure_json)
        .bind(scan.job_id.to_string())
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        let changed = query(
            "UPDATE jobs SET state = 'cancelled', completed_at = ?, result_json = ?
             WHERE id = ? AND state IN ('queued', 'leased')",
        )
        .bind(now)
        .bind(&failure_json)
        .bind(scan.job_id.to_string())
        .execute(&mut *connection)
        .await
        .map_err(map_write)?
        .rows_affected();
        if changed != 1 {
            return Err(StorageError::Conflict);
        }
        query(
            "INSERT INTO recipe_catalog_scan_terminals
             (scan_id, snapshot_id, failure_json, completed_at) VALUES (?, NULL, ?, ?)",
        )
        .bind(scan_id.to_string())
        .bind(&failure_json)
        .bind(now)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        scan.state = JobState::Cancelled;
        scan.failure = Some(failure);
        scan.completed_at = Some(now);
        let response = serde_json::to_string(&scan).map_err(|error| {
            invalid_data(format!("serializing cancelled catalog scan: {error}"))
        })?;
        query(
            "INSERT INTO idempotency_keys (scope, key, response_json, created_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(&scope)
        .bind(idempotency_key)
        .bind(response)
        .bind(now)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        insert_audit(&mut connection, audit).await?;
        connection
            .commit("committing catalog scan cancellation")
            .await?;
        Ok(scan)
    }

    #[allow(clippy::too_many_lines)] // Snapshot publication and job terminalization are one transaction.
    async fn complete_recipe_catalog_scan(
        &self,
        worker_id: WorkerId,
        lease: &Lease,
        idempotency_key: &str,
        result: &RecipeCatalogScanExecutionResult,
        audit: &AuditEvent,
        now: DateTime<Utc>,
    ) -> Result<RecipeCatalogScanCompletion, StorageError> {
        if idempotency_key.is_empty() || idempotency_key.len() > 255 {
            return Err(invalid_data("idempotency key must contain 1-255 bytes"));
        }
        let scope = job_completion_scope(lease.job_id);
        let mut connection = self.acquire_write().await?;
        let replay: Option<String> =
            query_scalar("SELECT response_json FROM idempotency_keys WHERE scope = ? AND key = ?")
                .bind(&scope)
                .bind(idempotency_key)
                .fetch_optional(&mut *connection)
                .await
                .map_err(|error| backend("checking catalog scan completion replay", &error))?;
        if let Some(replay) = replay {
            let mut completion: RecipeCatalogScanCompletion = serde_json::from_str(&replay)
                .map_err(|error| {
                    invalid_data(format!("invalid catalog scan completion replay: {error}"))
                })?;
            completion.outcome = CompletionOutcome::Replayed;
            connection
                .commit("committing catalog scan completion replay")
                .await?;
            return Ok(completion);
        }
        let row = query(
            "SELECT j.payload_json FROM attempts a
             JOIN jobs j ON j.id = a.job_id
             JOIN recipe_catalog_scans s ON s.id = j.subject_id AND s.job_id = j.id
             WHERE a.id = ? AND a.job_id = ? AND a.worker_id = ? AND a.state = 'leased'
               AND a.expires_at > ? AND j.state = 'leased'
               AND j.subject_kind = 'recipe_catalog_scan' AND s.id = ?",
        )
        .bind(lease.attempt_id.to_string())
        .bind(lease.job_id.to_string())
        .bind(worker_id.to_string())
        .bind(now)
        .bind(result.scan_id.to_string())
        .fetch_optional(&mut *connection)
        .await
        .map_err(|error| backend("validating catalog scan completion lease", &error))?
        .ok_or(StorageError::InvalidLease)?;
        let payload_json: String = row
            .try_get("payload_json")
            .map_err(|error| backend("decoding catalog scan job", &error))?;
        let envelope: RecipeCatalogScanJob = serde_json::from_str(&payload_json)
            .map_err(|error| invalid_data(format!("invalid catalog scan job: {error}")))?;
        envelope
            .validate()
            .and_then(|()| result.validate_for(&envelope.request))
            .map_err(|error| invalid_data(format!("invalid catalog scan result: {error}")))?;
        let snapshot = RecipeCatalogSnapshotRecord {
            id: RecipeCatalogSnapshotId::new(),
            worker_id,
            manifest_digest: result
                .manifest
                .canonical_digest()
                .map_err(|error| invalid_data(format!("hashing catalog scan result: {error}")))?,
            manifest: result.manifest.clone(),
            observed_at: now,
        };
        let publication = persist_recipe_catalog_snapshot(&mut connection, &snapshot).await?;
        let result_json = serde_json::to_string(result)
            .map_err(|error| invalid_data(format!("serializing catalog scan result: {error}")))?;
        query(
            "UPDATE attempts SET state = 'succeeded', completed_at = ?, result_json = ?
             WHERE id = ?",
        )
        .bind(now)
        .bind(&result_json)
        .bind(lease.attempt_id.to_string())
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        query(
            "UPDATE jobs SET state = 'succeeded', completed_at = ?, result_json = ? WHERE id = ?",
        )
        .bind(now)
        .bind(&result_json)
        .bind(lease.job_id.to_string())
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        query(
            "INSERT INTO recipe_catalog_scan_terminals
             (scan_id, snapshot_id, failure_json, completed_at) VALUES (?, ?, NULL, ?)",
        )
        .bind(result.scan_id.to_string())
        .bind(publication.snapshot.id.to_string())
        .bind(now)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        let completion = RecipeCatalogScanCompletion {
            outcome: CompletionOutcome::Completed,
            publication,
        };
        let response = serde_json::to_string(&completion).map_err(|error| {
            invalid_data(format!("serializing catalog scan completion: {error}"))
        })?;
        query(
            "INSERT INTO idempotency_keys (scope, key, response_json, created_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(&scope)
        .bind(idempotency_key)
        .bind(response)
        .bind(now)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        insert_audit(&mut connection, audit).await?;
        connection.commit("committing catalog scan result").await?;
        Ok(completion)
    }

    async fn fail_recipe_catalog_scan(
        &self,
        worker_id: WorkerId,
        lease: &Lease,
        idempotency_key: &str,
        failure: &RecipeCatalogScanExecutionFailure,
        audit: &AuditEvent,
        now: DateTime<Utc>,
    ) -> Result<CompletionOutcome, StorageError> {
        if idempotency_key.is_empty() || idempotency_key.len() > 255 {
            return Err(invalid_data("idempotency key must contain 1-255 bytes"));
        }
        let scope = job_completion_scope(lease.job_id);
        let mut connection = self.acquire_write().await?;
        let replay: Option<String> =
            query_scalar("SELECT response_json FROM idempotency_keys WHERE scope = ? AND key = ?")
                .bind(&scope)
                .bind(idempotency_key)
                .fetch_optional(&mut *connection)
                .await
                .map_err(|error| backend("checking catalog scan failure replay", &error))?;
        if replay.is_some() {
            connection
                .commit("committing catalog scan failure replay")
                .await?;
            return Ok(CompletionOutcome::Replayed);
        }
        let payload_json: Option<String> = query_scalar(
            "SELECT j.payload_json FROM attempts a
             JOIN jobs j ON j.id = a.job_id
             JOIN recipe_catalog_scans s ON s.id = j.subject_id AND s.job_id = j.id
             WHERE a.id = ? AND a.job_id = ? AND a.worker_id = ? AND a.state = 'leased'
               AND a.expires_at > ? AND j.state = 'leased'
               AND j.subject_kind = 'recipe_catalog_scan' AND s.id = ?",
        )
        .bind(lease.attempt_id.to_string())
        .bind(lease.job_id.to_string())
        .bind(worker_id.to_string())
        .bind(now)
        .bind(failure.scan_id.to_string())
        .fetch_optional(&mut *connection)
        .await
        .map_err(|error| backend("validating catalog scan failure lease", &error))?;
        let payload_json = payload_json.ok_or(StorageError::InvalidLease)?;
        let envelope: RecipeCatalogScanJob = serde_json::from_str(&payload_json)
            .map_err(|error| invalid_data(format!("invalid catalog scan job: {error}")))?;
        envelope
            .validate()
            .and_then(|()| failure.validate_for(&envelope.request))
            .map_err(|error| invalid_data(format!("invalid catalog scan failure: {error}")))?;
        let failure_json = serde_json::to_string(failure)
            .map_err(|error| invalid_data(format!("serializing catalog scan failure: {error}")))?;
        query(
            "UPDATE attempts SET state = 'failed', completed_at = ?, result_json = ? WHERE id = ?",
        )
        .bind(now)
        .bind(&failure_json)
        .bind(lease.attempt_id.to_string())
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        query("UPDATE jobs SET state = 'failed', completed_at = ?, result_json = ? WHERE id = ?")
            .bind(now)
            .bind(&failure_json)
            .bind(lease.job_id.to_string())
            .execute(&mut *connection)
            .await
            .map_err(map_write)?;
        query(
            "INSERT INTO recipe_catalog_scan_terminals
             (scan_id, snapshot_id, failure_json, completed_at) VALUES (?, NULL, ?, ?)",
        )
        .bind(failure.scan_id.to_string())
        .bind(&failure_json)
        .bind(now)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        query(
            "INSERT INTO idempotency_keys (scope, key, response_json, created_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(&scope)
        .bind(idempotency_key)
        .bind(&failure_json)
        .bind(now)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        insert_audit(&mut connection, audit).await?;
        connection.commit("committing catalog scan failure").await?;
        Ok(CompletionOutcome::Completed)
    }
}
