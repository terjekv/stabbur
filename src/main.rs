//! `stabbur-server` runtime and narrow local administrative commands.

use std::{
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use stabbur_auth_core::PasswordPolicy;
use stabbur_builder_autopkg::{AutoPkgCatalogGenerator, PinnedSource};
use stabbur_server::{config::ServiceConfig, runtime};
use stabbur_storage_core::{AuditActor, Storage};
use stabbur_storage_runtime::open as open_storage;
use tracing_subscriber::EnvFilter;

mod worker_prepare;

#[derive(Debug, Parser)]
#[command(name = "stabbur-server", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run only the REST control plane.
    Api(ServiceConfig),
    /// Run an outbound-only worker.
    Worker(WorkerArgs),
    /// Run the API and an embedded portable fake-builder worker.
    All(ServiceConfig),
    /// Run a narrow local break-glass operation.
    Admin {
        /// Administrative operation.
        #[command(subcommand)]
        command: AdminCommand,
    },
    /// Generate or verify the committed OpenAPI contract.
    Openapi(OpenApiArgs),
    /// Generate builder-neutral recipe catalog manifests for automation and inspection.
    Catalog {
        /// Catalog operation.
        #[command(subcommand)]
        command: CatalogCommand,
    },
}

#[derive(Debug, Subcommand)]
enum CatalogCommand {
    /// Materialize and normalize one exact AutoPkg Git revision.
    GenerateAutopkg(GenerateAutoPkgCatalogArgs),
}

#[derive(Debug, Args)]
struct GenerateAutoPkgCatalogArgs {
    /// Absolute credential-free HTTPS Git repository URL.
    #[arg(long)]
    source_url: String,
    /// Full lowercase 40-character Git commit hash.
    #[arg(long)]
    source_revision: String,
    /// JSON output path. Omit to write the canonical manifest to stdout.
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
struct WorkerArgs {
    /// One-shot local worker provisioning operation.
    #[command(subcommand)]
    command: Option<WorkerCommand>,
    /// Print locally detected capabilities and tool versions, then exit.
    #[arg(long)]
    print_capabilities: bool,
    /// Server base URL. Standalone workers never open the server database or a listener.
    #[arg(
        long,
        env = "STABBUR_SERVER_URL",
        required_unless_present = "print_capabilities"
    )]
    server_url: Option<String>,
    /// Owner-only server-issued JSON worker credential file.
    #[arg(
        long,
        env = "STABBUR_WORKER_TOKEN_FILE",
        required_unless_present = "print_capabilities"
    )]
    token_file: Option<PathBuf>,
    /// Private worker state directory.
    #[arg(
        long,
        env = "STABBUR_WORKER_DATA_DIR",
        default_value = ".stabbur-worker"
    )]
    data_dir: PathBuf,
    /// Absolute worker-local AutoPkg executable path; never supplied by the control plane.
    #[arg(long, env = "STABBUR_AUTOPKG_PROGRAM")]
    autopkg_program: Option<PathBuf>,
    /// Optional builder-neutral catalog manifest published after authenticated registration.
    #[arg(long, env = "STABBUR_RECIPE_CATALOG_MANIFEST")]
    catalog_manifest: Option<PathBuf>,
    /// Discover this worker account's AutoPkg recipes every five minutes (read-only).
    #[arg(long, env = "STABBUR_DISCOVER_AUTOPKG")]
    discover_autopkg: bool,
    /// Local AutoPkg preferences file used only for discovery.
    #[arg(long, env = "STABBUR_AUTOPKG_PREFS", requires = "discover_autopkg")]
    autopkg_prefs: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum WorkerCommand {
    /// Verify and install an exactly pinned AutoPkg macOS package.
    Prepare(WorkerPrepareArgs),
}

#[derive(Debug, Args)]
struct WorkerPrepareArgs {
    /// Absolute path to the trusted, non-secret AutoPkg installer manifest.
    #[arg(long)]
    manifest: PathBuf,
    /// Absolute path for the owner-only idempotency receipt.
    #[arg(long, default_value = "/var/db/stabbur/worker/autopkg-prepared.json")]
    receipt: PathBuf,
    /// Verify the manifest, package digest, and declared signature policy without installing.
    #[arg(long)]
    check: bool,
}

#[derive(Debug, Subcommand)]
enum AdminCommand {
    /// Inspect selected immutable objects and available disk space under exclusive local access.
    StoreInspect(StoreInspectArgs),
    /// Remove expired upload staging files with the server stopped; immutable artifacts remain.
    PruneUploads(PruneUploadsArgs),
    /// Create the first administrator through exclusive local access.
    Bootstrap(AdminBootstrapArgs),
    /// Reset a local human password and revoke existing credentials.
    ResetPassword(AdminPasswordArgs),
    /// Revoke all sessions and tokens owned by a local human.
    RevokeSessions(AdminUserArgs),
    /// Apply embedded database migrations.
    Migrate(AdminConfig),
    /// Inspect local persistence without mutating domain resources.
    Doctor(AdminConfig),
}

#[derive(Args)]
struct StoreInspectArgs {
    #[command(flatten)]
    config: AdminConfig,
    /// Repeat for up to 1000 lowercase SHA-256 identities.
    #[arg(long)]
    digest: Vec<stabbur_domain::Sha256Digest>,
    /// Maximum total bytes hashed in this inspection pass.
    #[arg(long, default_value_t = 1_073_741_824)]
    maximum_bytes: u64,
}
impl fmt::Debug for StoreInspectArgs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoreInspectArgs")
            .field("digests", &self.digest.len())
            .finish_non_exhaustive()
    }
}
#[derive(Args)]
struct PruneUploadsArgs {
    #[command(flatten)]
    config: AdminConfig,
    /// Minimum upload age; must be at least 3600 seconds.
    #[arg(long, default_value_t = 86400)]
    older_than_seconds: u64,
    /// Maximum directory entries examined.
    #[arg(long, default_value = "1000")]
    limit: std::num::NonZeroU16,
}
impl fmt::Debug for PruneUploadsArgs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PruneUploadsArgs").finish_non_exhaustive()
    }
}

#[derive(Args)]
struct AdminConfig {
    /// Private durable Stabbur data directory.
    #[arg(long, env = "STABBUR_DATA_DIR", default_value = ".stabbur")]
    data_dir: PathBuf,
    /// Statically linked relational backend name.
    #[arg(long, env = "STABBUR_STORAGE_BACKEND", default_value = "sqlite")]
    storage_backend: String,
    /// Backend database URL. SQLite defaults to `<data-dir>/stabbur.db` when omitted.
    #[arg(long, env = "STABBUR_DATABASE_URL", hide_env_values = true)]
    database_url: Option<String>,
}

impl AdminConfig {
    fn service_config(&self) -> ServiceConfig {
        ServiceConfig {
            data_dir: self.data_dir.clone(),
            bind: "127.0.0.1:8080"
                .parse()
                .expect("static socket address is valid"),
            storage_backend: self.storage_backend.clone(),
            database_url: self.database_url.clone(),
            scheduler_poll_seconds: 5,
            disable_scheduler: true,
        }
    }
}

impl fmt::Debug for AdminConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdminConfig")
            .field("data_dir", &self.data_dir)
            .field("storage_backend", &self.storage_backend)
            .field(
                "database_url",
                &self.database_url.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

#[derive(Debug, Args)]
struct AdminBootstrapArgs {
    #[command(flatten)]
    config: AdminConfig,
    /// First administrator login name.
    #[arg(long)]
    username: String,
    /// Owner-only password file. If omitted, read from an interactive terminal.
    #[arg(long)]
    password_file: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct AdminPasswordArgs {
    #[command(flatten)]
    config: AdminConfig,
    /// Existing local human login name.
    user: String,
    /// Owner-only password file. If omitted, read from an interactive terminal.
    #[arg(long)]
    password_file: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct AdminUserArgs {
    #[command(flatten)]
    config: AdminConfig,
    /// Existing local human login name.
    user: String,
}

#[derive(Debug, Args)]
struct OpenApiArgs {
    /// Fail when the generated document differs from the output file.
    #[arg(long)]
    check: bool,
    /// Committed contract path.
    #[arg(long, default_value = "docs/openapi.json")]
    output: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    match Cli::parse().command {
        Command::Api(config) => {
            let initialized = runtime::initialize(&config, true).await?;
            runtime::serve(config, initialized.state).await
        }
        Command::All(config) => {
            let initialized = runtime::initialize(&config, true).await?;
            let worker_storage = initialized.storage.clone();
            let worker_id = initialized.embedded_worker_id;
            tokio::spawn(async move {
                stabbur_server::runtime::run_embedded_worker(worker_storage, worker_id).await;
            });
            runtime::serve(config, initialized.state).await
        }
        Command::Worker(arguments) => {
            if let Some(WorkerCommand::Prepare(prepare)) = arguments.command {
                let report =
                    worker_prepare::prepare(&prepare.manifest, &prepare.receipt, prepare.check)
                        .await?;
                println!("{}", serde_json::to_string_pretty(&report)?);
                return Ok(());
            }
            if arguments.print_capabilities {
                let report = stabbur_server::runtime::worker_capability_report(
                    arguments.autopkg_program.as_deref(),
                )
                .await?;
                println!("{}", serde_json::to_string_pretty(&report)?);
                return Ok(());
            }
            stabbur_server::runtime::run_outbound_worker(
                arguments
                    .server_url
                    .as_deref()
                    .context("worker server URL is required")?,
                arguments
                    .token_file
                    .as_deref()
                    .context("worker token file is required")?,
                &arguments.data_dir,
                arguments.autopkg_program.as_deref(),
                arguments.catalog_manifest.as_deref(),
                arguments
                    .discover_autopkg
                    .then_some(arguments.autopkg_prefs.as_deref()),
            )
            .await
        }
        Command::Admin { command } => run_admin(command).await,
        Command::Openapi(arguments) => generate_openapi(&arguments).await,
        Command::Catalog { command } => run_catalog(command).await,
    }
}

async fn run_catalog(command: CatalogCommand) -> Result<()> {
    match command {
        CatalogCommand::GenerateAutopkg(arguments) => {
            let source = PinnedSource {
                url: arguments.source_url,
                commit: arguments.source_revision,
            };
            source.validate()?;
            let work = std::env::temp_dir().join(format!(
                "stabbur-catalog-generator-{}",
                uuid::Uuid::now_v7()
            ));
            tokio::fs::create_dir(&work)
                .await
                .context("creating catalog generator isolation directory")?;
            let generated = AutoPkgCatalogGenerator::generate(&source, &work).await;
            let cleanup = tokio::fs::remove_dir_all(&work).await;
            let manifest = generated?;
            cleanup.context("removing catalog generator isolation directory")?;
            let mut json = serde_json::to_string_pretty(&manifest)?;
            json.push('\n');
            if let Some(output) = arguments.output {
                tokio::fs::write(&output, json)
                    .await
                    .with_context(|| format!("writing catalog manifest {}", output.display()))?;
            } else {
                print!("{json}");
            }
            Ok(())
        }
    }
}

async fn run_admin(command: AdminCommand) -> Result<()> {
    match command {
        AdminCommand::StoreInspect(arguments) => {
            let _exclusive = open_admin_storage(&arguments.config, false).await?;
            let store = stabbur_store_fs::FsArtifactStore::open(
                arguments.config.service_config().store_path(),
                stabbur_store_core::StoreRole::Primary,
            )
            .await?;
            let budget = stabbur_store_fs::InspectionBudget::new(1000, arguments.maximum_bytes)?;
            let report = store.inspect(&arguments.digest, budget).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            if report
                .objects
                .iter()
                .any(|object| object.outcome != "verified")
            {
                bail!("one or more objects were not verified");
            }
            Ok(())
        }
        AdminCommand::PruneUploads(arguments) => {
            let _exclusive = open_admin_storage(&arguments.config, false).await?;
            let store = stabbur_store_fs::FsArtifactStore::open(
                arguments.config.service_config().store_path(),
                stabbur_store_core::StoreRole::Primary,
            )
            .await?;
            let removed = store
                .prune_stale_uploads(
                    std::time::Duration::from_secs(arguments.older_than_seconds),
                    arguments.limit,
                )
                .await?;
            println!("{}", serde_json::json!({"removed_uploads": removed}));
            Ok(())
        }
        AdminCommand::Migrate(arguments) => {
            let storage = open_admin_storage(&arguments, false).await?;
            storage.migrate().await?;
            println!("migrations applied");
            Ok(())
        }
        AdminCommand::Doctor(arguments) => {
            let storage = open_admin_storage(&arguments, true).await?;
            let health = storage.doctor().await?;
            println!("{}", serde_json::to_string_pretty(&health)?);
            Ok(())
        }
        AdminCommand::Bootstrap(arguments) => {
            let config = arguments.config.service_config();
            let storage = open_admin_storage(&arguments.config, false).await?;
            let password = read_password(arguments.password_file.as_deref()).await?;
            let password_hash = PasswordPolicy::default().hash(&password)?;
            storage
                .bootstrap_admin_local(&arguments.username, &password_hash, chrono::Utc::now())
                .await?;
            match tokio::fs::remove_file(config.bootstrap_secret_path()).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("removing consumed bootstrap secret"),
            }
            println!("administrator created");
            Ok(())
        }
        AdminCommand::ResetPassword(arguments) => {
            let storage = open_admin_storage(&arguments.config, false).await?;
            let password = read_password(arguments.password_file.as_deref()).await?;
            let password_hash = PasswordPolicy::default().hash(&password)?;
            storage
                .reset_password(
                    &arguments.user,
                    &password_hash,
                    AuditActor::LocalBreakGlass,
                    chrono::Utc::now(),
                )
                .await?;
            println!("password reset and credentials revoked");
            Ok(())
        }
        AdminCommand::RevokeSessions(arguments) => {
            let storage = open_admin_storage(&arguments.config, false).await?;
            let count = storage
                .revoke_credentials(
                    &arguments.user,
                    AuditActor::LocalBreakGlass,
                    chrono::Utc::now(),
                )
                .await?;
            println!("revoked {count} credential(s)");
            Ok(())
        }
    }
}

async fn open_admin_storage(config: &AdminConfig, read_only: bool) -> Result<Arc<dyn Storage>> {
    let service = config.service_config();
    if !read_only {
        tokio::fs::create_dir_all(&service.data_dir)
            .await
            .with_context(|| format!("creating data directory {}", service.data_dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&service.data_dir, std::fs::Permissions::from_mode(0o700))
                .await
                .context("securing Stabbur data directory")?;
        }
    }
    let settings = if read_only {
        service.storage_settings(true)
    } else {
        service.storage_settings(false).with_exclusive_access()
    };
    open_storage(&settings)
        .await
        .map(|handle| handle.storage())
        .context("opening configured Stabbur storage backend")
}

async fn read_password(path: Option<&Path>) -> Result<String> {
    let password = if let Some(path) = path {
        read_owner_only(path).await?
    } else {
        tokio::task::spawn_blocking(|| rpassword::prompt_password("New password: "))
            .await
            .context("joining password prompt")??
    };
    Ok(password.trim_end_matches(['\r', '\n']).to_owned())
}

async fn read_owner_only(path: &Path) -> Result<String> {
    let metadata = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("reading metadata for {}", path.display()))?;
    if !metadata.is_file() {
        bail!("credential input must be a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("credential file must be owner-only (mode 0600 or stricter)");
        }
    }
    tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("reading owner-only file {}", path.display()))
}

async fn generate_openapi(arguments: &OpenApiArgs) -> Result<()> {
    let generated = stabbur_server::openapi::json();
    if arguments.check {
        let existing = tokio::fs::read_to_string(&arguments.output)
            .await
            .with_context(|| format!("reading {}", arguments.output.display()))?;
        if existing != generated {
            bail!(
                "{} is stale; regenerate it with `stabbur-server openapi`",
                arguments.output.display()
            );
        }
        println!("{} is current", arguments.output.display());
        return Ok(());
    }
    if let Some(parent) = arguments.output.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    tokio::fs::write(&arguments.output, generated)
        .await
        .with_context(|| format!("writing {}", arguments.output.display()))?;
    println!("wrote {}", arguments.output.display());
    Ok(())
}

#[cfg(test)]
mod command_tests {
    use super::*;

    #[test]
    fn capability_inspection_needs_no_server_credential() {
        let command = Cli::try_parse_from(["stabbur-server", "worker", "--print-capabilities"])
            .expect("local capability inspection must parse without a server");
        let Command::Worker(worker) = command.command else {
            panic!("worker command expected");
        };
        assert!(worker.print_capabilities);
        assert!(worker.command.is_none());
        assert!(worker.server_url.is_none());
        assert!(worker.token_file.is_none());
        assert!(worker.autopkg_program.is_none());
    }

    #[test]
    fn worker_prepare_needs_no_server_credential() {
        let command = Cli::try_parse_from([
            "stabbur-server",
            "worker",
            "prepare",
            "--manifest",
            "/private/tmp/autopkg-installer.json",
            "--check",
        ])
        .expect("local worker preparation must parse without a server credential");
        let Command::Worker(worker) = command.command else {
            panic!("worker command expected");
        };
        let Some(WorkerCommand::Prepare(prepare)) = worker.command else {
            panic!("worker prepare command expected");
        };
        assert_eq!(
            prepare.manifest,
            PathBuf::from("/private/tmp/autopkg-installer.json")
        );
        assert_eq!(
            prepare.receipt,
            PathBuf::from("/var/db/stabbur/worker/autopkg-prepared.json")
        );
        assert!(prepare.check);
    }

    #[test]
    fn running_worker_requires_server_and_credential() {
        let error = Cli::try_parse_from(["stabbur-server", "worker"])
            .expect_err("remote execution needs both connection inputs");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn capability_inspection_accepts_local_autopkg_program() {
        let command = Cli::try_parse_from([
            "stabbur-server",
            "worker",
            "--print-capabilities",
            "--autopkg-program",
            "/opt/stabbur/autopkg",
        ])
        .expect("local AutoPkg selection must parse");
        let Command::Worker(worker) = command.command else {
            panic!("worker command expected");
        };
        assert_eq!(
            worker.autopkg_program.as_deref(),
            Some(Path::new("/opt/stabbur/autopkg"))
        );
    }

    #[test]
    fn catalog_generator_requires_an_exact_source_and_supports_file_output() {
        let command = Cli::try_parse_from([
            "stabbur-server",
            "catalog",
            "generate-autopkg",
            "--source-url",
            "https://example.test/recipes.git",
            "--source-revision",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--output",
            "catalog.json",
        ])
        .expect("catalog generator inputs must parse");
        let Command::Catalog {
            command: CatalogCommand::GenerateAutopkg(arguments),
        } = command.command
        else {
            panic!("catalog generator command expected");
        };
        assert_eq!(arguments.output.as_deref(), Some(Path::new("catalog.json")));
    }
}
