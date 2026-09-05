//! SQLite implementation of the artifacts ports.
use super::{
    Artifact, ArtifactLocation, ArtifactStorage, Row, Sha256Digest, SqliteStorage, StorageError,
    StoreId, StoreRecord, Utc, artifact_from_row, async_trait, backend, location_state, map_write,
    parse_value, query, store_from_row,
};

#[async_trait]
impl ArtifactStorage for SqliteStorage {
    async fn artifact(&self, digest: &Sha256Digest) -> Result<Option<Artifact>, StorageError> {
        let row =
            query("SELECT digest, size, media_type, created_at FROM artifacts WHERE digest = ?")
                .bind(digest.as_str())
                .fetch_optional(&self.pool)
                .await
                .map_err(|error| backend("loading artifact", &error))?;
        row.as_ref().map(artifact_from_row).transpose()
    }

    async fn artifact_locations(
        &self,
        digest: &Sha256Digest,
    ) -> Result<Vec<ArtifactLocation>, StorageError> {
        let rows = query(
            "SELECT id, digest, store_id, state, verified_at, last_error
             FROM artifact_locations WHERE digest = ? ORDER BY id",
        )
        .bind(digest.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(|error| backend("listing artifact locations", &error))?;
        rows.iter()
            .map(|row| {
                let state: String = row
                    .try_get("state")
                    .map_err(|error| backend("decoding location state", &error))?;
                Ok(ArtifactLocation {
                    id: parse_value(
                        row.try_get("id")
                            .map_err(|error| backend("decoding location ID", &error))?,
                        "location ID",
                    )?,
                    digest: parse_value(
                        row.try_get("digest")
                            .map_err(|error| backend("decoding location digest", &error))?,
                        "artifact digest",
                    )?,
                    store_id: parse_value(
                        row.try_get("store_id")
                            .map_err(|error| backend("decoding store ID", &error))?,
                        "store ID",
                    )?,
                    state: location_state(&state)?,
                    verified_at: row
                        .try_get("verified_at")
                        .map_err(|error| backend("decoding location verification time", &error))?,
                    last_error: row
                        .try_get("last_error")
                        .map_err(|error| backend("decoding location error", &error))?,
                })
            })
            .collect()
    }

    async fn ensure_local_primary_store(
        &self,
        id: StoreId,
        name: &str,
    ) -> Result<StoreRecord, StorageError> {
        query(
            "INSERT INTO stores (id, name, role, kind, enabled, created_at)
             VALUES (?, ?, 'primary', 'fs', 1, ?)
             ON CONFLICT(name) DO NOTHING",
        )
        .bind(id.to_string())
        .bind(name)
        .bind(Utc::now())
        .execute(&self.pool)
        .await
        .map_err(map_write)?;
        let row =
            query("SELECT id, name, role, kind, enabled, revision FROM stores WHERE name = ?")
                .bind(name)
                .fetch_one(&self.pool)
                .await
                .map_err(|error| backend("loading local primary store", &error))?;
        store_from_row(&row)
    }

    async fn stores(&self) -> Result<Vec<StoreRecord>, StorageError> {
        let rows = query("SELECT id, name, role, kind, enabled, revision FROM stores ORDER BY id")
            .fetch_all(&self.pool)
            .await
            .map_err(|error| backend("listing artifact stores", &error))?;
        rows.iter().map(store_from_row).collect()
    }

    async fn store(&self, store_id: StoreId) -> Result<Option<StoreRecord>, StorageError> {
        let row = query("SELECT id, name, role, kind, enabled, revision FROM stores WHERE id = ?")
            .bind(store_id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| backend("loading artifact store", &error))?;
        row.as_ref().map(store_from_row).transpose()
    }
}
