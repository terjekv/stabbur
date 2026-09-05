//! Runtime configuration owned by the application crate.

use std::{fmt, net::SocketAddr, path::PathBuf};

use clap::Args;
use stabbur_storage_runtime::StorageSettings;

/// Common durable service configuration.
#[derive(Clone, Args)]
pub struct ServiceConfig {
    /// Private durable Stabbur data directory.
    #[arg(long, env = "STABBUR_DATA_DIR", default_value = ".stabbur")]
    pub data_dir: PathBuf,
    /// HTTP listen address for API-bearing roles.
    #[arg(long, env = "STABBUR_BIND", default_value = "127.0.0.1:8080")]
    pub bind: SocketAddr,
    /// Statically linked relational backend name.
    #[arg(long, env = "STABBUR_STORAGE_BACKEND", default_value = "sqlite")]
    pub storage_backend: String,
    /// Backend database URL. SQLite defaults to `<data-dir>/stabbur.db` when omitted.
    #[arg(long, env = "STABBUR_DATABASE_URL", hide_env_values = true)]
    pub database_url: Option<String>,
    /// Durable build-target scheduler poll interval.
    #[arg(
        long,
        env = "STABBUR_SCHEDULER_POLL_SECONDS",
        default_value_t = 5,
        value_parser = clap::value_parser!(u64).range(1..=300)
    )]
    pub scheduler_poll_seconds: u64,
    /// Disable automatic due-target scheduling for maintenance or a dedicated API replica.
    #[arg(long, env = "STABBUR_DISABLE_SCHEDULER", default_value_t = false)]
    pub disable_scheduler: bool,
}

impl ServiceConfig {
    /// SQLite database file.
    #[must_use]
    pub fn database_path(&self) -> PathBuf {
        self.data_dir.join("stabbur.db")
    }

    /// Backend-neutral settings consumed only by storage composition.
    #[must_use]
    pub fn storage_settings(&self, read_only: bool) -> StorageSettings {
        StorageSettings::new(
            &self.storage_backend,
            self.database_url.clone(),
            self.database_path(),
            read_only,
        )
    }

    /// Private local content-addressed store root.
    #[must_use]
    pub fn store_path(&self) -> PathBuf {
        self.data_dir.join("store")
    }

    /// One-time bootstrap secret file.
    #[must_use]
    pub fn bootstrap_secret_path(&self) -> PathBuf {
        self.data_dir.join("bootstrap.secret")
    }
}

impl fmt::Debug for ServiceConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServiceConfig")
            .field("data_dir", &self.data_dir)
            .field("bind", &self.bind)
            .field("storage_backend", &self.storage_backend)
            .field("scheduler_poll_seconds", &self.scheduler_poll_seconds)
            .field("disable_scheduler", &self.disable_scheduler)
            .field(
                "database_url",
                &self.database_url.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}
