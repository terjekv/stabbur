//! SQLite implementation of the jobs ports.
use super::{
    AttemptId, CapabilitySet, ClaimedJob, CompletionOutcome, DateTime, Duration, Job, JobId,
    JobState, JobStorage, JobSubject, JobSummaryRecord, Lease, SqliteStorage, StorageError, Utc,
    WorkerId, async_trait, backend, invalid_data, job_completion_scope, job_from_row,
    job_state_name, job_subject_fields, job_summary_from_row, map_write, query, query_scalar,
    terminalize_exhausted_catalog_scans,
};

#[async_trait]
impl JobStorage for SqliteStorage {
    async fn enqueue_job(&self, job: &Job) -> Result<(), StorageError> {
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
        .execute(&self.pool)
        .await
        .map_err(map_write)?;
        Ok(())
    }

    async fn job(&self, job_id: JobId) -> Result<Option<Job>, StorageError> {
        let row = query(
            "SELECT id, subject_kind, subject_id, required_capabilities_json, payload_json, state,
                    maximum_attempts, attempt_count, created_at
             FROM jobs WHERE id = ?",
        )
        .bind(job_id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| backend("loading job", &error))?;
        row.as_ref().map(job_from_row).transpose()
    }

    async fn list_jobs(
        &self,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<JobSummaryRecord>, StorageError> {
        if !(1..=200).contains(&limit) {
            return Err(invalid_data("job page limit must be between 1 and 200"));
        }
        let rows = query(
            "SELECT id, subject_kind, subject_id, required_capabilities_json, state,
                    maximum_attempts, attempt_count, created_at
             FROM jobs WHERE id > ? ORDER BY id LIMIT ?",
        )
        .bind(after.unwrap_or(""))
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(|error| backend("listing jobs", &error))?;
        rows.iter().map(job_summary_from_row).collect()
    }

    #[allow(clippy::too_many_lines)] // Expiry recovery and exclusive compatible claim share one transaction.
    async fn claim_job(
        &self,
        worker_id: WorkerId,
        capabilities: &CapabilitySet,
        now: DateTime<Utc>,
        lease_seconds: u32,
    ) -> Result<Option<ClaimedJob>, StorageError> {
        if lease_seconds == 0 {
            return Err(invalid_data("lease duration must be positive"));
        }
        let mut connection = self.acquire_write().await?;
        let eligible: i64 = query_scalar(
            "SELECT EXISTS(SELECT 1 FROM workers WHERE id = ? AND enabled = 1 AND draining = 0)",
        )
        .bind(worker_id.to_string())
        .fetch_one(&mut *connection)
        .await
        .map_err(|error| backend("checking worker claim eligibility", &error))?;
        if eligible == 0 {
            connection.commit("closing ineligible claim").await?;
            return Ok(None);
        }
        query(
            "UPDATE attempts SET state = 'expired', completed_at = ?
             WHERE state = 'leased' AND expires_at <= ?",
        )
        .bind(now)
        .bind(now)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        query(
            "UPDATE jobs SET
               state = CASE WHEN attempt_count >= maximum_attempts THEN 'failed' ELSE 'queued' END,
               completed_at = CASE WHEN attempt_count >= maximum_attempts THEN ? ELSE NULL END
             WHERE state = 'leased' AND NOT EXISTS (
               SELECT 1 FROM attempts a WHERE a.job_id = jobs.id AND a.state = 'leased'
             )",
        )
        .bind(now)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        terminalize_exhausted_catalog_scans(&mut connection, now).await?;
        query(
            "UPDATE runs SET state = 'failed', completed_at = ?
             WHERE state IN ('queued', 'running') AND id IN (
               SELECT subject_id FROM jobs
               WHERE subject_kind = 'build_run' AND state = 'failed'
             )",
        )
        .bind(now)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;

        let advertised = serde_json::to_string(capabilities)
            .map_err(|error| invalid_data(format!("serializing worker capabilities: {error}")))?;
        let row = query(
            "SELECT id, subject_kind, subject_id, required_capabilities_json, payload_json, state,
                    maximum_attempts, attempt_count, created_at
             FROM jobs j
             WHERE state = 'queued' AND attempt_count < maximum_attempts
               AND NOT EXISTS (
                 SELECT 1 FROM json_each(j.required_capabilities_json) required
                 WHERE required.value NOT IN (SELECT value FROM json_each(?))
               )
             ORDER BY created_at, id LIMIT 1",
        )
        .bind(advertised)
        .fetch_optional(&mut *connection)
        .await
        .map_err(|error| backend("selecting queued jobs", &error))?;
        let Some(row) = row else {
            connection.commit("committing empty job claim").await?;
            return Ok(None);
        };
        let mut job = job_from_row(&row)?;
        job.state = JobState::Leased;
        job.attempt_count += 1;
        let changed = query(
            "UPDATE jobs SET state = 'leased', attempt_count = attempt_count + 1
             WHERE id = ? AND state = 'queued'",
        )
        .bind(job.id.to_string())
        .execute(&mut *connection)
        .await
        .map_err(map_write)?
        .rows_affected();
        if changed != 1 {
            return Err(StorageError::Conflict);
        }
        let lease = Lease {
            attempt_id: AttemptId::new(),
            job_id: job.id,
            worker_id,
            heartbeat_at: now,
            expires_at: now + Duration::seconds(i64::from(lease_seconds)),
        };
        query(
            "INSERT INTO attempts (id, job_id, worker_id, state, heartbeat_at, expires_at)
             VALUES (?, ?, ?, 'leased', ?, ?)",
        )
        .bind(lease.attempt_id.to_string())
        .bind(lease.job_id.to_string())
        .bind(lease.worker_id.to_string())
        .bind(lease.heartbeat_at)
        .bind(lease.expires_at)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        if let JobSubject::BuildRun { run_id } = job.subject {
            query("UPDATE runs SET state = 'running' WHERE id = ? AND state = 'queued'")
                .bind(run_id.to_string())
                .execute(&mut *connection)
                .await
                .map_err(map_write)?;
        }
        connection.commit("committing job claim").await?;
        Ok(Some(ClaimedJob { job, lease }))
    }

    async fn heartbeat_job(
        &self,
        worker_id: WorkerId,
        lease: &Lease,
        now: DateTime<Utc>,
        lease_seconds: u32,
    ) -> Result<Lease, StorageError> {
        if lease_seconds == 0 {
            return Err(invalid_data("lease duration must be positive"));
        }
        let expires_at = now + Duration::seconds(i64::from(lease_seconds));
        let changed = query(
            "UPDATE attempts SET heartbeat_at = ?, expires_at = ?
             WHERE id = ? AND job_id = ? AND worker_id = ? AND state = 'leased'
               AND expires_at > ?",
        )
        .bind(now)
        .bind(expires_at)
        .bind(lease.attempt_id.to_string())
        .bind(lease.job_id.to_string())
        .bind(worker_id.to_string())
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(map_write)?
        .rows_affected();
        if changed != 1 {
            return Err(StorageError::InvalidLease);
        }
        Ok(Lease {
            heartbeat_at: now,
            expires_at,
            ..lease.clone()
        })
    }

    async fn complete_job(
        &self,
        worker_id: WorkerId,
        lease: &Lease,
        idempotency_key: &str,
        result: &serde_json::Value,
        now: DateTime<Utc>,
    ) -> Result<CompletionOutcome, StorageError> {
        if idempotency_key.is_empty() || idempotency_key.len() > 255 {
            return Err(invalid_data("idempotency key must contain 1-255 bytes"));
        }
        let mut connection = self.acquire_write().await?;
        let scope = job_completion_scope(lease.job_id);
        let replay: Option<String> =
            query_scalar("SELECT response_json FROM idempotency_keys WHERE scope = ? AND key = ?")
                .bind(&scope)
                .bind(idempotency_key)
                .fetch_optional(&mut *connection)
                .await
                .map_err(|error| backend("checking job idempotency key", &error))?;
        if replay.is_some() {
            connection.commit("committing replayed job result").await?;
            return Ok(CompletionOutcome::Replayed);
        }
        let active: Option<i64> = query_scalar(
            "SELECT 1 FROM attempts
             WHERE id = ? AND job_id = ? AND worker_id = ? AND state = 'leased'
               AND expires_at > ?",
        )
        .bind(lease.attempt_id.to_string())
        .bind(lease.job_id.to_string())
        .bind(worker_id.to_string())
        .bind(now)
        .fetch_optional(&mut *connection)
        .await
        .map_err(|error| backend("validating job lease", &error))?;
        if active.is_none() {
            return Err(StorageError::InvalidLease);
        }
        let result_json = serde_json::to_string(result)
            .map_err(|error| invalid_data(format!("serializing job result: {error}")))?;
        query(
            "INSERT INTO idempotency_keys (scope, key, response_json, created_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(&scope)
        .bind(idempotency_key)
        .bind(&result_json)
        .bind(now)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
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
            "UPDATE runs SET state = 'succeeded', completed_at = ?, result_json = ?
             WHERE id = (SELECT subject_id FROM jobs
                         WHERE id = ? AND subject_kind = 'build_run')",
        )
        .bind(now)
        .bind(&result_json)
        .bind(lease.job_id.to_string())
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        connection.commit("committing job result").await?;
        Ok(CompletionOutcome::Completed)
    }

    async fn fail_job(
        &self,
        worker_id: WorkerId,
        lease: &Lease,
        idempotency_key: &str,
        failure: &serde_json::Value,
        now: DateTime<Utc>,
    ) -> Result<CompletionOutcome, StorageError> {
        if idempotency_key.is_empty() || idempotency_key.len() > 255 {
            return Err(invalid_data("idempotency key must contain 1-255 bytes"));
        }
        let mut connection = self.acquire_write().await?;
        let scope = job_completion_scope(lease.job_id);
        let replay: Option<String> =
            query_scalar("SELECT response_json FROM idempotency_keys WHERE scope = ? AND key = ?")
                .bind(&scope)
                .bind(idempotency_key)
                .fetch_optional(&mut *connection)
                .await
                .map_err(|error| backend("checking job failure idempotency key", &error))?;
        if replay.is_some() {
            connection.commit("committing replayed job failure").await?;
            return Ok(CompletionOutcome::Replayed);
        }
        let active: Option<i64> = query_scalar(
            "SELECT 1 FROM attempts
             WHERE id = ? AND job_id = ? AND worker_id = ? AND state = 'leased'
               AND expires_at > ?",
        )
        .bind(lease.attempt_id.to_string())
        .bind(lease.job_id.to_string())
        .bind(worker_id.to_string())
        .bind(now)
        .fetch_optional(&mut *connection)
        .await
        .map_err(|error| backend("validating failed job lease", &error))?;
        if active.is_none() {
            return Err(StorageError::InvalidLease);
        }
        let failure_json = serde_json::to_string(failure)
            .map_err(|error| invalid_data(format!("serializing job failure: {error}")))?;
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
            "UPDATE runs SET state = 'failed', completed_at = ?, result_json = ?
             WHERE id = (SELECT subject_id FROM jobs
                         WHERE id = ? AND subject_kind = 'build_run')",
        )
        .bind(now)
        .bind(&failure_json)
        .bind(lease.job_id.to_string())
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        connection.commit("committing job failure").await?;
        Ok(CompletionOutcome::Completed)
    }
}
