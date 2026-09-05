//! Aggregate- and operation-shaped storage capabilities and explicit transaction context.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use stabbur_auth_core::{
    PasswordHash, Permission, Principal, PrincipalId, Role, RoleName, TokenHash,
};
use stabbur_builder_core::{
    BuilderExecutionResult, RecipeCatalogEntry, RecipeCatalogManifest,
    RecipeCatalogScanExecutionFailure, RecipeCatalogScanExecutionResult, RecipeCatalogSource,
};
use stabbur_domain::{
    Artifact, ArtifactRole, AttemptId, AuditEventId, BuildTargetId, JobId, LocationId,
    LocationState, RecipeCatalogScanId, RecipeCatalogSnapshotId, RecipeId, RecipeRevisionId,
    Release, ReleaseId, RunId, Sha256Digest, Software, SoftwareId, SoftwareInstallation, StoreId,
    Variant, VariantId, WorkerId,
};
use stabbur_jobs_core::{CapabilitySet, Job, JobState, JobSubject, Lease};
use stabbur_store_core::StoreRole;
use thiserror::Error;

/// A persistence failure expressed without adapter implementation types.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StorageError {
    /// A requested aggregate does not exist.
    #[error("resource was not found")]
    NotFound,
    /// A unique identity or idempotency invariant conflicts.
    #[error("resource already exists or conflicts with current state")]
    Conflict,
    /// An optimistic concurrency precondition is stale.
    #[error("resource revision does not match If-Match")]
    StaleRevision,
    /// Bootstrap is disabled or the one-time secret is invalid.
    #[error("bootstrap is unavailable")]
    BootstrapUnavailable,
    /// Authentication failed.
    #[error("invalid credentials")]
    InvalidCredentials,
    /// A lease is missing, expired, or owned by another worker.
    #[error("job lease is no longer active")]
    InvalidLease,
    /// Input violates a storage-level constraint.
    #[error("persisted value is invalid: {message}")]
    InvalidData {
        /// Safe validation diagnostic.
        message: String,
    },
    /// Adapter operation failed without exposing a database type or statement.
    #[error("persistence operation failed: {message}")]
    Backend {
        /// Safe diagnostic text.
        message: String,
    },
}

/// An actor representation stored with every auditable mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "identity", rename_all = "snake_case")]
pub enum AuditActor {
    /// An authenticated human, service, or worker principal.
    Principal(PrincipalId),
    /// An explicitly narrow local administrative command.
    LocalBreakGlass,
    /// One-time first-administrator bootstrap.
    Bootstrap,
    /// Server-owned background policy such as the build scheduler.
    System,
}

/// Append-only audit evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Event identity.
    pub id: AuditEventId,
    /// Actor responsible for the operation.
    pub actor: AuditActor,
    /// Stable operation name.
    pub action: String,
    /// Stable resource kind.
    pub resource_kind: String,
    /// Optional domain resource identity.
    pub resource_id: Option<String>,
    /// Safe structured details; credentials must never be included.
    pub details: serde_json::Value,
    /// Correlation/request identity.
    pub request_id: Option<String>,
    /// Event time.
    pub occurred_at: DateTime<Utc>,
}

/// Human authentication data read for local password verification.
#[derive(Debug, Clone)]
pub struct HumanCredential {
    /// Authenticated principal.
    pub principal: Principal,
    /// Encoded Argon2id password hash.
    pub password_hash: PasswordHash,
}

/// Administrative principal view with concurrency metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrincipalRecord {
    /// Authentication and role identity.
    pub principal: Principal,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Optimistic-concurrency revision.
    pub revision: u64,
}

/// Named bearer credential metadata; the secret and its hash are never returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiTokenRecord {
    /// Opaque UUID identity.
    pub id: String,
    /// Owning principal.
    pub principal_id: PrincipalId,
    /// Required operator-facing name.
    pub name: String,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Optional expiration time.
    pub expires_at: Option<DateTime<Utc>>,
    /// Revocation time, when revoked.
    pub revoked_at: Option<DateTime<Utc>>,
}

/// Role policy and concurrency metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleRecord {
    /// Named permission set.
    pub role: Role,
    /// Optimistic-concurrency revision.
    pub revision: u64,
}

/// One durable location of immutable artifact bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactLocation {
    /// Location identity.
    pub id: LocationId,
    /// Artifact digest.
    pub digest: Sha256Digest,
    /// Store identity.
    pub store_id: StoreId,
    /// Independently tracked state.
    pub state: LocationState,
    /// Last server verification time.
    pub verified_at: Option<DateTime<Utc>>,
    /// Safe last-error summary.
    pub last_error: Option<String>,
}

/// Store configuration visible to application services.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreRecord {
    /// Store identity.
    pub id: StoreId,
    /// Unique operator-facing name.
    pub name: String,
    /// Placement role.
    pub role: StoreRole,
    /// Adapter kind, such as `fs`.
    pub kind: String,
    /// Whether placement and serving may use this store.
    pub enabled: bool,
    /// Optimistic-concurrency revision.
    pub revision: u64,
}

/// Result of creating the one-time bootstrap credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapPreparation {
    /// A new secret hash was persisted and the caller must write the secret file.
    Created,
    /// Bootstrap was already prepared and remains pending.
    Pending,
    /// Bootstrap was permanently disabled.
    Disabled,
}

/// Result of an idempotent terminal job submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionOutcome {
    /// This request committed the terminal result.
    Completed,
    /// The same idempotency key previously committed it.
    Replayed,
}

/// Result of atomically creating or replaying a queued run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunCreation {
    /// Durable run selected by the idempotency record.
    pub run: RunRecord,
    /// Whether this call created or replayed the run.
    pub outcome: CompletionOutcome,
}

/// Outcome of atomic builder completion and release publication policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildDisposition {
    /// This build created the immutable release and its evidence graph.
    ReleaseCreated,
    /// The exact release artifact graph and evidence already existed.
    NoChange,
    /// Identical immutable output was observed with new successful build evidence.
    EvidenceChanged,
    /// Verification did not permit accepting this observation.
    VerificationFailed,
    /// The opaque version existed with different immutable output.
    VersionContentConflict,
}

/// Outcome of atomic builder completion and release publication policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildCompletion {
    /// Whether this submission committed or replayed an existing idempotent result.
    pub outcome: CompletionOutcome,
    /// Publication decision made for the submitted opaque version.
    pub disposition: BuildDisposition,
    /// Created release or existing same-version release used for comparison.
    pub release: Release,
}

/// An artifact attached to a variant with server-observed readability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VariantArtifactRecord {
    /// Immutable artifact metadata.
    pub artifact: Artifact,
    /// Semantic role within the variant.
    pub role: ArtifactRole,
    /// Whether an enabled readable location is currently present.
    pub readable: bool,
}

/// One mutable channel binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelRecord {
    /// Parent software identity.
    pub software_id: SoftwareId,
    /// Lowercase channel name.
    pub name: String,
    /// Current release target.
    pub release_id: ReleaseId,
    /// Optional variant pin honored before compatibility resolution.
    pub pinned_variant_id: Option<VariantId>,
    /// Optimistic-concurrency revision.
    pub revision: u64,
}

/// A job and its newly created exclusive lease.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimedJob {
    /// Durable job request.
    pub job: Job,
    /// Exclusive attempt lease.
    pub lease: Lease,
}

/// Operator-visible immutable-revision recipe metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipeRecord {
    /// Recipe identity.
    pub id: RecipeId,
    /// Unique operator-facing name.
    pub name: String,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Optimistic-concurrency revision of mutable recipe metadata.
    pub revision: u64,
}

/// New immutable execution definition awaiting its per-recipe sequence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewRecipeRevision {
    /// Optional next sequence precondition checked in the append transaction.
    #[serde(default)]
    pub expected_sequence: Option<std::num::NonZeroU64>,
    /// Immutable revision identity.
    pub id: RecipeRevisionId,
    /// Parent recipe.
    pub recipe_id: RecipeId,
    /// Stable builder adapter name.
    pub builder: String,
    /// Adapter-owned definition represented without implementation types.
    pub definition: serde_json::Value,
    /// Worker capabilities required to execute this revision.
    pub required_capabilities: CapabilitySet,
    /// Creation time.
    pub created_at: DateTime<Utc>,
}

/// One persisted immutable execution definition for a recipe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecipeRevisionRecord {
    /// Immutable revision identity.
    pub id: RecipeRevisionId,
    /// Parent recipe.
    pub recipe_id: RecipeId,
    /// Monotonic sequence within the recipe.
    pub sequence: u64,
    /// Stable builder adapter name.
    pub builder: String,
    /// Adapter-owned definition represented without implementation types.
    pub definition: serde_json::Value,
    /// Worker capabilities required to execute this revision.
    pub required_capabilities: CapabilitySet,
    /// Creation time.
    pub created_at: DateTime<Utc>,
}

/// One immutable worker observation of a builder-neutral recipe catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipeCatalogSnapshotRecord {
    /// Snapshot identity.
    pub id: RecipeCatalogSnapshotId,
    /// Authenticated worker that published the observation.
    pub worker_id: WorkerId,
    /// Digest of the canonical manifest JSON.
    pub manifest_digest: Sha256Digest,
    /// Validated builder-neutral manifest.
    pub manifest: RecipeCatalogManifest,
    /// Server receipt time.
    pub observed_at: DateTime<Utc>,
}

/// Lightweight catalog snapshot collection record without its bounded manifest body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipeCatalogSnapshotSummaryRecord {
    /// Snapshot identity.
    pub id: RecipeCatalogSnapshotId,
    /// Authenticated publishing worker.
    pub worker_id: WorkerId,
    /// Stable producer adapter name.
    pub producer: String,
    /// Stable source locator.
    pub source_locator: String,
    /// Exact opaque source revision.
    pub source_revision: String,
    /// Digest of the canonical manifest JSON.
    pub manifest_digest: Sha256Digest,
    /// Number of normalized recipes in the manifest.
    pub recipe_count: u32,
    /// Number of safe validation diagnostics in the manifest.
    pub diagnostic_count: u32,
    /// Server receipt time.
    pub observed_at: DateTime<Utc>,
}

/// Latest-source observation containing one exact catalog recipe identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipeCatalogMatch {
    /// Snapshot identity containing the entry.
    pub snapshot_id: RecipeCatalogSnapshotId,
    /// Authenticated worker that published the snapshot.
    pub worker_id: WorkerId,
    /// Stable producer adapter name.
    pub producer: String,
    /// Stable source locator.
    pub source_locator: String,
    /// Exact opaque source revision observed by the worker.
    pub source_revision: String,
    /// Normalized builder-neutral recipe entry.
    pub recipe: RecipeCatalogEntry,
    /// Server receipt time.
    pub observed_at: DateTime<Utc>,
}

/// Idempotent result of publishing a canonical catalog snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecipeCatalogPublishOutcome {
    /// A new append-only snapshot and audit event were committed.
    Published,
    /// This worker already published the exact manifest digest.
    Replayed,
}

/// Persisted snapshot selected by an idempotent catalog publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipeCatalogPublication {
    /// Whether this call appended or replayed the snapshot.
    pub outcome: RecipeCatalogPublishOutcome,
    /// Newly appended or previously persisted snapshot.
    pub snapshot: RecipeCatalogSnapshotRecord,
}

/// Durable operator request for one worker-produced catalog observation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecipeCatalogScanRecord {
    /// Scan identity.
    pub id: RecipeCatalogScanId,
    /// Durable worker job executing the scan.
    pub job_id: JobId,
    /// Stable producer adapter selector.
    pub producer: String,
    /// Exact immutable source requested by the server.
    pub source: RecipeCatalogSource,
    /// Current scheduling state derived from the owning job.
    pub state: JobState,
    /// Published immutable snapshot after successful completion.
    pub snapshot_id: Option<RecipeCatalogSnapshotId>,
    /// Typed safe terminal failure, when supplied by the worker.
    pub failure: Option<RecipeCatalogScanExecutionFailure>,
    /// Server request time.
    pub requested_at: DateTime<Utc>,
    /// Terminal completion time.
    pub completed_at: Option<DateTime<Utc>>,
}

/// Lightweight catalog-scan collection record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipeCatalogScanSummaryRecord {
    /// Scan identity.
    pub id: RecipeCatalogScanId,
    /// Durable worker job executing the scan.
    pub job_id: JobId,
    /// Stable producer adapter selector.
    pub producer: String,
    /// Exact immutable source requested by the server.
    pub source: RecipeCatalogSource,
    /// Current scheduling state derived from the owning job.
    pub state: JobState,
    /// Published immutable snapshot after successful completion.
    pub snapshot_id: Option<RecipeCatalogSnapshotId>,
    /// Server request time.
    pub requested_at: DateTime<Utc>,
    /// Terminal completion time.
    pub completed_at: Option<DateTime<Utc>>,
}

/// Idempotent catalog-scan creation result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecipeCatalogScanCreation {
    /// Created or replayed durable scan.
    pub scan: RecipeCatalogScanRecord,
    /// Whether this request created or replayed the scan.
    pub outcome: CompletionOutcome,
}

/// Atomic catalog-scan completion and snapshot publication result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipeCatalogScanCompletion {
    /// Whether this worker submission completed or replayed the job.
    pub outcome: CompletionOutcome,
    /// Created or content-addressed replayed snapshot.
    pub publication: RecipeCatalogPublication,
}

/// Minimum supported recurring build interval.
pub const MIN_BUILD_INTERVAL_SECONDS: u32 = 60;
/// Maximum supported recurring build interval.
pub const MAX_BUILD_INTERVAL_SECONDS: u32 = 365 * 24 * 60 * 60;

/// Builder-neutral trigger policy for one persisted build target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BuildTargetSchedule {
    /// Runs are created only through an explicit trigger request.
    Manual,
    /// The server creates a run whenever the durable cursor becomes due.
    Interval {
        /// Fixed interval between eligible run cursors.
        every_seconds: u32,
    },
}

/// Current configuration and operational cursor for one build target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BuildTargetRecord {
    /// Stable target identity.
    pub id: BuildTargetId,
    /// Unique operator-facing target name.
    pub name: String,
    /// Software receiving build output.
    pub software_id: SoftwareId,
    /// Exact immutable recipe revision to execute.
    pub recipe_revision_id: RecipeRevisionId,
    /// Non-secret builder parameters.
    pub parameters: serde_json::Value,
    /// Manual or recurring trigger policy.
    pub schedule: BuildTargetSchedule,
    /// Whether manual and scheduled triggering is allowed.
    pub enabled: bool,
    /// Next durable recurring cursor, absent for manual targets.
    pub next_run_at: Option<DateTime<Utc>>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Latest configuration or scheduler-cursor change time.
    pub updated_at: DateTime<Utc>,
    /// Optimistic-concurrency revision for configuration and scheduler cursor state.
    pub revision: u64,
}

/// Reason one target created a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BuildTargetRunTrigger {
    /// An authenticated caller explicitly requested the run.
    Manual,
    /// The server consumed one exact due cursor and advanced it atomically.
    Scheduled {
        /// Target revision observed when composing this run.
        target_revision: u64,
        /// Cursor consumed by this run.
        due_at: DateTime<Utc>,
        /// First recurring cursor strictly after the scheduler observation time.
        next_run_at: DateTime<Utc>,
    },
}

/// Durable run lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// Waiting for a compatible worker.
    Queued,
    /// Held by an active worker attempt.
    Running,
    /// Completed successfully.
    Succeeded,
    /// Reached a terminal failure.
    Failed,
    /// Cancelled before completion.
    Cancelled,
}

/// Durable execution of an immutable recipe revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    /// Run identity.
    pub id: RunId,
    /// Immutable recipe revision.
    pub recipe_revision_id: RecipeRevisionId,
    /// Software receiving discovered release data.
    pub software_id: SoftwareId,
    /// Durable lifecycle state.
    pub state: RunState,
    /// Non-secret execution parameters.
    pub parameters: serde_json::Value,
    /// Terminal builder result when complete.
    pub result: Option<serde_json::Value>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Terminal completion time.
    pub completed_at: Option<DateTime<Utc>>,
}

/// Lightweight run collection record without potentially large parameters or terminal evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSummaryRecord {
    /// Run identity.
    pub id: RunId,
    /// Immutable recipe revision.
    pub recipe_revision_id: RecipeRevisionId,
    /// Software receiving discovered release data.
    pub software_id: SoftwareId,
    /// Durable lifecycle state.
    pub state: RunState,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Terminal completion time.
    pub completed_at: Option<DateTime<Utc>>,
}

/// Lightweight job collection record without the potentially large builder envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSummaryRecord {
    /// Job identity.
    pub id: JobId,
    /// Domain work item controlled by this job.
    pub subject: JobSubject,
    /// Capabilities required for a claim.
    pub required_capabilities: CapabilitySet,
    /// Durable scheduling state.
    pub state: JobState,
    /// Maximum attempts before terminal failure.
    pub maximum_attempts: u32,
    /// Attempts already issued.
    pub attempt_count: u32,
    /// Creation time.
    pub created_at: DateTime<Utc>,
}

/// Origin stream for one ordered run-log entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunLogStream {
    /// Builder standard output.
    Stdout,
    /// Builder standard error.
    Stderr,
    /// Worker or control-plane lifecycle message.
    System,
}

/// One log entry awaiting a server-assigned global run sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRunLogEntry {
    /// Source stream.
    pub stream: RunLogStream,
    /// Exact bytes, which need not be UTF-8.
    pub message: Vec<u8>,
}

/// One immutable globally ordered run-log entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunLogRecord {
    /// Parent run.
    pub run_id: RunId,
    /// Attempt that emitted the entry.
    pub attempt_id: stabbur_domain::AttemptId,
    /// Monotonic sequence across all attempts for this run.
    pub sequence: u64,
    /// Source stream.
    pub stream: RunLogStream,
    /// Exact bytes, which need not be UTF-8.
    pub message: Vec<u8>,
    /// Server receipt time.
    pub occurred_at: DateTime<Utc>,
}

/// Durable sequence range assigned to one idempotent log batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunLogReceipt {
    /// First assigned sequence.
    pub first_sequence: u64,
    /// Last assigned sequence.
    pub last_sequence: u64,
    /// Number of appended entries.
    pub count: u32,
}

/// Result of idempotently appending one worker log batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunLogAppendOutcome {
    /// This request appended the batch.
    Appended(RunLogReceipt),
    /// This idempotency key previously appended the same batch.
    Replayed(RunLogReceipt),
}

/// Maximum exact bytes in one persisted log entry.
pub const MAX_RUN_LOG_ENTRY_BYTES: usize = 64 * 1024;
/// Maximum exact bytes in one worker log batch.
pub const MAX_RUN_LOG_BATCH_BYTES: usize = 1024 * 1024;

/// Provisioned worker identity and its server-defined capability ceiling.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerRecord {
    /// Drain prevents new claims while preserving authentication and active leases.
    #[serde(default)]
    pub draining: bool,
    /// Worker identity placed in its owner-only credential file.
    pub id: WorkerId,
    /// Worker-scoped authentication principal.
    pub principal_id: Option<PrincipalId>,
    /// Unique operator-facing worker name.
    pub name: String,
    /// Maximum capabilities this worker is allowed to advertise.
    pub allowed_capabilities: CapabilitySet,
    /// Capabilities detected and advertised by the latest registration.
    pub advertised_capabilities: CapabilitySet,
    /// Whether this worker may authenticate and claim work.
    pub enabled: bool,
    /// Latest successful registration or claim activity.
    pub last_seen_at: DateTime<Utc>,
    /// Optimistic-concurrency revision.
    pub revision: u64,
}

/// Explicit short write transaction for ordinary aggregate mutations.
#[async_trait]
pub trait StorageTransaction: Send {
    /// Creates a software aggregate.
    async fn create_software(&mut self, software: &Software) -> Result<(), StorageError>;

    /// Adds installation and detection metadata in the same creation transaction.
    async fn set_software_installation(
        &mut self,
        installation: &SoftwareInstallation,
    ) -> Result<(), StorageError>;

    /// Records immutable artifact metadata and one independently tracked location.
    async fn record_artifact_location(
        &mut self,
        artifact: &Artifact,
        location: &ArtifactLocation,
    ) -> Result<(), StorageError>;

    /// Appends audit evidence in the same transaction as its mutation.
    async fn append_audit(&mut self, event: &AuditEvent) -> Result<(), StorageError>;

    /// Commits this transaction.
    async fn commit(self: Box<Self>) -> Result<(), StorageError>;

    /// Explicitly rolls back this transaction.
    async fn rollback(self: Box<Self>) -> Result<(), StorageError>;
}

/// Starts backend-neutral units of work.
#[async_trait]
pub trait TransactionalStorage: Send + Sync {
    /// Starts a short explicit write transaction.
    async fn begin(&self) -> Result<Box<dyn StorageTransaction>, StorageError>;
}

/// Persists the one-time bootstrap lifecycle.
#[async_trait]
pub trait BootstrapStorage: Send + Sync {
    /// Prepares a one-time bootstrap secret if bootstrap has never been initialized.
    async fn prepare_bootstrap(
        &self,
        secret_hash: &TokenHash,
    ) -> Result<BootstrapPreparation, StorageError>;

    /// Atomically creates the first administrator and permanently disables bootstrap.
    async fn bootstrap_admin(
        &self,
        secret: &str,
        username: &str,
        password_hash: &PasswordHash,
        now: DateTime<Utc>,
    ) -> Result<Principal, StorageError>;

    /// Atomically creates the first administrator through an exclusive local break-glass path.
    ///
    /// The application must establish adapter-specific exclusive access before calling this
    /// operation. The backend still fails closed if bootstrap was permanently disabled or any
    /// principal already exists.
    async fn bootstrap_admin_local(
        &self,
        username: &str,
        password_hash: &PasswordHash,
        now: DateTime<Utc>,
    ) -> Result<Principal, StorageError>;
}

/// Persists human and bearer credentials.
#[async_trait]
pub trait CredentialStorage: Send + Sync {
    /// Loads a human principal and password hash by login name.
    async fn human_credential(
        &self,
        username: &str,
    ) -> Result<Option<HumanCredential>, StorageError>;

    /// Creates a short- or long-lived hashed bearer credential.
    async fn create_credential(
        &self,
        principal_id: PrincipalId,
        name: Option<&str>,
        kind: &str,
        token_hash: &TokenHash,
        expires_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Result<(), StorageError>;

    /// Authenticates an unrevoked, unexpired bearer token by its one-way hash.
    async fn principal_by_token(
        &self,
        token_hash: &TokenHash,
        now: DateTime<Utc>,
    ) -> Result<Option<Principal>, StorageError>;

    /// Revokes all credentials owned by a named human principal.
    async fn revoke_credentials(
        &self,
        username: &str,
        actor: AuditActor,
        now: DateTime<Utc>,
    ) -> Result<u64, StorageError>;

    /// Replaces a human password in an audited operation.
    async fn reset_password(
        &self,
        username: &str,
        password_hash: &PasswordHash,
        actor: AuditActor,
        now: DateTime<Utc>,
    ) -> Result<(), StorageError>;
}

/// Persists ordinary principal, token, role, and assignment administration.
#[async_trait]
pub trait IdentityAdminStorage: Send + Sync {
    /// Lists principals after an opaque UUID cursor.
    async fn list_principals(
        &self,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<PrincipalRecord>, StorageError>;

    /// Loads a principal by UUID or case-insensitive name.
    async fn principal_record(
        &self,
        identity: &str,
    ) -> Result<Option<PrincipalRecord>, StorageError>;

    /// Creates a human or service principal and initial role assignments atomically.
    async fn create_principal(
        &self,
        principal: &Principal,
        password_hash: Option<&PasswordHash>,
        audit: &AuditEvent,
        now: DateTime<Utc>,
    ) -> Result<PrincipalRecord, StorageError>;

    /// Replaces role assignments under optimistic concurrency.
    async fn assign_roles(
        &self,
        principal_id: PrincipalId,
        roles: &[RoleName],
        expected_revision: u64,
        audit: &AuditEvent,
    ) -> Result<PrincipalRecord, StorageError>;

    /// Enables or disables a principal and revokes active credentials when disabling.
    async fn set_principal_enabled(
        &self,
        principal_id: PrincipalId,
        enabled: bool,
        expected_revision: u64,
        audit: &AuditEvent,
        now: DateTime<Utc>,
    ) -> Result<PrincipalRecord, StorageError>;

    /// Lists named API credentials for one principal.
    async fn api_tokens(
        &self,
        principal_id: PrincipalId,
    ) -> Result<Vec<ApiTokenRecord>, StorageError>;

    /// Creates a hashed named API credential and audit evidence atomically.
    async fn create_api_token(
        &self,
        token: &ApiTokenRecord,
        token_hash: &TokenHash,
        audit: &AuditEvent,
    ) -> Result<(), StorageError>;

    /// Revokes one named API credential idempotently.
    async fn revoke_api_token(
        &self,
        token_id: &str,
        audit: &AuditEvent,
        now: DateTime<Utc>,
    ) -> Result<ApiTokenRecord, StorageError>;

    /// Lists built-in and custom roles.
    async fn roles(&self) -> Result<Vec<RoleRecord>, StorageError>;

    /// Creates one custom role after validating its complete permission set.
    async fn create_role(
        &self,
        name: &RoleName,
        permissions: &[Permission],
        audit: &AuditEvent,
    ) -> Result<RoleRecord, StorageError>;
}

/// Persists software aggregates.
#[async_trait]
pub trait SoftwareStorage: Send + Sync {
    /// Returns bounded execution and publication status for one software item.
    async fn software_status(
        &self,
        software_id: SoftwareId,
        now: DateTime<Utc>,
    ) -> Result<SoftwareStatus, StorageError>;
    /// Loads software by UUIDv7 identity or lowercase slug.
    async fn software(&self, identity: &str) -> Result<Option<Software>, StorageError>;

    /// Lists software after an opaque UUID cursor.
    async fn list_software(
        &self,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<Software>, StorageError>;

    /// Loads optional installation and installed-state detection metadata.
    async fn software_installation(
        &self,
        software_id: SoftwareId,
    ) -> Result<Option<SoftwareInstallation>, StorageError>;

    /// Updates mutable display or installation metadata under optimistic concurrency.
    async fn update_software(
        &self,
        software_id: SoftwareId,
        name: Option<&str>,
        installation: Option<&SoftwareInstallation>,
        expected_revision: u64,
        audit: &AuditEvent,
    ) -> Result<Software, StorageError>;
}

/// Persists immutable artifact metadata, locations, and store registrations.
#[async_trait]
pub trait ArtifactStorage: Send + Sync {
    /// Loads immutable artifact metadata.
    async fn artifact(&self, digest: &Sha256Digest) -> Result<Option<Artifact>, StorageError>;

    /// Lists independently tracked locations for an artifact.
    async fn artifact_locations(
        &self,
        digest: &Sha256Digest,
    ) -> Result<Vec<ArtifactLocation>, StorageError>;

    /// Ensures the configured local primary store has a durable identity.
    async fn ensure_local_primary_store(
        &self,
        id: StoreId,
        name: &str,
    ) -> Result<StoreRecord, StorageError>;

    /// Lists configured artifact stores.
    async fn stores(&self) -> Result<Vec<StoreRecord>, StorageError>;

    /// Loads one configured artifact store.
    async fn store(&self, store_id: StoreId) -> Result<Option<StoreRecord>, StorageError>;
}

/// Persists worker registrations and capability advertisements.
#[async_trait]
pub trait WorkerStorage: Send + Sync {
    /// Changes claim eligibility without invalidating active attempts or credentials.
    async fn set_worker_draining(
        &self,
        worker_id: WorkerId,
        draining: bool,
        expected_revision: u64,
        audit: &AuditEvent,
    ) -> Result<WorkerRecord, StorageError>;

    /// Atomically creates a worker principal, hashed credential, capability ceiling, and audit.
    async fn provision_worker(
        &self,
        worker: &WorkerRecord,
        token_hash: &TokenHash,
        audit: &AuditEvent,
    ) -> Result<(), StorageError>;

    /// Loads a worker by its worker-scoped principal.
    async fn worker_for_principal(
        &self,
        principal_id: PrincipalId,
    ) -> Result<Option<WorkerRecord>, StorageError>;

    /// Loads a worker by identity.
    async fn worker(&self, worker_id: WorkerId) -> Result<Option<WorkerRecord>, StorageError>;

    /// Appends or refreshes a worker and its complete capability advertisement.
    async fn register_worker(
        &self,
        worker_id: WorkerId,
        name: &str,
        capabilities: &CapabilitySet,
        now: DateTime<Utc>,
    ) -> Result<(), StorageError>;

    /// Lists workers after an opaque UUID cursor.
    async fn list_workers(
        &self,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<WorkerRecord>, StorageError>;

    /// Changes worker status or capability ceiling under optimistic concurrency.
    async fn update_worker(
        &self,
        worker_id: WorkerId,
        enabled: Option<bool>,
        allowed_capabilities: Option<&CapabilitySet>,
        expected_revision: u64,
        audit: &AuditEvent,
        now: DateTime<Utc>,
    ) -> Result<WorkerRecord, StorageError>;

    /// Revokes prior worker credentials and installs one new hash atomically.
    async fn rotate_worker_credential(
        &self,
        worker_id: WorkerId,
        token_hash: &TokenHash,
        expected_revision: u64,
        audit: &AuditEvent,
        now: DateTime<Utc>,
    ) -> Result<WorkerRecord, StorageError>;
}

/// Persists recipes and their immutable revisions.
#[async_trait]
pub trait RecipeStorage: Send + Sync {
    /// Creates recipe metadata and audit evidence atomically.
    async fn create_recipe(
        &self,
        recipe: &RecipeRecord,
        audit: &AuditEvent,
    ) -> Result<(), StorageError>;

    /// Loads a recipe by UUIDv7 identity or exact name.
    async fn recipe(&self, identity: &str) -> Result<Option<RecipeRecord>, StorageError>;

    /// Appends an immutable recipe revision and audit evidence atomically.
    async fn create_recipe_revision(
        &self,
        revision: &NewRecipeRevision,
        audit: &AuditEvent,
    ) -> Result<RecipeRevisionRecord, StorageError>;

    /// Loads one immutable recipe revision.
    async fn recipe_revision(
        &self,
        revision_id: RecipeRevisionId,
    ) -> Result<Option<RecipeRevisionRecord>, StorageError>;

    /// Lists recipe metadata after an opaque UUID cursor.
    async fn list_recipes(
        &self,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<RecipeRecord>, StorageError>;

    /// Lists immutable revisions for one recipe in sequence order.
    async fn recipe_revisions(
        &self,
        recipe_id: RecipeId,
    ) -> Result<Vec<RecipeRevisionRecord>, StorageError>;
}

/// Persists immutable builder-neutral catalog observations published by authenticated workers.
#[async_trait]
pub trait RecipeCatalogStorage: Send + Sync {
    /// Appends one canonical snapshot and audit event, or replays its worker-scoped digest.
    async fn publish_recipe_catalog(
        &self,
        snapshot: &RecipeCatalogSnapshotRecord,
        audit: &AuditEvent,
    ) -> Result<RecipeCatalogPublication, StorageError>;

    /// Loads one immutable catalog snapshot.
    async fn recipe_catalog_snapshot(
        &self,
        snapshot_id: RecipeCatalogSnapshotId,
    ) -> Result<Option<RecipeCatalogSnapshotRecord>, StorageError>;

    /// Lists snapshot metadata after an opaque UUID cursor.
    async fn list_recipe_catalog_snapshots(
        &self,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<RecipeCatalogSnapshotSummaryRecord>, StorageError>;

    /// Finds an exact identifier only in the latest snapshot for every producer/source pair.
    async fn latest_recipe_catalog_matches(
        &self,
        identifier: &str,
    ) -> Result<Vec<RecipeCatalogMatch>, StorageError>;
}

/// Persists durable server-requested catalog scans and their atomic terminal observations.
#[async_trait]
pub trait RecipeCatalogScanStorage: Send + Sync {
    /// Creates a queued scan and worker job atomically, or replays its idempotency key.
    async fn create_recipe_catalog_scan(
        &self,
        scan: &RecipeCatalogScanRecord,
        job: &Job,
        idempotency_scope: &str,
        idempotency_key: &str,
        audit: &AuditEvent,
    ) -> Result<RecipeCatalogScanCreation, StorageError>;

    /// Loads one durable catalog scan.
    async fn recipe_catalog_scan(
        &self,
        scan_id: RecipeCatalogScanId,
    ) -> Result<Option<RecipeCatalogScanRecord>, StorageError>;

    /// Lists lightweight scan records after an opaque UUID cursor.
    async fn list_recipe_catalog_scans(
        &self,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<RecipeCatalogScanSummaryRecord>, StorageError>;

    /// Cancels queued or leased scan work and closes an active attempt atomically.
    async fn cancel_recipe_catalog_scan(
        &self,
        scan_id: RecipeCatalogScanId,
        idempotency_key: &str,
        audit: &AuditEvent,
        now: DateTime<Utc>,
    ) -> Result<RecipeCatalogScanRecord, StorageError>;

    /// Publishes a validated manifest and closes its leased scan atomically.
    async fn complete_recipe_catalog_scan(
        &self,
        worker_id: WorkerId,
        lease: &Lease,
        idempotency_key: &str,
        result: &RecipeCatalogScanExecutionResult,
        audit: &AuditEvent,
        now: DateTime<Utc>,
    ) -> Result<RecipeCatalogScanCompletion, StorageError>;

    /// Persists a typed safe scan failure and closes its lease atomically.
    async fn fail_recipe_catalog_scan(
        &self,
        worker_id: WorkerId,
        lease: &Lease,
        idempotency_key: &str,
        failure: &RecipeCatalogScanExecutionFailure,
        audit: &AuditEvent,
        now: DateTime<Utc>,
    ) -> Result<CompletionOutcome, StorageError>;
}

/// Persists runs and their atomic job creation.
#[async_trait]
pub trait RunStorage: Send + Sync {
    /// Creates a queued run, its first job, and audit evidence atomically.
    async fn create_run(
        &self,
        run: &RunRecord,
        job: &Job,
        idempotency_scope: &str,
        idempotency_key: &str,
        audit: &AuditEvent,
    ) -> Result<RunCreation, StorageError>;

    /// Loads one run.
    async fn run(&self, run_id: RunId) -> Result<Option<RunRecord>, StorageError>;

    /// Cancels queued or running work and invalidates every active worker lease atomically.
    async fn cancel_run(
        &self,
        run_id: RunId,
        idempotency_key: &str,
        audit: &AuditEvent,
        now: DateTime<Utc>,
    ) -> Result<RunRecord, StorageError>;

    /// Lists runs with optional recipe or software scoping after an opaque UUID cursor.
    async fn list_runs(
        &self,
        recipe_id: Option<RecipeId>,
        software_id: Option<SoftwareId>,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<RunSummaryRecord>, StorageError>;
}

/// Persists desired build targets and atomically turns trigger events into ordinary runs.
#[async_trait]
pub trait BuildTargetStorage: Send + Sync {
    /// Creates current target state, its first immutable revision, and audit evidence atomically.
    async fn create_build_target(
        &self,
        target: &BuildTargetRecord,
        audit: &AuditEvent,
    ) -> Result<BuildTargetRecord, StorageError>;

    /// Loads a target by UUIDv7 identity or exact name.
    async fn build_target(&self, identity: &str)
    -> Result<Option<BuildTargetRecord>, StorageError>;

    /// Lists current targets after an opaque UUID cursor.
    async fn list_build_targets(
        &self,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<BuildTargetRecord>, StorageError>;

    /// Replaces operator configuration under optimistic concurrency and appends its snapshot.
    async fn update_build_target(
        &self,
        target: &BuildTargetRecord,
        expected_revision: u64,
        audit: &AuditEvent,
    ) -> Result<BuildTargetRecord, StorageError>;

    /// Returns enabled recurring targets whose durable cursors are due.
    async fn due_build_targets(
        &self,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<BuildTargetRecord>, StorageError>;

    /// Lists runs created from one target after an opaque UUID cursor.
    async fn list_build_target_runs(
        &self,
        target_id: BuildTargetId,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<RunSummaryRecord>, StorageError>;

    /// Creates a target-linked run/job and consumes a scheduled cursor when applicable.
    #[allow(clippy::too_many_arguments)] // Trigger identity, job, audit, and time are independent transaction inputs.
    async fn create_build_target_run(
        &self,
        target_id: BuildTargetId,
        trigger: BuildTargetRunTrigger,
        run: &RunRecord,
        job: &Job,
        idempotency_key: &str,
        audit: &AuditEvent,
        now: DateTime<Utc>,
    ) -> Result<RunCreation, StorageError>;
}

/// Persists lease-bound build outputs and atomically applies publication policy.
#[async_trait]
pub trait BuildStorage: Send + Sync {
    /// Records server-verified artifact bytes against the run held by an active attempt.
    #[allow(clippy::too_many_arguments)] // The port names each independently verified lease and artifact invariant.
    async fn record_run_artifact(
        &self,
        worker_id: WorkerId,
        attempt_id: AttemptId,
        artifact: &Artifact,
        location: &ArtifactLocation,
        role: ArtifactRole,
        audit: &AuditEvent,
        now: DateTime<Utc>,
    ) -> Result<RunId, StorageError>;

    /// Completes a leased run and creates, reuses, or conflicts with a release atomically.
    async fn finalize_build(
        &self,
        worker_id: WorkerId,
        lease: &Lease,
        idempotency_key: &str,
        execution: &BuilderExecutionResult,
        audit: &AuditEvent,
        now: DateTime<Utc>,
    ) -> Result<BuildCompletion, StorageError>;
}

/// Persists release catalog reads and transactional channel/lifecycle mutations.
#[async_trait]
pub trait CatalogStorage: Send + Sync {
    /// Withdraws publication eligibility and removes channel pointers in one audited transaction.
    async fn withdraw_release(
        &self,
        release_id: ReleaseId,
        reason: &stabbur_domain::WithdrawalReason,
        expected_revision: u64,
        audit: &AuditEvent,
        now: DateTime<Utc>,
    ) -> Result<Release, StorageError>;

    /// Loads one release by UUIDv7 identity.
    async fn release(&self, release_id: ReleaseId) -> Result<Option<Release>, StorageError>;

    /// Lists releases for software after an opaque UUID cursor.
    async fn list_releases(
        &self,
        software_id: SoftwareId,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<Release>, StorageError>;

    /// Lists variants belonging to one release.
    async fn release_variants(&self, release_id: ReleaseId) -> Result<Vec<Variant>, StorageError>;

    /// Lists artifact metadata and roles belonging to one variant.
    async fn variant_artifacts(
        &self,
        variant_id: VariantId,
    ) -> Result<Vec<VariantArtifactRecord>, StorageError>;

    /// Lists current channel bindings for one software aggregate.
    async fn channels(&self, software_id: SoftwareId) -> Result<Vec<ChannelRecord>, StorageError>;

    /// Loads one current channel binding.
    async fn channel(
        &self,
        software_id: SoftwareId,
        name: &str,
    ) -> Result<Option<ChannelRecord>, StorageError>;

    /// Creates or advances a channel and its release lifecycle in one audited transaction.
    #[allow(clippy::too_many_arguments)] // Promotion atomically binds concurrency, lifecycle, actor, and audit inputs.
    async fn promote_channel(
        &self,
        software_id: SoftwareId,
        name: &str,
        release_id: ReleaseId,
        pinned_variant_id: Option<VariantId>,
        expected_revision: u64,
        reason: Option<&str>,
        audit: &AuditEvent,
        now: DateTime<Utc>,
    ) -> Result<ChannelRecord, StorageError>;

    /// Rejects a release and removes every active channel binding transactionally.
    async fn reject_release(
        &self,
        release_id: ReleaseId,
        expected_revision: u64,
        reason: &str,
        audit: &AuditEvent,
        now: DateTime<Utc>,
    ) -> Result<Release, StorageError>;
}

/// Persists append-only globally ordered run logs.
#[async_trait]
pub trait RunLogStorage: Send + Sync {
    /// Appends one idempotent batch while its worker attempt lease is active.
    async fn append_run_logs(
        &self,
        worker_id: WorkerId,
        lease: &Lease,
        idempotency_key: &str,
        entries: &[NewRunLogEntry],
        now: DateTime<Utc>,
    ) -> Result<RunLogAppendOutcome, StorageError>;

    /// Reads globally ordered entries after an exclusive sequence cursor.
    async fn run_logs(
        &self,
        run_id: RunId,
        after: Option<u64>,
        limit: u32,
    ) -> Result<Vec<RunLogRecord>, StorageError>;
}

/// Persists builder-neutral jobs and their leased attempts.
#[async_trait]
pub trait JobStorage: Send + Sync {
    /// Persists a queued builder-neutral job.
    async fn enqueue_job(&self, job: &Job) -> Result<(), StorageError>;

    /// Loads one durable job for protocol-envelope validation.
    async fn job(&self, job_id: JobId) -> Result<Option<Job>, StorageError>;

    /// Lists durable jobs after an opaque UUID cursor.
    async fn list_jobs(
        &self,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<JobSummaryRecord>, StorageError>;

    /// Atomically recovers expired leases and claims one compatible job.
    async fn claim_job(
        &self,
        worker_id: WorkerId,
        capabilities: &CapabilitySet,
        now: DateTime<Utc>,
        lease_seconds: u32,
    ) -> Result<Option<ClaimedJob>, StorageError>;

    /// Extends an active lease owned by a worker.
    async fn heartbeat_job(
        &self,
        worker_id: WorkerId,
        lease: &Lease,
        now: DateTime<Utc>,
        lease_seconds: u32,
    ) -> Result<Lease, StorageError>;

    /// Commits an idempotent terminal job result and closes its lease.
    async fn complete_job(
        &self,
        worker_id: WorkerId,
        lease: &Lease,
        idempotency_key: &str,
        result: &serde_json::Value,
        now: DateTime<Utc>,
    ) -> Result<CompletionOutcome, StorageError>;

    /// Commits an idempotent terminal job failure and closes its lease.
    async fn fail_job(
        &self,
        worker_id: WorkerId,
        lease: &Lease,
        idempotency_key: &str,
        failure: &serde_json::Value,
        now: DateTime<Utc>,
    ) -> Result<CompletionOutcome, StorageError>;
}

/// Reads append-only audit evidence.
#[async_trait]
pub trait AuditStorage: Send + Sync {
    /// Reads append-only audit evidence in chronological identity order.
    async fn audit_events(
        &self,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<AuditEvent>, StorageError>;
}

/// Performs portable operational persistence actions.
#[async_trait]
pub trait OperationalStorage: Send + Sync {
    /// Returns durable queue and worker measurements without backend diagnostics.
    async fn operational_status(&self) -> Result<OperationalStatus, StorageError>;
    /// Applies the adapter's embedded schema migrations.
    async fn migrate(&self) -> Result<(), StorageError>;

    /// Verifies that persistence can serve normal requests without performing a mutation.
    async fn check_readiness(&self) -> Result<(), StorageError>;

    /// Returns a small read-only persistence health summary.
    async fn doctor(&self) -> Result<StorageHealth, StorageError>;
}

/// Complete persistence contract required of every selectable backend.
///
/// This method-free aggregate deliberately has no blanket implementation. An adapter must
/// implement every operation trait and then opt into this contract explicitly, so missing
/// behavior is a compile-time error rather than a runtime `unsupported` response.
pub trait Storage:
    TransactionalStorage
    + BootstrapStorage
    + CredentialStorage
    + IdentityAdminStorage
    + SoftwareStorage
    + ArtifactStorage
    + WorkerStorage
    + RecipeStorage
    + RecipeCatalogStorage
    + RecipeCatalogScanStorage
    + BuildTargetStorage
    + RunStorage
    + BuildStorage
    + CatalogStorage
    + RunLogStorage
    + JobStorage
    + AuditStorage
    + OperationalStorage
    + Send
    + Sync
{
}

/// Read-only persistence health returned by the doctor command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageHealth {
    /// Stable adapter name, such as `sqlite` or `postgresql`.
    pub backend: String,
    /// Schema migrations can be queried.
    pub database_ready: bool,
    /// Number of durable software aggregates.
    pub software_count: u64,
    /// Number of durable artifacts.
    pub artifact_count: u64,
    /// Number of queued or leased jobs.
    pub active_job_count: u64,
}

/// Converts a job ID to an idempotency scope without exposing adapter keys.
#[must_use]
pub fn job_completion_scope(job_id: JobId) -> String {
    format!("job:{job_id}:complete")
}

/// One currently published channel and exact opaque release version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SoftwareChannelSummary {
    /// Channel name.
    pub name: String,
    /// Exact release identity.
    pub release_id: ReleaseId,
    /// Opaque version.
    pub version: stabbur_domain::Version,
    /// Channel concurrency revision.
    pub revision: u64,
}

/// A target for which no recently observed, enabled, non-draining worker matches all requirements.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedBuildTarget {
    /// Target identity.
    pub id: BuildTargetId,
    /// Operator name.
    pub name: String,
    /// Complete requirements; matching must occur on one worker.
    pub required_capabilities: CapabilitySet,
}

/// Operator view of software execution and publication, independent of SQL representation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SoftwareStatus {
    /// Owning software identity.
    pub software_id: SoftwareId,
    /// Current channel bindings.
    pub channels: Vec<SoftwareChannelSummary>,
    /// Latest execution observation.
    pub latest_run: Option<RunSummaryRecord>,
    /// Most recent successful check.
    pub last_success_at: Option<DateTime<Utc>>,
    /// Next enabled recurring cursor.
    pub next_run_at: Option<DateTime<Utc>>,
    /// Number of enabled targets.
    pub enabled_targets: u64,
    /// Number of queued or running builds.
    pub outstanding_runs: u64,
    /// First 200 targets blocked by current worker availability.
    pub blocked_targets: Vec<BlockedBuildTarget>,
}

/// Bounded durable queue and worker measurements for operators.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationalStatus {
    /// Jobs waiting for a worker.
    pub queued_jobs: u64,
    /// Jobs with a leased attempt.
    pub running_jobs: u64,
    /// Terminally failed jobs retained in history.
    pub failed_jobs: u64,
    /// Historical expired attempts.
    pub expired_attempts: u64,
    /// Enabled workers paused for maintenance.
    pub draining_workers: u64,
    /// Creation time of the oldest queued job.
    pub oldest_queued_at: Option<DateTime<Utc>>,
}
