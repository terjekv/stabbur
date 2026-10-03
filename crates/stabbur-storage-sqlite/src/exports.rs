//! Atomic saved export configuration and publication.
use super::{
    AuditEvent, Row, SqliteRow, SqliteStorage, StorageError, TokenHash, async_trait, insert_audit,
    invalid_data, map_write, query, query_scalar,
};
use stabbur_domain::{
    ExportId,
    exports::{ExportDefinition, PreparedExport},
};
use stabbur_storage_core::{ExportRecord, ExportSnapshot, ExportStorage};

fn number(value: u64) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| invalid_data("export revision exceeds the supported range"))
}
fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, StorageError> {
    serde_json::from_str(value).map_err(|_| invalid_data("invalid persisted export"))
}
fn encode<T: serde::Serialize>(value: &T) -> Result<String, StorageError> {
    serde_json::to_string(value).map_err(|_| invalid_data("invalid export serialization"))
}
fn record(row: &SqliteRow) -> Result<ExportRecord, StorageError> {
    Ok(ExportRecord {
        id: row
            .try_get::<String, _>("id")
            .map_err(map_write)?
            .parse()
            .map_err(|_| invalid_data("invalid export identity"))?,
        definition: decode(
            &row.try_get::<String, _>("definition_json")
                .map_err(map_write)?,
        )?,
        revision: u64::try_from(row.try_get::<i64, _>("revision").map_err(map_write)?)
            .map_err(|_| invalid_data("invalid export revision"))?,
        generation: u64::try_from(row.try_get::<i64, _>("generation").map_err(map_write)?)
            .map_err(|_| invalid_data("invalid export generation"))?,
        updated_at: row.try_get("updated_at").map_err(map_write)?,
    })
}
fn page(limit: u32) -> Result<i64, StorageError> {
    if !(1..=200).contains(&limit) {
        return Err(invalid_data("export page limit must be between 1 and 200"));
    }
    Ok(i64::from(limit))
}
#[async_trait]
impl ExportStorage for SqliteStorage {
    async fn list_exports(
        &self,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<ExportRecord>, StorageError> {
        query("SELECT * FROM exports WHERE id > ? ORDER BY id LIMIT ?")
            .bind(after.unwrap_or(""))
            .bind(page(limit)?)
            .fetch_all(&self.pool)
            .await
            .map_err(map_write)?
            .iter()
            .map(record)
            .collect()
    }
    async fn export(&self, identity: &str) -> Result<Option<ExportRecord>, StorageError> {
        query("SELECT * FROM exports WHERE id = ? OR slug = ?")
            .bind(identity)
            .bind(identity)
            .fetch_optional(&self.pool)
            .await
            .map_err(map_write)?
            .as_ref()
            .map(record)
            .transpose()
    }
    async fn create_export(
        &self,
        id: ExportId,
        definition: &ExportDefinition,
        audit: &AuditEvent,
    ) -> Result<ExportRecord, StorageError> {
        let mut connection = self.acquire_write().await?;
        for selection in &definition.data().selections {
            let exists: i64 = query_scalar("SELECT COUNT(*) FROM software WHERE id = ?")
                .bind(selection.software.to_string())
                .fetch_one(&mut *connection)
                .await
                .map_err(map_write)?;
            if exists != 1 {
                return Err(StorageError::NotFound);
            }
        }
        let json = encode(definition)?;
        query("INSERT INTO exports(id, slug, definition_json, revision, updated_at) VALUES (?, ?, ?, 1, ?)")
            .bind(id.to_string()).bind(definition.data().slug.as_str()).bind(&json).bind(audit.occurred_at).execute(&mut *connection).await.map_err(map_write)?;
        query("INSERT INTO export_definitions VALUES (?, 1, ?, ?)")
            .bind(id.to_string())
            .bind(json)
            .bind(audit.occurred_at)
            .execute(&mut *connection)
            .await
            .map_err(map_write)?;
        insert_audit(&mut connection, audit).await?;
        connection.commit("creating saved export").await?;
        Ok(ExportRecord {
            id,
            definition: definition.clone(),
            revision: 1,
            generation: 0,
            updated_at: audit.occurred_at,
        })
    }
    async fn update_export(
        &self,
        id: ExportId,
        definition: &ExportDefinition,
        revision: u64,
        audit: &AuditEvent,
    ) -> Result<ExportRecord, StorageError> {
        let mut connection = self.acquire_write().await?;
        let row = query("SELECT * FROM exports WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&mut *connection)
            .await
            .map_err(map_write)?
            .ok_or(StorageError::NotFound)?;
        let previous = record(&row)?;
        if previous.revision != revision {
            return Err(StorageError::StaleRevision);
        }
        for selection in &definition.data().selections {
            let exists: i64 = query_scalar("SELECT COUNT(*) FROM software WHERE id = ?")
                .bind(selection.software.to_string())
                .fetch_one(&mut *connection)
                .await
                .map_err(map_write)?;
            if exists != 1 {
                return Err(StorageError::NotFound);
            }
        }
        let next = revision.checked_add(1).ok_or(StorageError::Conflict)?;
        let json = encode(definition)?;
        query("UPDATE exports SET slug = ?, definition_json = ?, revision = ?, updated_at = ? WHERE id = ?")
            .bind(definition.data().slug.as_str()).bind(&json).bind(number(next)?).bind(audit.occurred_at).bind(id.to_string()).execute(&mut *connection).await.map_err(map_write)?;
        query("INSERT INTO export_definitions VALUES (?, ?, ?, ?)")
            .bind(id.to_string())
            .bind(number(next)?)
            .bind(json)
            .bind(audit.occurred_at)
            .execute(&mut *connection)
            .await
            .map_err(map_write)?;
        insert_audit(&mut connection, audit).await?;
        connection.commit("updating saved export").await?;
        Ok(ExportRecord {
            id,
            definition: definition.clone(),
            revision: next,
            generation: previous.generation,
            updated_at: audit.occurred_at,
        })
    }
    #[allow(clippy::too_many_lines)] // Keep all publication fences in one auditable transaction.
    async fn apply_export(
        &self,
        publication: &PreparedExport,
        audit: &AuditEvent,
    ) -> Result<ExportSnapshot, StorageError> {
        let mut connection = self.acquire_write().await?;
        let row = query("SELECT * FROM exports WHERE id = ?")
            .bind(publication.id().to_string())
            .fetch_optional(&mut *connection)
            .await
            .map_err(map_write)?
            .ok_or(StorageError::NotFound)?;
        let current = record(&row)?;
        if current.revision != publication.revision()
            || current.generation != publication.generation()
        {
            return Err(StorageError::StaleRevision);
        }
        if publication.bindings().len() != current.definition.data().selections.len() {
            return Err(invalid_data(
                "export bindings do not cover the saved selection",
            ));
        }
        for selection in &current.definition.data().selections {
            let binding = publication
                .bindings()
                .iter()
                .find(|b| b.software == selection.software)
                .ok_or_else(|| invalid_data("export selection has no binding"))?;
            let source_matches = match &selection.source {
                stabbur_domain::exports::ExportSource::Channel { channel } => binding
                    .channel
                    .as_ref()
                    .is_some_and(|(name, _)| name == channel),
                stabbur_domain::exports::ExportSource::Release { release } => {
                    binding.release == *release && binding.channel.is_none()
                }
            };
            if !source_matches
                || !publication
                    .items()
                    .iter()
                    .any(|i| i.software == selection.software)
            {
                return Err(invalid_data("export selection is incomplete"));
            }
        }
        for item in publication.items() {
            let selection = current
                .definition
                .data()
                .selections
                .iter()
                .find(|s| s.software == item.software)
                .ok_or_else(|| invalid_data("unselected export item"))?;
            if selection.settings.as_ref() != Some(&item.settings) {
                return Err(invalid_data(
                    "export settings differ from the saved definition",
                ));
            }
            let hardware = match item.architecture {
                stabbur_domain::Architecture::Universal => vec![
                    stabbur_domain::Architecture::Aarch64,
                    stabbur_domain::Architecture::X86_64,
                ],
                arch => vec![arch],
            };
            let expected: Vec<_> = hardware
                .into_iter()
                .filter(|a| {
                    selection.architectures.is_empty() || selection.architectures.contains(a)
                })
                .collect();
            if expected != item.architectures {
                return Err(invalid_data(
                    "export architecture filter differs from the saved selection",
                ));
            }
            let count: i64 = query_scalar("SELECT COUNT(*) FROM variants v JOIN variant_artifacts va ON va.variant_id = v.id JOIN artifacts a ON a.digest = va.digest JOIN releases r ON r.id = v.release_id JOIN software s ON s.id = r.software_id WHERE v.id = ? AND r.id = ? AND s.id = ? AND a.digest = ? AND a.size = ? AND va.role = 'primary_installer' AND v.platform = 'mac_os' AND s.slug = ? AND s.name = ? AND r.version = ? AND v.architecture = ? AND v.minimum_macos IS ? AND v.maximum_macos IS ?")
                .bind(item.variant.to_string()).bind(item.release.to_string()).bind(item.software.to_string()).bind(item.digest.as_str()).bind(number(item.size)?).bind(item.slug.as_str()).bind(&item.name).bind(item.version.as_str()).bind(super::architecture_name(item.architecture)).bind(item.minimum_macos.as_ref().map(ToString::to_string)).bind(item.maximum_macos.as_ref().map(ToString::to_string)).fetch_one(&mut *connection).await.map_err(map_write)?;
            if let stabbur_domain::exports::ExportSource::Channel { channel } = &selection.source {
                let valid: i64 = query_scalar("SELECT COUNT(*) FROM channels WHERE software_id = ? AND name = ? AND (pinned_variant_id IS NULL OR pinned_variant_id = ?)")
                    .bind(item.software.to_string()).bind(channel.as_str()).bind(item.variant.to_string()).fetch_one(&mut *connection).await.map_err(map_write)?;
                if valid != 1 {
                    return Err(StorageError::StaleRevision);
                }
            }
            if count != 1 {
                return Err(invalid_data(
                    "export item differs from immutable library facts",
                ));
            }
        }
        for binding in publication.bindings() {
            let valid: i64 = query_scalar("SELECT COUNT(*) FROM releases r JOIN software s ON r.software_id = s.id WHERE r.id = ? AND s.id = ? AND r.revision = ? AND s.revision = ? AND r.state IN ('testing', 'stable') AND json_extract(r.availability_json, '$.kind') = 'available'")
                .bind(binding.release.to_string()).bind(binding.software.to_string()).bind(number(binding.release_revision)?).bind(number(binding.software_revision)?).fetch_one(&mut *connection).await.map_err(map_write)?;
            if valid != 1 {
                return Err(StorageError::StaleRevision);
            }
            if let Some((channel, revision)) = &binding.channel {
                let valid: i64 = query_scalar("SELECT COUNT(*) FROM channels WHERE software_id = ? AND name = ? AND release_id = ? AND revision = ?")
                    .bind(binding.software.to_string()).bind(channel.as_str()).bind(binding.release.to_string()).bind(number(*revision)?).fetch_one(&mut *connection).await.map_err(map_write)?;
                if valid != 1 {
                    return Err(StorageError::StaleRevision);
                }
            }
        }
        let generation = current
            .generation
            .checked_add(1)
            .ok_or(StorageError::Conflict)?;
        let snapshot = ExportSnapshot {
            export: current.id,
            generation,
            definition_revision: current.revision,
            definition: current.definition,
            items: publication.items().to_vec(),
            created_at: audit.occurred_at,
        };
        query("INSERT INTO export_snapshots VALUES (?, ?, ?)")
            .bind(current.id.to_string())
            .bind(number(generation)?)
            .bind(encode(&snapshot)?)
            .execute(&mut *connection)
            .await
            .map_err(map_write)?;
        query("UPDATE exports SET generation = ? WHERE id = ?")
            .bind(number(generation)?)
            .bind(current.id.to_string())
            .execute(&mut *connection)
            .await
            .map_err(map_write)?;
        insert_audit(&mut connection, audit).await?;
        connection
            .commit("publishing complete export snapshot")
            .await?;
        Ok(snapshot)
    }
    async fn export_snapshot(
        &self,
        id: ExportId,
        generation: u64,
    ) -> Result<Option<ExportSnapshot>, StorageError> {
        let json: Option<String> = query_scalar(
            "SELECT snapshot_json FROM export_snapshots WHERE export_id = ? AND generation = ?",
        )
        .bind(id.to_string())
        .bind(number(generation)?)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_write)?;
        json.as_deref().map(decode).transpose()
    }
    async fn export_history(
        &self,
        id: ExportId,
        after: u64,
        limit: u32,
    ) -> Result<Vec<ExportSnapshot>, StorageError> {
        let rows: Vec<String> = query_scalar("SELECT snapshot_json FROM export_snapshots WHERE export_id = ? AND generation > ? ORDER BY generation LIMIT ?").bind(id.to_string()).bind(number(after)?).bind(page(limit)?).fetch_all(&self.pool).await.map_err(map_write)?;
        rows.iter().map(|s| decode(s)).collect()
    }
    async fn create_export_reader(
        &self,
        id: ExportId,
        hash: &TokenHash,
        audit: &AuditEvent,
    ) -> Result<(), StorageError> {
        let mut connection = self.acquire_write().await?;
        let epoch: Option<i64> = query_scalar("SELECT reader_epoch FROM exports WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&mut *connection)
            .await
            .map_err(map_write)?;
        let epoch = epoch.ok_or(StorageError::NotFound)?;
        let count: i64 =
            query_scalar("SELECT COUNT(*) FROM export_readers WHERE export_id = ? AND epoch = ?")
                .bind(id.to_string())
                .bind(epoch)
                .fetch_one(&mut *connection)
                .await
                .map_err(map_write)?;
        if count >= 1000 {
            return Err(StorageError::Conflict);
        }
        query("INSERT INTO export_readers VALUES (?, ?, ?, ?)")
            .bind(hash.expose_for_persistence())
            .bind(id.to_string())
            .bind(epoch)
            .bind(audit.occurred_at)
            .execute(&mut *connection)
            .await
            .map_err(map_write)?;
        insert_audit(&mut connection, audit).await?;
        connection.commit("issuing export reader").await
    }
    async fn export_reader_valid(
        &self,
        id: ExportId,
        hash: &TokenHash,
    ) -> Result<bool, StorageError> {
        let count: i64 = query_scalar("SELECT COUNT(*) FROM export_readers r JOIN exports e ON e.id = r.export_id AND e.reader_epoch = r.epoch WHERE r.export_id = ? AND r.token_hash = ?")
            .bind(id.to_string()).bind(hash.expose_for_persistence()).fetch_one(&self.pool).await.map_err(map_write)?;
        Ok(count == 1)
    }
    async fn revoke_export_readers(
        &self,
        id: ExportId,
        audit: &AuditEvent,
    ) -> Result<(), StorageError> {
        let mut connection = self.acquire_write().await?;
        let affected = query("UPDATE exports SET reader_epoch = reader_epoch + 1 WHERE id = ?")
            .bind(id.to_string())
            .execute(&mut *connection)
            .await
            .map_err(map_write)?
            .rows_affected();
        if affected != 1 {
            return Err(StorageError::NotFound);
        }
        insert_audit(&mut connection, audit).await?;
        connection.commit("revoking export readers").await
    }
}
