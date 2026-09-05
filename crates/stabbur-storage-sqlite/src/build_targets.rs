//! SQLite implementation of the build targets ports.
use super::{
    AuditEvent, BuildTargetId, BuildTargetRecord, BuildTargetRunTrigger, BuildTargetSchedule,
    BuildTargetStorage, CompletionOutcome, DateTime, Duration, Job, RunCreation, RunRecord,
    RunSummaryRecord, SqliteStorage, StorageError, Utc, async_trait, backend,
    build_target_from_row, build_target_schedule_fields, insert_audit,
    insert_build_target_revision, insert_queued_run_and_job, invalid_data, map_write, query,
    query_scalar, run_summary_from_row, validate_build_target, validate_queued_run_and_job,
};

#[async_trait]
impl BuildTargetStorage for SqliteStorage {
    async fn create_build_target(
        &self,
        target: &BuildTargetRecord,
        audit: &AuditEvent,
    ) -> Result<BuildTargetRecord, StorageError> {
        validate_build_target(target)?;
        if target.revision != 1 || target.created_at != target.updated_at {
            return Err(invalid_data(
                "new build targets must start at revision one with matching timestamps",
            ));
        }
        let parameters = serde_json::to_string(&target.parameters).map_err(|error| {
            invalid_data(format!("serializing build target parameters: {error}"))
        })?;
        let (trigger_kind, interval_seconds) = build_target_schedule_fields(target.schedule);
        let mut connection = self.acquire_write().await?;
        query(
            "INSERT INTO build_targets
             (id, name, software_id, recipe_revision_id, parameters_json, trigger_kind,
              interval_seconds, enabled, next_run_at, revision, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?)",
        )
        .bind(target.id.to_string())
        .bind(&target.name)
        .bind(target.software_id.to_string())
        .bind(target.recipe_revision_id.to_string())
        .bind(parameters)
        .bind(trigger_kind)
        .bind(interval_seconds)
        .bind(i64::from(target.enabled))
        .bind(target.next_run_at)
        .bind(target.created_at)
        .bind(target.updated_at)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        insert_build_target_revision(&mut connection, target).await?;
        insert_audit(&mut connection, audit).await?;
        connection
            .commit("committing build target creation")
            .await?;
        Ok(target.clone())
    }

    async fn build_target(
        &self,
        identity: &str,
    ) -> Result<Option<BuildTargetRecord>, StorageError> {
        let row = query(
            "SELECT id, name, software_id, recipe_revision_id, parameters_json, trigger_kind,
                    interval_seconds, enabled, next_run_at, revision, created_at, updated_at
             FROM build_targets WHERE id = ? OR name = ?",
        )
        .bind(identity)
        .bind(identity)
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| backend("loading build target", &error))?;
        row.as_ref().map(build_target_from_row).transpose()
    }

    async fn list_build_targets(
        &self,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<BuildTargetRecord>, StorageError> {
        if !(1..=200).contains(&limit) {
            return Err(invalid_data(
                "build target page limit must be between 1 and 200",
            ));
        }
        let rows = query(
            "SELECT id, name, software_id, recipe_revision_id, parameters_json, trigger_kind,
                    interval_seconds, enabled, next_run_at, revision, created_at, updated_at
             FROM build_targets WHERE id > ? ORDER BY id LIMIT ?",
        )
        .bind(after.unwrap_or(""))
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(|error| backend("listing build targets", &error))?;
        rows.iter().map(build_target_from_row).collect()
    }

    async fn update_build_target(
        &self,
        target: &BuildTargetRecord,
        expected_revision: u64,
        audit: &AuditEvent,
    ) -> Result<BuildTargetRecord, StorageError> {
        validate_build_target(target)?;
        let mut connection = self.acquire_write().await?;
        let row = query(
            "SELECT id, name, software_id, recipe_revision_id, parameters_json, trigger_kind,
                    interval_seconds, enabled, next_run_at, revision, created_at, updated_at
             FROM build_targets WHERE id = ?",
        )
        .bind(target.id.to_string())
        .fetch_optional(&mut *connection)
        .await
        .map_err(|error| backend("loading build target for update", &error))?
        .ok_or(StorageError::NotFound)?;
        let current = build_target_from_row(&row)?;
        if current.revision != expected_revision {
            return Err(StorageError::StaleRevision);
        }
        if current.software_id != target.software_id || current.created_at != target.created_at {
            return Err(invalid_data(
                "build target software and creation time are immutable",
            ));
        }
        let mut updated = target.clone();
        updated.updated_at = updated.updated_at.max(current.updated_at);
        updated.revision = expected_revision
            .checked_add(1)
            .ok_or_else(|| invalid_data("build target revision overflow"))?;
        validate_build_target(&updated)?;
        let parameters = serde_json::to_string(&updated.parameters).map_err(|error| {
            invalid_data(format!("serializing build target parameters: {error}"))
        })?;
        let (trigger_kind, interval_seconds) = build_target_schedule_fields(updated.schedule);
        let changed = query(
            "UPDATE build_targets SET name = ?, recipe_revision_id = ?, parameters_json = ?,
                    trigger_kind = ?, interval_seconds = ?, enabled = ?, next_run_at = ?,
                    revision = ?, updated_at = ?
             WHERE id = ? AND revision = ?",
        )
        .bind(&updated.name)
        .bind(updated.recipe_revision_id.to_string())
        .bind(parameters)
        .bind(trigger_kind)
        .bind(interval_seconds)
        .bind(i64::from(updated.enabled))
        .bind(updated.next_run_at)
        .bind(
            i64::try_from(updated.revision)
                .map_err(|_| invalid_data("build target revision exceeds SQLite range"))?,
        )
        .bind(updated.updated_at)
        .bind(updated.id.to_string())
        .bind(
            i64::try_from(expected_revision)
                .map_err(|_| invalid_data("build target revision exceeds SQLite range"))?,
        )
        .execute(&mut *connection)
        .await
        .map_err(map_write)?
        .rows_affected();
        if changed != 1 {
            return Err(StorageError::StaleRevision);
        }
        insert_build_target_revision(&mut connection, &updated).await?;
        insert_audit(&mut connection, audit).await?;
        connection.commit("committing build target update").await?;
        Ok(updated)
    }

    async fn due_build_targets(
        &self,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<BuildTargetRecord>, StorageError> {
        if !(1..=200).contains(&limit) {
            return Err(invalid_data(
                "due build target limit must be between 1 and 200",
            ));
        }
        let rows = query(
            "SELECT id, name, software_id, recipe_revision_id, parameters_json, trigger_kind,
                    interval_seconds, enabled, next_run_at, revision, created_at, updated_at
             FROM build_targets
             WHERE enabled = 1 AND trigger_kind = 'interval' AND next_run_at <= ?
             AND NOT EXISTS (SELECT 1 FROM build_target_runs tr JOIN runs r ON r.id = tr.run_id
                             WHERE tr.target_id = build_targets.id AND r.state IN ('queued', 'running'))
             ORDER BY next_run_at, id LIMIT ?",
        )
        .bind(now)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(|error| backend("listing due build targets", &error))?;
        rows.iter().map(build_target_from_row).collect()
    }

    async fn list_build_target_runs(
        &self,
        target_id: BuildTargetId,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<RunSummaryRecord>, StorageError> {
        if !(1..=200).contains(&limit) {
            return Err(invalid_data(
                "build target run page limit must be between 1 and 200",
            ));
        }
        let rows = query(
            "SELECT r.id, r.recipe_revision_id, r.software_id, r.state,
                    r.created_at, r.completed_at
             FROM build_target_runs tr JOIN runs r ON r.id = tr.run_id
             WHERE tr.target_id = ? AND r.id > ? ORDER BY r.id LIMIT ?",
        )
        .bind(target_id.to_string())
        .bind(after.unwrap_or(""))
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(|error| backend("listing build target runs", &error))?;
        rows.iter().map(run_summary_from_row).collect()
    }

    #[allow(clippy::too_many_lines)] // Cursor consumption, run creation, history, audit, and replay are one transaction.
    async fn create_build_target_run(
        &self,
        target_id: BuildTargetId,
        trigger: BuildTargetRunTrigger,
        run: &RunRecord,
        job: &Job,
        idempotency_key: &str,
        audit: &AuditEvent,
        now: DateTime<Utc>,
    ) -> Result<RunCreation, StorageError> {
        validate_queued_run_and_job(run, job)?;
        if idempotency_key.is_empty() || idempotency_key.len() > 255 {
            return Err(invalid_data(
                "build target run idempotency key must contain 1-255 bytes",
            ));
        }
        let trigger_name = match trigger {
            BuildTargetRunTrigger::Manual => "manual",
            BuildTargetRunTrigger::Scheduled { .. } => "scheduled",
        };
        let scope = format!("build-target:{target_id}:run:{trigger_name}");
        let mut connection = self.acquire_write().await?;
        let replay: Option<String> =
            query_scalar("SELECT response_json FROM idempotency_keys WHERE scope = ? AND key = ?")
                .bind(&scope)
                .bind(idempotency_key)
                .fetch_optional(&mut *connection)
                .await
                .map_err(|error| backend("checking build target run idempotency", &error))?;
        if let Some(replay) = replay {
            let prior: RunRecord = serde_json::from_str(&replay).map_err(|error| {
                invalid_data(format!("decoding build target run replay: {error}"))
            })?;
            if prior.recipe_revision_id != run.recipe_revision_id
                || prior.software_id != run.software_id
                || prior.parameters != run.parameters
            {
                return Err(StorageError::Conflict);
            }
            connection.commit("closing build target run replay").await?;
            return Ok(RunCreation {
                run: prior,
                outcome: CompletionOutcome::Replayed,
            });
        }
        let row = query(
            "SELECT id, name, software_id, recipe_revision_id, parameters_json, trigger_kind,
                    interval_seconds, enabled, next_run_at, revision, created_at, updated_at
             FROM build_targets WHERE id = ?",
        )
        .bind(target_id.to_string())
        .fetch_optional(&mut *connection)
        .await
        .map_err(|error| backend("loading triggered build target", &error))?
        .ok_or(StorageError::NotFound)?;
        let target = build_target_from_row(&row)?;
        if !target.enabled
            || target.software_id != run.software_id
            || target.recipe_revision_id != run.recipe_revision_id
            || target.parameters != run.parameters
        {
            return Err(StorageError::Conflict);
        }
        if matches!(trigger, BuildTargetRunTrigger::Scheduled { .. }) {
            let busy: i64 = query_scalar("SELECT EXISTS(SELECT 1 FROM build_target_runs tr JOIN runs r ON r.id = tr.run_id WHERE tr.target_id = ? AND r.state IN ('queued','running'))")
                .bind(target_id.to_string()).fetch_one(&mut *connection).await.map_err(|error| backend("checking outstanding target run", &error))?;
            if busy != 0 {
                return Err(StorageError::Conflict);
            }
        }
        let (scheduled_for, next_run_at) = match trigger {
            BuildTargetRunTrigger::Manual => (None, None),
            BuildTargetRunTrigger::Scheduled {
                target_revision,
                due_at,
                next_run_at,
            } => {
                let BuildTargetSchedule::Interval { every_seconds } = target.schedule else {
                    return Err(StorageError::Conflict);
                };
                let interval = Duration::seconds(i64::from(every_seconds));
                let advance = next_run_at.signed_duration_since(due_at);
                let advance_seconds = advance.num_seconds();
                let previous_cursor = next_run_at.checked_sub_signed(interval);
                if target.revision != target_revision
                    || target.next_run_at != Some(due_at)
                    || due_at > now
                    || next_run_at <= now
                    || next_run_at <= due_at
                    || advance != Duration::seconds(advance_seconds)
                    || advance_seconds % i64::from(every_seconds) != 0
                    || previous_cursor.is_none_or(|previous| previous > now)
                {
                    return Err(StorageError::Conflict);
                }
                let advanced_revision = target
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| invalid_data("build target revision overflow"))?;
                let changed = query(
                    "UPDATE build_targets SET next_run_at = ?, revision = ?, updated_at = ?
                     WHERE id = ? AND enabled = 1 AND trigger_kind = 'interval'
                       AND next_run_at = ? AND revision = ?",
                )
                .bind(next_run_at)
                .bind(
                    i64::try_from(advanced_revision)
                        .map_err(|_| invalid_data("build target revision exceeds SQLite range"))?,
                )
                .bind(now)
                .bind(target_id.to_string())
                .bind(due_at)
                .bind(
                    i64::try_from(target.revision)
                        .map_err(|_| invalid_data("build target revision exceeds SQLite range"))?,
                )
                .execute(&mut *connection)
                .await
                .map_err(map_write)?
                .rows_affected();
                if changed != 1 {
                    return Err(StorageError::Conflict);
                }
                let mut advanced = target.clone();
                advanced.next_run_at = Some(next_run_at);
                advanced.updated_at = now.max(target.updated_at);
                advanced.revision = advanced_revision;
                insert_build_target_revision(&mut connection, &advanced).await?;
                (Some(due_at), Some(next_run_at))
            }
        };
        insert_queued_run_and_job(&mut connection, run, job).await?;
        query(
            "INSERT INTO build_target_runs
             (run_id, target_id, target_revision, trigger_kind, scheduled_for, next_run_at,
              created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(run.id.to_string())
        .bind(target_id.to_string())
        .bind(
            i64::try_from(target.revision)
                .map_err(|_| invalid_data("build target revision exceeds SQLite range"))?,
        )
        .bind(trigger_name)
        .bind(scheduled_for)
        .bind(next_run_at)
        .bind(now)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        insert_audit(&mut connection, audit).await?;
        let response = serde_json::to_string(run).map_err(|error| {
            invalid_data(format!("serializing build target run response: {error}"))
        })?;
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
        connection.commit("committing build target run").await?;
        Ok(RunCreation {
            run: run.clone(),
            outcome: CompletionOutcome::Completed,
        })
    }
}
