//! SQLx SQLite adapter with embedded migrations, WAL defaults, and short write transactions.

mod artifacts;
mod build_targets;
mod catalog;
mod exports;
mod identity;
mod jobs;
mod library;
mod operations;
mod recipes;
mod runs;
mod software;
mod workers;

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration as StdDuration,
};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use sqlx_core::{
    Error as SqlxError,
    error::DatabaseError,
    migrate::{Migration, MigrationType, Migrator},
    pool::PoolConnection,
    query::query,
    query_scalar::query_scalar,
    row::Row,
    transaction::TransactionManager,
};
use sqlx_sqlite::{
    Sqlite, SqliteConnectOptions, SqliteConnection, SqliteJournalMode, SqlitePool,
    SqlitePoolOptions, SqliteRow, SqliteSynchronous, SqliteTransactionManager,
};
use stabbur_auth_core::{
    PasswordHash, Permission, Principal, PrincipalId, PrincipalKind, Role, RoleName, TokenHash,
};
use stabbur_builder_core::{
    BuildResult, BuilderExecutionResult, RecipeCatalogManifest, RecipeCatalogScanExecutionFailure,
    RecipeCatalogScanExecutionResult, RecipeCatalogScanJob, RecipeCatalogSource,
};
use stabbur_domain::{
    Architecture, Artifact, ArtifactRole, AttemptId, AuditEventId, BuildTargetId, Compatibility,
    JobId, LifecycleEventId, LocationState, MacOsVersion, Platform, PromotionEventId,
    RecipeCatalogScanId, RecipeCatalogSnapshotId, RecipeId, RecipeRevisionId, Release, ReleaseId,
    ReleaseState, RunId, Sha256Digest, Software, SoftwareInstallation, StoreId, Variant, VariantId,
    WorkerId,
};
use stabbur_jobs_core::{CapabilitySet, Job, JobState, JobSubject, Lease};
use stabbur_storage_core::{
    ApiTokenRecord, ArtifactLocation, ArtifactStorage, AuditActor, AuditEvent, AuditStorage,
    BootstrapPreparation, BootstrapStorage, BuildCompletion, BuildDisposition, BuildStorage,
    BuildTargetRecord, BuildTargetRunTrigger, BuildTargetSchedule, BuildTargetStorage,
    CatalogStorage, ChannelRecord, ClaimedJob, CompletionOutcome, CredentialStorage,
    HumanCredential, IdentityAdminStorage, JobStorage, JobSummaryRecord,
    MAX_BUILD_INTERVAL_SECONDS, MAX_RUN_LOG_BATCH_BYTES, MAX_RUN_LOG_ENTRY_BYTES,
    MIN_BUILD_INTERVAL_SECONDS, NewRecipeRevision, NewRunLogEntry, OperationalStorage,
    PrincipalRecord, RecipeCatalogMatch, RecipeCatalogPublication, RecipeCatalogPublishOutcome,
    RecipeCatalogScanCompletion, RecipeCatalogScanCreation, RecipeCatalogScanRecord,
    RecipeCatalogScanStorage, RecipeCatalogScanSummaryRecord, RecipeCatalogSnapshotRecord,
    RecipeCatalogSnapshotSummaryRecord, RecipeCatalogStorage, RecipeRecord, RecipeRevisionRecord,
    RecipeStorage, RoleRecord, RunCreation, RunLogAppendOutcome, RunLogReceipt, RunLogRecord,
    RunLogStorage, RunLogStream, RunRecord, RunState, RunStorage, RunSummaryRecord,
    SoftwareStorage, Storage, StorageError, StorageHealth, StorageTransaction, StoreRecord,
    TransactionalStorage, VariantArtifactRecord, WorkerRecord, WorkerStorage, job_completion_scope,
};
use stabbur_store_core::StoreRole;

fn embedded_migrator() -> Migrator {
    let migrations = vec![
        Migration::new(
            1,
            Cow::Borrowed("initial"),
            MigrationType::Simple,
            Cow::Borrowed(include_str!("../migrations/0001_initial.sql")),
            false,
        ),
        Migration::new(
            2,
            Cow::Borrowed("operator workflows"),
            MigrationType::Simple,
            Cow::Borrowed(include_str!("../migrations/0002_operator_workflows.sql")),
            false,
        ),
        Migration::new(
            3,
            Cow::Borrowed("saved exports"),
            MigrationType::Simple,
            Cow::Borrowed(include_str!("../migrations/0003_saved_exports.sql")),
            false,
        ),
        Migration::new(
            4,
            Cow::Borrowed("software library"),
            MigrationType::Simple,
            Cow::Borrowed(include_str!("../migrations/0004_library.sql")),
            false,
        ),
    ];
    Migrator {
        migrations: Cow::Owned(migrations),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    }
}

/// SQLite implementation of the Stabbur persistence port.
#[derive(Debug, Clone)]
pub struct SqliteStorage {
    pool: SqlitePool,
    _process_lock: Option<Arc<File>>,
}

impl SqliteStorage {
    /// Opens a durable SQLite database with WAL, foreign keys, and a busy timeout.
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let lock = process_lock(path.as_ref(), false)?;
        let options = SqliteConnectOptions::new()
            .filename(path.as_ref())
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(StdDuration::from_secs(5));
        Self::connect_with(options, 5, Some(lock)).await
    }

    /// Opens a durable database while excluding every service or other administrative process.
    pub async fn connect_exclusive(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let lock = process_lock(path.as_ref(), true)?;
        let options = SqliteConnectOptions::new()
            .filename(path.as_ref())
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(StdDuration::from_secs(5));
        Self::connect_with(options, 1, Some(lock)).await
    }

    /// Opens an existing database without creating it or running migrations.
    ///
    /// This is reserved for the read-only local doctor command.
    pub async fn connect_read_only(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let lock = process_lock(path.as_ref(), false)?;
        let options = SqliteConnectOptions::new()
            .filename(path.as_ref())
            .create_if_missing(false)
            .read_only(true)
            .foreign_keys(true)
            .busy_timeout(StdDuration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .map_err(|error| backend("opening read-only SQLite", &error))?;
        Ok(Self {
            pool,
            _process_lock: Some(lock),
        })
    }

    /// Opens a single-connection in-memory database for adapter and API tests.
    pub async fn in_memory() -> Result<Self, StorageError> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .map_err(|error| backend("parsing in-memory SQLite options", &error))?
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Memory)
            .busy_timeout(StdDuration::from_secs(5));
        Self::connect_with(options, 1, None).await
    }

    async fn connect_with(
        options: SqliteConnectOptions,
        maximum_connections: u32,
        process_lock: Option<Arc<File>>,
    ) -> Result<Self, StorageError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(maximum_connections)
            .connect_with(options)
            .await
            .map_err(|error| backend("opening SQLite", &error))?;
        let storage = Self {
            pool,
            _process_lock: process_lock,
        };
        storage.run_migrations().await?;
        Ok(storage)
    }

    async fn run_migrations(&self) -> Result<(), StorageError> {
        embedded_migrator()
            .run(&self.pool)
            .await
            .map_err(|error| StorageError::Backend {
                message: format!("running embedded migrations: {error}"),
            })
    }

    /// Returns the adapter-private pool only for root-crate integration tests and migrations.
    #[doc(hidden)]
    #[must_use]
    pub fn pool_for_tests(&self) -> &SqlitePool {
        &self.pool
    }

    async fn acquire_write(&self) -> Result<ImmediateWrite, StorageError> {
        let mut connection = self
            .pool
            .acquire()
            .await
            .map_err(|error| backend("acquiring SQLite connection", &error))?;
        query("BEGIN IMMEDIATE")
            .execute(&mut *connection)
            .await
            .map_err(|error| backend("starting SQLite transaction", &error))?;
        Ok(ImmediateWrite {
            connection: Some(connection),
            active: true,
        })
    }
}

fn process_lock(database: &Path, exclusive: bool) -> Result<Arc<File>, StorageError> {
    let mut lock_name = database.as_os_str().to_owned();
    lock_name.push(".lock");
    let lock_path = PathBuf::from(lock_name);
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(lock_path)
        .map_err(|error| backend("opening SQLite process lock", &error))?;
    let result = if exclusive {
        fs2::FileExt::try_lock_exclusive(&file)
    } else {
        fs2::FileExt::try_lock_shared(&file)
    };
    result.map_err(|_| StorageError::Backend {
        message: if exclusive {
            "exclusive SQLite access is unavailable while another process is using the database"
                .into()
        } else {
            "SQLite access is blocked by a local break-glass operation".into()
        },
    })?;
    Ok(Arc::new(file))
}

/// `BEGIN IMMEDIATE` connection that cannot leak a transaction back into the pool.
struct ImmediateWrite {
    connection: Option<PoolConnection<Sqlite>>,
    active: bool,
}

impl ImmediateWrite {
    async fn commit(mut self, operation: &str) -> Result<(), StorageError> {
        query("COMMIT")
            .execute(&mut *self)
            .await
            .map_err(|error| backend(operation, &error))?;
        self.active = false;
        Ok(())
    }

    fn into_connection(mut self) -> PoolConnection<Sqlite> {
        self.active = false;
        self.connection
            .take()
            .expect("an active immediate write owns a connection")
    }
}

impl Deref for ImmediateWrite {
    type Target = SqliteConnection;

    fn deref(&self) -> &Self::Target {
        self.connection
            .as_deref()
            .expect("an active immediate write owns a connection")
    }
}

impl DerefMut for ImmediateWrite {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.connection
            .as_deref_mut()
            .expect("an active immediate write owns a connection")
    }
}

impl Drop for ImmediateWrite {
    fn drop(&mut self) {
        if self.active
            && let Some(mut connection) = self.connection.take()
        {
            // Keep the pooled connection checked out until ROLLBACK has actually completed.
            // A lazy rollback can otherwise race the next BEGIN IMMEDIATE.
            tokio::spawn(async move {
                let _ = query("ROLLBACK").execute(&mut *connection).await;
            });
        }
    }
}

fn backend(operation: &str, error: &impl std::fmt::Display) -> StorageError {
    StorageError::Backend {
        message: format!("{operation}: {error}"),
    }
}

#[allow(clippy::needless_pass_by_value)] // This signature composes directly with `Result::map_err`.
fn map_write(error: SqlxError) -> StorageError {
    if error
        .as_database_error()
        .is_some_and(DatabaseError::is_unique_violation)
    {
        StorageError::Conflict
    } else if error
        .as_database_error()
        .is_some_and(DatabaseError::is_foreign_key_violation)
    {
        StorageError::InvalidData {
            message: "referenced resource does not exist".into(),
        }
    } else {
        backend("writing SQLite", &error)
    }
}

fn invalid_data(message: impl Into<String>) -> StorageError {
    StorageError::InvalidData {
        message: message.into(),
    }
}

fn parse_value<T>(value: &str, kind: &str) -> Result<T, StorageError>
where
    T: FromStr,
{
    value
        .parse()
        .map_err(|_| invalid_data(format!("invalid persisted {kind}")))
}

fn software_from_row(row: &SqliteRow) -> Result<Software, StorageError> {
    let revision: i64 = row
        .try_get("revision")
        .map_err(|error| backend("decoding software revision", &error))?;
    Ok(Software {
        id: parse_value(
            row.try_get("id")
                .map_err(|error| backend("decoding software ID", &error))?,
            "software ID",
        )?,
        slug: parse_value(
            row.try_get("slug")
                .map_err(|error| backend("decoding software slug", &error))?,
            "software slug",
        )?,
        name: row
            .try_get("name")
            .map_err(|error| backend("decoding software name", &error))?,
        created_at: row
            .try_get("created_at")
            .map_err(|error| backend("decoding software creation time", &error))?,
        revision: u64::try_from(revision)
            .map_err(|_| invalid_data("negative software revision"))?,
    })
}

fn software_installation_from_row(row: &SqliteRow) -> Result<SoftwareInstallation, StorageError> {
    let install: String = row
        .try_get("install_json")
        .map_err(|error| backend("decoding installation metadata", &error))?;
    let detection: String = row
        .try_get("detection_json")
        .map_err(|error| backend("decoding detection metadata", &error))?;
    Ok(SoftwareInstallation {
        software_id: parse_value(
            row.try_get("software_id")
                .map_err(|error| backend("decoding installation software ID", &error))?,
            "software ID",
        )?,
        install: serde_json::from_str(&install)
            .map_err(|error| invalid_data(format!("invalid installation metadata: {error}")))?,
        detection: serde_json::from_str(&detection)
            .map_err(|error| invalid_data(format!("invalid detection metadata: {error}")))?,
    })
}

fn artifact_from_row(row: &SqliteRow) -> Result<Artifact, StorageError> {
    let size: i64 = row
        .try_get("size")
        .map_err(|error| backend("decoding artifact size", &error))?;
    Ok(Artifact {
        digest: parse_value(
            row.try_get("digest")
                .map_err(|error| backend("decoding artifact digest", &error))?,
            "artifact digest",
        )?,
        size: u64::try_from(size).map_err(|_| invalid_data("negative artifact size"))?,
        media_type: row
            .try_get("media_type")
            .map_err(|error| backend("decoding artifact media type", &error))?,
        created_at: row
            .try_get("created_at")
            .map_err(|error| backend("decoding artifact creation time", &error))?,
    })
}

fn capability_set_from_json(value: &str, kind: &str) -> Result<CapabilitySet, StorageError> {
    serde_json::from_str(value)
        .map_err(|error| invalid_data(format!("invalid persisted {kind}: {error}")))
}

fn worker_from_row(row: &SqliteRow) -> Result<WorkerRecord, StorageError> {
    let principal_id = row
        .try_get::<Option<String>, _>("principal_id")
        .map_err(|error| backend("decoding worker principal ID", &error))?
        .map(|value| parse_value(&value, "worker principal ID"))
        .transpose()?;
    let allowed: String = row
        .try_get("allowed_capabilities_json")
        .map_err(|error| backend("decoding allowed worker capabilities", &error))?;
    let advertised: String = row
        .try_get("advertised_capabilities_json")
        .map_err(|error| backend("decoding advertised worker capabilities", &error))?;
    let enabled: i64 = row
        .try_get("enabled")
        .map_err(|error| backend("decoding worker status", &error))?;
    let revision: i64 = row
        .try_get("revision")
        .map_err(|error| backend("decoding worker revision", &error))?;
    Ok(WorkerRecord {
        draining: row
            .try_get::<i64, _>("draining")
            .map_err(|error| backend("decoding worker drain state", &error))?
            == 1,
        id: parse_value(
            row.try_get("id")
                .map_err(|error| backend("decoding worker ID", &error))?,
            "worker ID",
        )?,
        principal_id,
        name: row
            .try_get("name")
            .map_err(|error| backend("decoding worker name", &error))?,
        allowed_capabilities: capability_set_from_json(&allowed, "allowed worker capabilities")?,
        advertised_capabilities: capability_set_from_json(
            &advertised,
            "advertised worker capabilities",
        )?,
        enabled: enabled != 0,
        last_seen_at: row
            .try_get("last_seen_at")
            .map_err(|error| backend("decoding worker last-seen time", &error))?,
        revision: u64::try_from(revision).map_err(|_| invalid_data("negative worker revision"))?,
    })
}

fn recipe_from_row(row: &SqliteRow) -> Result<RecipeRecord, StorageError> {
    let revision: i64 = row
        .try_get("revision")
        .map_err(|error| backend("decoding recipe revision", &error))?;
    Ok(RecipeRecord {
        id: parse_value(
            row.try_get("id")
                .map_err(|error| backend("decoding recipe ID", &error))?,
            "recipe ID",
        )?,
        name: row
            .try_get("name")
            .map_err(|error| backend("decoding recipe name", &error))?,
        created_at: row
            .try_get("created_at")
            .map_err(|error| backend("decoding recipe creation time", &error))?,
        revision: u64::try_from(revision).map_err(|_| invalid_data("negative recipe revision"))?,
    })
}

fn recipe_revision_from_row(row: &SqliteRow) -> Result<RecipeRevisionRecord, StorageError> {
    let sequence: i64 = row
        .try_get("sequence")
        .map_err(|error| backend("decoding recipe sequence", &error))?;
    let definition: String = row
        .try_get("definition_json")
        .map_err(|error| backend("decoding recipe definition", &error))?;
    let required: String = row
        .try_get("required_capabilities_json")
        .map_err(|error| backend("decoding recipe capabilities", &error))?;
    Ok(RecipeRevisionRecord {
        id: parse_value(
            row.try_get("id")
                .map_err(|error| backend("decoding recipe revision ID", &error))?,
            "recipe revision ID",
        )?,
        recipe_id: parse_value(
            row.try_get("recipe_id")
                .map_err(|error| backend("decoding parent recipe ID", &error))?,
            "recipe ID",
        )?,
        sequence: u64::try_from(sequence).map_err(|_| invalid_data("negative recipe sequence"))?,
        builder: row
            .try_get("builder")
            .map_err(|error| backend("decoding recipe builder", &error))?,
        definition: serde_json::from_str(&definition)
            .map_err(|error| invalid_data(format!("invalid recipe definition: {error}")))?,
        required_capabilities: capability_set_from_json(&required, "recipe capabilities")?,
        created_at: row
            .try_get("created_at")
            .map_err(|error| backend("decoding recipe revision creation time", &error))?,
    })
}

fn recipe_catalog_snapshot_from_row(
    row: &SqliteRow,
) -> Result<RecipeCatalogSnapshotRecord, StorageError> {
    let manifest_json: String = row
        .try_get("manifest_json")
        .map_err(|error| backend("decoding recipe catalog manifest", &error))?;
    let manifest: RecipeCatalogManifest = serde_json::from_str(&manifest_json)
        .map_err(|error| invalid_data(format!("invalid recipe catalog manifest: {error}")))?;
    manifest
        .validate()
        .map_err(|error| invalid_data(format!("invalid recipe catalog manifest: {error}")))?;
    let producer: String = row
        .try_get("producer")
        .map_err(|error| backend("decoding recipe catalog producer", &error))?;
    let source_locator: String = row
        .try_get("source_locator")
        .map_err(|error| backend("decoding recipe catalog source", &error))?;
    let source_revision: String = row
        .try_get("source_revision")
        .map_err(|error| backend("decoding recipe catalog source revision", &error))?;
    let schema_version: i64 = row
        .try_get("schema_version")
        .map_err(|error| backend("decoding recipe catalog schema version", &error))?;
    let recipe_count: i64 = row
        .try_get("recipe_count")
        .map_err(|error| backend("decoding recipe catalog recipe count", &error))?;
    let diagnostic_count: i64 = row
        .try_get("diagnostic_count")
        .map_err(|error| backend("decoding recipe catalog diagnostic count", &error))?;
    if u32::try_from(schema_version).ok() != Some(manifest.schema_version)
        || producer != manifest.producer
        || source_locator != manifest.source.locator
        || source_revision != manifest.source.revision
        || usize::try_from(recipe_count).ok() != Some(manifest.recipes.len())
        || usize::try_from(diagnostic_count).ok() != Some(manifest.diagnostics.len())
    {
        return Err(invalid_data(
            "recipe catalog index fields differ from manifest",
        ));
    }
    let snapshot = RecipeCatalogSnapshotRecord {
        id: parse_value(
            row.try_get("id")
                .map_err(|error| backend("decoding recipe catalog snapshot ID", &error))?,
            "recipe catalog snapshot ID",
        )?,
        worker_id: parse_value(
            row.try_get("worker_id")
                .map_err(|error| backend("decoding recipe catalog worker ID", &error))?,
            "worker ID",
        )?,
        manifest_digest: parse_value(
            row.try_get("manifest_digest")
                .map_err(|error| backend("decoding recipe catalog digest", &error))?,
            "recipe catalog digest",
        )?,
        manifest,
        observed_at: row
            .try_get("observed_at")
            .map_err(|error| backend("decoding recipe catalog observation time", &error))?,
    };
    if snapshot
        .manifest
        .canonical_digest()
        .map_err(|error| invalid_data(format!("hashing recipe catalog manifest: {error}")))?
        != snapshot.manifest_digest
    {
        return Err(invalid_data(
            "recipe catalog digest does not match manifest",
        ));
    }
    Ok(snapshot)
}

fn recipe_catalog_summary_from_row(
    row: &SqliteRow,
) -> Result<RecipeCatalogSnapshotSummaryRecord, StorageError> {
    let recipe_count: i64 = row
        .try_get("recipe_count")
        .map_err(|error| backend("decoding recipe catalog recipe count", &error))?;
    let diagnostic_count: i64 = row
        .try_get("diagnostic_count")
        .map_err(|error| backend("decoding recipe catalog diagnostic count", &error))?;
    Ok(RecipeCatalogSnapshotSummaryRecord {
        id: parse_value(
            row.try_get("id")
                .map_err(|error| backend("decoding recipe catalog snapshot ID", &error))?,
            "recipe catalog snapshot ID",
        )?,
        worker_id: parse_value(
            row.try_get("worker_id")
                .map_err(|error| backend("decoding recipe catalog worker ID", &error))?,
            "worker ID",
        )?,
        producer: row
            .try_get("producer")
            .map_err(|error| backend("decoding recipe catalog producer", &error))?,
        source_locator: row
            .try_get("source_locator")
            .map_err(|error| backend("decoding recipe catalog source", &error))?,
        source_revision: row
            .try_get("source_revision")
            .map_err(|error| backend("decoding recipe catalog source revision", &error))?,
        manifest_digest: parse_value(
            row.try_get("manifest_digest")
                .map_err(|error| backend("decoding recipe catalog digest", &error))?,
            "recipe catalog digest",
        )?,
        recipe_count: u32::try_from(recipe_count)
            .map_err(|_| invalid_data("invalid recipe catalog recipe count"))?,
        diagnostic_count: u32::try_from(diagnostic_count)
            .map_err(|_| invalid_data("invalid recipe catalog diagnostic count"))?,
        observed_at: row
            .try_get("observed_at")
            .map_err(|error| backend("decoding recipe catalog observation time", &error))?,
    })
}

fn recipe_catalog_scan_from_row(row: &SqliteRow) -> Result<RecipeCatalogScanRecord, StorageError> {
    let state: String = row
        .try_get("state")
        .map_err(|error| backend("decoding recipe catalog scan state", &error))?;
    let failure = row
        .try_get::<Option<String>, _>("failure_json")
        .map_err(|error| backend("decoding recipe catalog scan failure", &error))?
        .map(|value| {
            serde_json::from_str(&value)
                .map_err(|error| invalid_data(format!("invalid catalog scan failure: {error}")))
        })
        .transpose()?;
    Ok(RecipeCatalogScanRecord {
        id: parse_value(
            row.try_get("id")
                .map_err(|error| backend("decoding recipe catalog scan ID", &error))?,
            "recipe catalog scan ID",
        )?,
        job_id: parse_value(
            row.try_get("job_id")
                .map_err(|error| backend("decoding recipe catalog scan job ID", &error))?,
            "job ID",
        )?,
        producer: row
            .try_get("producer")
            .map_err(|error| backend("decoding recipe catalog scan producer", &error))?,
        source: RecipeCatalogSource {
            locator: row
                .try_get("source_locator")
                .map_err(|error| backend("decoding recipe catalog scan source", &error))?,
            revision: row
                .try_get("source_revision")
                .map_err(|error| backend("decoding recipe catalog scan source revision", &error))?,
        },
        state: job_state(&state)?,
        snapshot_id: row
            .try_get::<Option<String>, _>("snapshot_id")
            .map_err(|error| backend("decoding recipe catalog scan snapshot", &error))?
            .map(|value| parse_value(&value, "recipe catalog snapshot ID"))
            .transpose()?,
        failure,
        requested_at: row
            .try_get("requested_at")
            .map_err(|error| backend("decoding recipe catalog scan request time", &error))?,
        completed_at: row
            .try_get("completed_at")
            .map_err(|error| backend("decoding recipe catalog scan completion time", &error))?,
    })
}

fn recipe_catalog_scan_summary_from_row(
    row: &SqliteRow,
) -> Result<RecipeCatalogScanSummaryRecord, StorageError> {
    let scan = recipe_catalog_scan_from_row(row)?;
    Ok(RecipeCatalogScanSummaryRecord {
        id: scan.id,
        job_id: scan.job_id,
        producer: scan.producer,
        source: scan.source,
        state: scan.state,
        snapshot_id: scan.snapshot_id,
        requested_at: scan.requested_at,
        completed_at: scan.completed_at,
    })
}

fn validate_build_target(target: &BuildTargetRecord) -> Result<(), StorageError> {
    if target.name.trim() != target.name || !(1..=128).contains(&target.name.len()) {
        return Err(invalid_data("build target name must contain 1-128 bytes"));
    }
    if !target.parameters.is_object() {
        return Err(invalid_data(
            "build target parameters must be a JSON object",
        ));
    }
    if target.revision == 0 || target.updated_at < target.created_at {
        return Err(invalid_data(
            "build target revision or timestamps are invalid",
        ));
    }
    match (target.schedule, target.next_run_at) {
        (BuildTargetSchedule::Manual, None) => Ok(()),
        (BuildTargetSchedule::Interval { every_seconds }, Some(_))
            if (MIN_BUILD_INTERVAL_SECONDS..=MAX_BUILD_INTERVAL_SECONDS)
                .contains(&every_seconds) =>
        {
            Ok(())
        }
        _ => Err(invalid_data(
            "manual targets cannot have a cursor and interval targets require a bounded cursor",
        )),
    }
}

const fn build_target_schedule_fields(
    schedule: BuildTargetSchedule,
) -> (&'static str, Option<i64>) {
    match schedule {
        BuildTargetSchedule::Manual => ("manual", None),
        BuildTargetSchedule::Interval { every_seconds } => ("interval", Some(every_seconds as i64)),
    }
}

fn build_target_from_row(row: &SqliteRow) -> Result<BuildTargetRecord, StorageError> {
    let trigger_kind: String = row
        .try_get("trigger_kind")
        .map_err(|error| backend("decoding build target trigger", &error))?;
    let interval_seconds: Option<i64> = row
        .try_get("interval_seconds")
        .map_err(|error| backend("decoding build target interval", &error))?;
    let schedule = match (trigger_kind.as_str(), interval_seconds) {
        ("manual", None) => BuildTargetSchedule::Manual,
        ("interval", Some(seconds)) => BuildTargetSchedule::Interval {
            every_seconds: u32::try_from(seconds)
                .map_err(|_| invalid_data("invalid persisted build target interval"))?,
        },
        _ => return Err(invalid_data("invalid persisted build target schedule")),
    };
    let parameters: String = row
        .try_get("parameters_json")
        .map_err(|error| backend("decoding build target parameters", &error))?;
    let enabled: i64 = row
        .try_get("enabled")
        .map_err(|error| backend("decoding build target status", &error))?;
    let revision: i64 = row
        .try_get("revision")
        .map_err(|error| backend("decoding build target revision", &error))?;
    let target = BuildTargetRecord {
        id: parse_value(
            row.try_get("id")
                .map_err(|error| backend("decoding build target ID", &error))?,
            "build target ID",
        )?,
        name: row
            .try_get("name")
            .map_err(|error| backend("decoding build target name", &error))?,
        software_id: parse_value(
            row.try_get("software_id")
                .map_err(|error| backend("decoding build target software", &error))?,
            "software ID",
        )?,
        recipe_revision_id: parse_value(
            row.try_get("recipe_revision_id")
                .map_err(|error| backend("decoding build target recipe revision", &error))?,
            "recipe revision ID",
        )?,
        parameters: serde_json::from_str(&parameters)
            .map_err(|error| invalid_data(format!("invalid build target parameters: {error}")))?,
        schedule,
        enabled: enabled == 1,
        next_run_at: row
            .try_get("next_run_at")
            .map_err(|error| backend("decoding build target cursor", &error))?,
        created_at: row
            .try_get("created_at")
            .map_err(|error| backend("decoding build target creation time", &error))?,
        updated_at: row
            .try_get("updated_at")
            .map_err(|error| backend("decoding build target update time", &error))?,
        revision: u64::try_from(revision)
            .map_err(|_| invalid_data("negative build target revision"))?,
    };
    validate_build_target(&target)?;
    Ok(target)
}

async fn insert_build_target_revision(
    connection: &mut SqliteConnection,
    target: &BuildTargetRecord,
) -> Result<(), StorageError> {
    let parameters = serde_json::to_string(&target.parameters)
        .map_err(|error| invalid_data(format!("serializing build target parameters: {error}")))?;
    let revision = i64::try_from(target.revision)
        .map_err(|_| invalid_data("build target revision exceeds SQLite range"))?;
    let (trigger_kind, interval_seconds) = build_target_schedule_fields(target.schedule);
    query(
        "INSERT INTO build_target_revisions
         (target_id, revision, name, software_id, recipe_revision_id, parameters_json,
          trigger_kind, interval_seconds, enabled, next_run_at, changed_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(target.id.to_string())
    .bind(revision)
    .bind(&target.name)
    .bind(target.software_id.to_string())
    .bind(target.recipe_revision_id.to_string())
    .bind(parameters)
    .bind(trigger_kind)
    .bind(interval_seconds)
    .bind(i64::from(target.enabled))
    .bind(target.next_run_at)
    .bind(target.updated_at)
    .execute(&mut *connection)
    .await
    .map_err(map_write)?;
    Ok(())
}

fn validate_queued_run_and_job(run: &RunRecord, job: &Job) -> Result<(), StorageError> {
    if job.subject != (JobSubject::BuildRun { run_id: run.id })
        || run.state != RunState::Queued
        || job.state != JobState::Queued
    {
        return Err(invalid_data("run and job are not one matching queued unit"));
    }
    Ok(())
}

async fn insert_queued_run_and_job(
    connection: &mut SqliteConnection,
    run: &RunRecord,
    job: &Job,
) -> Result<(), StorageError> {
    validate_queued_run_and_job(run, job)?;
    let parameters = serde_json::to_string(&run.parameters)
        .map_err(|error| invalid_data(format!("serializing run parameters: {error}")))?;
    let required = serde_json::to_string(&job.required_capabilities)
        .map_err(|error| invalid_data(format!("serializing job capabilities: {error}")))?;
    let payload = serde_json::to_string(&job.payload)
        .map_err(|error| invalid_data(format!("serializing job payload: {error}")))?;
    let (subject_kind, subject_id) = job_subject_fields(job.subject);
    query(
        "INSERT INTO runs
         (id, recipe_revision_id, software_id, state, parameters_json, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(run.id.to_string())
    .bind(run.recipe_revision_id.to_string())
    .bind(run.software_id.to_string())
    .bind(run_state_name(run.state))
    .bind(parameters)
    .bind(run.created_at)
    .execute(&mut *connection)
    .await
    .map_err(map_write)?;
    query(
        "INSERT INTO jobs
         (id, subject_kind, subject_id, required_capabilities_json, payload_json, state,
          maximum_attempts, attempt_count, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(job.id.to_string())
    .bind(subject_kind)
    .bind(subject_id)
    .bind(required)
    .bind(payload)
    .bind(job_state_name(job.state))
    .bind(i64::from(job.maximum_attempts))
    .bind(i64::from(job.attempt_count))
    .bind(job.created_at)
    .execute(&mut *connection)
    .await
    .map_err(map_write)?;
    Ok(())
}

const fn run_state_name(value: RunState) -> &'static str {
    match value {
        RunState::Queued => "queued",
        RunState::Running => "running",
        RunState::Succeeded => "succeeded",
        RunState::Failed => "failed",
        RunState::Cancelled => "cancelled",
    }
}

fn run_state(value: &str) -> Result<RunState, StorageError> {
    match value {
        "queued" => Ok(RunState::Queued),
        "running" => Ok(RunState::Running),
        "succeeded" => Ok(RunState::Succeeded),
        "failed" => Ok(RunState::Failed),
        "cancelled" => Ok(RunState::Cancelled),
        _ => Err(invalid_data("invalid persisted run state")),
    }
}

fn run_from_row(row: &SqliteRow) -> Result<RunRecord, StorageError> {
    let state: String = row
        .try_get("state")
        .map_err(|error| backend("decoding run state", &error))?;
    let parameters: String = row
        .try_get("parameters_json")
        .map_err(|error| backend("decoding run parameters", &error))?;
    let result = row
        .try_get::<Option<String>, _>("result_json")
        .map_err(|error| backend("decoding run result", &error))?
        .map(|value| {
            serde_json::from_str(&value)
                .map_err(|error| invalid_data(format!("invalid run result: {error}")))
        })
        .transpose()?;
    Ok(RunRecord {
        id: parse_value(
            row.try_get("id")
                .map_err(|error| backend("decoding run ID", &error))?,
            "run ID",
        )?,
        recipe_revision_id: parse_value(
            row.try_get("recipe_revision_id")
                .map_err(|error| backend("decoding run recipe revision ID", &error))?,
            "recipe revision ID",
        )?,
        software_id: parse_value(
            row.try_get("software_id")
                .map_err(|error| backend("decoding run software ID", &error))?,
            "software ID",
        )?,
        state: run_state(&state)?,
        parameters: serde_json::from_str(&parameters)
            .map_err(|error| invalid_data(format!("invalid run parameters: {error}")))?,
        result,
        created_at: row
            .try_get("created_at")
            .map_err(|error| backend("decoding run creation time", &error))?,
        completed_at: row
            .try_get("completed_at")
            .map_err(|error| backend("decoding run completion time", &error))?,
    })
}

fn run_summary_from_row(row: &SqliteRow) -> Result<RunSummaryRecord, StorageError> {
    let state: String = row
        .try_get("state")
        .map_err(|error| backend("decoding run state", &error))?;
    Ok(RunSummaryRecord {
        id: parse_value(
            row.try_get("id")
                .map_err(|error| backend("decoding run ID", &error))?,
            "run ID",
        )?,
        recipe_revision_id: parse_value(
            row.try_get("recipe_revision_id")
                .map_err(|error| backend("decoding run recipe revision ID", &error))?,
            "recipe revision ID",
        )?,
        software_id: parse_value(
            row.try_get("software_id")
                .map_err(|error| backend("decoding run software ID", &error))?,
            "software ID",
        )?,
        state: run_state(&state)?,
        created_at: row
            .try_get("created_at")
            .map_err(|error| backend("decoding run creation time", &error))?,
        completed_at: row
            .try_get("completed_at")
            .map_err(|error| backend("decoding run completion time", &error))?,
    })
}

const fn run_log_stream_name(value: RunLogStream) -> &'static str {
    match value {
        RunLogStream::Stdout => "stdout",
        RunLogStream::Stderr => "stderr",
        RunLogStream::System => "system",
    }
}

fn run_log_stream(value: &str) -> Result<RunLogStream, StorageError> {
    match value {
        "stdout" => Ok(RunLogStream::Stdout),
        "stderr" => Ok(RunLogStream::Stderr),
        "system" => Ok(RunLogStream::System),
        _ => Err(invalid_data("invalid persisted run log stream")),
    }
}

fn run_log_from_row(row: &SqliteRow) -> Result<RunLogRecord, StorageError> {
    let sequence: i64 = row
        .try_get("sequence")
        .map_err(|error| backend("decoding run log sequence", &error))?;
    let stream: String = row
        .try_get("stream")
        .map_err(|error| backend("decoding run log stream", &error))?;
    Ok(RunLogRecord {
        run_id: parse_value(
            row.try_get("run_id")
                .map_err(|error| backend("decoding run log run ID", &error))?,
            "run ID",
        )?,
        attempt_id: parse_value(
            row.try_get("attempt_id")
                .map_err(|error| backend("decoding run log attempt ID", &error))?,
            "attempt ID",
        )?,
        sequence: u64::try_from(sequence).map_err(|_| invalid_data("negative run log sequence"))?,
        stream: run_log_stream(&stream)?,
        message: row
            .try_get("message")
            .map_err(|error| backend("decoding run log message", &error))?,
        occurred_at: row
            .try_get("occurred_at")
            .map_err(|error| backend("decoding run log receipt time", &error))?,
    })
}

fn location_state(value: &str) -> Result<LocationState, StorageError> {
    match value {
        "pending" => Ok(LocationState::Pending),
        "replicating" => Ok(LocationState::Replicating),
        "present" => Ok(LocationState::Present),
        "remote" => Ok(LocationState::Remote),
        "missing" => Ok(LocationState::Missing),
        "corrupt" => Ok(LocationState::Corrupt),
        "failed" => Ok(LocationState::Failed),
        _ => Err(invalid_data("invalid persisted location state")),
    }
}

const fn artifact_role_name(value: ArtifactRole) -> &'static str {
    match value {
        ArtifactRole::PrimaryInstaller => "primary_installer",
        ArtifactRole::Signature => "signature",
        ArtifactRole::Sbom => "sbom",
        ArtifactRole::DebugSymbols => "debug_symbols",
        ArtifactRole::Metadata => "metadata",
    }
}

fn artifact_role(value: &str) -> Result<ArtifactRole, StorageError> {
    match value {
        "primary_installer" => Ok(ArtifactRole::PrimaryInstaller),
        "signature" => Ok(ArtifactRole::Signature),
        "sbom" => Ok(ArtifactRole::Sbom),
        "debug_symbols" => Ok(ArtifactRole::DebugSymbols),
        "metadata" => Ok(ArtifactRole::Metadata),
        _ => Err(invalid_data("invalid persisted artifact role")),
    }
}

const fn platform_name(value: Platform) -> &'static str {
    match value {
        Platform::MacOs => "mac_os",
        Platform::Linux => "linux",
        Platform::Windows => "windows",
    }
}

fn platform(value: &str) -> Result<Platform, StorageError> {
    match value {
        "mac_os" => Ok(Platform::MacOs),
        "linux" => Ok(Platform::Linux),
        "windows" => Ok(Platform::Windows),
        _ => Err(invalid_data("invalid persisted platform")),
    }
}

const fn architecture_name(value: Architecture) -> &'static str {
    match value {
        Architecture::X86_64 => "x86_64",
        Architecture::Aarch64 => "aarch64",
        Architecture::Universal => "universal",
    }
}

fn architecture(value: &str) -> Result<Architecture, StorageError> {
    match value {
        "x86_64" => Ok(Architecture::X86_64),
        "aarch64" => Ok(Architecture::Aarch64),
        "universal" => Ok(Architecture::Universal),
        _ => Err(invalid_data("invalid persisted architecture")),
    }
}

const fn release_state_name(value: ReleaseState) -> &'static str {
    match value {
        ReleaseState::Discovered => "discovered",
        ReleaseState::Built => "built",
        ReleaseState::Inspected => "inspected",
        ReleaseState::Verified => "verified",
        ReleaseState::Candidate => "candidate",
        ReleaseState::Testing => "testing",
        ReleaseState::Stable => "stable",
        ReleaseState::Failed => "failed",
        ReleaseState::Rejected => "rejected",
    }
}

fn release_state(value: &str) -> Result<ReleaseState, StorageError> {
    match value {
        "discovered" => Ok(ReleaseState::Discovered),
        "built" => Ok(ReleaseState::Built),
        "inspected" => Ok(ReleaseState::Inspected),
        "verified" => Ok(ReleaseState::Verified),
        "candidate" => Ok(ReleaseState::Candidate),
        "testing" => Ok(ReleaseState::Testing),
        "stable" => Ok(ReleaseState::Stable),
        "failed" => Ok(ReleaseState::Failed),
        "rejected" => Ok(ReleaseState::Rejected),
        _ => Err(invalid_data("invalid persisted release state")),
    }
}

fn release_from_row(row: &SqliteRow) -> Result<Release, StorageError> {
    let state: String = row
        .try_get("state")
        .map_err(|error| backend("decoding release state", &error))?;
    let revision: i64 = row
        .try_get("revision")
        .map_err(|error| backend("decoding release revision", &error))?;
    Ok(Release {
        availability: serde_json::from_str(
            row.try_get::<&str, _>("availability_json")
                .map_err(|error| backend("decoding release availability", &error))?,
        )
        .map_err(|_| invalid_data("invalid release availability"))?,
        id: parse_value(
            row.try_get("id")
                .map_err(|error| backend("decoding release ID", &error))?,
            "release ID",
        )?,
        software_id: parse_value(
            row.try_get("software_id")
                .map_err(|error| backend("decoding release software ID", &error))?,
            "software ID",
        )?,
        version: parse_value(
            row.try_get("version")
                .map_err(|error| backend("decoding release version", &error))?,
            "release version",
        )?,
        state: release_state(&state)?,
        created_at: row
            .try_get("created_at")
            .map_err(|error| backend("decoding release creation time", &error))?,
        revision: u64::try_from(revision).map_err(|_| invalid_data("negative release revision"))?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ReleaseGraphArtifact {
    digest: String,
    role: String,
    size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ReleaseGraphVariant {
    platform: String,
    architecture: String,
    minimum_macos: Option<String>,
    maximum_macos: Option<String>,
    resolution_priority: i32,
    artifacts: Vec<ReleaseGraphArtifact>,
}

fn submitted_release_graph(result: &BuildResult) -> Vec<ReleaseGraphVariant> {
    let mut variants = result
        .variants
        .iter()
        .map(|variant| {
            let mut artifacts = variant
                .artifacts
                .iter()
                .map(|artifact| ReleaseGraphArtifact {
                    digest: artifact.digest.to_string(),
                    role: artifact_role_name(artifact.role).to_owned(),
                    size: artifact.size,
                })
                .collect::<Vec<_>>();
            artifacts.sort();
            ReleaseGraphVariant {
                platform: platform_name(variant.platform).to_owned(),
                architecture: architecture_name(variant.architecture).to_owned(),
                minimum_macos: variant.minimum_macos.as_ref().map(ToString::to_string),
                maximum_macos: variant.maximum_macos.as_ref().map(ToString::to_string),
                resolution_priority: variant.resolution_priority,
                artifacts,
            }
        })
        .collect::<Vec<_>>();
    variants.sort();
    variants
}

async fn persisted_release_graph(
    connection: &mut SqliteConnection,
    release_id: ReleaseId,
) -> Result<Vec<ReleaseGraphVariant>, StorageError> {
    let rows = query(
        "SELECT id, platform, architecture, minimum_macos, maximum_macos, resolution_priority
         FROM variants WHERE release_id = ?",
    )
    .bind(release_id.to_string())
    .fetch_all(&mut *connection)
    .await
    .map_err(|error| backend("loading same-version release variants", &error))?;
    let mut variants = BTreeMap::new();
    for row in rows {
        let id: String = row
            .try_get("id")
            .map_err(|error| backend("decoding same-version variant ID", &error))?;
        variants.insert(
            id,
            ReleaseGraphVariant {
                platform: row
                    .try_get("platform")
                    .map_err(|error| backend("decoding same-version platform", &error))?,
                architecture: row
                    .try_get("architecture")
                    .map_err(|error| backend("decoding same-version architecture", &error))?,
                minimum_macos: row
                    .try_get("minimum_macos")
                    .map_err(|error| backend("decoding same-version minimum macOS", &error))?,
                maximum_macos: row
                    .try_get("maximum_macos")
                    .map_err(|error| backend("decoding same-version maximum macOS", &error))?,
                resolution_priority: row
                    .try_get("resolution_priority")
                    .map_err(|error| backend("decoding same-version priority", &error))?,
                artifacts: Vec::new(),
            },
        );
    }
    let rows = query(
        "SELECT va.variant_id, va.digest, va.role, a.size
         FROM variant_artifacts va
         JOIN variants v ON v.id = va.variant_id
         JOIN artifacts a ON a.digest = va.digest
         WHERE v.release_id = ?",
    )
    .bind(release_id.to_string())
    .fetch_all(&mut *connection)
    .await
    .map_err(|error| backend("loading same-version release artifacts", &error))?;
    for row in rows {
        let variant_id: String = row
            .try_get("variant_id")
            .map_err(|error| backend("decoding same-version artifact variant", &error))?;
        let size: i64 = row
            .try_get("size")
            .map_err(|error| backend("decoding same-version artifact size", &error))?;
        variants
            .get_mut(&variant_id)
            .ok_or_else(|| invalid_data("same-version artifact has no persisted variant"))?
            .artifacts
            .push(ReleaseGraphArtifact {
                digest: row
                    .try_get("digest")
                    .map_err(|error| backend("decoding same-version artifact digest", &error))?,
                role: row
                    .try_get("role")
                    .map_err(|error| backend("decoding same-version artifact role", &error))?,
                size: u64::try_from(size)
                    .map_err(|_| invalid_data("negative same-version artifact size"))?,
            });
    }
    let mut variants = variants.into_values().collect::<Vec<_>>();
    for variant in &mut variants {
        variant.artifacts.sort();
    }
    variants.sort();
    Ok(variants)
}

async fn same_release_evidence(
    connection: &mut SqliteConnection,
    release_id: ReleaseId,
    result: &BuildResult,
) -> Result<bool, StorageError> {
    if persisted_release_graph(connection, release_id).await? != submitted_release_graph(result) {
        return Ok(false);
    }
    let baseline_run: Option<String> = query_scalar(
        "SELECT run_id FROM run_releases
         WHERE release_id = ? AND disposition = 'release_created'",
    )
    .bind(release_id.to_string())
    .fetch_optional(&mut *connection)
    .await
    .map_err(|error| backend("loading same-version creating run", &error))?;
    let Some(baseline_run) = baseline_run else {
        return Ok(false);
    };
    let provenance: Option<String> =
        query_scalar("SELECT provenance_json FROM run_provenance WHERE run_id = ?")
            .bind(&baseline_run)
            .fetch_optional(&mut *connection)
            .await
            .map_err(|error| backend("loading same-version provenance", &error))?;
    let Some(provenance) = provenance else {
        return Ok(false);
    };
    let mut persisted_provenance: serde_json::Value = serde_json::from_str(&provenance)
        .map_err(|error| invalid_data(format!("invalid persisted build provenance: {error}")))?;
    let mut submitted_provenance = serde_json::to_value(&result.provenance)
        .map_err(|error| invalid_data(format!("serializing build provenance: {error}")))?;
    for evidence in [&mut persisted_provenance, &mut submitted_provenance] {
        evidence
            .as_object_mut()
            .ok_or_else(|| invalid_data("build provenance must serialize as an object"))?
            .remove("captured_at");
    }
    if persisted_provenance != submitted_provenance {
        return Ok(false);
    }
    let rows = query(
        "SELECT check_name, required, succeeded, detail
         FROM verification_results WHERE run_id = ?",
    )
    .bind(&baseline_run)
    .fetch_all(&mut *connection)
    .await
    .map_err(|error| backend("loading same-version verification evidence", &error))?;
    let mut persisted_checks = BTreeMap::new();
    for row in rows {
        let name: String = row
            .try_get("check_name")
            .map_err(|error| backend("decoding same-version verification name", &error))?;
        let required: i64 = row
            .try_get("required")
            .map_err(|error| backend("decoding same-version verification policy", &error))?;
        let succeeded: i64 = row
            .try_get("succeeded")
            .map_err(|error| backend("decoding same-version verification result", &error))?;
        let detail: Option<String> = row
            .try_get("detail")
            .map_err(|error| backend("decoding same-version verification detail", &error))?;
        persisted_checks.insert(name, (required == 1, succeeded == 1, detail));
    }
    let submitted_checks = result
        .verification_results
        .iter()
        .map(|check| {
            (
                check.check.clone(),
                (check.required, check.succeeded, check.detail.clone()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    Ok(persisted_checks == submitted_checks)
}

const fn build_disposition_name(disposition: BuildDisposition) -> &'static str {
    match disposition {
        BuildDisposition::ReleaseCreated => "release_created",
        BuildDisposition::NoChange => "no_change",
        BuildDisposition::EvidenceChanged => "evidence_changed",
        BuildDisposition::VerificationFailed => "verification_failed",
        BuildDisposition::VersionContentConflict => "version_content_conflict",
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // One helper atomically closes all build-owned records.
async fn persist_build_terminal(
    connection: &mut SqliteConnection,
    lease: &Lease,
    run_id: &str,
    scope: &str,
    idempotency_key: &str,
    execution: &BuilderExecutionResult,
    release: &Release,
    disposition: BuildDisposition,
    audit: &AuditEvent,
    now: DateTime<Utc>,
) -> Result<BuildCompletion, StorageError> {
    let result = execution
        .build_result
        .as_ref()
        .ok_or_else(|| invalid_data("build terminal result requires selected output"))?;
    let provenance = serde_json::to_string(&result.provenance)
        .map_err(|error| invalid_data(format!("serializing build provenance: {error}")))?;
    query("INSERT INTO run_provenance (run_id, provenance_json) VALUES (?, ?)")
        .bind(run_id)
        .bind(provenance)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
    for check in &result.verification_results {
        query(
            "INSERT INTO verification_results
             (run_id, check_name, required, succeeded, detail) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(run_id)
        .bind(&check.check)
        .bind(i64::from(check.required))
        .bind(i64::from(check.succeeded))
        .bind(&check.detail)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
    }
    query(
        "INSERT INTO run_releases (run_id, release_id, disposition, created_at)
         VALUES (?, ?, ?, ?)",
    )
    .bind(run_id)
    .bind(release.id.to_string())
    .bind(build_disposition_name(disposition))
    .bind(now)
    .execute(&mut *connection)
    .await
    .map_err(map_write)?;

    let mut result_json = serde_json::to_value(execution)
        .map_err(|error| invalid_data(format!("serializing build result: {error}")))?;
    result_json
        .as_object_mut()
        .ok_or_else(|| invalid_data("builder execution result must serialize as an object"))?
        .insert(
            "publication".to_owned(),
            serde_json::json!({
                "disposition": build_disposition_name(disposition),
                "release_id": release.id,
            }),
        );
    let result_json = serde_json::to_string(&result_json)
        .map_err(|error| invalid_data(format!("serializing enriched build result: {error}")))?;
    let completion = BuildCompletion {
        outcome: CompletionOutcome::Completed,
        disposition,
        release: release.clone(),
    };
    let replay_json = serde_json::to_string(&completion)
        .map_err(|error| invalid_data(format!("serializing build completion: {error}")))?;
    query(
        "INSERT INTO idempotency_keys (scope, key, response_json, created_at)
         VALUES (?, ?, ?, ?)",
    )
    .bind(scope)
    .bind(idempotency_key)
    .bind(replay_json)
    .bind(now)
    .execute(&mut *connection)
    .await
    .map_err(map_write)?;
    let terminal_state = if matches!(
        disposition,
        BuildDisposition::VersionContentConflict | BuildDisposition::VerificationFailed
    ) || !result.passes_candidate_gate()
    {
        "failed"
    } else {
        "succeeded"
    };
    query("UPDATE attempts SET state = ?, completed_at = ?, result_json = ? WHERE id = ?")
        .bind(terminal_state)
        .bind(now)
        .bind(&result_json)
        .bind(lease.attempt_id.to_string())
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
    query("UPDATE jobs SET state = ?, completed_at = ?, result_json = ? WHERE id = ?")
        .bind(terminal_state)
        .bind(now)
        .bind(&result_json)
        .bind(lease.job_id.to_string())
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
    query("UPDATE runs SET state = ?, completed_at = ?, result_json = ? WHERE id = ?")
        .bind(terminal_state)
        .bind(now)
        .bind(&result_json)
        .bind(run_id)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
    insert_audit(connection, audit).await?;
    Ok(completion)
}

fn variant_from_row(row: &SqliteRow) -> Result<Variant, StorageError> {
    let platform_value: String = row
        .try_get("platform")
        .map_err(|error| backend("decoding variant platform", &error))?;
    let architecture_value: String = row
        .try_get("architecture")
        .map_err(|error| backend("decoding variant architecture", &error))?;
    let minimum_macos = row
        .try_get::<Option<String>, _>("minimum_macos")
        .map_err(|error| backend("decoding minimum macOS version", &error))?
        .map(|value| parse_value::<MacOsVersion>(&value, "minimum macOS version"))
        .transpose()?;
    let maximum_macos = row
        .try_get::<Option<String>, _>("maximum_macos")
        .map_err(|error| backend("decoding maximum macOS version", &error))?
        .map(|value| parse_value::<MacOsVersion>(&value, "maximum macOS version"))
        .transpose()?;
    Ok(Variant {
        id: parse_value(
            row.try_get("id")
                .map_err(|error| backend("decoding variant ID", &error))?,
            "variant ID",
        )?,
        release_id: parse_value(
            row.try_get("release_id")
                .map_err(|error| backend("decoding variant release ID", &error))?,
            "release ID",
        )?,
        compatibility: Compatibility {
            platform: platform(&platform_value)?,
            architecture: architecture(&architecture_value)?,
            minimum_macos,
            maximum_macos,
        },
        resolution_priority: row
            .try_get("resolution_priority")
            .map_err(|error| backend("decoding variant resolution priority", &error))?,
    })
}

fn channel_from_row(row: &SqliteRow) -> Result<ChannelRecord, StorageError> {
    let revision: i64 = row
        .try_get("revision")
        .map_err(|error| backend("decoding channel revision", &error))?;
    Ok(ChannelRecord {
        software_id: parse_value(
            row.try_get("software_id")
                .map_err(|error| backend("decoding channel software ID", &error))?,
            "software ID",
        )?,
        name: row
            .try_get("name")
            .map_err(|error| backend("decoding channel name", &error))?,
        release_id: parse_value(
            row.try_get("release_id")
                .map_err(|error| backend("decoding channel release ID", &error))?,
            "release ID",
        )?,
        pinned_variant_id: row
            .try_get::<Option<String>, _>("pinned_variant_id")
            .map_err(|error| backend("decoding pinned variant ID", &error))?
            .map(|value| parse_value(&value, "variant ID"))
            .transpose()?,
        revision: u64::try_from(revision).map_err(|_| invalid_data("negative channel revision"))?,
    })
}

fn store_from_row(row: &SqliteRow) -> Result<StoreRecord, StorageError> {
    let role: String = row
        .try_get("role")
        .map_err(|error| backend("decoding store role", &error))?;
    let enabled: i64 = row
        .try_get("enabled")
        .map_err(|error| backend("decoding store status", &error))?;
    let revision: i64 = row
        .try_get("revision")
        .map_err(|error| backend("decoding store revision", &error))?;
    Ok(StoreRecord {
        id: parse_value(
            row.try_get("id")
                .map_err(|error| backend("decoding store ID", &error))?,
            "store ID",
        )?,
        name: row
            .try_get("name")
            .map_err(|error| backend("decoding store name", &error))?,
        role: store_role(&role)?,
        kind: row
            .try_get("kind")
            .map_err(|error| backend("decoding store kind", &error))?,
        enabled: enabled != 0,
        revision: u64::try_from(revision).map_err(|_| invalid_data("negative store revision"))?,
    })
}

const fn location_state_name(value: LocationState) -> &'static str {
    match value {
        LocationState::Pending => "pending",
        LocationState::Replicating => "replicating",
        LocationState::Present => "present",
        LocationState::Remote => "remote",
        LocationState::Missing => "missing",
        LocationState::Corrupt => "corrupt",
        LocationState::Failed => "failed",
    }
}

fn store_role(value: &str) -> Result<StoreRole, StorageError> {
    match value {
        "primary" => Ok(StoreRole::Primary),
        "replica" => Ok(StoreRole::Replica),
        "cache" => Ok(StoreRole::Cache),
        "read_only" => Ok(StoreRole::ReadOnly),
        _ => Err(invalid_data("invalid persisted store role")),
    }
}

const fn principal_kind_name(value: PrincipalKind) -> &'static str {
    match value {
        PrincipalKind::Human => "human",
        PrincipalKind::Service => "service",
        PrincipalKind::Worker => "worker",
    }
}

fn principal_kind(value: &str) -> Result<PrincipalKind, StorageError> {
    match value {
        "human" => Ok(PrincipalKind::Human),
        "service" => Ok(PrincipalKind::Service),
        "worker" => Ok(PrincipalKind::Worker),
        _ => Err(invalid_data("invalid persisted principal kind")),
    }
}

const fn job_state_name(value: JobState) -> &'static str {
    match value {
        JobState::Queued => "queued",
        JobState::Leased => "leased",
        JobState::Succeeded => "succeeded",
        JobState::Failed => "failed",
        JobState::Cancelled => "cancelled",
    }
}

fn job_state(value: &str) -> Result<JobState, StorageError> {
    match value {
        "queued" => Ok(JobState::Queued),
        "leased" => Ok(JobState::Leased),
        "succeeded" => Ok(JobState::Succeeded),
        "failed" => Ok(JobState::Failed),
        "cancelled" => Ok(JobState::Cancelled),
        _ => Err(invalid_data("invalid persisted job state")),
    }
}

fn job_subject_fields(subject: JobSubject) -> (&'static str, String) {
    match subject {
        JobSubject::BuildRun { run_id } => ("build_run", run_id.to_string()),
        JobSubject::RecipeCatalogScan { scan_id } => ("recipe_catalog_scan", scan_id.to_string()),
    }
}

fn job_subject(kind: &str, identity: &str) -> Result<JobSubject, StorageError> {
    match kind {
        "build_run" => Ok(JobSubject::BuildRun {
            run_id: parse_value(identity, "run ID")?,
        }),
        "recipe_catalog_scan" => Ok(JobSubject::RecipeCatalogScan {
            scan_id: parse_value(identity, "recipe catalog scan ID")?,
        }),
        _ => Err(invalid_data("invalid persisted job subject kind")),
    }
}

async fn load_principal(
    connection: &mut SqliteConnection,
    id: PrincipalId,
) -> Result<Option<Principal>, StorageError> {
    let row = query("SELECT id, name, kind, enabled FROM principals WHERE id = ?")
        .bind(id.to_string())
        .fetch_optional(&mut *connection)
        .await
        .map_err(|error| backend("loading principal", &error))?;
    let Some(row) = row else {
        return Ok(None);
    };
    let role_rows =
        query("SELECT role_name FROM principal_roles WHERE principal_id = ? ORDER BY role_name")
            .bind(id.to_string())
            .fetch_all(&mut *connection)
            .await
            .map_err(|error| backend("loading principal roles", &error))?;
    let roles = role_rows
        .into_iter()
        .map(|role| {
            let value: String = role
                .try_get("role_name")
                .map_err(|error| backend("decoding principal role", &error))?;
            RoleName::new(value).map_err(|_| invalid_data("invalid persisted role name"))
        })
        .collect::<Result<_, _>>()?;
    let kind: String = row
        .try_get("kind")
        .map_err(|error| backend("decoding principal kind", &error))?;
    let enabled: i64 = row
        .try_get("enabled")
        .map_err(|error| backend("decoding principal status", &error))?;
    Ok(Some(Principal {
        id,
        name: row
            .try_get("name")
            .map_err(|error| backend("decoding principal name", &error))?,
        kind: principal_kind(&kind)?,
        roles,
        enabled: enabled != 0,
    }))
}

async fn load_principal_record(
    connection: &mut SqliteConnection,
    identity: &str,
) -> Result<Option<PrincipalRecord>, StorageError> {
    let row = query(
        "SELECT id, created_at, revision FROM principals
         WHERE id = ? OR name = ? COLLATE NOCASE",
    )
    .bind(identity)
    .bind(identity)
    .fetch_optional(&mut *connection)
    .await
    .map_err(|error| backend("loading principal record", &error))?;
    let Some(row) = row else {
        return Ok(None);
    };
    let id: String = row
        .try_get("id")
        .map_err(|error| backend("decoding principal record ID", &error))?;
    let revision: i64 = row
        .try_get("revision")
        .map_err(|error| backend("decoding principal revision", &error))?;
    let principal = load_principal(connection, parse_value(&id, "principal ID")?)
        .await?
        .ok_or_else(|| invalid_data("principal record disappeared while loading"))?;
    Ok(Some(PrincipalRecord {
        principal,
        created_at: row
            .try_get("created_at")
            .map_err(|error| backend("decoding principal creation time", &error))?,
        revision: u64::try_from(revision)
            .map_err(|_| invalid_data("negative principal revision"))?,
    }))
}

fn token_from_row(row: &SqliteRow) -> Result<ApiTokenRecord, StorageError> {
    Ok(ApiTokenRecord {
        id: row
            .try_get("id")
            .map_err(|error| backend("decoding API token ID", &error))?,
        principal_id: parse_value(
            row.try_get("principal_id")
                .map_err(|error| backend("decoding API token principal ID", &error))?,
            "principal ID",
        )?,
        name: row
            .try_get("name")
            .map_err(|error| backend("decoding API token name", &error))?,
        created_at: row
            .try_get("created_at")
            .map_err(|error| backend("decoding API token creation time", &error))?,
        expires_at: row
            .try_get("expires_at")
            .map_err(|error| backend("decoding API token expiry", &error))?,
        revoked_at: row
            .try_get("revoked_at")
            .map_err(|error| backend("decoding API token revocation time", &error))?,
    })
}

async fn insert_audit(
    connection: &mut SqliteConnection,
    event: &AuditEvent,
) -> Result<(), StorageError> {
    let actor = serde_json::to_string(&event.actor)
        .map_err(|error| invalid_data(format!("serializing audit actor: {error}")))?;
    let details = serde_json::to_string(&event.details)
        .map_err(|error| invalid_data(format!("serializing audit details: {error}")))?;
    query(
        "INSERT INTO audit_events
         (id, actor_json, action, resource_kind, resource_id, details_json, request_id, occurred_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(event.id.to_string())
    .bind(actor)
    .bind(&event.action)
    .bind(&event.resource_kind)
    .bind(&event.resource_id)
    .bind(details)
    .bind(&event.request_id)
    .bind(event.occurred_at)
    .execute(connection)
    .await
    .map_err(map_write)?;
    Ok(())
}

/// Adapter-private transaction owning one pooled connection.
struct SqliteStorageTransaction {
    connection: Option<PoolConnection<Sqlite>>,
    active: bool,
}

impl SqliteStorageTransaction {
    fn connection(&mut self) -> Result<&mut SqliteConnection, StorageError> {
        self.connection
            .as_deref_mut()
            .ok_or_else(|| invalid_data("transaction is already closed"))
    }
}

impl Drop for SqliteStorageTransaction {
    fn drop(&mut self) {
        if self.active
            && let Some(connection) = self.connection.as_deref_mut()
        {
            SqliteTransactionManager::start_rollback(connection);
        }
    }
}

#[async_trait]
impl StorageTransaction for SqliteStorageTransaction {
    async fn create_software(&mut self, software: &Software) -> Result<(), StorageError> {
        let revision = i64::try_from(software.revision)
            .map_err(|_| invalid_data("software revision exceeds SQLite range"))?;
        query("INSERT INTO software (id, slug, name, created_at, revision) VALUES (?, ?, ?, ?, ?)")
            .bind(software.id.to_string())
            .bind(software.slug.as_str())
            .bind(&software.name)
            .bind(software.created_at)
            .bind(revision)
            .execute(self.connection()?)
            .await
            .map_err(map_write)?;
        Ok(())
    }

    async fn set_software_installation(
        &mut self,
        installation: &SoftwareInstallation,
    ) -> Result<(), StorageError> {
        let install = serde_json::to_string(&installation.install)
            .map_err(|error| invalid_data(format!("serializing installation metadata: {error}")))?;
        let detection = serde_json::to_string(&installation.detection)
            .map_err(|error| invalid_data(format!("serializing detection metadata: {error}")))?;
        query(
            "INSERT INTO software_installation (software_id, install_json, detection_json)
             VALUES (?, ?, ?)
             ON CONFLICT(software_id) DO UPDATE SET
               install_json = excluded.install_json, detection_json = excluded.detection_json",
        )
        .bind(installation.software_id.to_string())
        .bind(install)
        .bind(detection)
        .execute(self.connection()?)
        .await
        .map_err(map_write)?;
        Ok(())
    }

    async fn record_artifact_location(
        &mut self,
        artifact: &Artifact,
        location: &ArtifactLocation,
    ) -> Result<(), StorageError> {
        if location.digest != artifact.digest {
            return Err(invalid_data("artifact and location digests differ"));
        }
        let size = i64::try_from(artifact.size)
            .map_err(|_| invalid_data("artifact size exceeds SQLite range"))?;
        query(
            "INSERT INTO artifacts (digest, size, media_type, created_at) VALUES (?, ?, ?, ?)
             ON CONFLICT(digest) DO NOTHING",
        )
        .bind(artifact.digest.as_str())
        .bind(size)
        .bind(&artifact.media_type)
        .bind(artifact.created_at)
        .execute(&mut *self.connection()?)
        .await
        .map_err(map_write)?;
        let persisted = query("SELECT size, media_type FROM artifacts WHERE digest = ?")
            .bind(artifact.digest.as_str())
            .fetch_one(&mut *self.connection()?)
            .await
            .map_err(|error| backend("verifying artifact metadata", &error))?;
        let persisted_size: i64 = persisted
            .try_get("size")
            .map_err(|error| backend("decoding artifact size", &error))?;
        let persisted_media_type: String = persisted
            .try_get("media_type")
            .map_err(|error| backend("decoding artifact media type", &error))?;
        if persisted_size != size || persisted_media_type != artifact.media_type {
            return Err(StorageError::Conflict);
        }
        query(
            "INSERT INTO artifact_locations
             (id, digest, store_id, state, verified_at, last_error) VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(digest, store_id) DO UPDATE SET
               state = excluded.state, verified_at = excluded.verified_at,
               last_error = excluded.last_error",
        )
        .bind(location.id.to_string())
        .bind(location.digest.as_str())
        .bind(location.store_id.to_string())
        .bind(location_state_name(location.state))
        .bind(location.verified_at)
        .bind(&location.last_error)
        .execute(self.connection()?)
        .await
        .map_err(map_write)?;
        Ok(())
    }

    async fn append_audit(&mut self, event: &AuditEvent) -> Result<(), StorageError> {
        insert_audit(self.connection()?, event).await
    }

    async fn commit(mut self: Box<Self>) -> Result<(), StorageError> {
        query("COMMIT")
            .execute(self.connection()?)
            .await
            .map_err(|error| backend("committing SQLite transaction", &error))?;
        self.active = false;
        Ok(())
    }

    async fn rollback(mut self: Box<Self>) -> Result<(), StorageError> {
        query("ROLLBACK")
            .execute(self.connection()?)
            .await
            .map_err(|error| backend("rolling back SQLite transaction", &error))?;
        self.active = false;
        Ok(())
    }
}

#[async_trait]
impl TransactionalStorage for SqliteStorage {
    async fn begin(&self) -> Result<Box<dyn StorageTransaction>, StorageError> {
        Ok(Box::new(SqliteStorageTransaction {
            connection: Some(self.acquire_write().await?.into_connection()),
            active: true,
        }))
    }
}

fn validate_bootstrap_username(username: &str) -> Result<(), StorageError> {
    if username.trim() != username || !(1..=128).contains(&username.len()) {
        return Err(invalid_data(
            "username must contain 1-128 non-padding bytes",
        ));
    }
    Ok(())
}

async fn insert_bootstrap_admin(
    connection: &mut ImmediateWrite,
    username: &str,
    password_hash: &PasswordHash,
    actor: AuditActor,
    now: DateTime<Utc>,
) -> Result<Principal, StorageError> {
    for role in Role::built_ins().into_values() {
        query("INSERT OR IGNORE INTO roles (name, built_in) VALUES (?, 1)")
            .bind(role.name.as_str())
            .execute(&mut **connection)
            .await
            .map_err(map_write)?;
        for permission in role.permissions {
            let value = serde_json::to_value(permission)
                .map_err(|error| invalid_data(format!("serializing permission: {error}")))?;
            let name = value
                .as_str()
                .ok_or_else(|| invalid_data("permission did not serialize as text"))?;
            query("INSERT OR IGNORE INTO role_permissions (role_name, permission) VALUES (?, ?)")
                .bind(role.name.as_str())
                .bind(name)
                .execute(&mut **connection)
                .await
                .map_err(map_write)?;
        }
    }

    let principal = Principal {
        id: PrincipalId::new(),
        name: username.to_owned(),
        kind: PrincipalKind::Human,
        roles: [RoleName::new("admin").expect("built-in role name is valid")].into(),
        enabled: true,
    };
    query(
        "INSERT INTO principals (id, name, kind, password_hash, enabled, created_at)
             VALUES (?, ?, ?, ?, 1, ?)",
    )
    .bind(principal.id.to_string())
    .bind(&principal.name)
    .bind(principal_kind_name(principal.kind))
    .bind(password_hash.expose_for_persistence())
    .bind(now)
    .execute(&mut **connection)
    .await
    .map_err(map_write)?;
    query("INSERT INTO principal_roles (principal_id, role_name) VALUES (?, 'admin')")
        .bind(principal.id.to_string())
        .execute(&mut **connection)
        .await
        .map_err(map_write)?;
    query("DELETE FROM settings WHERE key = 'bootstrap_secret_hash'")
        .execute(&mut **connection)
        .await
        .map_err(map_write)?;
    query(
        "INSERT INTO settings (key, value) VALUES ('bootstrap_disabled', 'true')
             ON CONFLICT(key) DO UPDATE SET value = 'true'",
    )
    .execute(&mut **connection)
    .await
    .map_err(map_write)?;
    insert_audit(
        connection,
        &AuditEvent {
            id: AuditEventId::new(),
            actor,
            action: "auth.bootstrap".into(),
            resource_kind: "principal".into(),
            resource_id: Some(principal.id.to_string()),
            details: serde_json::json!({"username": username}),
            request_id: None,
            occurred_at: now,
        },
    )
    .await?;
    Ok(principal)
}

async fn terminalize_exhausted_catalog_scans(
    connection: &mut SqliteConnection,
    now: DateTime<Utc>,
) -> Result<(), StorageError> {
    let rows = query(
        "SELECT s.id, s.producer FROM recipe_catalog_scans s
         JOIN jobs j ON j.id = s.job_id
         LEFT JOIN recipe_catalog_scan_terminals t ON t.scan_id = s.id
         WHERE j.subject_kind = 'recipe_catalog_scan' AND j.state = 'failed'
           AND t.scan_id IS NULL",
    )
    .fetch_all(&mut *connection)
    .await
    .map_err(|error| backend("loading exhausted catalog scans", &error))?;
    for row in rows {
        let scan_id: RecipeCatalogScanId = parse_value(
            row.try_get("id")
                .map_err(|error| backend("decoding exhausted catalog scan ID", &error))?,
            "recipe catalog scan ID",
        )?;
        let producer: String = row
            .try_get("producer")
            .map_err(|error| backend("decoding exhausted catalog scan producer", &error))?;
        let failure = RecipeCatalogScanExecutionFailure {
            schema_version: RecipeCatalogScanJob::SCHEMA_VERSION,
            scan_id,
            producer: Some(producer),
            code: "attempts_exhausted".to_owned(),
            detail: "Catalog scan attempts were exhausted without a terminal worker result."
                .to_owned(),
            failed_at: now,
        };
        let failure_json = serde_json::to_string(&failure).map_err(|error| {
            invalid_data(format!("serializing exhausted catalog scan: {error}"))
        })?;
        query(
            "INSERT INTO recipe_catalog_scan_terminals
             (scan_id, snapshot_id, failure_json, completed_at) VALUES (?, NULL, ?, ?)",
        )
        .bind(scan_id.to_string())
        .bind(&failure_json)
        .bind(now)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        query(
            "UPDATE jobs SET result_json = ?
             WHERE subject_kind = 'recipe_catalog_scan' AND subject_id = ?",
        )
        .bind(&failure_json)
        .bind(scan_id.to_string())
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
    }
    Ok(())
}

async fn invalidate_worker_leases(
    connection: &mut SqliteConnection,
    worker_id: WorkerId,
    now: DateTime<Utc>,
) -> Result<(), StorageError> {
    query(
        "UPDATE attempts SET state = 'expired', completed_at = ?
         WHERE worker_id = ? AND state = 'leased'",
    )
    .bind(now)
    .bind(worker_id.to_string())
    .execute(&mut *connection)
    .await
    .map_err(map_write)?;
    query(
        "UPDATE jobs SET
           state = CASE WHEN attempt_count >= maximum_attempts THEN 'failed' ELSE 'queued' END,
           completed_at = CASE WHEN attempt_count >= maximum_attempts THEN ? ELSE NULL END
         WHERE state = 'leased' AND NOT EXISTS (
           SELECT 1 FROM attempts a WHERE a.job_id = jobs.id AND a.state = 'leased'
         )",
    )
    .bind(now)
    .execute(&mut *connection)
    .await
    .map_err(map_write)?;
    terminalize_exhausted_catalog_scans(connection, now).await?;
    query(
        "UPDATE runs SET state = 'failed', completed_at = ?
         WHERE state IN ('queued', 'running') AND id IN (
           SELECT subject_id FROM jobs WHERE subject_kind = 'build_run' AND state = 'failed'
         )",
    )
    .bind(now)
    .execute(&mut *connection)
    .await
    .map_err(map_write)?;
    Ok(())
}

async fn persist_recipe_catalog_snapshot(
    connection: &mut SqliteConnection,
    snapshot: &RecipeCatalogSnapshotRecord,
) -> Result<RecipeCatalogPublication, StorageError> {
    snapshot
        .manifest
        .validate()
        .map_err(|error| invalid_data(format!("invalid recipe catalog manifest: {error}")))?;
    let expected_digest = snapshot
        .manifest
        .canonical_digest()
        .map_err(|error| invalid_data(format!("hashing recipe catalog manifest: {error}")))?;
    if expected_digest != snapshot.manifest_digest {
        return Err(invalid_data(
            "recipe catalog digest does not match canonical manifest",
        ));
    }
    let manifest_json = serde_json::to_string(&snapshot.manifest)
        .map_err(|error| invalid_data(format!("serializing recipe catalog: {error}")))?;
    let recipe_count = i64::try_from(snapshot.manifest.recipes.len())
        .map_err(|_| invalid_data("recipe catalog recipe count exceeds SQLite range"))?;
    let diagnostic_count = i64::try_from(snapshot.manifest.diagnostics.len())
        .map_err(|_| invalid_data("recipe catalog diagnostic count exceeds SQLite range"))?;
    let prior = query(
        "SELECT id, worker_id, schema_version, producer, source_locator, source_revision,
                    manifest_digest, recipe_count, diagnostic_count, manifest_json, observed_at
             FROM recipe_catalog_snapshots
             WHERE worker_id = ? AND manifest_digest = ?",
    )
    .bind(snapshot.worker_id.to_string())
    .bind(snapshot.manifest_digest.as_str())
    .fetch_optional(&mut *connection)
    .await
    .map_err(|error| backend("loading replayed recipe catalog", &error))?;
    if let Some(prior) = prior {
        let prior = recipe_catalog_snapshot_from_row(&prior)?;
        if prior.worker_id != snapshot.worker_id
            || prior.manifest_digest != snapshot.manifest_digest
            || prior.manifest != snapshot.manifest
        {
            return Err(StorageError::Conflict);
        }
        return Ok(RecipeCatalogPublication {
            outcome: RecipeCatalogPublishOutcome::Replayed,
            snapshot: prior,
        });
    }
    query(
        "INSERT INTO recipe_catalog_snapshots
             (id, worker_id, schema_version, producer, source_locator, source_revision,
              manifest_digest, recipe_count, diagnostic_count, manifest_json, observed_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(snapshot.id.to_string())
    .bind(snapshot.worker_id.to_string())
    .bind(i64::from(snapshot.manifest.schema_version))
    .bind(&snapshot.manifest.producer)
    .bind(&snapshot.manifest.source.locator)
    .bind(&snapshot.manifest.source.revision)
    .bind(snapshot.manifest_digest.as_str())
    .bind(recipe_count)
    .bind(diagnostic_count)
    .bind(manifest_json)
    .bind(snapshot.observed_at)
    .execute(&mut *connection)
    .await
    .map_err(map_write)?;
    Ok(RecipeCatalogPublication {
        outcome: RecipeCatalogPublishOutcome::Published,
        snapshot: snapshot.clone(),
    })
}

const RECIPE_CATALOG_SCAN_SELECT: &str =
    "SELECT s.id, s.job_id, s.producer, s.source_locator, s.source_revision,
            s.requested_at, j.state, t.snapshot_id, t.failure_json,
            COALESCE(t.completed_at, j.completed_at) AS completed_at
     FROM recipe_catalog_scans s
     JOIN jobs j ON j.id = s.job_id
     LEFT JOIN recipe_catalog_scan_terminals t ON t.scan_id = s.id";

fn validate_run_log_batch(
    idempotency_key: &str,
    entries: &[NewRunLogEntry],
) -> Result<(), StorageError> {
    if idempotency_key.is_empty() || idempotency_key.len() > 255 {
        return Err(invalid_data("idempotency key must contain 1-255 bytes"));
    }
    if entries.is_empty() {
        return Err(invalid_data("run log batch must not be empty"));
    }
    let mut total = 0_usize;
    for entry in entries {
        if entry.message.is_empty() || entry.message.len() > MAX_RUN_LOG_ENTRY_BYTES {
            return Err(invalid_data("run log entry must contain 1-65536 bytes"));
        }
        total = total
            .checked_add(entry.message.len())
            .ok_or_else(|| invalid_data("run log batch size overflow"))?;
    }
    if total > MAX_RUN_LOG_BATCH_BYTES {
        return Err(invalid_data("run log batch exceeds 1048576 bytes"));
    }
    Ok(())
}

impl Storage for SqliteStorage {}

fn job_from_row(row: &SqliteRow) -> Result<Job, StorageError> {
    let subject_kind: String = row
        .try_get("subject_kind")
        .map_err(|error| backend("decoding job subject kind", &error))?;
    let subject_id: String = row
        .try_get("subject_id")
        .map_err(|error| backend("decoding job subject identity", &error))?;
    let capabilities_json: String = row
        .try_get("required_capabilities_json")
        .map_err(|error| backend("decoding job capabilities", &error))?;
    let payload_json: String = row
        .try_get("payload_json")
        .map_err(|error| backend("decoding job payload", &error))?;
    let state: String = row
        .try_get("state")
        .map_err(|error| backend("decoding job state", &error))?;
    let maximum_attempts: i64 = row
        .try_get("maximum_attempts")
        .map_err(|error| backend("decoding maximum attempts", &error))?;
    let attempt_count: i64 = row
        .try_get("attempt_count")
        .map_err(|error| backend("decoding attempt count", &error))?;
    Ok(Job {
        id: parse_value(
            row.try_get("id")
                .map_err(|error| backend("decoding job ID", &error))?,
            "job ID",
        )?,
        subject: job_subject(&subject_kind, &subject_id)?,
        required_capabilities: serde_json::from_str(&capabilities_json)
            .map_err(|error| invalid_data(format!("invalid job capabilities: {error}")))?,
        payload: serde_json::from_str(&payload_json)
            .map_err(|error| invalid_data(format!("invalid job payload: {error}")))?,
        state: job_state(&state)?,
        maximum_attempts: u32::try_from(maximum_attempts)
            .map_err(|_| invalid_data("invalid maximum attempts"))?,
        attempt_count: u32::try_from(attempt_count)
            .map_err(|_| invalid_data("invalid attempt count"))?,
        created_at: row
            .try_get("created_at")
            .map_err(|error| backend("decoding job creation time", &error))?,
    })
}

fn job_summary_from_row(row: &SqliteRow) -> Result<JobSummaryRecord, StorageError> {
    let subject_kind: String = row
        .try_get("subject_kind")
        .map_err(|error| backend("decoding job subject kind", &error))?;
    let subject_id: String = row
        .try_get("subject_id")
        .map_err(|error| backend("decoding job subject identity", &error))?;
    let capabilities_json: String = row
        .try_get("required_capabilities_json")
        .map_err(|error| backend("decoding job capabilities", &error))?;
    let state: String = row
        .try_get("state")
        .map_err(|error| backend("decoding job state", &error))?;
    let maximum_attempts: i64 = row
        .try_get("maximum_attempts")
        .map_err(|error| backend("decoding maximum attempts", &error))?;
    let attempt_count: i64 = row
        .try_get("attempt_count")
        .map_err(|error| backend("decoding attempt count", &error))?;
    Ok(JobSummaryRecord {
        id: parse_value(
            row.try_get("id")
                .map_err(|error| backend("decoding job ID", &error))?,
            "job ID",
        )?,
        subject: job_subject(&subject_kind, &subject_id)?,
        required_capabilities: serde_json::from_str(&capabilities_json)
            .map_err(|error| invalid_data(format!("invalid job capabilities: {error}")))?,
        state: job_state(&state)?,
        maximum_attempts: u32::try_from(maximum_attempts)
            .map_err(|_| invalid_data("invalid maximum attempts"))?,
        attempt_count: u32::try_from(attempt_count)
            .map_err(|_| invalid_data("invalid attempt count"))?,
        created_at: row
            .try_get("created_at")
            .map_err(|error| backend("decoding job creation time", &error))?,
    })
}

fn audit_from_row(row: &SqliteRow) -> Result<AuditEvent, StorageError> {
    let actor_json: String = row
        .try_get("actor_json")
        .map_err(|error| backend("decoding audit actor", &error))?;
    let details_json: String = row
        .try_get("details_json")
        .map_err(|error| backend("decoding audit details", &error))?;
    Ok(AuditEvent {
        id: parse_value(
            row.try_get("id")
                .map_err(|error| backend("decoding audit ID", &error))?,
            "audit event ID",
        )?,
        actor: serde_json::from_str(&actor_json)
            .map_err(|error| invalid_data(format!("invalid audit actor: {error}")))?,
        action: row
            .try_get("action")
            .map_err(|error| backend("decoding audit action", &error))?,
        resource_kind: row
            .try_get("resource_kind")
            .map_err(|error| backend("decoding audit resource kind", &error))?,
        resource_id: row
            .try_get("resource_id")
            .map_err(|error| backend("decoding audit resource ID", &error))?,
        details: serde_json::from_str(&details_json)
            .map_err(|error| invalid_data(format!("invalid audit details: {error}")))?,
        request_id: row
            .try_get("request_id")
            .map_err(|error| backend("decoding audit request ID", &error))?,
        occurred_at: row
            .try_get("occurred_at")
            .map_err(|error| backend("decoding audit time", &error))?,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::Duration;
    use stabbur_auth_core::{PasswordPolicy, generate_token};
    use stabbur_domain::{JobId, LocationId, RunId, SoftwareId, SoftwareSlug};
    use stabbur_jobs_core::Capability;

    use super::*;

    async fn storage() -> SqliteStorage {
        SqliteStorage::in_memory().await.unwrap()
    }

    #[tokio::test]
    async fn operator_migration_preserves_existing_release_worker_and_run_evidence() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let mut baseline = embedded_migrator();
        baseline.migrations = Cow::Owned(
            baseline
                .migrations
                .into_owned()
                .into_iter()
                .take(1)
                .collect(),
        );
        baseline.run(&pool).await.unwrap();
        for statement in [
            "INSERT INTO software(id,slug,name,created_at) VALUES('s','old','Old','2026-01-01T00:00:00Z')",
            "INSERT INTO releases(id,software_id,version,state,created_at,revision) VALUES('r','s','opaque-beta','stable','2026-01-01T00:00:00Z',7)",
            "INSERT INTO workers(id,name,registered_at,last_seen_at) VALUES('w','Old worker','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')",
            "INSERT INTO runs(id,state,created_at) VALUES('run','succeeded','2026-01-01T00:00:00Z')",
            "INSERT INTO run_releases VALUES('run','r','release_created','2026-01-01T00:00:00Z')",
        ] {
            query(statement).execute(&pool).await.unwrap();
        }
        embedded_migrator().run(&pool).await.unwrap();
        let row =
            query("SELECT state,version,revision,availability_json FROM releases WHERE id='r'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.try_get::<String, _>("state").unwrap(), "stable");
        assert_eq!(row.try_get::<String, _>("version").unwrap(), "opaque-beta");
        assert_eq!(row.try_get::<i64, _>("revision").unwrap(), 7);
        assert_eq!(
            row.try_get::<String, _>("availability_json").unwrap(),
            "{\"kind\":\"available\"}"
        );
        assert_eq!(
            query_scalar::<_, i64>("SELECT draining FROM workers WHERE id='w'")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            query_scalar::<_, String>("SELECT disposition FROM run_releases WHERE run_id='run'")
                .fetch_one(&pool)
                .await
                .unwrap(),
            "release_created"
        );
        assert_eq!(
            query_scalar::<_, i64>("SELECT COUNT(*) FROM pragma_foreign_key_check")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        embedded_migrator().run(&pool).await.unwrap();
    }

    #[tokio::test]
    async fn fresh_schema_applies_all_embedded_migrations() {
        let storage = storage().await;
        let migration_count: i64 = query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
            .fetch_one(storage.pool_for_tests())
            .await
            .unwrap();
        assert_eq!(migration_count, 4);
        let catalog_table: i64 = query_scalar(
            "SELECT COUNT(*) FROM sqlite_schema
             WHERE type = 'table' AND name = 'recipe_catalog_snapshots'",
        )
        .fetch_one(storage.pool_for_tests())
        .await
        .unwrap();
        assert_eq!(catalog_table, 1);
        let target_table: i64 = query_scalar(
            "SELECT COUNT(*) FROM sqlite_schema
             WHERE type = 'table' AND name = 'build_targets'",
        )
        .fetch_one(storage.pool_for_tests())
        .await
        .unwrap();
        assert_eq!(target_table, 1);
        let worker_revision: i64 = query_scalar(
            "SELECT COUNT(*) FROM pragma_table_info('workers') WHERE name = 'revision'",
        )
        .fetch_one(storage.pool_for_tests())
        .await
        .unwrap();
        assert_eq!(worker_revision, 1);
        let recipe_builder: i64 = query_scalar(
            "SELECT COUNT(*) FROM pragma_table_info('recipe_revisions') WHERE name = 'builder'",
        )
        .fetch_one(storage.pool_for_tests())
        .await
        .unwrap();
        assert_eq!(recipe_builder, 1);
    }

    #[tokio::test]
    async fn satisfies_shared_backend_contract() {
        stabbur_storage_conformance::assert_storage_contract(Arc::new(storage().await)).await;
    }

    #[tokio::test]
    async fn satisfies_shared_local_bootstrap_contract() {
        stabbur_storage_conformance::assert_local_bootstrap_contract(
            Arc::new(storage().await),
            Arc::new(storage().await),
        )
        .await;
    }

    #[tokio::test]
    async fn bootstrap_is_one_time_and_authenticates_hashed_credentials() {
        let storage = storage().await;
        let (bootstrap_secret, bootstrap_hash) = generate_token();
        assert_eq!(
            storage.prepare_bootstrap(&bootstrap_hash).await.unwrap(),
            BootstrapPreparation::Created
        );
        assert_eq!(
            storage.prepare_bootstrap(&bootstrap_hash).await.unwrap(),
            BootstrapPreparation::Pending
        );
        let password = PasswordPolicy::default()
            .hash("correct horse battery staple")
            .unwrap();
        let now = Utc::now();
        let admin = storage
            .bootstrap_admin(bootstrap_secret.expose_secret(), "admin", &password, now)
            .await
            .unwrap();
        assert!(admin.roles.contains(&RoleName::new("admin").unwrap()));
        assert_eq!(
            storage.prepare_bootstrap(&bootstrap_hash).await.unwrap(),
            BootstrapPreparation::Disabled
        );
        assert_eq!(
            storage
                .bootstrap_admin(bootstrap_secret.expose_secret(), "other", &password, now,)
                .await,
            Err(StorageError::BootstrapUnavailable)
        );
        let human = storage.human_credential("ADMIN").await.unwrap().unwrap();
        assert_eq!(human.principal.id, admin.id);
        PasswordPolicy::default()
            .verify("correct horse battery staple", &human.password_hash)
            .unwrap();

        let (session, session_hash) = generate_token();
        storage
            .create_credential(
                admin.id,
                Some("cli"),
                "session",
                &session_hash,
                Some(now + Duration::hours(1)),
                now,
            )
            .await
            .unwrap();
        let looked_up = storage
            .principal_by_token(&TokenHash::from_secret(session.expose_secret()), now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(looked_up.id, admin.id);
        assert_eq!(storage.audit_events(None, 50).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn explicit_transaction_commits_software_artifact_location_and_audit() {
        let storage = storage().await;
        let store = storage
            .ensure_local_primary_store(StoreId::new(), "local")
            .await
            .unwrap();
        let software = Software {
            id: SoftwareId::new(),
            slug: SoftwareSlug::new("firefox").unwrap(),
            name: "Firefox".into(),
            created_at: Utc::now(),
            revision: 1,
        };
        let artifact = Artifact {
            digest: Sha256Digest::new("a".repeat(64)).unwrap(),
            size: 42,
            media_type: "application/octet-stream".into(),
            created_at: Utc::now(),
        };
        let location = ArtifactLocation {
            id: LocationId::new(),
            digest: artifact.digest.clone(),
            store_id: store.id,
            state: LocationState::Present,
            verified_at: Some(Utc::now()),
            last_error: None,
        };
        let audit = AuditEvent {
            id: AuditEventId::new(),
            actor: AuditActor::LocalBreakGlass,
            action: "test.create".into(),
            resource_kind: "software".into(),
            resource_id: Some(software.id.to_string()),
            details: serde_json::json!({}),
            request_id: Some("test-request".into()),
            occurred_at: Utc::now(),
        };
        let mut transaction = storage.begin().await.unwrap();
        transaction.create_software(&software).await.unwrap();
        transaction
            .record_artifact_location(&artifact, &location)
            .await
            .unwrap();
        transaction.append_audit(&audit).await.unwrap();
        transaction.commit().await.unwrap();

        assert_eq!(storage.software("firefox").await.unwrap(), Some(software));
        assert_eq!(
            storage.artifact(&artifact.digest).await.unwrap(),
            Some(artifact)
        );
        assert_eq!(
            storage.artifact_locations(&location.digest).await.unwrap(),
            vec![location]
        );
        assert_eq!(storage.audit_events(None, 50).await.unwrap(), vec![audit]);
    }

    #[tokio::test]
    async fn claims_heartbeats_and_idempotently_completes_jobs() {
        let storage = storage().await;
        let worker = WorkerId::new();
        let capabilities = CapabilitySet::new([
            Capability::new("os.macos").unwrap(),
            Capability::new("builder.autopkg").unwrap(),
        ]);
        let now = Utc::now();
        storage
            .register_worker(worker, "worker-one", &capabilities, now)
            .await
            .unwrap();
        let job = Job {
            id: JobId::new(),
            subject: JobSubject::BuildRun {
                run_id: RunId::new(),
            },
            required_capabilities: CapabilitySet::new(
                [Capability::new("builder.autopkg").unwrap()],
            ),
            payload: serde_json::json!({"build": "firefox"}),
            state: JobState::Queued,
            maximum_attempts: 3,
            attempt_count: 0,
            created_at: now,
        };
        storage.enqueue_job(&job).await.unwrap();
        let claimed = storage
            .claim_job(worker, &capabilities, now, 30)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.job.state, JobState::Leased);
        let lease = storage
            .heartbeat_job(worker, &claimed.lease, now + Duration::seconds(5), 30)
            .await
            .unwrap();
        assert_eq!(
            storage
                .complete_job(
                    worker,
                    &lease,
                    "completion-one",
                    &serde_json::json!({"ok": true}),
                    now + Duration::seconds(6),
                )
                .await
                .unwrap(),
            CompletionOutcome::Completed
        );
        assert_eq!(
            storage
                .complete_job(
                    worker,
                    &lease,
                    "completion-one",
                    &serde_json::json!({"ok": true}),
                    now + Duration::seconds(7),
                )
                .await
                .unwrap(),
            CompletionOutcome::Replayed
        );
    }

    #[tokio::test]
    async fn compatible_work_does_not_starve_behind_large_incompatible_queue() {
        let storage = storage().await;
        let worker = WorkerId::new();
        let available = CapabilitySet::new([Capability::new("os.linux").unwrap()]);
        let unavailable = CapabilitySet::new([Capability::new("os.macos").unwrap()]);
        let now = Utc::now();
        storage
            .register_worker(worker, "linux-worker", &available, now)
            .await
            .unwrap();
        for offset in 0..150 {
            storage
                .enqueue_job(&Job {
                    id: JobId::new(),
                    subject: JobSubject::BuildRun {
                        run_id: RunId::new(),
                    },
                    required_capabilities: unavailable.clone(),
                    payload: serde_json::json!({"incompatible": offset}),
                    state: JobState::Queued,
                    maximum_attempts: 3,
                    attempt_count: 0,
                    created_at: now + Duration::milliseconds(offset),
                })
                .await
                .unwrap();
        }
        let compatible = Job {
            id: JobId::new(),
            subject: JobSubject::BuildRun {
                run_id: RunId::new(),
            },
            required_capabilities: available.clone(),
            payload: serde_json::json!({"compatible": true}),
            state: JobState::Queued,
            maximum_attempts: 3,
            attempt_count: 0,
            created_at: now + Duration::seconds(1),
        };
        storage.enqueue_job(&compatible).await.unwrap();
        let queues = storage.operational_status().await.unwrap();
        assert!(!queues.capability_queues_truncated);
        assert_eq!(queues.capability_queues.len(), 2);
        let missing = queues
            .capability_queues
            .iter()
            .find(|q| q.required_capabilities == unavailable)
            .unwrap();
        assert_eq!(missing.queued_jobs, 150);
        assert_eq!(missing.matching_workers, 0);
        let ready = queues
            .capability_queues
            .iter()
            .find(|q| q.required_capabilities == available)
            .unwrap();
        assert_eq!(ready.matching_workers, 1);
        assert_eq!(ready.workers_with_active_leases, 0);

        let claimed = storage
            .claim_job(worker, &available, now + Duration::seconds(2), 30)
            .await
            .unwrap()
            .expect("compatible work beyond the first queue window must be claimable");
        assert_eq!(claimed.job.id, compatible.id);
        storage
            .enqueue_job(&Job {
                id: JobId::new(),
                subject: JobSubject::BuildRun {
                    run_id: RunId::new(),
                },
                ..compatible
            })
            .await
            .unwrap();
        let queues = storage.operational_status().await.unwrap();
        let busy = queues
            .capability_queues
            .iter()
            .find(|q| q.required_capabilities == available)
            .unwrap();
        assert_eq!(busy.matching_workers, 1);
        assert_eq!(busy.workers_with_active_leases, 1);
    }

    #[tokio::test]
    async fn concurrent_workers_cannot_lease_the_same_job() {
        let storage = Arc::new(storage().await);
        let capabilities = CapabilitySet::new([Capability::new("builder.fake").unwrap()]);
        let first = WorkerId::new();
        let second = WorkerId::new();
        let now = Utc::now();
        storage
            .register_worker(first, "race-one", &capabilities, now)
            .await
            .unwrap();
        storage
            .register_worker(second, "race-two", &capabilities, now)
            .await
            .unwrap();
        let job = Job {
            id: JobId::new(),
            subject: JobSubject::BuildRun {
                run_id: RunId::new(),
            },
            required_capabilities: capabilities.clone(),
            payload: serde_json::json!({"race": true}),
            state: JobState::Queued,
            maximum_attempts: 3,
            attempt_count: 0,
            created_at: now,
        };
        storage.enqueue_job(&job).await.unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let claim = |worker| {
            let storage = storage.clone();
            let capabilities = capabilities.clone();
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                storage
                    .claim_job(worker, &capabilities, now, 30)
                    .await
                    .unwrap()
            }
        };
        let (left, right) = tokio::join!(claim(first), claim(second));
        assert_eq!(
            usize::from(left.is_some()) + usize::from(right.is_some()),
            1
        );
        assert_eq!(storage.job(job.id).await.unwrap().unwrap().attempt_count, 1);
    }

    #[tokio::test]
    async fn expired_lease_recovers_after_database_reopen() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("stabbur.db");
        let capabilities = CapabilitySet::new([Capability::new("builder.fake").unwrap()]);
        let first = WorkerId::new();
        let second = WorkerId::new();
        let now = Utc::now();
        let job = Job {
            id: JobId::new(),
            subject: JobSubject::BuildRun {
                run_id: RunId::new(),
            },
            required_capabilities: capabilities.clone(),
            payload: serde_json::json!({"restart": true}),
            state: JobState::Queued,
            maximum_attempts: 3,
            attempt_count: 0,
            created_at: now,
        };
        {
            let storage = SqliteStorage::connect(&database).await.unwrap();
            storage
                .register_worker(first, "restart-one", &capabilities, now)
                .await
                .unwrap();
            storage
                .register_worker(second, "restart-two", &capabilities, now)
                .await
                .unwrap();
            storage.enqueue_job(&job).await.unwrap();
            let claimed = storage
                .claim_job(first, &capabilities, now, 1)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(claimed.job.id, job.id);
        }
        let reopened = SqliteStorage::connect(&database).await.unwrap();
        let recovered = reopened
            .claim_job(second, &capabilities, now + Duration::seconds(2), 30)
            .await
            .unwrap()
            .expect("expired durable lease must be claimable after restart");
        assert_eq!(recovered.job.id, job.id);
        assert_eq!(recovered.job.attempt_count, 2);
    }

    #[tokio::test]
    async fn durable_database_uses_wal_and_survives_reopen() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("stabbur.db");
        let storage = SqliteStorage::connect(&database).await.unwrap();
        let software = Software {
            id: SoftwareId::new(),
            slug: SoftwareSlug::new("thunderbird").unwrap(),
            name: "Thunderbird".into(),
            created_at: Utc::now(),
            revision: 1,
        };
        let mut transaction = storage.begin().await.unwrap();
        transaction.create_software(&software).await.unwrap();
        transaction.commit().await.unwrap();
        let journal_mode: String = query_scalar("PRAGMA journal_mode")
            .fetch_one(storage.pool_for_tests())
            .await
            .unwrap();
        let foreign_keys: i64 = query_scalar("PRAGMA foreign_keys")
            .fetch_one(storage.pool_for_tests())
            .await
            .unwrap();
        let health = storage.doctor().await.unwrap();
        assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
        assert_eq!(foreign_keys, 1);
        assert_eq!(health.backend, "sqlite");
        assert!(health.database_ready);
        drop(storage);

        let reopened = SqliteStorage::connect_read_only(&database).await.unwrap();
        assert_eq!(
            reopened.software("thunderbird").await.unwrap(),
            Some(software)
        );
    }

    #[tokio::test]
    async fn break_glass_access_requires_exclusive_process_lock() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("stabbur.db");
        let service = SqliteStorage::connect(&database).await.unwrap();
        assert!(SqliteStorage::connect(&database).await.is_ok());
        assert!(SqliteStorage::connect_exclusive(&database).await.is_err());
        drop(service);

        // All shared handles must be gone before exclusive access is possible.
        let exclusive = SqliteStorage::connect_exclusive(&database).await.unwrap();
        assert!(SqliteStorage::connect(&database).await.is_err());
        drop(exclusive);
        assert!(SqliteStorage::connect_read_only(&database).await.is_ok());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let lock = std::fs::metadata(database.with_extension("db.lock")).unwrap();
            assert_eq!(lock.permissions().mode() & 0o077, 0);
        }
    }

    #[tokio::test]
    async fn critical_read_and_queue_paths_use_expected_indexes() {
        async fn plan(storage: &SqliteStorage, statement: &str) -> String {
            let rows = query(&format!("EXPLAIN QUERY PLAN {statement}"))
                .fetch_all(storage.pool_for_tests())
                .await
                .unwrap();
            rows.iter()
                .map(|row| row.try_get::<String, _>("detail").unwrap())
                .collect::<Vec<_>>()
                .join("\n")
        }

        let storage = storage().await;
        for (statement, index) in [
            (
                "SELECT id FROM software WHERE name > 'A' ORDER BY name, id LIMIT 51",
                "software_library_name",
            ),
            (
                "SELECT state FROM runs WHERE software_id = 'app' ORDER BY created_at DESC, id DESC LIMIT 1",
                "runs_software_recent",
            ),
            (
                "SELECT COUNT(*) FROM build_targets WHERE software_id = 'app' AND enabled = 1",
                "targets_software_enabled",
            ),
        ] {
            let actual = plan(&storage, statement).await;
            assert!(actual.contains(index), "{actual}");
        }
        let token = plan(
            &storage,
            "SELECT p.id FROM credentials c JOIN principals p ON p.id = c.principal_id
             WHERE c.token_hash = 'digest' AND c.revoked_at IS NULL",
        )
        .await;
        assert!(token.contains("token_hash"), "{token}");
        assert!(!token.contains("SCAN c"), "{token}");

        let queue = plan(
            &storage,
            "SELECT id FROM jobs j WHERE state = 'queued'
             AND attempt_count < maximum_attempts
             AND NOT EXISTS (
               SELECT 1 FROM json_each(j.required_capabilities_json) required
               WHERE required.value NOT IN (SELECT value FROM json_each('[]'))
             ) ORDER BY created_at, id LIMIT 1",
        )
        .await;
        assert!(queue.contains("jobs_claim_idx"), "{queue}");

        let expiry = plan(
            &storage,
            "SELECT job_id FROM attempts
             WHERE state = 'leased' AND expires_at <= '2099-01-01T00:00:00Z'",
        )
        .await;
        assert!(expiry.contains("attempts_expiry_idx"), "{expiry}");

        let releases = plan(
            &storage,
            "SELECT id FROM releases
             WHERE software_id = 'software' AND id > '' ORDER BY id LIMIT 50",
        )
        .await;
        assert!(
            releases.contains("releases_software_page_idx"),
            "{releases}"
        );

        let due_targets = plan(
            &storage,
            "SELECT id FROM build_targets
             WHERE enabled = 1 AND trigger_kind = 'interval'
               AND next_run_at <= '2099-01-01T00:00:00Z'
             ORDER BY next_run_at, id LIMIT 50",
        )
        .await;
        assert!(
            due_targets.contains("build_targets_due_idx"),
            "{due_targets}"
        );
    }
}
