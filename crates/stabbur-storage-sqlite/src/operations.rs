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
        Ok(stabbur_storage_core::OperationalStatus {
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
