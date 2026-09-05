//! SQLite implementation of the runs ports.
use super::{
    Artifact, ArtifactLocation, ArtifactRole, AttemptId, AuditEvent, BuildCompletion,
    BuildDisposition, BuildStorage, BuilderExecutionResult, CompletionOutcome, DateTime, Job,
    Lease, LifecycleEventId, LocationState, NewRunLogEntry, PromotionEventId, RecipeId, Release,
    ReleaseId, ReleaseState, Row, RunCreation, RunId, RunLogAppendOutcome, RunLogReceipt,
    RunLogRecord, RunLogStorage, RunRecord, RunState, RunStorage, RunSummaryRecord, SqliteStorage,
    StorageError, Utc, VariantId, WorkerId, architecture_name, artifact_role_name, async_trait,
    backend, insert_audit, insert_queued_run_and_job, invalid_data, job_completion_scope,
    location_state_name, map_write, parse_value, persist_build_terminal, persisted_release_graph,
    platform_name, query, query_scalar, release_from_row, release_state_name, run_from_row,
    run_log_from_row, run_log_stream_name, run_summary_from_row, same_release_evidence,
    submitted_release_graph, validate_queued_run_and_job, validate_run_log_batch,
};

#[async_trait]
impl RunStorage for SqliteStorage {
    async fn create_run(
        &self,
        run: &RunRecord,
        job: &Job,
        idempotency_scope: &str,
        idempotency_key: &str,
        audit: &AuditEvent,
    ) -> Result<RunCreation, StorageError> {
        validate_queued_run_and_job(run, job)?;
        if idempotency_scope.is_empty()
            || idempotency_scope.len() > 255
            || idempotency_key.is_empty()
            || idempotency_key.len() > 255
        {
            return Err(invalid_data(
                "idempotency scope and key must each contain 1-255 bytes",
            ));
        }
        let mut connection = self.acquire_write().await?;
        let replay: Option<String> =
            query_scalar("SELECT response_json FROM idempotency_keys WHERE scope = ? AND key = ?")
                .bind(idempotency_scope)
                .bind(idempotency_key)
                .fetch_optional(&mut *connection)
                .await
                .map_err(|error| backend("checking run creation idempotency", &error))?;
        if let Some(replay) = replay {
            let prior: RunRecord = serde_json::from_str(&replay)
                .map_err(|error| invalid_data(format!("decoding run creation replay: {error}")))?;
            if prior.recipe_revision_id != run.recipe_revision_id
                || prior.software_id != run.software_id
                || prior.parameters != run.parameters
            {
                return Err(StorageError::Conflict);
            }
            connection.commit("closing run creation replay").await?;
            return Ok(RunCreation {
                run: prior,
                outcome: CompletionOutcome::Replayed,
            });
        }
        insert_queued_run_and_job(&mut connection, run, job).await?;
        insert_audit(&mut connection, audit).await?;
        let response = serde_json::to_string(run)
            .map_err(|error| invalid_data(format!("serializing run creation response: {error}")))?;
        query(
            "INSERT INTO idempotency_keys (scope, key, response_json, created_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(idempotency_scope)
        .bind(idempotency_key)
        .bind(response)
        .bind(run.created_at)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        connection.commit("committing run and job creation").await?;
        Ok(RunCreation {
            run: run.clone(),
            outcome: CompletionOutcome::Completed,
        })
    }

    async fn run(&self, run_id: RunId) -> Result<Option<RunRecord>, StorageError> {
        let row = query(
            "SELECT id, recipe_revision_id, software_id, state, parameters_json, result_json,
                    created_at, completed_at
             FROM runs WHERE id = ?",
        )
        .bind(run_id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| backend("loading run", &error))?;
        row.as_ref().map(run_from_row).transpose()
    }

    async fn cancel_run(
        &self,
        run_id: RunId,
        idempotency_key: &str,
        audit: &AuditEvent,
        now: DateTime<Utc>,
    ) -> Result<RunRecord, StorageError> {
        if idempotency_key.is_empty() || idempotency_key.len() > 255 {
            return Err(invalid_data("idempotency key must contain 1-255 bytes"));
        }
        let scope = format!("run:{run_id}:cancel");
        let mut connection = self.acquire_write().await?;
        let replay: Option<String> =
            query_scalar("SELECT response_json FROM idempotency_keys WHERE scope = ? AND key = ?")
                .bind(&scope)
                .bind(idempotency_key)
                .fetch_optional(&mut *connection)
                .await
                .map_err(|error| backend("checking run cancellation idempotency", &error))?;
        if let Some(replay) = replay {
            let run = serde_json::from_str(&replay)
                .map_err(|error| invalid_data(format!("invalid cancellation replay: {error}")))?;
            connection
                .commit("committing replayed run cancellation")
                .await?;
            return Ok(run);
        }
        let row = query(
            "SELECT id, recipe_revision_id, software_id, state, parameters_json, result_json,
                    created_at, completed_at FROM runs WHERE id = ?",
        )
        .bind(run_id.to_string())
        .fetch_optional(&mut *connection)
        .await
        .map_err(|error| backend("loading cancelled run", &error))?
        .ok_or(StorageError::NotFound)?;
        let mut run = run_from_row(&row)?;
        if run.state == RunState::Cancelled {
            connection
                .commit("committing already cancelled run lookup")
                .await?;
            return Ok(run);
        }
        if !matches!(run.state, RunState::Queued | RunState::Running) {
            return Err(StorageError::Conflict);
        }
        let result = serde_json::json!({
            "code": "cancelled",
            "detail": "Run was cancelled by an authenticated operator."
        });
        let result_json = serde_json::to_string(&result)
            .map_err(|error| invalid_data(format!("serializing cancellation: {error}")))?;
        query(
            "UPDATE attempts SET state = 'expired', completed_at = ?, result_json = ?
             WHERE state = 'leased' AND job_id IN (
               SELECT id FROM jobs WHERE subject_kind = 'build_run' AND subject_id = ?
             )",
        )
        .bind(now)
        .bind(&result_json)
        .bind(run_id.to_string())
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        query(
            "UPDATE jobs SET state = 'cancelled', completed_at = ?, result_json = ?
             WHERE subject_kind = 'build_run' AND subject_id = ?
               AND state IN ('queued', 'leased')",
        )
        .bind(now)
        .bind(&result_json)
        .bind(run_id.to_string())
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        query(
            "UPDATE runs SET state = 'cancelled', completed_at = ?, result_json = ? WHERE id = ?",
        )
        .bind(now)
        .bind(&result_json)
        .bind(run_id.to_string())
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        run.state = RunState::Cancelled;
        run.result = Some(result);
        run.completed_at = Some(now);
        let response = serde_json::to_string(&run)
            .map_err(|error| invalid_data(format!("serializing cancelled run: {error}")))?;
        query(
            "INSERT INTO idempotency_keys (scope, key, response_json, created_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(scope)
        .bind(idempotency_key)
        .bind(response)
        .bind(now)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        insert_audit(&mut connection, audit).await?;
        connection.commit("committing run cancellation").await?;
        Ok(run)
    }

    async fn list_runs(
        &self,
        recipe_id: Option<RecipeId>,
        software_id: Option<stabbur_domain::SoftwareId>,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<RunSummaryRecord>, StorageError> {
        if !(1..=200).contains(&limit) {
            return Err(invalid_data("run page limit must be between 1 and 200"));
        }
        let recipe_id = recipe_id.map(|value| value.to_string());
        let software_id = software_id.map(|value| value.to_string());
        let rows = query(
            "SELECT r.id, r.recipe_revision_id, r.software_id, r.state,
                    r.created_at, r.completed_at
             FROM runs r JOIN recipe_revisions rr ON rr.id = r.recipe_revision_id
             WHERE (? IS NULL OR rr.recipe_id = ?)
               AND (? IS NULL OR r.software_id = ?)
               AND r.id > ? ORDER BY r.id LIMIT ?",
        )
        .bind(&recipe_id)
        .bind(&recipe_id)
        .bind(&software_id)
        .bind(&software_id)
        .bind(after.unwrap_or(""))
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(|error| backend("listing runs", &error))?;
        rows.iter().map(run_summary_from_row).collect()
    }
}

#[async_trait]
impl BuildStorage for SqliteStorage {
    async fn record_run_artifact(
        &self,
        worker_id: WorkerId,
        attempt_id: AttemptId,
        artifact: &Artifact,
        location: &ArtifactLocation,
        role: ArtifactRole,
        audit: &AuditEvent,
        now: DateTime<Utc>,
    ) -> Result<RunId, StorageError> {
        if artifact.digest != location.digest || location.state != LocationState::Present {
            return Err(invalid_data(
                "worker artifact and verified present location must have the same digest",
            ));
        }
        let size = i64::try_from(artifact.size)
            .map_err(|_| invalid_data("artifact size exceeds SQLite range"))?;
        let mut connection = self.acquire_write().await?;
        let run_id: Option<String> = query_scalar(
            "SELECT j.subject_id FROM attempts a
             JOIN jobs j ON j.id = a.job_id
             WHERE a.id = ? AND a.worker_id = ? AND a.state = 'leased'
               AND a.expires_at > ? AND j.state = 'leased'
               AND j.subject_kind = 'build_run'",
        )
        .bind(attempt_id.to_string())
        .bind(worker_id.to_string())
        .bind(now)
        .fetch_optional(&mut *connection)
        .await
        .map_err(|error| backend("validating artifact upload lease", &error))?;
        let run_id = run_id.ok_or(StorageError::InvalidLease)?;
        query(
            "INSERT INTO artifacts (digest, size, media_type, created_at) VALUES (?, ?, ?, ?)
             ON CONFLICT(digest) DO NOTHING",
        )
        .bind(artifact.digest.as_str())
        .bind(size)
        .bind(&artifact.media_type)
        .bind(artifact.created_at)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        let metadata = query("SELECT size, media_type FROM artifacts WHERE digest = ?")
            .bind(artifact.digest.as_str())
            .fetch_one(&mut *connection)
            .await
            .map_err(|error| backend("verifying uploaded artifact metadata", &error))?;
        let persisted_size: i64 = metadata
            .try_get("size")
            .map_err(|error| backend("decoding uploaded artifact size", &error))?;
        let persisted_media_type: String = metadata
            .try_get("media_type")
            .map_err(|error| backend("decoding uploaded artifact media type", &error))?;
        if persisted_size != size || persisted_media_type != artifact.media_type {
            return Err(StorageError::Conflict);
        }
        query(
            "INSERT INTO artifact_locations
             (id, digest, store_id, state, verified_at, last_error) VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(digest, store_id) DO UPDATE SET
               state = excluded.state, verified_at = excluded.verified_at,
               last_error = excluded.last_error",
        )
        .bind(location.id.to_string())
        .bind(location.digest.as_str())
        .bind(location.store_id.to_string())
        .bind(location_state_name(location.state))
        .bind(location.verified_at)
        .bind(&location.last_error)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        query(
            "INSERT INTO run_artifacts (run_id, digest, role) VALUES (?, ?, ?)
             ON CONFLICT(run_id, digest, role) DO NOTHING",
        )
        .bind(&run_id)
        .bind(artifact.digest.as_str())
        .bind(artifact_role_name(role))
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        insert_audit(&mut connection, audit).await?;
        connection
            .commit("committing worker artifact upload")
            .await?;
        parse_value(&run_id, "run ID")
    }

    #[allow(clippy::too_many_lines)] // Publication gating must remain visibly inside one database transaction.
    async fn finalize_build(
        &self,
        worker_id: WorkerId,
        lease: &Lease,
        idempotency_key: &str,
        execution: &BuilderExecutionResult,
        audit: &AuditEvent,
        now: DateTime<Utc>,
    ) -> Result<BuildCompletion, StorageError> {
        if idempotency_key.is_empty() || idempotency_key.len() > 255 {
            return Err(invalid_data("idempotency key must contain 1-255 bytes"));
        }
        let result = execution
            .build_result
            .as_ref()
            .ok_or_else(|| invalid_data("production build completion requires selected output"))?;
        if result.variants.is_empty() || result.uploaded_artifacts.is_empty() {
            return Err(invalid_data(
                "build result must contain variants and uploaded artifacts",
            ));
        }
        let mut expected = std::collections::BTreeMap::new();
        let mut selected_digests = std::collections::BTreeSet::new();
        for variant in &result.variants {
            if variant.variant_id.is_some()
                || variant
                    .artifacts
                    .iter()
                    .filter(|artifact| artifact.role == ArtifactRole::PrimaryInstaller)
                    .count()
                    != 1
            {
                return Err(invalid_data(
                    "new build variants require exactly one primary installer and no identity",
                ));
            }
            for artifact in &variant.artifacts {
                selected_digests.insert(artifact.digest.to_string());
                let key = (
                    artifact.digest.to_string(),
                    artifact_role_name(artifact.role).to_owned(),
                );
                if expected.insert(key, artifact.size).is_some() {
                    return Err(invalid_data("build result repeats an artifact role"));
                }
            }
        }
        let uploaded_digests = result
            .uploaded_artifacts
            .iter()
            .map(ToString::to_string)
            .collect::<std::collections::BTreeSet<_>>();
        if uploaded_digests.len() != result.uploaded_artifacts.len()
            || uploaded_digests != selected_digests
        {
            return Err(invalid_data(
                "uploaded artifact identities must exactly match selected variant artifacts",
            ));
        }
        let mut check_names = std::collections::BTreeSet::new();
        if result
            .verification_results
            .iter()
            .any(|check| check.check.trim() != check.check || !check_names.insert(&check.check))
        {
            return Err(invalid_data("verification check names must be unique"));
        }

        let mut connection = self.acquire_write().await?;
        let scope = job_completion_scope(lease.job_id);
        let replay: Option<String> =
            query_scalar("SELECT response_json FROM idempotency_keys WHERE scope = ? AND key = ?")
                .bind(&scope)
                .bind(idempotency_key)
                .fetch_optional(&mut *connection)
                .await
                .map_err(|error| backend("checking build idempotency key", &error))?;
        if let Some(replay) = replay {
            let mut completion: BuildCompletion =
                serde_json::from_str(&replay).map_err(|error| {
                    invalid_data(format!("invalid build completion replay response: {error}"))
                })?;
            completion.outcome = CompletionOutcome::Replayed;
            connection
                .commit("committing replayed build result")
                .await?;
            return Ok(completion);
        }
        let active = query(
            "SELECT j.subject_id AS run_id, r.software_id FROM attempts a
             JOIN jobs j ON j.id = a.job_id
             JOIN runs r ON r.id = j.subject_id
             WHERE a.id = ? AND a.job_id = ? AND a.worker_id = ? AND a.state = 'leased'
               AND a.expires_at > ? AND j.state = 'leased' AND r.state = 'running'
               AND j.subject_kind = 'build_run'",
        )
        .bind(lease.attempt_id.to_string())
        .bind(lease.job_id.to_string())
        .bind(worker_id.to_string())
        .bind(now)
        .fetch_optional(&mut *connection)
        .await
        .map_err(|error| backend("validating build completion lease", &error))?
        .ok_or(StorageError::InvalidLease)?;
        let run_id: String = active
            .try_get("run_id")
            .map_err(|error| backend("decoding build run ID", &error))?;
        let software_id: String = active
            .try_get("software_id")
            .map_err(|error| backend("decoding build software ID", &error))?;
        if run_id != execution.run_id.to_string() {
            return Err(invalid_data(
                "builder result run does not match its active lease",
            ));
        }

        let rows = query(
            "SELECT ra.digest, ra.role, a.size,
                    EXISTS(SELECT 1 FROM artifact_locations l
                           JOIN stores s ON s.id = l.store_id
                           WHERE l.digest = ra.digest AND l.state = 'present'
                             AND l.verified_at IS NOT NULL AND s.role = 'primary'
                             AND s.enabled = 1) AS readable_primary
             FROM run_artifacts ra JOIN artifacts a ON a.digest = ra.digest
             WHERE ra.run_id = ?",
        )
        .bind(&run_id)
        .fetch_all(&mut *connection)
        .await
        .map_err(|error| backend("loading server-verified build artifacts", &error))?;
        let mut verified = std::collections::BTreeMap::new();
        for row in rows {
            let digest: String = row
                .try_get("digest")
                .map_err(|error| backend("decoding build artifact digest", &error))?;
            let role: String = row
                .try_get("role")
                .map_err(|error| backend("decoding build artifact role", &error))?;
            let size: i64 = row
                .try_get("size")
                .map_err(|error| backend("decoding build artifact size", &error))?;
            let readable_primary: i64 = row
                .try_get("readable_primary")
                .map_err(|error| backend("decoding build artifact placement", &error))?;
            if readable_primary != 1 {
                return Err(invalid_data(
                    "every selected artifact requires a verified present primary location",
                ));
            }
            verified.insert(
                (digest, role),
                u64::try_from(size).map_err(|_| invalid_data("negative artifact size"))?,
            );
        }
        if verified != expected {
            return Err(invalid_data(
                "builder-selected artifacts differ from server-verified run uploads",
            ));
        }

        let existing_release = query(
            "SELECT id, software_id, version, state, created_at, revision, availability_json
             FROM releases WHERE software_id = ? AND version = ?",
        )
        .bind(&software_id)
        .bind(result.discovered_version.as_str())
        .fetch_optional(&mut *connection)
        .await
        .map_err(|error| backend("loading same-version release", &error))?
        .as_ref()
        .map(release_from_row)
        .transpose()?;
        if let Some(release) = existing_release {
            let disposition = if persisted_release_graph(&mut connection, release.id).await?
                != submitted_release_graph(result)
            {
                BuildDisposition::VersionContentConflict
            } else if !result.passes_candidate_gate() {
                BuildDisposition::VerificationFailed
            } else if same_release_evidence(&mut connection, release.id, result).await? {
                BuildDisposition::NoChange
            } else {
                BuildDisposition::EvidenceChanged
            };
            let completion = persist_build_terminal(
                &mut connection,
                lease,
                &run_id,
                &scope,
                idempotency_key,
                execution,
                &release,
                disposition,
                audit,
                now,
            )
            .await?;
            connection
                .commit("committing same-version build result")
                .await?;
            return Ok(completion);
        }

        let candidate = result.passes_candidate_gate();
        let final_state = if candidate {
            ReleaseState::Candidate
        } else {
            ReleaseState::Failed
        };
        let release = Release {
            availability: stabbur_domain::ReleaseAvailability::Available,
            id: ReleaseId::new(),
            software_id: parse_value(&software_id, "software ID")?,
            version: result.discovered_version.clone(),
            state: final_state,
            created_at: now,
            revision: 1,
        };
        query(
            "INSERT INTO releases (id, software_id, version, state, created_at, revision)
             VALUES (?, ?, ?, ?, ?, 1)",
        )
        .bind(release.id.to_string())
        .bind(&software_id)
        .bind(release.version.as_str())
        .bind(release_state_name(release.state))
        .bind(now)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        let actor = serde_json::to_string(&audit.actor)
            .map_err(|error| invalid_data(format!("serializing lifecycle actor: {error}")))?;
        let mut lifecycle = vec![(None, ReleaseState::Discovered)];
        lifecycle.extend([
            (Some(ReleaseState::Discovered), ReleaseState::Built),
            (Some(ReleaseState::Built), ReleaseState::Inspected),
        ]);
        if candidate {
            lifecycle.extend([
                (Some(ReleaseState::Inspected), ReleaseState::Verified),
                (Some(ReleaseState::Verified), ReleaseState::Candidate),
            ]);
        } else {
            lifecycle.push((Some(ReleaseState::Inspected), ReleaseState::Failed));
        }
        for (from, to) in lifecycle {
            query(
                "INSERT INTO release_lifecycle_events
                 (id, release_id, from_state, to_state, actor_json, reason, occurred_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(LifecycleEventId::new().to_string())
            .bind(release.id.to_string())
            .bind(from.map(release_state_name))
            .bind(release_state_name(to))
            .bind(&actor)
            .bind(if to == ReleaseState::Failed {
                Some("required verification or recipe trust did not succeed")
            } else {
                None
            })
            .bind(now)
            .execute(&mut *connection)
            .await
            .map_err(map_write)?;
        }
        for built in &result.variants {
            let variant_id = VariantId::new();
            query(
                "INSERT INTO variants
                 (id, release_id, platform, architecture, minimum_macos, maximum_macos,
                  resolution_priority) VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(variant_id.to_string())
            .bind(release.id.to_string())
            .bind(platform_name(built.platform))
            .bind(architecture_name(built.architecture))
            .bind(built.minimum_macos.as_ref().map(ToString::to_string))
            .bind(built.maximum_macos.as_ref().map(ToString::to_string))
            .bind(built.resolution_priority)
            .execute(&mut *connection)
            .await
            .map_err(map_write)?;
            for artifact in &built.artifacts {
                query("INSERT INTO variant_artifacts (variant_id, digest, role) VALUES (?, ?, ?)")
                    .bind(variant_id.to_string())
                    .bind(artifact.digest.as_str())
                    .bind(artifact_role_name(artifact.role))
                    .execute(&mut *connection)
                    .await
                    .map_err(map_write)?;
            }
        }
        if candidate {
            let previous_release_id: Option<String> = query_scalar(
                "SELECT release_id FROM channels WHERE software_id = ? AND name = 'candidate'",
            )
            .bind(&software_id)
            .fetch_optional(&mut *connection)
            .await
            .map_err(|error| backend("loading previous candidate channel", &error))?;
            query(
                "INSERT INTO channels
                 (software_id, name, release_id, pinned_variant_id, revision)
                 VALUES (?, 'candidate', ?, NULL, 1)
                 ON CONFLICT(software_id, name) DO UPDATE SET
                   release_id = excluded.release_id, pinned_variant_id = NULL,
                   revision = channels.revision + 1",
            )
            .bind(&software_id)
            .bind(release.id.to_string())
            .execute(&mut *connection)
            .await
            .map_err(map_write)?;
            query(
                "INSERT INTO promotion_events
                 (id, software_id, channel_name, previous_release_id, release_id,
                  pinned_variant_id, actor_json, reason, occurred_at)
                 VALUES (?, ?, 'candidate', ?, ?, NULL, ?, ?, ?)",
            )
            .bind(PromotionEventId::new().to_string())
            .bind(&software_id)
            .bind(previous_release_id)
            .bind(release.id.to_string())
            .bind(&actor)
            .bind("automatic publication after server verification")
            .bind(now)
            .execute(&mut *connection)
            .await
            .map_err(map_write)?;
        }
        let completion = persist_build_terminal(
            &mut connection,
            lease,
            &run_id,
            &scope,
            idempotency_key,
            execution,
            &release,
            BuildDisposition::ReleaseCreated,
            audit,
            now,
        )
        .await?;
        connection
            .commit("committing build result and release")
            .await?;
        Ok(completion)
    }
}

#[async_trait]
impl RunLogStorage for SqliteStorage {
    async fn append_run_logs(
        &self,
        worker_id: WorkerId,
        lease: &Lease,
        idempotency_key: &str,
        entries: &[NewRunLogEntry],
        now: DateTime<Utc>,
    ) -> Result<RunLogAppendOutcome, StorageError> {
        validate_run_log_batch(idempotency_key, entries)?;
        let scope = format!("attempt:{}:logs", lease.attempt_id);
        let mut connection = self.acquire_write().await?;
        let replay: Option<String> =
            query_scalar("SELECT response_json FROM idempotency_keys WHERE scope = ? AND key = ?")
                .bind(&scope)
                .bind(idempotency_key)
                .fetch_optional(&mut *connection)
                .await
                .map_err(|error| backend("checking run log idempotency key", &error))?;
        if let Some(replay) = replay {
            let receipt = serde_json::from_str(&replay)
                .map_err(|error| invalid_data(format!("invalid run log receipt: {error}")))?;
            connection.commit("committing replayed run logs").await?;
            return Ok(RunLogAppendOutcome::Replayed(receipt));
        }
        let run_id: Option<String> = query_scalar(
            "SELECT j.subject_id FROM attempts a
             JOIN jobs j ON j.id = a.job_id
             WHERE a.id = ? AND a.job_id = ? AND a.worker_id = ? AND a.state = 'leased'
               AND a.expires_at > ? AND j.subject_kind = 'build_run'",
        )
        .bind(lease.attempt_id.to_string())
        .bind(lease.job_id.to_string())
        .bind(worker_id.to_string())
        .bind(now)
        .fetch_optional(&mut *connection)
        .await
        .map_err(|error| backend("validating run log lease", &error))?;
        let run_id = run_id.ok_or(StorageError::InvalidLease)?;
        let first_sequence: i64 =
            query_scalar("SELECT COALESCE(MAX(sequence), -1) + 1 FROM run_logs WHERE run_id = ?")
                .bind(&run_id)
                .fetch_one(&mut *connection)
                .await
                .map_err(|error| backend("allocating run log sequences", &error))?;
        for (offset, entry) in entries.iter().enumerate() {
            let offset = i64::try_from(offset)
                .map_err(|_| invalid_data("run log batch exceeds SQLite range"))?;
            query(
                "INSERT INTO run_logs
                 (run_id, sequence, stream, message, occurred_at, attempt_id)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(&run_id)
            .bind(first_sequence + offset)
            .bind(run_log_stream_name(entry.stream))
            .bind(&entry.message)
            .bind(now)
            .bind(lease.attempt_id.to_string())
            .execute(&mut *connection)
            .await
            .map_err(map_write)?;
        }
        let count = u32::try_from(entries.len())
            .map_err(|_| invalid_data("run log batch count exceeds range"))?;
        let last_sequence = first_sequence
            + i64::try_from(entries.len() - 1)
                .map_err(|_| invalid_data("run log batch exceeds SQLite range"))?;
        let receipt = RunLogReceipt {
            first_sequence: u64::try_from(first_sequence)
                .map_err(|_| invalid_data("negative run log sequence"))?,
            last_sequence: u64::try_from(last_sequence)
                .map_err(|_| invalid_data("negative run log sequence"))?,
            count,
        };
        let response = serde_json::to_string(&receipt)
            .map_err(|error| invalid_data(format!("serializing run log receipt: {error}")))?;
        query(
            "INSERT INTO idempotency_keys (scope, key, response_json, created_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(scope)
        .bind(idempotency_key)
        .bind(response)
        .bind(now)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        connection.commit("committing run logs").await?;
        Ok(RunLogAppendOutcome::Appended(receipt))
    }

    async fn run_logs(
        &self,
        run_id: RunId,
        after: Option<u64>,
        limit: u32,
    ) -> Result<Vec<RunLogRecord>, StorageError> {
        if !(1..=200).contains(&limit) {
            return Err(invalid_data("run log page limit must be between 1 and 200"));
        }
        let after = after.map_or(Ok(-1), |value| {
            i64::try_from(value).map_err(|_| invalid_data("run log cursor exceeds SQLite range"))
        })?;
        let rows = query(
            "SELECT run_id, attempt_id, sequence, stream, message, occurred_at
             FROM run_logs WHERE run_id = ? AND sequence > ? ORDER BY sequence LIMIT ?",
        )
        .bind(run_id.to_string())
        .bind(after)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(|error| backend("reading run logs", &error))?;
        rows.iter().map(run_log_from_row).collect()
    }
}
