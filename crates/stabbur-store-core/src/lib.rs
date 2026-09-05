//! A Stabbur-owned, backend-neutral streaming artifact-store contract.

use std::pin::Pin;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_core::Stream;
use serde::{Deserialize, Serialize};
use stabbur_domain::Sha256Digest;
use thiserror::Error;

/// A fallible bounded-memory byte stream.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, StoreError>> + Send + 'static>>;

/// The placement role of an artifact store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreRole {
    /// At least one verified primary is required for candidate publication.
    Primary,
    /// Durable redundant copy.
    Replica,
    /// Reconstructible performance cache.
    Cache,
    /// External source that Stabbur cannot mutate.
    ReadOnly,
}

/// Operations a store adapter supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)] // The public contract intentionally names six orthogonal capabilities.
pub struct StoreCapabilities {
    /// Metadata and content reads.
    pub read: bool,
    /// Streaming writes.
    pub write: bool,
    /// Object deletion.
    pub delete: bool,
    /// Single byte-range reads.
    pub range: bool,
    /// Multipart upload lifecycle.
    pub multipart: bool,
    /// Redirect or presigned content access.
    pub redirect: bool,
}

/// A capability-gated store operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreOperation {
    /// Metadata or content read.
    Read,
    /// Content write.
    Write,
    /// Content deletion.
    Delete,
    /// Ranged content read.
    Range,
    /// Multipart upload.
    Multipart,
    /// Redirect generation.
    Redirect,
}

/// Backend-neutral store failure.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StoreError {
    /// The digest has no object in this store.
    #[error("artifact is not present in the store")]
    NotFound,
    /// A requested range cannot be satisfied.
    #[error("requested byte range is not satisfiable for an object of {size} bytes")]
    RangeNotSatisfiable {
        /// Current object size.
        size: u64,
    },
    /// A caller used an unsupported capability.
    #[error("store does not support {operation:?}")]
    UnsupportedCapability {
        /// Unsupported operation.
        operation: StoreOperation,
    },
    /// Content has a different digest than declared.
    #[error("artifact digest verification failed")]
    DigestMismatch {
        /// Declared content identity.
        expected: Sha256Digest,
        /// Computed content identity.
        actual: Sha256Digest,
    },
    /// Content has a different length than declared.
    #[error("artifact size verification failed: expected {expected}, received {actual}")]
    SizeMismatch {
        /// Declared length.
        expected: u64,
        /// Received length.
        actual: u64,
    },
    /// A backend operation failed. The message must not contain a backend path or credential.
    #[error("artifact store operation failed: {message}")]
    Backend {
        /// Safe diagnostic text.
        message: String,
    },
}

/// Metadata for one immutable stored object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMetadata {
    /// Content digest.
    pub digest: Sha256Digest,
    /// Exact byte length.
    pub size: u64,
}

/// Caller-supplied single byte range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteRange {
    /// Inclusive first and last byte offsets.
    Inclusive {
        /// First included offset.
        start: u64,
        /// Last included offset.
        end: u64,
    },
    /// A starting offset through the end of the object.
    From {
        /// First included offset.
        start: u64,
    },
    /// The final number of bytes.
    Suffix {
        /// Requested number of final bytes.
        length: u64,
    },
}

/// A validated half-open byte interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NormalizedRange {
    /// First included offset.
    start: u64,
    /// First excluded offset.
    end_exclusive: u64,
    /// Full object size.
    object_size: u64,
}

impl NormalizedRange {
    /// First included offset, proven smaller than the object size.
    #[must_use]
    pub const fn start(self) -> u64 {
        self.start
    }
    /// Full object size used to validate this interval.
    #[must_use]
    pub const fn object_size(self) -> u64 {
        self.object_size
    }

    /// Number of bytes in the interval.
    #[must_use]
    pub const fn length(self) -> u64 {
        self.end_exclusive - self.start
    }

    /// Inclusive last byte offset for an HTTP `Content-Range` header.
    #[must_use]
    pub const fn end_inclusive(self) -> u64 {
        self.end_exclusive - 1
    }
}

impl ByteRange {
    /// Validates and bounds this range against an object size.
    pub fn normalize(self, size: u64) -> Result<NormalizedRange, StoreError> {
        let invalid = || StoreError::RangeNotSatisfiable { size };
        if size == 0 {
            return Err(invalid());
        }
        let (start, end_exclusive) = match self {
            Self::Inclusive { start, end } if start <= end && start < size => {
                (start, end.saturating_add(1).min(size))
            }
            Self::From { start } if start < size => (start, size),
            Self::Suffix { length } if length > 0 => (size.saturating_sub(length), size),
            _ => return Err(invalid()),
        };
        Ok(NormalizedRange {
            start,
            end_exclusive,
            object_size: size,
        })
    }
}

/// Result of opening a store read.
pub struct StoreRead {
    /// Object metadata.
    pub object: ObjectMetadata,
    /// Served range, absent for a full-object response.
    pub range: Option<NormalizedRange>,
    /// Bounded-memory byte stream.
    pub stream: ByteStream,
}

impl std::fmt::Debug for StoreRead {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoreRead")
            .field("object", &self.object)
            .field("range", &self.range)
            .field("stream", &"<byte stream>")
            .finish()
    }
}

/// Outcome of a verified immutable write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// New bytes were atomically published.
    Created,
    /// Existing verified bytes were reused.
    Reused,
}

/// Opaque Stabbur multipart upload identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultipartUpload {
    /// Adapter-generated upload identity.
    pub id: String,
}

/// A safe redirect to immutable bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreRedirect {
    /// Absolute URL, held as text to avoid exposing a third-party URL type.
    pub location: String,
    /// Redirect expiration time.
    pub expires_at: DateTime<Utc>,
}

/// Streaming artifact storage owned by Stabbur rather than a particular backend SDK.
#[async_trait]
pub trait ArtifactStore: Send + Sync {
    /// Placement role.
    fn role(&self) -> StoreRole;

    /// Supported operations.
    fn capabilities(&self) -> StoreCapabilities;

    /// Verifies that the adapter can serve normal requests without modifying stored bytes.
    async fn check_readiness(&self) -> Result<(), StoreError>;

    /// Reads immutable metadata without opening content.
    async fn head(&self, digest: &Sha256Digest) -> Result<ObjectMetadata, StoreError>;

    /// Opens a full or single-range bounded-memory stream.
    async fn read(
        &self,
        digest: &Sha256Digest,
        range: Option<ByteRange>,
    ) -> Result<StoreRead, StoreError>;

    /// Streams, rehashes, fsyncs, and atomically publishes immutable content.
    async fn write(
        &self,
        digest: &Sha256Digest,
        size: u64,
        source: ByteStream,
    ) -> Result<WriteOutcome, StoreError>;

    /// Deletes content when supported.
    async fn delete(&self, _digest: &Sha256Digest) -> Result<(), StoreError> {
        Err(StoreError::UnsupportedCapability {
            operation: StoreOperation::Delete,
        })
    }

    /// Starts a multipart upload when supported.
    async fn begin_multipart(
        &self,
        _digest: &Sha256Digest,
        _size: u64,
    ) -> Result<MultipartUpload, StoreError> {
        Err(StoreError::UnsupportedCapability {
            operation: StoreOperation::Multipart,
        })
    }

    /// Aborts a multipart upload when supported.
    async fn abort_multipart(&self, _upload: &MultipartUpload) -> Result<(), StoreError> {
        Err(StoreError::UnsupportedCapability {
            operation: StoreOperation::Multipart,
        })
    }

    /// Creates a time-limited immutable redirect when supported.
    async fn redirect(&self, _digest: &Sha256Digest) -> Result<StoreRedirect, StoreError> {
        Err(StoreError::UnsupportedCapability {
            operation: StoreOperation::Redirect,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_all_single_range_forms() {
        assert_eq!(
            ByteRange::Inclusive { start: 2, end: 5 }
                .normalize(10)
                .unwrap(),
            NormalizedRange {
                start: 2,
                end_exclusive: 6,
                object_size: 10
            }
        );
        assert_eq!(
            ByteRange::From { start: 8 }.normalize(10).unwrap().length(),
            2
        );
        assert_eq!(
            ByteRange::Suffix { length: 3 }.normalize(10).unwrap().start,
            7
        );
        assert_eq!(
            ByteRange::Suffix { length: 30 }
                .normalize(10)
                .unwrap()
                .start,
            0
        );
    }

    #[test]
    fn rejects_empty_and_invalid_ranges() {
        assert!(ByteRange::From { start: 0 }.normalize(0).is_err());
        assert!(ByteRange::From { start: 10 }.normalize(10).is_err());
        assert!(ByteRange::Suffix { length: 0 }.normalize(10).is_err());
        assert!(
            ByteRange::Inclusive { start: 4, end: 3 }
                .normalize(10)
                .is_err()
        );
    }
}
