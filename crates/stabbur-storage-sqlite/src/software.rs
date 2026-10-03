//! SQLite implementation of the software ports.
use super::{
    AuditEvent, DateTime, Duration, Row, Software, SoftwareInstallation, SoftwareStorage,
    SqliteStorage, StorageError, Utc, async_trait, backend, insert_audit, invalid_data, map_write,
    parse_value, query, query_scalar, run_summary_from_row, software_from_row,
    software_installation_from_row,
};

#[async_trait]
impl SoftwareStorage for SqliteStorage {
    async fn software_library(
        &self,
        request: &stabbur_storage_core::LibraryQuery,
        now: DateTime<Utc>,
    ) -> Result<Vec<stabbur_storage_core::LibraryEntry>, StorageError> {
        super::library::read(self, request, now).await
    }
    async fn software_status(
        &self,
        software_id: stabbur_domain::SoftwareId,
        now: DateTime<Utc>,
    ) -> Result<stabbur_storage_core::SoftwareStatus, StorageError> {
        use stabbur_storage_core::{BlockedBuildTarget, SoftwareChannelSummary, SoftwareStatus};
        let rows = query("SELECT c.name, c.release_id, r.version, c.revision FROM channels c JOIN releases r ON r.id = c.release_id WHERE c.software_id = ? ORDER BY c.name")
            .bind(software_id.to_string()).fetch_all(&self.pool).await.map_err(|error| backend("loading status channels", &error))?;
        let channels = rows
            .iter()
            .map(|row| {
                Ok(SoftwareChannelSummary {
                    name: row.try_get("name").map_err(map_write)?,
                    release_id: parse_value(
                        row.try_get("release_id").map_err(map_write)?,
                        "release ID",
                    )?,
                    version: parse_value(row.try_get("version").map_err(map_write)?, "version")?,
                    revision: u64::try_from(row.try_get::<i64, _>("revision").map_err(map_write)?)
                        .map_err(|_| invalid_data("invalid channel revision"))?,
                })
            })
            .collect::<Result<Vec<_>, StorageError>>()?;
        let row = query("SELECT id, recipe_revision_id, software_id, state, created_at, completed_at FROM runs WHERE software_id = ? ORDER BY created_at DESC, id DESC LIMIT 1")
            .bind(software_id.to_string()).fetch_optional(&self.pool).await.map_err(map_write)?;
        let latest_run = row.as_ref().map(run_summary_from_row).transpose()?;
        let last_success_at = query_scalar(
            "SELECT MAX(completed_at) FROM runs WHERE software_id = ? AND state = 'succeeded'",
        )
        .bind(software_id.to_string())
        .fetch_one(&self.pool)
        .await
        .map_err(map_write)?;
        let next_run_at = query_scalar(
            "SELECT MIN(next_run_at) FROM build_targets WHERE software_id = ? AND enabled = 1",
        )
        .bind(software_id.to_string())
        .fetch_one(&self.pool)
        .await
        .map_err(map_write)?;
        let enabled_targets: i64 = query_scalar(
            "SELECT COUNT(*) FROM build_targets WHERE software_id = ? AND enabled = 1",
        )
        .bind(software_id.to_string())
        .fetch_one(&self.pool)
        .await
        .map_err(map_write)?;
        let outstanding_runs: i64 = query_scalar(
            "SELECT COUNT(*) FROM runs WHERE software_id = ? AND state IN ('queued','running')",
        )
        .bind(software_id.to_string())
        .fetch_one(&self.pool)
        .await
        .map_err(map_write)?;
        let rows = query("SELECT t.id, t.name, rr.required_capabilities_json FROM build_targets t JOIN recipe_revisions rr ON rr.id = t.recipe_revision_id
            WHERE t.software_id = ? AND t.enabled = 1 AND NOT EXISTS (
                SELECT 1 FROM workers w WHERE w.enabled = 1 AND w.draining = 0 AND w.last_seen_at >= ?
                AND NOT EXISTS (SELECT 1 FROM json_each(rr.required_capabilities_json) requirement
                    WHERE NOT EXISTS (SELECT 1 FROM worker_capabilities wc WHERE wc.worker_id = w.id AND wc.capability = requirement.value)))
            ORDER BY t.id LIMIT 200")
            .bind(software_id.to_string()).bind(now - Duration::minutes(5)).fetch_all(&self.pool).await.map_err(map_write)?;
        let blocked_targets = rows
            .iter()
            .map(|row| {
                Ok(BlockedBuildTarget {
                    id: parse_value(row.try_get("id").map_err(map_write)?, "target ID")?,
                    name: row.try_get("name").map_err(map_write)?,
                    required_capabilities: serde_json::from_str(
                        row.try_get::<&str, _>("required_capabilities_json")
                            .map_err(map_write)?,
                    )
                    .map_err(|_| invalid_data("invalid target capabilities"))?,
                })
            })
            .collect::<Result<Vec<_>, StorageError>>()?;
        Ok(SoftwareStatus {
            software_id,
            channels,
            latest_run,
            last_success_at,
            next_run_at,
            enabled_targets: u64::try_from(enabled_targets)
                .map_err(|_| invalid_data("invalid target count"))?,
            outstanding_runs: u64::try_from(outstanding_runs)
                .map_err(|_| invalid_data("invalid run count"))?,
            blocked_targets,
        })
    }

    async fn software(&self, identity: &str) -> Result<Option<Software>, StorageError> {
        let row = query(
            "SELECT id, slug, name, created_at, revision FROM software
             WHERE id = ? OR slug = ?",
        )
        .bind(identity)
        .bind(identity)
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| backend("loading software", &error))?;
        row.as_ref().map(software_from_row).transpose()
    }

    async fn list_software(
        &self,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<Software>, StorageError> {
        let rows = query(
            "SELECT id, slug, name, created_at, revision FROM software
             WHERE id > ? ORDER BY id LIMIT ?",
        )
        .bind(after.unwrap_or(""))
        .bind(i64::from(limit.min(200)))
        .fetch_all(&self.pool)
        .await
        .map_err(|error| backend("listing software", &error))?;
        rows.iter().map(software_from_row).collect()
    }

    async fn software_installation(
        &self,
        software_id: stabbur_domain::SoftwareId,
    ) -> Result<Option<SoftwareInstallation>, StorageError> {
        let row = query(
            "SELECT software_id, install_json, detection_json
             FROM software_installation WHERE software_id = ?",
        )
        .bind(software_id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| backend("loading software installation metadata", &error))?;
        row.as_ref().map(software_installation_from_row).transpose()
    }

    async fn update_software(
        &self,
        software_id: stabbur_domain::SoftwareId,
        name: Option<&str>,
        installation: Option<&SoftwareInstallation>,
        expected_revision: u64,
        audit: &AuditEvent,
    ) -> Result<Software, StorageError> {
        if name.is_none() == installation.is_none()
            || name.is_some_and(|name| name.trim() != name || !(1..=255).contains(&name.len()))
            || installation.is_some_and(|installation| installation.software_id != software_id)
        {
            return Err(invalid_data(
                "change exactly one valid software name or installation field",
            ));
        }
        let mut connection = self.acquire_write().await?;
        let changed = query(
            "UPDATE software SET name = COALESCE(?, name), revision = revision + 1
             WHERE id = ? AND revision = ?",
        )
        .bind(name)
        .bind(software_id.to_string())
        .bind(
            i64::try_from(expected_revision)
                .map_err(|_| invalid_data("software revision exceeds SQLite range"))?,
        )
        .execute(&mut *connection)
        .await
        .map_err(map_write)?
        .rows_affected();
        if changed != 1 {
            let exists: bool = query_scalar("SELECT EXISTS(SELECT 1 FROM software WHERE id = ?)")
                .bind(software_id.to_string())
                .fetch_one(&mut *connection)
                .await
                .map_err(|error| backend("checking updated software", &error))?;
            return Err(if exists {
                StorageError::StaleRevision
            } else {
                StorageError::NotFound
            });
        }
        if let Some(installation) = installation {
            let install = serde_json::to_string(&installation.install).map_err(|error| {
                invalid_data(format!("serializing installation metadata: {error}"))
            })?;
            let detection = serde_json::to_string(&installation.detection).map_err(|error| {
                invalid_data(format!("serializing detection metadata: {error}"))
            })?;
            query(
                "INSERT INTO software_installation (software_id, install_json, detection_json)
                 VALUES (?, ?, ?)
                 ON CONFLICT(software_id) DO UPDATE SET
                   install_json = excluded.install_json, detection_json = excluded.detection_json",
            )
            .bind(software_id.to_string())
            .bind(install)
            .bind(detection)
            .execute(&mut *connection)
            .await
            .map_err(map_write)?;
        }
        insert_audit(&mut connection, audit).await?;
        let row = query("SELECT id, slug, name, created_at, revision FROM software WHERE id = ?")
            .bind(software_id.to_string())
            .fetch_one(&mut *connection)
            .await
            .map_err(|error| backend("loading updated software", &error))?;
        let software = software_from_row(&row)?;
        connection.commit("committing software update").await?;
        Ok(software)
    }
}
