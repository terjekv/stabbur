//! Durable, private local filesystem implementation of the artifact-store port.

mod maintenance;
pub use maintenance::{Inspection, InspectionBudget, ObjectInspection};

use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use futures_util::{StreamExt, TryStreamExt};
use sha2::{Digest, Sha256};
use stabbur_domain::Sha256Digest;
use stabbur_store_core::{
    ArtifactStore, ByteRange, ByteStream, ObjectMetadata, StoreCapabilities, StoreError, StoreRead,
    StoreRole, WriteOutcome,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

/// A local private content-addressed store.
#[derive(Debug, Clone)]
pub struct FsArtifactStore {
    root: PathBuf,
    role: StoreRole,
}

impl FsArtifactStore {
    /// Opens or creates the store below a private data directory.
    pub async fn open(root: impl Into<PathBuf>, role: StoreRole) -> Result<Self, StoreError> {
        let store = Self {
            root: root.into(),
            role,
        };
        tokio::fs::create_dir_all(store.root.join("objects/sha256"))
            .await
            .map_err(|error| backend("creating object directory", &error))?;
        tokio::fs::create_dir_all(store.root.join("uploads"))
            .await
            .map_err(|error| backend("creating upload directory", &error))?;
        Ok(store)
    }

    fn object_path(&self, digest: &Sha256Digest) -> PathBuf {
        let value = digest.as_str();
        self.root
            .join("objects/sha256")
            .join(&value[0..2])
            .join(&value[2..4])
            .join(value)
    }

    async fn verify_existing(&self, digest: &Sha256Digest, size: u64) -> Result<(), StoreError> {
        let path = self.object_path(digest);
        let mut file = tokio::fs::File::open(path)
            .await
            .map_err(|error| map_not_found("opening existing object", &error))?;
        let metadata = file
            .metadata()
            .await
            .map_err(|error| backend("reading existing object metadata", &error))?;
        if metadata.len() != size {
            return Err(StoreError::SizeMismatch {
                expected: size,
                actual: metadata.len(),
            });
        }
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .await
                .map_err(|error| backend("verifying existing object", &error))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        let actual = Sha256Digest::new(hex::encode(hasher.finalize()))
            .expect("SHA-256 always produces a valid digest");
        if &actual == digest {
            Ok(())
        } else {
            Err(StoreError::DigestMismatch {
                expected: digest.clone(),
                actual,
            })
        }
    }

    async fn discard_upload(path: &Path) {
        if let Err(error) = tokio::fs::remove_file(path).await
            && error.kind() != ErrorKind::NotFound
        {
            // The caller already has a more useful verification/backend error. A private stale
            // upload is safe to collect during a later maintenance pass.
        }
    }

    async fn sync_directory(path: &Path) -> Result<(), StoreError> {
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || {
            let mut options = std::fs::OpenOptions::new();
            options.read(true);
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;

                // Directory handles require backup semantics. FlushFileBuffers,
                // which implements sync_all on Windows, also requires write access.
                const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
                options.write(true).custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
            }
            options.open(path)?.sync_all()
        })
        .await
        .map_err(|_| StoreError::Backend {
            message: "directory sync task failed".into(),
        })?
        .map_err(|error| backend("syncing object directory", &error))
    }
}

fn backend(operation: &str, error: &std::io::Error) -> StoreError {
    StoreError::Backend {
        message: format!("{operation}: {}", error.kind()),
    }
}

fn map_not_found(operation: &str, error: &std::io::Error) -> StoreError {
    if error.kind() == ErrorKind::NotFound {
        StoreError::NotFound
    } else {
        backend(operation, error)
    }
}

#[async_trait]
impl ArtifactStore for FsArtifactStore {
    fn role(&self) -> StoreRole {
        self.role
    }

    fn capabilities(&self) -> StoreCapabilities {
        StoreCapabilities {
            read: true,
            write: true,
            delete: true,
            range: true,
            multipart: false,
            redirect: false,
        }
    }

    async fn check_readiness(&self) -> Result<(), StoreError> {
        for directory in [self.root.join("objects/sha256"), self.root.join("uploads")] {
            let _entries = tokio::fs::read_dir(directory)
                .await
                .map_err(|error| backend("opening required store directory", &error))?;
        }
        Ok(())
    }

    async fn head(&self, digest: &Sha256Digest) -> Result<ObjectMetadata, StoreError> {
        let metadata = tokio::fs::metadata(self.object_path(digest))
            .await
            .map_err(|error| map_not_found("reading object metadata", &error))?;
        Ok(ObjectMetadata {
            digest: digest.clone(),
            size: metadata.len(),
        })
    }

    async fn read(
        &self,
        digest: &Sha256Digest,
        range: Option<ByteRange>,
    ) -> Result<StoreRead, StoreError> {
        let object = self.head(digest).await?;
        let normalized = range
            .map(|range| range.normalize(object.size))
            .transpose()?;
        let mut file = tokio::fs::File::open(self.object_path(digest))
            .await
            .map_err(|error| map_not_found("opening object", &error))?;
        let (start, length) =
            normalized.map_or((0, object.size), |range| (range.start(), range.length()));
        file.seek(std::io::SeekFrom::Start(start))
            .await
            .map_err(|error| backend("seeking object", &error))?;
        let stream = ReaderStream::new(file.take(length))
            .map_err(|error| backend("streaming object", &error));
        Ok(StoreRead {
            object,
            range: normalized,
            stream: Box::pin(stream),
        })
    }

    async fn write(
        &self,
        digest: &Sha256Digest,
        size: u64,
        mut source: ByteStream,
    ) -> Result<WriteOutcome, StoreError> {
        let upload = self.root.join("uploads").join(Uuid::now_v7().to_string());
        let mut file = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&upload)
            .await
            .map_err(|error| backend("creating private upload", &error))?;
        let mut hasher = Sha256::new();
        let mut actual_size = 0_u64;
        while let Some(chunk) = source.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    Self::discard_upload(&upload).await;
                    return Err(error);
                }
            };
            actual_size =
                actual_size
                    .checked_add(chunk.len() as u64)
                    .ok_or_else(|| StoreError::Backend {
                        message: "artifact size overflow".into(),
                    })?;
            if let Err(error) = file.write_all(&chunk).await {
                Self::discard_upload(&upload).await;
                return Err(backend("writing private upload", &error));
            }
            hasher.update(&chunk);
        }
        if actual_size != size {
            Self::discard_upload(&upload).await;
            return Err(StoreError::SizeMismatch {
                expected: size,
                actual: actual_size,
            });
        }
        let actual_digest = Sha256Digest::new(hex::encode(hasher.finalize()))
            .expect("SHA-256 always produces a valid digest");
        if &actual_digest != digest {
            Self::discard_upload(&upload).await;
            return Err(StoreError::DigestMismatch {
                expected: digest.clone(),
                actual: actual_digest,
            });
        }
        file.sync_all()
            .await
            .map_err(|error| backend("syncing private upload", &error))?;
        drop(file);

        let destination = self.object_path(digest);
        let parent = destination
            .parent()
            .expect("an object path always has a parent");
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| backend("creating digest directory", &error))?;
        match tokio::fs::hard_link(&upload, &destination).await {
            Ok(()) => {
                Self::discard_upload(&upload).await;
                Self::sync_directory(parent).await?;
                Ok(WriteOutcome::Created)
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                Self::discard_upload(&upload).await;
                self.verify_existing(digest, size).await?;
                Ok(WriteOutcome::Reused)
            }
            Err(error) => {
                Self::discard_upload(&upload).await;
                Err(backend("publishing object", &error))
            }
        }
    }

    async fn delete(&self, digest: &Sha256Digest) -> Result<(), StoreError> {
        tokio::fs::remove_file(self.object_path(digest))
            .await
            .map_err(|error| map_not_found("deleting object", &error))
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use futures_util::{TryStreamExt, stream};

    use super::*;

    fn content() -> (&'static [u8], Sha256Digest) {
        let bytes = b"immutable stabbur artifact";
        let digest = Sha256Digest::new(hex::encode(Sha256::digest(bytes))).unwrap();
        (bytes, digest)
    }

    fn source(bytes: &'static [u8]) -> ByteStream {
        Box::pin(stream::iter([Ok(Bytes::copy_from_slice(bytes))]))
    }

    #[tokio::test]
    async fn writes_reuses_and_reads_verified_content() {
        let temp = tempfile::tempdir().unwrap();
        let store = FsArtifactStore::open(temp.path(), StoreRole::Primary)
            .await
            .unwrap();
        let (bytes, digest) = content();
        assert_eq!(
            store
                .write(&digest, bytes.len() as u64, source(bytes))
                .await
                .unwrap(),
            WriteOutcome::Created
        );
        assert_eq!(
            store
                .write(&digest, bytes.len() as u64, source(bytes))
                .await
                .unwrap(),
            WriteOutcome::Reused
        );
        let read = store.read(&digest, None).await.unwrap();
        let result = read.stream.try_collect::<Vec<_>>().await.unwrap().concat();
        assert_eq!(result, bytes);
    }

    #[tokio::test]
    async fn readiness_requires_accessible_store_directories() {
        let temp = tempfile::tempdir().unwrap();
        let store = FsArtifactStore::open(temp.path(), StoreRole::Primary)
            .await
            .unwrap();
        store.check_readiness().await.unwrap();

        tokio::fs::remove_dir(temp.path().join("uploads"))
            .await
            .unwrap();
        assert!(store.check_readiness().await.is_err());
    }

    #[tokio::test]
    async fn reads_bounded_ranges() {
        let temp = tempfile::tempdir().unwrap();
        let store = FsArtifactStore::open(temp.path(), StoreRole::Primary)
            .await
            .unwrap();
        let (bytes, digest) = content();
        store
            .write(&digest, bytes.len() as u64, source(bytes))
            .await
            .unwrap();
        let read = store
            .read(&digest, Some(ByteRange::Inclusive { start: 10, end: 16 }))
            .await
            .unwrap();
        assert_eq!(read.range.unwrap().length(), 7);
        let result = read.stream.try_collect::<Vec<_>>().await.unwrap().concat();
        assert_eq!(result, b"stabbur");
    }

    #[tokio::test]
    async fn rejects_digest_and_size_mismatches_without_publishing() {
        let temp = tempfile::tempdir().unwrap();
        let store = FsArtifactStore::open(temp.path(), StoreRole::Primary)
            .await
            .unwrap();
        let (bytes, digest) = content();
        assert!(matches!(
            store.write(&digest, 1, source(bytes)).await,
            Err(StoreError::SizeMismatch { .. })
        ));
        assert_eq!(store.head(&digest).await, Err(StoreError::NotFound));

        let wrong = Sha256Digest::new("0".repeat(64)).unwrap();
        assert!(matches!(
            store.write(&wrong, bytes.len() as u64, source(bytes)).await,
            Err(StoreError::DigestMismatch { .. })
        ));
        assert_eq!(store.head(&wrong).await, Err(StoreError::NotFound));
    }
}
