//! SQLite implementation of the workers ports.
use super::{
    AuditEvent, CapabilitySet, DateTime, PrincipalId, SqliteStorage, StorageError, TokenHash, Utc,
    WorkerId, WorkerRecord, WorkerStorage, async_trait, backend, insert_audit, invalid_data,
    invalidate_worker_leases, map_write, query, worker_from_row,
};

#[async_trait]
impl WorkerStorage for SqliteStorage {
    async fn set_worker_draining(
        &self,
        worker_id: WorkerId,
        draining: bool,
        expected_revision: u64,
        audit: &AuditEvent,
    ) -> Result<WorkerRecord, StorageError> {
        let mut connection = self.acquire_write().await?;
        let changed = query("UPDATE workers SET draining = ?, revision = revision + 1 WHERE id = ? AND revision = ? AND enabled = 1")
            .bind(i64::from(draining)).bind(worker_id.to_string())
            .bind(i64::try_from(expected_revision).map_err(|_| invalid_data("worker revision overflow"))?)
            .execute(&mut *connection).await.map_err(map_write)?.rows_affected();
        if changed != 1 {
            return Err(StorageError::StaleRevision);
        }
        let row = query("SELECT w.id, w.principal_id, w.name, w.allowed_capabilities_json, w.enabled, w.last_seen_at, w.revision, w.draining,
            COALESCE((SELECT json_group_array(capability) FROM worker_capabilities c WHERE c.worker_id = w.id), '[]') AS advertised_capabilities_json
            FROM workers w WHERE w.id = ?")
            .bind(worker_id.to_string()).fetch_one(&mut *connection).await.map_err(|error| backend("loading drained worker", &error))?;
        let worker = worker_from_row(&row)?;
        insert_audit(&mut connection, audit).await?;
        connection.commit("committing worker drain").await?;
        Ok(worker)
    }

    async fn provision_worker(
        &self,
        worker: &WorkerRecord,
        token_hash: &TokenHash,
        audit: &AuditEvent,
    ) -> Result<(), StorageError> {
        let principal_id = worker
            .principal_id
            .ok_or_else(|| invalid_data("provisioned worker requires a principal"))?;
        if worker.name.trim() != worker.name || !(1..=128).contains(&worker.name.len()) {
            return Err(invalid_data("worker name must contain 1-128 bytes"));
        }
        let allowed = serde_json::to_string(&worker.allowed_capabilities)
            .map_err(|error| invalid_data(format!("serializing allowed capabilities: {error}")))?;
        let mut connection = self.acquire_write().await?;
        query(
            "INSERT INTO principals (id, name, kind, password_hash, enabled, created_at)
             VALUES (?, ?, 'worker', NULL, ?, ?)",
        )
        .bind(principal_id.to_string())
        .bind(format!("worker-{}", worker.id))
        .bind(i64::from(worker.enabled))
        .bind(worker.last_seen_at)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        query(
            "INSERT INTO workers
             (id, name, enabled, registered_at, last_seen_at, principal_id,
              allowed_capabilities_json)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(worker.id.to_string())
        .bind(&worker.name)
        .bind(i64::from(worker.enabled))
        .bind(worker.last_seen_at)
        .bind(worker.last_seen_at)
        .bind(principal_id.to_string())
        .bind(allowed)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        query(
            "INSERT INTO credentials
             (id, principal_id, name, kind, token_hash, created_at, expires_at)
             VALUES (?, ?, ?, 'worker', ?, ?, NULL)",
        )
        .bind(uuid::Uuid::now_v7().to_string())
        .bind(principal_id.to_string())
        .bind(&worker.name)
        .bind(token_hash.expose_for_persistence())
        .bind(worker.last_seen_at)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        insert_audit(&mut connection, audit).await?;
        connection.commit("committing worker provisioning").await?;
        Ok(())
    }

    async fn worker_for_principal(
        &self,
        principal_id: PrincipalId,
    ) -> Result<Option<WorkerRecord>, StorageError> {
        let row = query(
            "SELECT w.id, w.principal_id, w.name, w.allowed_capabilities_json, w.enabled,
                    w.last_seen_at, w.revision, w.draining,
                    COALESCE((SELECT json_group_array(capability)
                              FROM worker_capabilities c WHERE c.worker_id = w.id), '[]')
                      AS advertised_capabilities_json
             FROM workers w WHERE w.principal_id = ?",
        )
        .bind(principal_id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| backend("loading worker by principal", &error))?;
        row.as_ref().map(worker_from_row).transpose()
    }

    async fn worker(&self, worker_id: WorkerId) -> Result<Option<WorkerRecord>, StorageError> {
        let row = query(
            "SELECT w.id, w.principal_id, w.name, w.allowed_capabilities_json, w.enabled,
                    w.last_seen_at, w.revision, w.draining,
                    COALESCE((SELECT json_group_array(capability)
                              FROM worker_capabilities c WHERE c.worker_id = w.id), '[]')
                      AS advertised_capabilities_json
             FROM workers w WHERE w.id = ?",
        )
        .bind(worker_id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| backend("loading worker", &error))?;
        row.as_ref().map(worker_from_row).transpose()
    }

    async fn register_worker(
        &self,
        worker_id: WorkerId,
        name: &str,
        capabilities: &CapabilitySet,
        now: DateTime<Utc>,
    ) -> Result<(), StorageError> {
        let mut connection = self.acquire_write().await?;
        query(
            "INSERT INTO workers (id, name, registered_at, last_seen_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET last_seen_at = excluded.last_seen_at",
        )
        .bind(worker_id.to_string())
        .bind(name)
        .bind(now)
        .bind(now)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        query("DELETE FROM worker_capabilities WHERE worker_id = ?")
            .bind(worker_id.to_string())
            .execute(&mut *connection)
            .await
            .map_err(map_write)?;
        for capability in capabilities.iter() {
            query("INSERT INTO worker_capabilities (worker_id, capability) VALUES (?, ?)")
                .bind(worker_id.to_string())
                .bind(capability.as_str())
                .execute(&mut *connection)
                .await
                .map_err(map_write)?;
        }
        connection.commit("committing worker registration").await?;
        Ok(())
    }

    async fn list_workers(
        &self,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<WorkerRecord>, StorageError> {
        if !(1..=200).contains(&limit) {
            return Err(invalid_data("worker page limit must be between 1 and 200"));
        }
        let rows = query(
            "SELECT w.id, w.principal_id, w.name, w.allowed_capabilities_json, w.enabled,
                    w.last_seen_at, w.revision, w.draining,
                    COALESCE((SELECT json_group_array(capability)
                              FROM worker_capabilities c WHERE c.worker_id = w.id), '[]')
                      AS advertised_capabilities_json
             FROM workers w WHERE w.id > ? ORDER BY w.id LIMIT ?",
        )
        .bind(after.unwrap_or(""))
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(|error| backend("listing workers", &error))?;
        rows.iter().map(worker_from_row).collect()
    }

    #[allow(clippy::too_many_lines)] // This transaction deliberately couples worker revocation and lease recovery.
    async fn update_worker(
        &self,
        worker_id: WorkerId,
        enabled: Option<bool>,
        allowed_capabilities: Option<&CapabilitySet>,
        expected_revision: u64,
        audit: &AuditEvent,
        now: DateTime<Utc>,
    ) -> Result<WorkerRecord, StorageError> {
        if enabled.is_some() == allowed_capabilities.is_some() {
            return Err(invalid_data(
                "change exactly one worker status or capability ceiling field",
            ));
        }
        let mut connection = self.acquire_write().await?;
        let current_row = query(
            "SELECT w.id, w.principal_id, w.name, w.allowed_capabilities_json, w.enabled,
                    w.last_seen_at, w.revision, w.draining,
                    COALESCE((SELECT json_group_array(capability)
                              FROM worker_capabilities c WHERE c.worker_id = w.id), '[]')
                      AS advertised_capabilities_json
             FROM workers w WHERE w.id = ?",
        )
        .bind(worker_id.to_string())
        .fetch_optional(&mut *connection)
        .await
        .map_err(|error| backend("loading updated worker", &error))?
        .ok_or(StorageError::NotFound)?;
        let current = worker_from_row(&current_row)?;
        if current.revision != expected_revision {
            return Err(StorageError::StaleRevision);
        }
        let principal_id = current
            .principal_id
            .ok_or_else(|| invalid_data("managed worker does not have a principal"))?;
        if let Some(enabled) = enabled {
            query(
                "UPDATE workers SET enabled = ?, revision = revision + 1
                 WHERE id = ? AND revision = ?",
            )
            .bind(i64::from(enabled))
            .bind(worker_id.to_string())
            .bind(
                i64::try_from(expected_revision)
                    .map_err(|_| invalid_data("worker revision exceeds SQLite range"))?,
            )
            .execute(&mut *connection)
            .await
            .map_err(map_write)?;
            query("UPDATE principals SET enabled = ?, revision = revision + 1 WHERE id = ?")
                .bind(i64::from(enabled))
                .bind(principal_id.to_string())
                .execute(&mut *connection)
                .await
                .map_err(map_write)?;
            if !enabled {
                query(
                    "UPDATE credentials SET revoked_at = ?
                     WHERE principal_id = ? AND revoked_at IS NULL",
                )
                .bind(now)
                .bind(principal_id.to_string())
                .execute(&mut *connection)
                .await
                .map_err(map_write)?;
                invalidate_worker_leases(&mut connection, worker_id, now).await?;
            }
        } else if let Some(allowed) = allowed_capabilities {
            let allowed = serde_json::to_string(allowed).map_err(|error| {
                invalid_data(format!("serializing allowed worker capabilities: {error}"))
            })?;
            query(
                "UPDATE workers SET allowed_capabilities_json = ?, revision = revision + 1
                 WHERE id = ? AND revision = ?",
            )
            .bind(allowed)
            .bind(worker_id.to_string())
            .bind(
                i64::try_from(expected_revision)
                    .map_err(|_| invalid_data("worker revision exceeds SQLite range"))?,
            )
            .execute(&mut *connection)
            .await
            .map_err(map_write)?;
            query("DELETE FROM worker_capabilities WHERE worker_id = ?")
                .bind(worker_id.to_string())
                .execute(&mut *connection)
                .await
                .map_err(map_write)?;
            invalidate_worker_leases(&mut connection, worker_id, now).await?;
        }
        insert_audit(&mut connection, audit).await?;
        let updated_row = query(
            "SELECT w.id, w.principal_id, w.name, w.allowed_capabilities_json, w.enabled,
                    w.last_seen_at, w.revision, w.draining,
                    COALESCE((SELECT json_group_array(capability)
                              FROM worker_capabilities c WHERE c.worker_id = w.id), '[]')
                      AS advertised_capabilities_json
             FROM workers w WHERE w.id = ?",
        )
        .bind(worker_id.to_string())
        .fetch_one(&mut *connection)
        .await
        .map_err(|error| backend("loading updated worker result", &error))?;
        let updated = worker_from_row(&updated_row)?;
        connection.commit("committing worker update").await?;
        Ok(updated)
    }

    async fn rotate_worker_credential(
        &self,
        worker_id: WorkerId,
        token_hash: &TokenHash,
        expected_revision: u64,
        audit: &AuditEvent,
        now: DateTime<Utc>,
    ) -> Result<WorkerRecord, StorageError> {
        let mut connection = self.acquire_write().await?;
        let current_row = query(
            "SELECT w.id, w.principal_id, w.name, w.allowed_capabilities_json, w.enabled,
                    w.last_seen_at, w.revision, w.draining,
                    COALESCE((SELECT json_group_array(capability)
                              FROM worker_capabilities c WHERE c.worker_id = w.id), '[]')
                      AS advertised_capabilities_json
             FROM workers w WHERE w.id = ?",
        )
        .bind(worker_id.to_string())
        .fetch_optional(&mut *connection)
        .await
        .map_err(|error| backend("loading credential-rotated worker", &error))?
        .ok_or(StorageError::NotFound)?;
        let current = worker_from_row(&current_row)?;
        if current.revision != expected_revision {
            return Err(StorageError::StaleRevision);
        }
        if !current.enabled {
            return Err(StorageError::Conflict);
        }
        let principal_id = current
            .principal_id
            .ok_or_else(|| invalid_data("managed worker does not have a principal"))?;
        query(
            "UPDATE credentials SET revoked_at = ?
             WHERE principal_id = ? AND kind = 'worker' AND revoked_at IS NULL",
        )
        .bind(now)
        .bind(principal_id.to_string())
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        query(
            "INSERT INTO credentials
             (id, principal_id, name, kind, token_hash, created_at, expires_at)
             VALUES (?, ?, ?, 'worker', ?, ?, NULL)",
        )
        .bind(uuid::Uuid::now_v7().to_string())
        .bind(principal_id.to_string())
        .bind(&current.name)
        .bind(token_hash.expose_for_persistence())
        .bind(now)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        let changed =
            query("UPDATE workers SET revision = revision + 1 WHERE id = ? AND revision = ?")
                .bind(worker_id.to_string())
                .bind(
                    i64::try_from(expected_revision)
                        .map_err(|_| invalid_data("worker revision exceeds SQLite range"))?,
                )
                .execute(&mut *connection)
                .await
                .map_err(map_write)?
                .rows_affected();
        if changed != 1 {
            return Err(StorageError::StaleRevision);
        }
        insert_audit(&mut connection, audit).await?;
        let updated_row = query(
            "SELECT w.id, w.principal_id, w.name, w.allowed_capabilities_json, w.enabled,
                    w.last_seen_at, w.revision, w.draining,
                    COALESCE((SELECT json_group_array(capability)
                              FROM worker_capabilities c WHERE c.worker_id = w.id), '[]')
                      AS advertised_capabilities_json
             FROM workers w WHERE w.id = ?",
        )
        .bind(worker_id.to_string())
        .fetch_one(&mut *connection)
        .await
        .map_err(|error| backend("loading credential rotation result", &error))?;
        let updated = worker_from_row(&updated_row)?;
        connection
            .commit("committing worker credential rotation")
            .await?;
        Ok(updated)
    }
}
