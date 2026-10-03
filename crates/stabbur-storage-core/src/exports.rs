//! Saved export persistence and publication concurrency.
use crate::{AuditEvent, StorageError};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use stabbur_auth_core::TokenHash;
use stabbur_domain::{
    ExportId,
    exports::{ExportDefinition, ExportItem, PreparedExport},
};

/// Current saved definition and its last published snapshot cursor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportRecord {
    /// Stable identity.
    pub id: ExportId,
    /// Validated desired state.
    pub definition: ExportDefinition,
    /// Optimistic definition revision.
    pub revision: u64,
    /// Current snapshot generation; zero means never published.
    pub generation: u64,
    /// Last definition change.
    pub updated_at: DateTime<Utc>,
}
/// Immutable publication history entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportSnapshot {
    /// Owning export.
    pub export: ExportId,
    /// Monotonic publication generation.
    pub generation: u64,
    /// Definition revision used to publish.
    pub definition_revision: u64,
    /// Exact reviewed definition.
    pub definition: ExportDefinition,
    /// All selected installers, replaced as one snapshot.
    pub items: Vec<ExportItem>,
    /// Time committed.
    pub created_at: DateTime<Utc>,
}
/// Complete saved-export capability, implemented by every selectable relational adapter.
#[async_trait]
pub trait ExportStorage: Send + Sync {
    /// Lists one bounded page.
    async fn list_exports(
        &self,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<ExportRecord>, StorageError>;
    /// Resolves an ID or slug.
    async fn export(&self, identity: &str) -> Result<Option<ExportRecord>, StorageError>;
    /// Creates a validated draft and append-only definition history.
    async fn create_export(
        &self,
        id: ExportId,
        definition: &ExportDefinition,
        audit: &AuditEvent,
    ) -> Result<ExportRecord, StorageError>;
    /// Appends a definition revision under a concurrency fence.
    async fn update_export(
        &self,
        id: ExportId,
        definition: &ExportDefinition,
        revision: u64,
        audit: &AuditEvent,
    ) -> Result<ExportRecord, StorageError>;
    /// Atomically rechecks mutable bindings and replaces the published snapshot cursor.
    async fn apply_export(
        &self,
        publication: &PreparedExport,
        audit: &AuditEvent,
    ) -> Result<ExportSnapshot, StorageError>;
    /// Loads one immutable snapshot.
    async fn export_snapshot(
        &self,
        id: ExportId,
        generation: u64,
    ) -> Result<Option<ExportSnapshot>, StorageError>;
    /// Lists one bounded page of immutable publication history.
    async fn export_history(
        &self,
        id: ExportId,
        after: u64,
        limit: u32,
    ) -> Result<Vec<ExportSnapshot>, StorageError>;
    /// Issues a repository-only credential; raw token material never reaches storage.
    async fn create_export_reader(
        &self,
        id: ExportId,
        hash: &TokenHash,
        audit: &AuditEvent,
    ) -> Result<(), StorageError>;
    /// Checks a credential against its one export and current revocation epoch.
    async fn export_reader_valid(
        &self,
        id: ExportId,
        hash: &TokenHash,
    ) -> Result<bool, StorageError>;
    /// Invalidates all prior device profiles without erasing credential/audit history.
    async fn revoke_export_readers(
        &self,
        id: ExportId,
        audit: &AuditEvent,
    ) -> Result<(), StorageError>;
}
