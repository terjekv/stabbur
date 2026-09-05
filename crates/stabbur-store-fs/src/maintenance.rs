//! Bounded offline inspection. Callers hold exclusive control-plane database access.
use super::{FsArtifactStore, backend};
use serde::Serialize;
use stabbur_domain::Sha256Digest;
use stabbur_store_core::{ArtifactStore, StoreError};
use std::time::{Duration, SystemTime};

/// Local maintenance measurements; backend paths are deliberately absent.
#[derive(Debug, Serialize)]
pub struct Inspection {
    /// Available filesystem capacity at inspection time.
    pub available_bytes: u64,
    /// Digest verification results in request order.
    pub objects: Vec<ObjectInspection>,
    /// Bytes actually scheduled for hash verification.
    pub checked_bytes: u64,
}
/// One requested immutable object's observed integrity.
#[derive(Debug, Serialize)]
pub struct ObjectInspection {
    /// Requested content identity.
    pub digest: Sha256Digest,
    /// Present and fully verified, absent, corrupt, or outside this pass's byte budget.
    pub outcome: &'static str,
}
/// Bound on an offline inspection pass, validated before filesystem work.
pub struct InspectionBudget {
    objects: usize,
    bytes: u64,
}
impl InspectionBudget {
    /// Bounds object count to 1..=1000 and total hashing to 1 byte..=1 TiB.
    pub fn new(objects: usize, bytes: u64) -> Result<Self, StoreError> {
        if !(1..=1000).contains(&objects) || !(1..=1_099_511_627_776).contains(&bytes) {
            return Err(StoreError::Backend {
                message: "inspection budget is outside supported bounds".into(),
            });
        }
        Ok(Self { objects, bytes })
    }
}
impl FsArtifactStore {
    /// Hashes explicitly selected digests under a total byte budget, without changing stored bytes.
    pub async fn inspect(
        &self,
        digests: &[Sha256Digest],
        budget: InspectionBudget,
    ) -> Result<Inspection, StoreError> {
        if digests.len() > budget.objects {
            return Err(StoreError::Backend {
                message: "too many requested digests".into(),
            });
        }
        let available_bytes = fs2::available_space(&self.root)
            .map_err(|error| backend("measuring available storage", &error))?;
        let mut report = Inspection {
            available_bytes,
            objects: Vec::new(),
            checked_bytes: 0,
        };
        for digest in digests {
            let outcome = match self.head(digest).await {
                Err(StoreError::NotFound) => "missing",
                Err(error) => return Err(error),
                Ok(metadata) if metadata.size > budget.bytes - report.checked_bytes => {
                    "budget_exceeded"
                }
                Ok(metadata) => {
                    report.checked_bytes += metadata.size;
                    match self.verify_existing(digest, metadata.size).await {
                        Ok(()) => "verified",
                        Err(
                            StoreError::DigestMismatch { .. } | StoreError::SizeMismatch { .. },
                        ) => "corrupt",
                        Err(error) => return Err(error),
                    }
                }
            };
            report.objects.push(ObjectInspection {
                digest: digest.clone(),
                outcome,
            });
        }
        Ok(report)
    }
    /// Removes only expired regular upload files. The caller must exclude live upload writers.
    ///
    /// Examines at most `limit` entries, rejects symlinks, and never removes immutable objects.
    pub async fn prune_stale_uploads(
        &self,
        age: Duration,
        limit: std::num::NonZeroU16,
    ) -> Result<u64, StoreError> {
        if age < Duration::from_secs(3600) {
            return Err(StoreError::Backend {
                message: "upload retention must be at least one hour".into(),
            });
        }
        let mut directory = tokio::fs::read_dir(self.root.join("uploads"))
            .await
            .map_err(|error| backend("reading private uploads", &error))?;
        let mut removed = 0;
        for _ in 0..limit.get() {
            let Some(entry) = directory
                .next_entry()
                .await
                .map_err(|error| backend("reading upload entry", &error))?
            else {
                break;
            };
            let metadata = tokio::fs::symlink_metadata(entry.path())
                .await
                .map_err(|error| backend("inspecting upload metadata", &error))?;
            if metadata.is_file()
                && metadata
                    .modified()
                    .ok()
                    .and_then(|time| SystemTime::now().duration_since(time).ok())
                    .is_some_and(|elapsed| elapsed >= age)
            {
                tokio::fs::remove_file(entry.path())
                    .await
                    .map_err(|error| backend("removing expired upload", &error))?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stabbur_store_core::StoreRole;
    #[tokio::test]
    async fn inspection_verifies_digest_and_enforces_total_byte_budget() {
        let directory = tempfile::tempdir().unwrap();
        let store = FsArtifactStore::open(directory.path(), StoreRole::Primary)
            .await
            .unwrap();
        let digest =
            Sha256Digest::new("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
                .unwrap();
        store
            .write(
                &digest,
                3,
                Box::pin(futures_util::stream::once(async {
                    Ok(bytes::Bytes::from_static(b"abc"))
                })),
            )
            .await
            .unwrap();
        let report = store
            .inspect(
                std::slice::from_ref(&digest),
                InspectionBudget::new(1, 2).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(report.objects[0].outcome, "budget_exceeded");
        assert_eq!(report.checked_bytes, 0);
        let report = store
            .inspect(
                std::slice::from_ref(&digest),
                InspectionBudget::new(1, 3).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(report.objects[0].outcome, "verified");
        assert_eq!(report.checked_bytes, 3);
        tokio::fs::write(store.object_path(&digest), b"bad")
            .await
            .unwrap();
        let report = store
            .inspect(&[digest], InspectionBudget::new(1, 3).unwrap())
            .await
            .unwrap();
        assert_eq!(report.objects[0].outcome, "corrupt");
    }
    #[tokio::test]
    async fn cleanup_preserves_fresh_uploads_and_rejects_short_retention() {
        let directory = tempfile::tempdir().unwrap();
        let store = FsArtifactStore::open(directory.path(), StoreRole::Primary)
            .await
            .unwrap();
        let upload = directory.path().join("uploads/fresh");
        tokio::fs::write(&upload, b"upload").await.unwrap();
        let limit = std::num::NonZeroU16::new(10).unwrap();
        assert!(
            store
                .prune_stale_uploads(Duration::from_secs(1), limit)
                .await
                .is_err()
        );
        assert_eq!(
            store
                .prune_stale_uploads(Duration::from_secs(3600), limit)
                .await
                .unwrap(),
            0
        );
        assert!(upload.exists());
    }
}
