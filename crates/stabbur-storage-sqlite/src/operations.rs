//! SQLite implementation of the operations ports.
use super::{
    AuditEvent, AuditStorage, OperationalStorage, Row, SqliteStorage, StorageError, StorageHealth,
    async_trait, audit_from_row, backend, embedded_migrator, invalid_data, map_write, query,
    query_scalar,
};

#[async_trait]
impl AuditStorage for SqliteStorage {
    async fn audit_events(
        &self,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<AuditEvent>, StorageError> {
        let rows = query(
            "SELECT id, actor_json, action, resource_kind, resource_id, details_json,
                    request_id, occurred_at
             FROM audit_events WHERE id > ? ORDER BY id LIMIT ?",
        )
        .bind(after.unwrap_or(""))
        .bind(i64::from(limit.min(200)))
        .fetch_all(&self.pool)
        .await
        .map_err(|error| backend("listing audit events", &error))?;
        rows.iter().map(audit_from_row).collect()
    }
}

#[async_trait]
impl OperationalStorage for SqliteStorage {
    async fn operational_status(
        &self,
    ) -> Result<stabbur_storage_core::OperationalStatus, StorageError> {
        let row = query(
            "SELECT (SELECT COUNT(*) FROM jobs WHERE state = 'queued') AS queued_jobs,
            (SELECT COUNT(*) FROM jobs WHERE state = 'leased') AS running_jobs,
            (SELECT COUNT(*) FROM jobs WHERE state = 'failed') AS failed_jobs,
            (SELECT COUNT(*) FROM attempts WHERE state = 'expired') AS expired_attempts,
            (SELECT COUNT(*) FROM workers WHERE enabled = 1 AND draining = 1) AS draining_workers,
            (SELECT MIN(created_at) FROM jobs WHERE state = 'queued') AS oldest_queued_at",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(map_write)?;
        let count = |name| -> Result<u64, StorageError> {
            u64::try_from(row.try_get::<i64, _>(name).map_err(map_write)?)
                .map_err(|_| invalid_data("invalid operational count"))
        };
        let now = chrono::Utc::now();
        let groups = query("WITH queues AS (
            SELECT required_capabilities_json, COUNT(*) AS queued_jobs, MIN(created_at) AS oldest_queued_at
            FROM jobs WHERE state = 'queued' GROUP BY required_capabilities_json ORDER BY oldest_queued_at, required_capabilities_json LIMIT 201
        ), matching AS (
            SELECT q.required_capabilities_json, w.id,
                EXISTS(SELECT 1 FROM attempts a WHERE a.worker_id = w.id AND a.state = 'leased' AND a.expires_at > ?1) AS busy
            FROM queues q JOIN workers w ON w.enabled = 1 AND w.draining = 0 AND w.last_seen_at >= ?2
            WHERE NOT EXISTS (SELECT 1 FROM json_each(q.required_capabilities_json) requirement
                WHERE NOT EXISTS (SELECT 1 FROM worker_capabilities wc WHERE wc.worker_id = w.id AND wc.capability = requirement.value))
        ) SELECT q.*, (SELECT COUNT(*) FROM matching m WHERE m.required_capabilities_json = q.required_capabilities_json) AS matching_workers,
            (SELECT COUNT(*) FROM matching m WHERE m.required_capabilities_json = q.required_capabilities_json AND m.busy) AS busy_workers
          FROM queues q ORDER BY oldest_queued_at, required_capabilities_json")
            .bind(now).bind(now - chrono::Duration::minutes(5)).fetch_all(&self.pool).await.map_err(map_write)?;
        let capability_queues_truncated = groups.len() > 200;
        let capability_queues = groups
            .iter()
            .take(200)
            .map(|row| {
                let count = |key| -> Result<u64, StorageError> {
                    u64::try_from(row.try_get::<i64, _>(key).map_err(map_write)?)
                        .map_err(|_| invalid_data("invalid capability queue count"))
                };
                Ok(stabbur_storage_core::CapabilityQueue {
                    required_capabilities: serde_json::from_str(
                        row.try_get("required_capabilities_json")
                            .map_err(map_write)?,
                    )
                    .map_err(|_| invalid_data("invalid queue requirements"))?,
                    queued_jobs: count("queued_jobs")?,
                    matching_workers: count("matching_workers")?,
                    workers_with_active_leases: count("busy_workers")?,
                    oldest_queued_at: row.try_get("oldest_queued_at").map_err(map_write)?,
                })
            })
            .collect::<Result<Vec<_>, StorageError>>()?;
        Ok(stabbur_storage_core::OperationalStatus {
            capability_queues,
            capability_queues_truncated,
            queued_jobs: count("queued_jobs")?,
            running_jobs: count("running_jobs")?,
            failed_jobs: count("failed_jobs")?,
            expired_attempts: count("expired_attempts")?,
            draining_workers: count("draining_workers")?,
            oldest_queued_at: row.try_get("oldest_queued_at").map_err(map_write)?,
        })
    }

    async fn migrate(&self) -> Result<(), StorageError> {
        self.run_migrations().await
    }

    async fn check_readiness(&self) -> Result<(), StorageError> {
        let foreign_keys: i64 = query_scalar("PRAGMA foreign_keys")
            .fetch_one(&self.pool)
            .await
            .map_err(|error| backend("checking SQLite readiness", &error))?;
        let applied_migrations: i64 =
            query_scalar("SELECT COUNT(*) FROM _sqlx_migrations WHERE success = TRUE")
                .fetch_one(&self.pool)
                .await
                .map_err(|error| backend("checking SQLite schema readiness", &error))?;
        let expected_migrations = i64::try_from(embedded_migrator().migrations.len())
            .map_err(|_| invalid_data("too many embedded migrations"))?;
        if foreign_keys != 1 || applied_migrations != expected_migrations {
            return Err(StorageError::Backend {
                message: "SQLite readiness invariant failed".into(),
            });
        }
        Ok(())
    }

    async fn doctor(&self) -> Result<StorageHealth, StorageError> {
        let foreign_keys: i64 = query_scalar("PRAGMA foreign_keys")
            .fetch_one(&self.pool)
            .await
            .map_err(|error| backend("checking SQLite foreign keys", &error))?;
        let software_count: i64 = query_scalar("SELECT COUNT(*) FROM software")
            .fetch_one(&self.pool)
            .await
            .map_err(|error| backend("counting software", &error))?;
        let artifact_count: i64 = query_scalar("SELECT COUNT(*) FROM artifacts")
            .fetch_one(&self.pool)
            .await
            .map_err(|error| backend("counting artifacts", &error))?;
        let active_job_count: i64 =
            query_scalar("SELECT COUNT(*) FROM jobs WHERE state IN ('queued', 'leased')")
                .fetch_one(&self.pool)
                .await
                .map_err(|error| backend("counting active jobs", &error))?;
        Ok(StorageHealth {
            backend: "sqlite".to_owned(),
            database_ready: foreign_keys == 1,
            software_count: u64::try_from(software_count)
                .map_err(|_| invalid_data("negative software count"))?,
            artifact_count: u64::try_from(artifact_count)
                .map_err(|_| invalid_data("negative artifact count"))?,
            active_job_count: u64::try_from(active_job_count)
                .map_err(|_| invalid_data("negative active job count"))?,
        })
    }
}
