//! Opaque application composition for statically linked relational storage adapters.

use std::{fmt, path::PathBuf, sync::Arc};

use stabbur_storage_core::{Storage, StorageError};
use stabbur_storage_sqlite::SqliteStorage;
use thiserror::Error;

/// Settings used to select and initialize one complete storage backend.
///
/// The optional database URL is private and redacted from debug output because it may contain
/// credentials.
#[derive(Clone)]
pub struct StorageSettings {
    backend: String,
    database_url: Option<String>,
    default_sqlite_path: PathBuf,
    read_only: bool,
    exclusive: bool,
}

impl StorageSettings {
    /// Creates generic settings without teaching the server about individual adapters.
    #[must_use]
    pub fn new(
        backend: impl Into<String>,
        database_url: Option<String>,
        default_sqlite_path: PathBuf,
        read_only: bool,
    ) -> Self {
        Self {
            backend: backend.into(),
            database_url,
            default_sqlite_path,
            read_only,
            exclusive: false,
        }
    }

    /// Requires adapter-specific exclusive access for local break-glass mutation.
    #[must_use]
    pub fn with_exclusive_access(mut self) -> Self {
        self.exclusive = true;
        self
    }
}

impl fmt::Debug for StorageSettings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StorageSettings")
            .field("backend", &self.backend)
            .field(
                "database_url",
                &self.database_url.as_ref().map(|_| "[REDACTED]"),
            )
            .field("default_sqlite_path", &self.default_sqlite_path)
            .field("read_only", &self.read_only)
            .field("exclusive", &self.exclusive)
            .finish()
    }
}

/// An initialized backend hidden behind the complete backend-neutral contract.
#[derive(Clone)]
pub struct StorageHandle {
    backend: Arc<str>,
    storage: Arc<dyn Storage>,
}

impl StorageHandle {
    /// Returns the stable selected backend name for logs and diagnostics.
    #[must_use]
    pub fn backend(&self) -> &str {
        &self.backend
    }

    /// Returns a cheaply cloned backend-neutral application handle.
    #[must_use]
    pub fn storage(&self) -> Arc<dyn Storage> {
        self.storage.clone()
    }
}

impl fmt::Debug for StorageHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StorageHandle")
            .field("backend", &self.backend)
            .finish_non_exhaustive()
    }
}

/// Failure to configure or initialize a selected backend.
#[derive(Debug, Error)]
pub enum StorageOpenError {
    /// The configured adapter is not statically linked into this server build.
    #[error("storage backend `{backend}` is not available in this server build")]
    UnknownBackend {
        /// Normalized configured backend name.
        backend: String,
    },
    /// A backend URL uses a form that its adapter cannot consume.
    #[error("invalid {backend} database URL: {message}")]
    InvalidDatabaseUrl {
        /// Stable selected backend name.
        backend: String,
        /// Safe validation detail without URL contents or credentials.
        message: &'static str,
    },
    /// The selected adapter failed to initialize.
    #[error("storage backend initialization failed: {0}")]
    Storage(#[from] StorageError),
}

/// Opens the selected complete backend through exhaustive static composition.
pub async fn open(settings: &StorageSettings) -> Result<StorageHandle, StorageOpenError> {
    let backend = settings.backend.trim().to_ascii_lowercase();
    match backend.as_str() {
        "sqlite" => open_sqlite(settings).await,
        _ => Err(StorageOpenError::UnknownBackend { backend }),
    }
}

async fn open_sqlite(settings: &StorageSettings) -> Result<StorageHandle, StorageOpenError> {
    let path = match settings.database_url.as_deref() {
        None => settings.default_sqlite_path.clone(),
        Some(url) => sqlite_path(url)?,
    };
    let storage = if settings.read_only {
        SqliteStorage::connect_read_only(path).await?
    } else if settings.exclusive {
        SqliteStorage::connect_exclusive(path).await?
    } else {
        SqliteStorage::connect(path).await?
    };
    Ok(StorageHandle {
        backend: Arc::from("sqlite"),
        storage: Arc::new(storage),
    })
}

fn sqlite_path(url: &str) -> Result<PathBuf, StorageOpenError> {
    let path = url
        .strip_prefix("sqlite://")
        .ok_or(StorageOpenError::InvalidDatabaseUrl {
            backend: "sqlite".to_owned(),
            message: "expected sqlite:// followed by an absolute or relative file path",
        })?;
    if path.is_empty() || path == ":memory:" {
        return Err(StorageOpenError::InvalidDatabaseUrl {
            backend: "sqlite".to_owned(),
            message: "a durable database file is required",
        });
    }
    Ok(PathBuf::from(path))
}

/// Opens an isolated complete backend for application-level tests.
#[doc(hidden)]
pub async fn open_test_storage() -> Result<StorageHandle, StorageOpenError> {
    Ok(StorageHandle {
        backend: Arc::from("sqlite"),
        storage: Arc::new(SqliteStorage::in_memory().await?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_debug_redacts_database_url() {
        let settings = StorageSettings::new(
            "postgresql",
            Some("postgresql://user:secret@example.invalid/stabbur".to_owned()),
            PathBuf::from("unused"),
            false,
        );
        let rendered = format!("{settings:?}");
        assert!(rendered.contains("[REDACTED]"));
        assert!(!rendered.contains("secret"));
    }

    #[tokio::test]
    async fn unknown_backend_fails_before_application_startup() {
        let settings = StorageSettings::new("oracle", None, PathBuf::from("unused"), false);
        assert!(matches!(
            open(&settings).await,
            Err(StorageOpenError::UnknownBackend { backend }) if backend == "oracle"
        ));
    }
}
