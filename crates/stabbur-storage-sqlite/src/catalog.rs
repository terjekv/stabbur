//! SQLite implementation of the catalog ports.
use super::{
    AuditEvent, CatalogStorage, ChannelRecord, DateTime, LifecycleEventId, PromotionEventId,
    Release, ReleaseId, ReleaseState, Row, SqliteStorage, StorageError, Utc, Variant,
    VariantArtifactRecord, VariantId, artifact_from_row, artifact_role, async_trait, backend,
    channel_from_row, insert_audit, invalid_data, map_write, query, query_scalar, release_from_row,
    release_state_name, variant_from_row,
};

#[async_trait]
#[allow(clippy::too_many_arguments)] // The port keeps concurrency, audit, and mutation inputs explicit.
impl CatalogStorage for SqliteStorage {
    async fn withdraw_release(
        &self,
        release_id: ReleaseId,
        reason: &stabbur_domain::WithdrawalReason,
        expected_revision: u64,
        audit: &AuditEvent,
        now: DateTime<Utc>,
    ) -> Result<Release, StorageError> {
        let mut connection = self.acquire_write().await?;
        let row = query("SELECT id, software_id, version, state, created_at, revision, availability_json FROM releases WHERE id = ?")
            .bind(release_id.to_string()).fetch_optional(&mut *connection).await
            .map_err(|error| backend("loading withdrawn release", &error))?.ok_or(StorageError::NotFound)?;
        let mut release = release_from_row(&row)?;
        if release.revision != expected_revision {
            return Err(StorageError::StaleRevision);
        }
        if !release.availability.is_available() {
            return Err(StorageError::Conflict);
        }
        release.availability = stabbur_domain::ReleaseAvailability::Withdrawn {
            reason: reason.clone(),
            at: now,
        };
        release.revision = release
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid_data("release revision overflow"))?;
        let availability = serde_json::to_string(&release.availability)
            .map_err(|_| invalid_data("invalid availability"))?;
        query("UPDATE releases SET availability_json = ?, revision = revision + 1 WHERE id = ?")
            .bind(&availability)
            .bind(release_id.to_string())
            .execute(&mut *connection)
            .await
            .map_err(map_write)?;
        query("INSERT INTO release_availability_events (id, release_id, availability_json, actor_json, occurred_at) VALUES (?, ?, ?, ?, ?)")
            .bind(uuid::Uuid::now_v7().to_string()).bind(release_id.to_string()).bind(availability)
            .bind(serde_json::to_string(&audit.actor).map_err(|_| invalid_data("invalid audit actor"))?).bind(now)
            .execute(&mut *connection).await.map_err(map_write)?;
        query("DELETE FROM channels WHERE release_id = ?")
            .bind(release_id.to_string())
            .execute(&mut *connection)
            .await
            .map_err(map_write)?;
        insert_audit(&mut connection, audit).await?;
        connection.commit("committing release withdrawal").await?;
        Ok(release)
    }

    async fn release(&self, release_id: ReleaseId) -> Result<Option<Release>, StorageError> {
        let row = query(
            "SELECT id, software_id, version, state, created_at, revision, availability_json
             FROM releases WHERE id = ?",
        )
        .bind(release_id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| backend("loading release", &error))?;
        row.as_ref().map(release_from_row).transpose()
    }

    async fn list_releases(
        &self,
        software_id: stabbur_domain::SoftwareId,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<Release>, StorageError> {
        if !(1..=200).contains(&limit) {
            return Err(invalid_data("release page limit must be between 1 and 200"));
        }
        let rows = query(
            "SELECT id, software_id, version, state, created_at, revision, availability_json FROM releases
             WHERE software_id = ? AND id > ? ORDER BY id LIMIT ?",
        )
        .bind(software_id.to_string())
        .bind(after.unwrap_or(""))
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(|error| backend("listing releases", &error))?;
        rows.iter().map(release_from_row).collect()
    }

    async fn release_variants(&self, release_id: ReleaseId) -> Result<Vec<Variant>, StorageError> {
        let rows = query(
            "SELECT id, release_id, platform, architecture, minimum_macos, maximum_macos,
                    resolution_priority FROM variants WHERE release_id = ? ORDER BY id",
        )
        .bind(release_id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(|error| backend("listing release variants", &error))?;
        rows.iter().map(variant_from_row).collect()
    }

    async fn variant_artifacts(
        &self,
        variant_id: VariantId,
    ) -> Result<Vec<VariantArtifactRecord>, StorageError> {
        let rows = query(
            "SELECT a.digest, a.size, a.media_type, a.created_at, va.role,
                    EXISTS(SELECT 1 FROM artifact_locations l
                           JOIN stores s ON s.id = l.store_id
                           WHERE l.digest = a.digest AND l.state = 'present' AND s.enabled = 1)
                      AS readable
             FROM variant_artifacts va JOIN artifacts a ON a.digest = va.digest
             WHERE va.variant_id = ? ORDER BY va.role, va.digest",
        )
        .bind(variant_id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(|error| backend("listing variant artifacts", &error))?;
        rows.iter()
            .map(|row| {
                let role: String = row
                    .try_get("role")
                    .map_err(|error| backend("decoding variant artifact role", &error))?;
                let readable: i64 = row
                    .try_get("readable")
                    .map_err(|error| backend("decoding variant artifact readability", &error))?;
                Ok(VariantArtifactRecord {
                    artifact: artifact_from_row(row)?,
                    role: artifact_role(&role)?,
                    readable: readable == 1,
                })
            })
            .collect()
    }

    async fn channels(
        &self,
        software_id: stabbur_domain::SoftwareId,
    ) -> Result<Vec<ChannelRecord>, StorageError> {
        let rows = query(
            "SELECT software_id, name, release_id, pinned_variant_id, revision
             FROM channels WHERE software_id = ? ORDER BY name",
        )
        .bind(software_id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(|error| backend("listing channels", &error))?;
        rows.iter().map(channel_from_row).collect()
    }

    async fn channel(
        &self,
        software_id: stabbur_domain::SoftwareId,
        name: &str,
    ) -> Result<Option<ChannelRecord>, StorageError> {
        let row = query(
            "SELECT software_id, name, release_id, pinned_variant_id, revision
             FROM channels WHERE software_id = ? AND name = ?",
        )
        .bind(software_id.to_string())
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| backend("loading channel", &error))?;
        row.as_ref().map(channel_from_row).transpose()
    }

    #[allow(clippy::too_many_lines)] // Channel, lifecycle, history, and audit writes must stay atomic.
    async fn promote_channel(
        &self,
        software_id: stabbur_domain::SoftwareId,
        name: &str,
        release_id: ReleaseId,
        pinned_variant_id: Option<VariantId>,
        expected_revision: u64,
        reason: Option<&str>,
        audit: &AuditEvent,
        now: DateTime<Utc>,
    ) -> Result<ChannelRecord, StorageError> {
        if !matches!(name, "testing" | "stable")
            || reason.is_some_and(|reason| reason.trim() != reason || reason.len() > 1024)
        {
            return Err(invalid_data("channel promotion input is invalid"));
        }
        let mut connection = self.acquire_write().await?;
        let release_row = query(
            "SELECT id, software_id, version, state, created_at, revision, availability_json
             FROM releases WHERE id = ? AND software_id = ?",
        )
        .bind(release_id.to_string())
        .bind(software_id.to_string())
        .fetch_optional(&mut *connection)
        .await
        .map_err(|error| backend("loading promotion release", &error))?
        .ok_or(StorageError::NotFound)?;
        let release = release_from_row(&release_row)?;
        if !release.availability.is_available() {
            return Err(StorageError::Conflict);
        }
        if let Some(variant_id) = pinned_variant_id {
            let exists: Option<i64> =
                query_scalar("SELECT 1 FROM variants WHERE id = ? AND release_id = ?")
                    .bind(variant_id.to_string())
                    .bind(release_id.to_string())
                    .fetch_optional(&mut *connection)
                    .await
                    .map_err(|error| backend("validating channel variant pin", &error))?;
            if exists.is_none() {
                return Err(invalid_data(
                    "pinned variant does not belong to channel release",
                ));
            }
        }
        let existing_row = query(
            "SELECT software_id, name, release_id, pinned_variant_id, revision
             FROM channels WHERE software_id = ? AND name = ?",
        )
        .bind(software_id.to_string())
        .bind(name)
        .fetch_optional(&mut *connection)
        .await
        .map_err(|error| backend("loading current channel", &error))?;
        let existing = existing_row.as_ref().map(channel_from_row).transpose()?;
        if existing.as_ref().map_or(expected_revision != 0, |channel| {
            channel.revision != expected_revision
        }) {
            return Err(StorageError::StaleRevision);
        }
        let target_state = match (name, release.state) {
            ("testing", ReleaseState::Candidate) => Some(ReleaseState::Testing),
            ("stable", ReleaseState::Candidate | ReleaseState::Testing) => {
                Some(ReleaseState::Stable)
            }
            ("testing" | "stable", ReleaseState::Stable) | ("testing", ReleaseState::Testing) => {
                None
            }
            _ => return Err(StorageError::Conflict),
        };
        let actor = serde_json::to_string(&audit.actor)
            .map_err(|error| invalid_data(format!("serializing promotion actor: {error}")))?;
        if let Some(target_state) = target_state {
            release
                .state
                .transition(target_state)
                .map_err(|_| StorageError::Conflict)?;
            query("UPDATE releases SET state = ?, revision = revision + 1 WHERE id = ?")
                .bind(release_state_name(target_state))
                .bind(release_id.to_string())
                .execute(&mut *connection)
                .await
                .map_err(map_write)?;
            query(
                "INSERT INTO release_lifecycle_events
                 (id, release_id, from_state, to_state, actor_json, reason, occurred_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(LifecycleEventId::new().to_string())
            .bind(release_id.to_string())
            .bind(release_state_name(release.state))
            .bind(release_state_name(target_state))
            .bind(&actor)
            .bind(reason)
            .bind(now)
            .execute(&mut *connection)
            .await
            .map_err(map_write)?;
        }
        let revision = expected_revision
            .checked_add(1)
            .ok_or_else(|| invalid_data("channel revision overflow"))?;
        query(
            "INSERT INTO channels
             (software_id, name, release_id, pinned_variant_id, revision)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(software_id, name) DO UPDATE SET
               release_id = excluded.release_id,
               pinned_variant_id = excluded.pinned_variant_id,
               revision = excluded.revision",
        )
        .bind(software_id.to_string())
        .bind(name)
        .bind(release_id.to_string())
        .bind(pinned_variant_id.map(|value| value.to_string()))
        .bind(i64::try_from(revision).map_err(|_| invalid_data("channel revision overflow"))?)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        query(
            "INSERT INTO promotion_events
             (id, software_id, channel_name, previous_release_id, release_id,
              pinned_variant_id, actor_json, reason, occurred_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(PromotionEventId::new().to_string())
        .bind(software_id.to_string())
        .bind(name)
        .bind(
            existing
                .as_ref()
                .map(|channel| channel.release_id.to_string()),
        )
        .bind(release_id.to_string())
        .bind(pinned_variant_id.map(|value| value.to_string()))
        .bind(actor)
        .bind(reason)
        .bind(now)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        insert_audit(&mut connection, audit).await?;
        connection.commit("committing channel promotion").await?;
        Ok(ChannelRecord {
            software_id,
            name: name.to_owned(),
            release_id,
            pinned_variant_id,
            revision,
        })
    }

    async fn reject_release(
        &self,
        release_id: ReleaseId,
        expected_revision: u64,
        reason: &str,
        audit: &AuditEvent,
        now: DateTime<Utc>,
    ) -> Result<Release, StorageError> {
        if reason.trim() != reason || reason.is_empty() || reason.len() > 1024 {
            return Err(invalid_data("rejection reason must contain 1-1024 bytes"));
        }
        let mut connection = self.acquire_write().await?;
        let row = query(
            "SELECT id, software_id, version, state, created_at, revision, availability_json
             FROM releases WHERE id = ?",
        )
        .bind(release_id.to_string())
        .fetch_optional(&mut *connection)
        .await
        .map_err(|error| backend("loading rejected release", &error))?
        .ok_or(StorageError::NotFound)?;
        let mut release = release_from_row(&row)?;
        if release.revision != expected_revision {
            return Err(StorageError::StaleRevision);
        }
        release
            .state
            .transition(ReleaseState::Rejected)
            .map_err(|_| StorageError::Conflict)?;
        let changed = query(
            "UPDATE releases SET state = 'rejected', revision = revision + 1
             WHERE id = ? AND revision = ?",
        )
        .bind(release_id.to_string())
        .bind(
            i64::try_from(expected_revision)
                .map_err(|_| invalid_data("release revision exceeds SQLite range"))?,
        )
        .execute(&mut *connection)
        .await
        .map_err(map_write)?
        .rows_affected();
        if changed != 1 {
            return Err(StorageError::StaleRevision);
        }
        query("DELETE FROM channels WHERE release_id = ?")
            .bind(release_id.to_string())
            .execute(&mut *connection)
            .await
            .map_err(map_write)?;
        let actor = serde_json::to_string(&audit.actor)
            .map_err(|error| invalid_data(format!("serializing rejection actor: {error}")))?;
        query(
            "INSERT INTO release_lifecycle_events
             (id, release_id, from_state, to_state, actor_json, reason, occurred_at)
             VALUES (?, ?, ?, 'rejected', ?, ?, ?)",
        )
        .bind(LifecycleEventId::new().to_string())
        .bind(release_id.to_string())
        .bind(release_state_name(release.state))
        .bind(actor)
        .bind(reason)
        .bind(now)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        insert_audit(&mut connection, audit).await?;
        connection.commit("committing release rejection").await?;
        release.state = ReleaseState::Rejected;
        release.revision = release
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid_data("release revision overflow"))?;
        Ok(release)
    }
}
