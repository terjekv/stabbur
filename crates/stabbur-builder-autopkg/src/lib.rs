//! AutoPkg detection and isolated, pinned-source execution on macOS workers.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
};

use serde::{Deserialize, Serialize};
use stabbur_builder_core::{
    BuildResult, RecipeCatalogDiagnostic, RecipeCatalogDiagnosticSeverity, RecipeCatalogEntry,
    RecipeCatalogManifest, RecipeCatalogSource, SourceProvenance, VerificationResult,
};
use stabbur_domain::{Architecture, ArtifactRole, MacOsVersion, Platform, Version};
use stabbur_jobs_core::{Capability, CapabilitySet};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::mpsc,
};
use url::Url;

const MAX_SOURCE_COUNT: usize = 16;
const MAX_SOURCE_URL_BYTES: usize = 2_048;
const MAX_ENTRYPOINT_BYTES: usize = 255;
const MAX_INPUT_COUNT: usize = 128;
const MAX_INPUT_NAME_BYTES: usize = 128;
const MAX_INPUT_VALUE_BYTES: usize = 8 * 1_024;
const MAX_VARIANT_COUNT: usize = 64;
const MAX_ARTIFACTS_PER_VARIANT: usize = 16;
const MAX_VERIFICATION_COUNT: usize = 128;
const MAX_REPORT_BYTES: u64 = 512 * 1_024;
const RECIPE_TRUST_POINTER: &str = "/stabbur/recipe_trust_succeeded";
const MAX_RECIPE_FILE_BYTES: u64 = 512 * 1_024;
const MAX_RECIPE_TREE_ENTRIES: usize = 50_000;
const MAX_RECIPE_TREE_DEPTH: usize = 32;
const MAX_RECEIPT_COUNT: usize = 8;
const MAX_RAW_REPORT_BYTES: usize = 1024 * 1024;

/// One immutable Git recipe source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedSource {
    /// HTTPS source URL.
    pub url: String,
    /// Full 40-character Git commit hash.
    pub commit: String,
}

impl PinnedSource {
    /// Validates an immutable source without leaking a URL-library type.
    pub fn validate(&self) -> Result<(), AutoPkgError> {
        if self.url.len() > MAX_SOURCE_URL_BYTES {
            return Err(AutoPkgError::InvalidSource);
        }
        let url = Url::parse(&self.url).map_err(|_| AutoPkgError::InvalidSource)?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(AutoPkgError::InvalidSource);
        }
        if self.commit.len() != 40
            || !self
                .commit
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(AutoPkgError::InvalidCommit);
        }
        Ok(())
    }
}

/// Immutable execution-specific AutoPkg recipe definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoPkgRecipe {
    /// One or more pinned recipe repositories.
    pub sources: Vec<PinnedSource>,
    /// AutoPkg recipe identifier, never interpolated into a shell command.
    pub entrypoint: String,
    /// Declared non-secret AutoPkg input overrides.
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
    /// Reviewed selectors that translate the generic report into builder-neutral output.
    pub output: AutoPkgOutputSelectors,
}

impl AutoPkgRecipe {
    /// Rejects unpinned sources, unsafe selectors, empty entrypoints, and secret-like inputs.
    pub fn validate(&self) -> Result<(), AutoPkgError> {
        if self.sources.is_empty()
            || self.sources.len() > MAX_SOURCE_COUNT
            || self.entrypoint.trim() != self.entrypoint
            || !(1..=MAX_ENTRYPOINT_BYTES).contains(&self.entrypoint.len())
            || !self
                .entrypoint
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
            || self.entrypoint.starts_with('-')
            || self.inputs.len() > MAX_INPUT_COUNT
        {
            return Err(AutoPkgError::InvalidRecipe);
        }
        for source in &self.sources {
            source.validate()?;
        }
        if self.inputs.iter().any(|(key, _value)| {
            let lower = key.to_ascii_lowercase();
            lower.contains("password") || lower.contains("token") || lower.contains("secret")
        }) {
            return Err(AutoPkgError::SensitiveInputRejected);
        }
        if self.inputs.iter().any(|(key, value)| {
            key.trim() != key
                || !(1..=MAX_INPUT_NAME_BYTES).contains(&key.len())
                || !key
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
                || key.starts_with('-')
                || value.len() > MAX_INPUT_VALUE_BYTES
                || value.chars().any(char::is_control)
        }) {
            return Err(AutoPkgError::InvalidRecipe);
        }
        self.output.validate()?;
        Ok(())
    }

    /// Independently checks the worker-selected evidence against this immutable definition.
    pub fn validate_build_result(
        &self,
        raw_report: &serde_json::Value,
        result: &BuildResult,
    ) -> Result<(), AutoPkgError> {
        let evidence = self.output.evidence(raw_report)?;
        if result.discovered_version != evidence.version
            || result.provenance.recipe_trust_succeeded != evidence.recipe_trust_succeeded
            || result.verification_results != evidence.verification_results
            || result.variants.len() != self.output.variants.len()
        {
            return Err(AutoPkgError::SelectorMismatch);
        }
        for (built, selected) in result.variants.iter().zip(&self.output.variants) {
            if built.platform != selected.platform
                || built.architecture != selected.architecture
                || built.minimum_macos != selected.minimum_macos
                || built.maximum_macos != selected.maximum_macos
                || built.resolution_priority != selected.resolution_priority
                || built.artifacts.len() != selected.artifacts.len()
                || built
                    .artifacts
                    .iter()
                    .zip(&selected.artifacts)
                    .any(|(artifact, selector)| artifact.role != selector.role)
            {
                return Err(AutoPkgError::SelectorMismatch);
            }
        }
        Ok(())
    }
}

/// Reviewed report selectors for a pinned AutoPkg revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoPkgOutputSelectors {
    /// JSON pointer resolving to the exact upstream version string.
    pub version_pointer: String,
    /// Reserved `/stabbur/recipe_trust_succeeded` pointer populated by the adapter.
    pub recipe_trust_pointer: String,
    /// Variant and artifact outputs selected from the report.
    pub variants: Vec<AutoPkgVariantSelector>,
    /// Required and advisory boolean verification outcomes.
    pub verification: Vec<AutoPkgVerificationSelector>,
}

impl AutoPkgOutputSelectors {
    fn validate(&self) -> Result<(), AutoPkgError> {
        validate_pointer(&self.version_pointer)?;
        if self.recipe_trust_pointer != RECIPE_TRUST_POINTER {
            return Err(AutoPkgError::InvalidSelector);
        }
        if self.variants.is_empty()
            || self.variants.len() > MAX_VARIANT_COUNT
            || self.verification.is_empty()
            || self.verification.len() > MAX_VERIFICATION_COUNT
            || !self.verification.iter().any(|check| check.required)
        {
            return Err(AutoPkgError::InvalidSelector);
        }
        for variant in &self.variants {
            variant.validate()?;
        }
        let mut names = std::collections::BTreeSet::new();
        for check in &self.verification {
            validate_pointer(&check.pointer)?;
            if check.name.trim() != check.name
                || !(1..=128).contains(&check.name.len())
                || !names.insert(&check.name)
            {
                return Err(AutoPkgError::InvalidSelector);
            }
        }
        Ok(())
    }

    fn evidence(&self, report: &serde_json::Value) -> Result<SelectedEvidence, AutoPkgError> {
        let version = report
            .pointer(&self.version_pointer)
            .and_then(serde_json::Value::as_str)
            .ok_or(AutoPkgError::InvalidReport)
            .and_then(|value| {
                Version::new(value.to_owned()).map_err(|_| AutoPkgError::InvalidReport)
            })?;
        let recipe_trust_succeeded = report
            .pointer(&self.recipe_trust_pointer)
            .and_then(serde_json::Value::as_bool)
            .ok_or(AutoPkgError::InvalidReport)?;
        let verification_results = self
            .verification
            .iter()
            .map(|selector| {
                let succeeded = report
                    .pointer(&selector.pointer)
                    .and_then(serde_json::Value::as_bool)
                    .ok_or(AutoPkgError::InvalidReport)?;
                Ok(VerificationResult {
                    check: selector.name.clone(),
                    required: selector.required,
                    succeeded,
                    detail: None,
                })
            })
            .collect::<Result<Vec<_>, AutoPkgError>>()?;
        Ok(SelectedEvidence {
            version,
            recipe_trust_succeeded,
            verification_results,
        })
    }
}

/// Compatibility and artifacts selected for one output variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoPkgVariantSelector {
    /// Target operating-system family.
    pub platform: Platform,
    /// Target architecture.
    pub architecture: Architecture,
    /// Inclusive minimum supported macOS version.
    pub minimum_macos: Option<MacOsVersion>,
    /// Inclusive maximum supported macOS version.
    pub maximum_macos: Option<MacOsVersion>,
    /// Explicit resolver priority.
    pub resolution_priority: i32,
    /// Selected files and their semantic roles.
    pub artifacts: Vec<AutoPkgArtifactSelector>,
}

impl AutoPkgVariantSelector {
    fn validate(&self) -> Result<(), AutoPkgError> {
        if self.artifacts.is_empty()
            || self.artifacts.len() > MAX_ARTIFACTS_PER_VARIANT
            || self
                .minimum_macos
                .as_ref()
                .zip(self.maximum_macos.as_ref())
                .is_some_and(|(minimum, maximum)| minimum > maximum)
            || (self.platform != Platform::MacOs
                && (self.minimum_macos.is_some() || self.maximum_macos.is_some()))
        {
            return Err(AutoPkgError::InvalidSelector);
        }
        for artifact in &self.artifacts {
            artifact.validate()?;
        }
        if self
            .artifacts
            .iter()
            .filter(|artifact| artifact.role == ArtifactRole::PrimaryInstaller)
            .count()
            != 1
        {
            return Err(AutoPkgError::InvalidSelector);
        }
        Ok(())
    }
}

/// One file path selected from the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoPkgArtifactSelector {
    /// JSON pointer resolving to an absolute or isolation-root-relative path string.
    pub path_pointer: String,
    /// Media type recorded with the immutable artifact.
    pub media_type: String,
    /// Semantic role within the selected variant.
    pub role: ArtifactRole,
}

impl AutoPkgArtifactSelector {
    fn validate(&self) -> Result<(), AutoPkgError> {
        validate_pointer(&self.path_pointer)?;
        if self.media_type.trim() != self.media_type
            || !(1..=255).contains(&self.media_type.len())
            || self.media_type.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(AutoPkgError::InvalidSelector);
        }
        Ok(())
    }
}

/// One boolean check selected from the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoPkgVerificationSelector {
    /// Stable result name.
    pub name: String,
    /// JSON pointer resolving to a boolean outcome.
    pub pointer: String,
    /// Whether candidate publication requires success.
    pub required: bool,
}

fn validate_pointer(pointer: &str) -> Result<(), AutoPkgError> {
    if pointer.starts_with('/') && pointer.len() <= 1024 {
        Ok(())
    } else {
        Err(AutoPkgError::InvalidSelector)
    }
}

#[derive(Debug)]
struct SelectedEvidence {
    version: Version,
    recipe_trust_succeeded: bool,
    verification_results: Vec<VerificationResult>,
}

/// A selected regular file that remains private to the worker integration.
#[derive(Debug)]
pub struct SelectedAutoPkgArtifact {
    /// Canonical file path contained by the attempt isolation root.
    pub path: PathBuf,
    /// Media type declared by the immutable revision.
    pub media_type: String,
    /// Semantic artifact role.
    pub role: ArtifactRole,
}

/// Selected files and compatibility for one variant.
#[derive(Debug)]
pub struct SelectedAutoPkgVariant {
    /// Target platform.
    pub platform: Platform,
    /// Target architecture.
    pub architecture: Architecture,
    /// Inclusive minimum supported macOS version.
    pub minimum_macos: Option<MacOsVersion>,
    /// Inclusive maximum supported macOS version.
    pub maximum_macos: Option<MacOsVersion>,
    /// Explicit resolver priority.
    pub resolution_priority: i32,
    /// Canonical selected artifact files.
    pub artifacts: Vec<SelectedAutoPkgArtifact>,
}

/// Safe selected output ready for worker-side hashing and upload.
#[derive(Debug)]
pub struct SelectedAutoPkgOutput {
    /// Exact discovered upstream version.
    pub discovered_version: Version,
    /// Selected output variants.
    pub variants: Vec<SelectedAutoPkgVariant>,
    /// Recipe trust result.
    pub recipe_trust_succeeded: bool,
    /// Selected verification evidence.
    pub verification_results: Vec<VerificationResult>,
}

/// Tool availability and versions detected by a worker at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoPkgAvailability {
    /// AutoPkg version output, absent when unavailable.
    pub autopkg_version: Option<String>,
    /// Xcode command-line tool version output, absent when unavailable.
    pub xcode_version: Option<String>,
}

impl AutoPkgAvailability {
    /// Adds only supported capabilities to a worker advertisement.
    #[must_use]
    pub fn capabilities(&self) -> CapabilitySet {
        let mut capabilities = vec![];
        if cfg!(target_os = "macos") {
            capabilities.push(Capability::new("os.macos").expect("static capability is valid"));
        }
        if self.autopkg_version.is_some() {
            capabilities
                .push(Capability::new("builder.autopkg").expect("static capability is valid"));
        }
        if self.xcode_version.is_some() {
            capabilities
                .push(Capability::new("tool.apple-xcode").expect("static capability is valid"));
        }
        CapabilitySet::new(capabilities)
    }
}

/// Adapter failure with backend paths and recipe inputs excluded from display output.
#[derive(Debug, Error)]
pub enum AutoPkgError {
    /// A source URL is not valid HTTPS.
    #[error("AutoPkg source URL must be an absolute HTTPS URL")]
    InvalidSource,
    /// A source is not pinned to a full lowercase commit hash.
    #[error("AutoPkg source must use a full lowercase Git commit hash")]
    InvalidCommit,
    /// The recipe definition is incomplete.
    #[error("AutoPkg recipe definition is invalid")]
    InvalidRecipe,
    /// v0.1 intentionally rejects secret-like inputs.
    #[error("sensitive AutoPkg inputs are not supported in v0.1")]
    SensitiveInputRejected,
    /// Required tooling is not installed.
    #[error("AutoPkg is not available on this worker")]
    Unavailable,
    /// A pinned source could not be materialized.
    #[error("a pinned AutoPkg source could not be materialized")]
    MaterializationFailed,
    /// AutoPkg returned a failed status.
    #[error("AutoPkg execution failed")]
    ExecutionFailed,
    /// AutoPkg rejected the entrypoint's stored parent-recipe trust information.
    #[error("AutoPkg recipe trust verification failed")]
    RecipeTrustFailed,
    /// The report plist is absent or malformed.
    #[error("AutoPkg report plist is invalid")]
    InvalidReport,
    /// An output selector is malformed or violates publication invariants.
    #[error("AutoPkg output selector is invalid")]
    InvalidSelector,
    /// A selected file is not a regular file contained by the attempt directory.
    #[error("AutoPkg selected an unsafe artifact path")]
    UnsafeArtifact,
    /// Selected evidence differs from the immutable recipe definition.
    #[error("AutoPkg result does not match its immutable selectors")]
    SelectorMismatch,
    /// Creation of an isolated execution directory failed.
    #[error("AutoPkg isolation setup failed")]
    IsolationFailed,
    /// A pinned recipe tree could not be normalized into the catalog contract.
    #[error("AutoPkg catalog generation failed")]
    CatalogGenerationFailed,
}

/// Captured adapter output before selected artifacts are uploaded by the worker.
#[derive(Debug)]
pub struct AutoPkgExecution {
    /// Process status.
    pub status: ExitStatus,
    /// Total standard-output bytes streamed without buffering the complete output.
    pub stdout_bytes: u64,
    /// Total standard-error bytes streamed without buffering the complete output.
    pub stderr_bytes: u64,
    /// Backend-neutral raw report representation.
    pub report: serde_json::Value,
    /// Fully pinned materialized sources.
    pub sources: Vec<SourceProvenance>,
}

/// AutoPkg process stream for one bounded exact-byte log chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoPkgLogStream {
    /// Process standard output.
    Stdout,
    /// Process standard error.
    Stderr,
}

/// One bounded exact-byte chunk emitted while AutoPkg is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoPkgLogChunk {
    /// Process stream.
    pub stream: AutoPkgLogStream,
    /// Exact bytes read from the child pipe.
    pub bytes: Vec<u8>,
}

/// Stateless direct-process AutoPkg adapter.
#[derive(Debug, Clone, Default)]
pub struct AutoPkgAdapter;

impl AutoPkgAdapter {
    /// Detects AutoPkg and Apple developer tooling without using a shell.
    pub async fn detect() -> AutoPkgAvailability {
        Self::detect_with_program(Path::new("autopkg")).await
    }

    /// Detects a worker-local AutoPkg executable without using a shell.
    pub async fn detect_with_program(program: &Path) -> AutoPkgAvailability {
        async fn version(program: &Path, arguments: &[&str]) -> Option<String> {
            let output = Command::new(program).args(arguments).output().await.ok()?;
            if !output.status.success() {
                return None;
            }
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            Some(if stdout.is_empty() { stderr } else { stdout })
        }

        AutoPkgAvailability {
            autopkg_version: version(program, &["version"]).await,
            xcode_version: version(Path::new("xcodebuild"), &["-version"]).await,
        }
    }

    /// Materializes all pinned sources, runs AutoPkg directly, and parses `--report-plist`.
    pub async fn execute(
        recipe: &AutoPkgRecipe,
        isolation_root: &Path,
    ) -> Result<AutoPkgExecution, AutoPkgError> {
        Self::execute_inner(recipe, isolation_root, Path::new("autopkg"), None).await
    }

    /// Executes AutoPkg while applying bounded backpressure to an ordered log channel.
    pub async fn execute_with_logs(
        recipe: &AutoPkgRecipe,
        isolation_root: &Path,
        logs: mpsc::Sender<AutoPkgLogChunk>,
    ) -> Result<AutoPkgExecution, AutoPkgError> {
        Self::execute_inner(recipe, isolation_root, Path::new("autopkg"), Some(logs)).await
    }

    /// Executes a worker-local AutoPkg executable while streaming ordered logs.
    pub async fn execute_with_program_and_logs(
        recipe: &AutoPkgRecipe,
        isolation_root: &Path,
        program: &Path,
        logs: mpsc::Sender<AutoPkgLogChunk>,
    ) -> Result<AutoPkgExecution, AutoPkgError> {
        Self::execute_inner(recipe, isolation_root, program, Some(logs)).await
    }

    /// Applies reviewed selectors and confines every selected file to the attempt directory.
    pub async fn select_outputs(
        recipe: &AutoPkgRecipe,
        report: &serde_json::Value,
        isolation_root: &Path,
    ) -> Result<SelectedAutoPkgOutput, AutoPkgError> {
        recipe.validate()?;
        let evidence = recipe.output.evidence(report)?;
        let canonical_root = tokio::fs::canonicalize(isolation_root)
            .await
            .map_err(|_| AutoPkgError::UnsafeArtifact)?;
        let mut variants = Vec::with_capacity(recipe.output.variants.len());
        for variant in &recipe.output.variants {
            let mut artifacts = Vec::with_capacity(variant.artifacts.len());
            for artifact in &variant.artifacts {
                let selected = report
                    .pointer(&artifact.path_pointer)
                    .and_then(serde_json::Value::as_str)
                    .ok_or(AutoPkgError::InvalidReport)?;
                let selected = PathBuf::from(selected);
                let selected = if selected.is_absolute() {
                    selected
                } else {
                    canonical_root.join(selected)
                };
                let link_metadata = tokio::fs::symlink_metadata(&selected)
                    .await
                    .map_err(|_| AutoPkgError::UnsafeArtifact)?;
                if link_metadata.file_type().is_symlink() {
                    return Err(AutoPkgError::UnsafeArtifact);
                }
                let canonical = tokio::fs::canonicalize(&selected)
                    .await
                    .map_err(|_| AutoPkgError::UnsafeArtifact)?;
                let metadata = tokio::fs::metadata(&canonical)
                    .await
                    .map_err(|_| AutoPkgError::UnsafeArtifact)?;
                if !canonical.starts_with(&canonical_root) || !metadata.is_file() {
                    return Err(AutoPkgError::UnsafeArtifact);
                }
                artifacts.push(SelectedAutoPkgArtifact {
                    path: canonical,
                    media_type: artifact.media_type.clone(),
                    role: artifact.role,
                });
            }
            variants.push(SelectedAutoPkgVariant {
                platform: variant.platform,
                architecture: variant.architecture,
                minimum_macos: variant.minimum_macos.clone(),
                maximum_macos: variant.maximum_macos.clone(),
                resolution_priority: variant.resolution_priority,
                artifacts,
            });
        }
        Ok(SelectedAutoPkgOutput {
            discovered_version: evidence.version,
            variants,
            recipe_trust_succeeded: evidence.recipe_trust_succeeded,
            verification_results: evidence.verification_results,
        })
    }

    async fn execute_inner(
        recipe: &AutoPkgRecipe,
        isolation_root: &Path,
        program: &Path,
        logs: Option<mpsc::Sender<AutoPkgLogChunk>>,
    ) -> Result<AutoPkgExecution, AutoPkgError> {
        recipe.validate()?;
        if !cfg!(target_os = "macos")
            || Self::detect_with_program(program)
                .await
                .autopkg_version
                .is_none()
        {
            return Err(AutoPkgError::Unavailable);
        }
        let directories = IsolationDirectories::create(isolation_root).await?;
        let log_redactions = log_redaction_needles(isolation_root).await?;
        let (provenance, recipe_dirs) = materialize_sources(recipe, &directories.sources).await?;
        let trust_method = discover_trust_method(&recipe_dirs, &recipe.entrypoint).await?;
        let process = AutoPkgProcessContext {
            program,
            directories: &directories,
            logs,
            redactions: &log_redactions,
        };
        let (trust_stdout_bytes, trust_stderr_bytes) =
            verify_recipe_trust(trust_method, recipe, &recipe_dirs, &process).await?;
        let (status, run_stdout_bytes, run_stderr_bytes, report) =
            run_recipe(recipe, &recipe_dirs, isolation_root, trust_method, &process).await?;
        let stdout_bytes = trust_stdout_bytes
            .checked_add(run_stdout_bytes)
            .ok_or(AutoPkgError::ExecutionFailed)?;
        let stderr_bytes = trust_stderr_bytes
            .checked_add(run_stderr_bytes)
            .ok_or(AutoPkgError::ExecutionFailed)?;
        Ok(AutoPkgExecution {
            status,
            stdout_bytes,
            stderr_bytes,
            report,
            sources: provenance,
        })
    }
}

/// Stateless generator for canonical manifests from one pinned AutoPkg repository.
#[derive(Debug, Clone, Default)]
pub struct AutoPkgCatalogGenerator;

impl AutoPkgCatalogGenerator {
    /// Materializes the exact source revision and scans bounded recipe documents without running
    /// recipes or exposing worker-local paths.
    pub async fn generate(
        source: &PinnedSource,
        isolation_root: &Path,
    ) -> Result<RecipeCatalogManifest, AutoPkgError> {
        source.validate()?;
        let source_root = isolation_root.join("source");
        materialize(source, &source_root).await?;
        let source_url = source.url.clone();
        let source_revision = source.commit.clone();
        let (recipes, diagnostics) =
            tokio::task::spawn_blocking(move || scan_catalog_source(&source_root))
                .await
                .map_err(|_| AutoPkgError::CatalogGenerationFailed)??;
        let manifest = RecipeCatalogManifest {
            schema_version: RecipeCatalogManifest::SCHEMA_VERSION,
            producer: "autopkg".to_owned(),
            source: RecipeCatalogSource {
                locator: source_url,
                revision: source_revision,
            },
            recipes,
            diagnostics,
        };
        manifest
            .validate()
            .map_err(|_| AutoPkgError::CatalogGenerationFailed)?;
        Ok(manifest)
    }
}

fn scan_catalog_source(
    root: &Path,
) -> Result<(Vec<RecipeCatalogEntry>, Vec<RecipeCatalogDiagnostic>), AutoPkgError> {
    let mut stack = vec![(root.to_path_buf(), 0_usize)];
    let mut entries_seen = 0_usize;
    let mut recipes = BTreeMap::<String, RecipeCatalogEntry>::new();
    let mut invalid_documents = 0_usize;
    let mut duplicate_identifiers = 0_usize;
    while let Some((directory, depth)) = stack.pop() {
        let entries =
            std::fs::read_dir(directory).map_err(|_| AutoPkgError::CatalogGenerationFailed)?;
        for entry in entries {
            let entry = entry.map_err(|_| AutoPkgError::CatalogGenerationFailed)?;
            entries_seen = entries_seen
                .checked_add(1)
                .ok_or(AutoPkgError::CatalogGenerationFailed)?;
            if entries_seen > MAX_RECIPE_TREE_ENTRIES {
                return Err(AutoPkgError::CatalogGenerationFailed);
            }
            let file_type = entry
                .file_type()
                .map_err(|_| AutoPkgError::CatalogGenerationFailed)?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if depth >= MAX_RECIPE_TREE_DEPTH {
                    return Err(AutoPkgError::CatalogGenerationFailed);
                }
                stack.push((entry.path(), depth + 1));
                continue;
            }
            if !file_type.is_file() || !is_recipe_path(&entry.path()) {
                continue;
            }
            let metadata = entry
                .metadata()
                .map_err(|_| AutoPkgError::CatalogGenerationFailed)?;
            if metadata.len() > MAX_RECIPE_FILE_BYTES {
                invalid_documents += 1;
                continue;
            }
            let path = entry.path();
            let Some(recipe) = recipe_catalog_entry(&path)? else {
                invalid_documents += 1;
                continue;
            };
            if recipes.insert(recipe.identifier.clone(), recipe).is_some() {
                duplicate_identifiers += 1;
            }
        }
    }
    let mut diagnostics = Vec::new();
    if duplicate_identifiers > 0 {
        diagnostics.push(RecipeCatalogDiagnostic {
            identifier: None,
            code: "duplicate_recipe_identifiers".to_owned(),
            severity: RecipeCatalogDiagnosticSeverity::Warning,
            detail: format!(
                "{duplicate_identifiers} duplicate recipe identifier(s) were collapsed."
            ),
        });
    }
    if invalid_documents > 0 {
        diagnostics.push(RecipeCatalogDiagnostic {
            identifier: None,
            code: "invalid_recipe_documents".to_owned(),
            severity: RecipeCatalogDiagnosticSeverity::Warning,
            detail: format!(
                "{invalid_documents} recipe document(s) were skipped because they were invalid or oversized."
            ),
        });
    }
    Ok((recipes.into_values().collect(), diagnostics))
}

fn recipe_catalog_entry(path: &Path) -> Result<Option<RecipeCatalogEntry>, AutoPkgError> {
    let bytes = std::fs::read(path).map_err(|_| AutoPkgError::CatalogGenerationFailed)?;
    let Some(document) = parse_recipe_document(path, &bytes) else {
        return Ok(None);
    };
    let Some(identifier) = document
        .get("Identifier")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
    else {
        return Ok(None);
    };
    let parents = document
        .get("ParentRecipe")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .into_iter()
        .collect::<Vec<_>>();
    if !valid_catalog_text(&identifier) || parents.iter().any(|parent| !valid_catalog_text(parent))
    {
        return Ok(None);
    }
    Ok(Some(RecipeCatalogEntry {
        identifier,
        builder: "autopkg".to_owned(),
        parents,
        required_capabilities: CapabilitySet::new([
            Capability::new("builder.autopkg").expect("static capability is valid"),
            Capability::new("os.macos").expect("static capability is valid"),
        ]),
    }))
}

fn valid_catalog_text(value: &str) -> bool {
    value.trim() == value
        && (1..=2_048).contains(&value.len())
        && !value.chars().any(char::is_control)
}

struct IsolationDirectories {
    home: PathBuf,
    cache: PathBuf,
    sources: PathBuf,
    work: PathBuf,
}

impl IsolationDirectories {
    async fn create(root: &Path) -> Result<Self, AutoPkgError> {
        let directories = Self {
            home: root.join("home"),
            cache: root.join("cache"),
            sources: root.join("sources"),
            work: root.join("work"),
        };
        for directory in [
            &directories.home,
            &directories.cache,
            &directories.sources,
            &directories.work,
        ] {
            tokio::fs::create_dir_all(directory)
                .await
                .map_err(|_| AutoPkgError::IsolationFailed)?;
        }
        Ok(directories)
    }
}

struct AutoPkgProcessContext<'a> {
    program: &'a Path,
    directories: &'a IsolationDirectories,
    logs: Option<mpsc::Sender<AutoPkgLogChunk>>,
    redactions: &'a [Vec<u8>],
}

async fn materialize_sources(
    recipe: &AutoPkgRecipe,
    sources_root: &Path,
) -> Result<(Vec<SourceProvenance>, Vec<PathBuf>), AutoPkgError> {
    let mut provenance = Vec::with_capacity(recipe.sources.len());
    let mut recipe_dirs = Vec::with_capacity(recipe.sources.len());
    for (index, source) in recipe.sources.iter().enumerate() {
        let directory = sources_root.join(index.to_string());
        materialize(source, &directory).await?;
        recipe_dirs.push(directory);
        provenance.push(SourceProvenance {
            url: source.url.clone(),
            commit: source.commit.clone(),
        });
    }
    Ok((provenance, recipe_dirs))
}

async fn discover_trust_method(
    recipe_dirs: &[PathBuf],
    entrypoint: &str,
) -> Result<RecipeTrustMethod, AutoPkgError> {
    let recipe_dirs = recipe_dirs.to_vec();
    let entrypoint = entrypoint.to_owned();
    tokio::task::spawn_blocking(move || discover_recipe_trust_method(&recipe_dirs, &entrypoint))
        .await
        .map_err(|_| AutoPkgError::InvalidRecipe)?
}

async fn verify_recipe_trust(
    method: RecipeTrustMethod,
    recipe: &AutoPkgRecipe,
    recipe_dirs: &[PathBuf],
    context: &AutoPkgProcessContext<'_>,
) -> Result<(u64, u64), AutoPkgError> {
    if method == RecipeTrustMethod::PinnedSource {
        return Ok((0, 0));
    }
    let directories = context.directories;
    let mut command = isolated_autopkg_command(
        context.program,
        &directories.home,
        &directories.cache,
        &directories.work,
    );
    command.arg("verify-trust-info").arg(&recipe.entrypoint);
    append_search_directories(&mut command, recipe_dirs);
    let (status, stdout_bytes, stderr_bytes) =
        run_autopkg_process(&mut command, context.logs.clone(), context.redactions).await?;
    if !status.success() {
        return Err(AutoPkgError::RecipeTrustFailed);
    }
    Ok((stdout_bytes, stderr_bytes))
}

async fn run_recipe(
    recipe: &AutoPkgRecipe,
    recipe_dirs: &[PathBuf],
    isolation_root: &Path,
    trust_method: RecipeTrustMethod,
    context: &AutoPkgProcessContext<'_>,
) -> Result<(ExitStatus, u64, u64, serde_json::Value), AutoPkgError> {
    let directories = context.directories;
    let report_path = directories.work.join("report.plist");
    let mut command = isolated_autopkg_command(
        context.program,
        &directories.home,
        &directories.cache,
        &directories.work,
    );
    command
        .arg("run")
        .arg(&recipe.entrypoint)
        .arg("--report-plist")
        .arg(&report_path);
    append_search_directories(&mut command, recipe_dirs);
    for (key, value) in &recipe.inputs {
        command.arg("--key").arg(format!("{key}={value}"));
    }
    let (status, stdout_bytes, stderr_bytes) =
        run_autopkg_process(&mut command, context.logs.clone(), context.redactions).await?;
    if !status.success() {
        return Err(AutoPkgError::ExecutionFailed);
    }
    let report = read_and_enrich_report(
        &report_path,
        &directories.cache,
        isolation_root,
        trust_method,
    )
    .await?;
    Ok((status, stdout_bytes, stderr_bytes, report))
}

async fn read_and_enrich_report(
    report_path: &Path,
    cache: &Path,
    isolation_root: &Path,
    trust_method: RecipeTrustMethod,
) -> Result<serde_json::Value, AutoPkgError> {
    let report_size = tokio::fs::metadata(report_path)
        .await
        .map_err(|_| AutoPkgError::InvalidReport)?
        .len();
    if report_size > MAX_REPORT_BYTES {
        return Err(AutoPkgError::InvalidReport);
    }
    let report_bytes = tokio::fs::read(report_path)
        .await
        .map_err(|_| AutoPkgError::InvalidReport)?;
    let plist = plist::Value::from_reader_xml(report_bytes.as_slice())
        .or_else(|_| plist::Value::from_reader(std::io::Cursor::new(&report_bytes)))
        .map_err(|_| AutoPkgError::InvalidReport)?;
    let mut report = serde_json::to_value(plist).map_err(|_| AutoPkgError::InvalidReport)?;
    let cache = cache.to_path_buf();
    let receipts = tokio::task::spawn_blocking(move || collect_receipts(&cache))
        .await
        .map_err(|_| AutoPkgError::InvalidReport)??;
    let report_object = report.as_object_mut().ok_or(AutoPkgError::InvalidReport)?;
    if report_object.contains_key("stabbur") {
        return Err(AutoPkgError::InvalidReport);
    }
    report_object.insert(
        "stabbur".to_owned(),
        serde_json::json!({
            "recipe_trust_succeeded": true,
            "recipe_trust_method": trust_method.as_str(),
            "receipts": receipts,
        }),
    );
    let canonical_root = tokio::fs::canonicalize(isolation_root)
        .await
        .map_err(|_| AutoPkgError::InvalidReport)?;
    normalize_report_paths(&mut report, &canonical_root);
    if serde_json::to_vec(&report)
        .map_err(|_| AutoPkgError::InvalidReport)?
        .len()
        > MAX_RAW_REPORT_BYTES
    {
        return Err(AutoPkgError::InvalidReport);
    }
    Ok(report)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecipeTrustMethod {
    PinnedSource,
    AutoPkgParentTrust,
}

impl RecipeTrustMethod {
    const fn as_str(self) -> &'static str {
        match self {
            Self::PinnedSource => "pinned_source",
            Self::AutoPkgParentTrust => "autopkg_parent_trust",
        }
    }
}

fn discover_recipe_trust_method(
    directories: &[PathBuf],
    entrypoint: &str,
) -> Result<RecipeTrustMethod, AutoPkgError> {
    let mut stack = directories
        .iter()
        .cloned()
        .map(|directory| (directory, 0_usize))
        .collect::<Vec<_>>();
    let mut entries_seen = 0_usize;
    let mut matched = None;

    while let Some((directory, depth)) = stack.pop() {
        let entries = std::fs::read_dir(&directory).map_err(|_| AutoPkgError::InvalidRecipe)?;
        for entry in entries {
            let entry = entry.map_err(|_| AutoPkgError::InvalidRecipe)?;
            entries_seen = entries_seen
                .checked_add(1)
                .ok_or(AutoPkgError::InvalidRecipe)?;
            if entries_seen > MAX_RECIPE_TREE_ENTRIES {
                return Err(AutoPkgError::InvalidRecipe);
            }
            let file_type = entry.file_type().map_err(|_| AutoPkgError::InvalidRecipe)?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if depth >= MAX_RECIPE_TREE_DEPTH {
                    return Err(AutoPkgError::InvalidRecipe);
                }
                stack.push((entry.path(), depth + 1));
                continue;
            }
            if !file_type.is_file() || !is_recipe_path(&entry.path()) {
                continue;
            }
            let metadata = entry.metadata().map_err(|_| AutoPkgError::InvalidRecipe)?;
            if metadata.len() > MAX_RECIPE_FILE_BYTES {
                if entry.file_name().to_string_lossy() == entrypoint {
                    return Err(AutoPkgError::InvalidRecipe);
                }
                continue;
            }
            let bytes = std::fs::read(entry.path()).map_err(|_| AutoPkgError::InvalidRecipe)?;
            let document = parse_recipe_document(&entry.path(), &bytes);
            let filename_matches = entry.file_name().to_string_lossy() == entrypoint;
            let identifier_matches = document
                .as_ref()
                .and_then(|document| document.get("Identifier"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|identifier| identifier == entrypoint);
            if !filename_matches && !identifier_matches {
                continue;
            }
            let document = document.ok_or(AutoPkgError::InvalidRecipe)?;
            let parent = document
                .get("ParentRecipe")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|parent| !parent.trim().is_empty());
            let method = if parent {
                RecipeTrustMethod::AutoPkgParentTrust
            } else {
                RecipeTrustMethod::PinnedSource
            };
            if matched.replace(method).is_some() {
                return Err(AutoPkgError::InvalidRecipe);
            }
        }
    }

    matched.ok_or(AutoPkgError::InvalidRecipe)
}

fn is_recipe_path(path: &Path) -> bool {
    let filename = path
        .file_name()
        .map(|filename| filename.to_string_lossy())
        .unwrap_or_default();
    filename.ends_with(".recipe")
        || filename.ends_with(".recipe.yaml")
        || filename.ends_with(".recipe.yml")
}

fn parse_recipe_document(path: &Path, bytes: &[u8]) -> Option<serde_json::Value> {
    let filename = path.file_name()?.to_string_lossy();
    if filename.ends_with(".yaml") || filename.ends_with(".yml") {
        yaml_serde::from_slice(bytes).ok()
    } else {
        let plist = plist::Value::from_reader(std::io::Cursor::new(bytes)).ok()?;
        serde_json::to_value(plist).ok()
    }
}

fn collect_receipts(cache: &Path) -> Result<Vec<serde_json::Value>, AutoPkgError> {
    let mut stack = vec![(cache.to_path_buf(), 0_usize)];
    let mut entries_seen = 0_usize;
    let mut receipt_paths = Vec::new();
    while let Some((directory, depth)) = stack.pop() {
        let entries = std::fs::read_dir(directory).map_err(|_| AutoPkgError::InvalidReport)?;
        for entry in entries {
            let entry = entry.map_err(|_| AutoPkgError::InvalidReport)?;
            entries_seen = entries_seen
                .checked_add(1)
                .ok_or(AutoPkgError::InvalidReport)?;
            if entries_seen > MAX_RECIPE_TREE_ENTRIES {
                return Err(AutoPkgError::InvalidReport);
            }
            let file_type = entry.file_type().map_err(|_| AutoPkgError::InvalidReport)?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if depth >= MAX_RECIPE_TREE_DEPTH {
                    return Err(AutoPkgError::InvalidReport);
                }
                stack.push((entry.path(), depth + 1));
                continue;
            }
            let path = entry.path();
            let in_receipts_directory = path
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == "receipts");
            if file_type.is_file()
                && in_receipts_directory
                && path
                    .extension()
                    .is_some_and(|extension| extension == "plist")
            {
                let metadata = entry.metadata().map_err(|_| AutoPkgError::InvalidReport)?;
                if metadata.len() > MAX_REPORT_BYTES || receipt_paths.len() >= MAX_RECEIPT_COUNT {
                    return Err(AutoPkgError::InvalidReport);
                }
                receipt_paths.push(path);
            }
        }
    }
    receipt_paths.sort();
    receipt_paths
        .into_iter()
        .map(|path| {
            let bytes = std::fs::read(path).map_err(|_| AutoPkgError::InvalidReport)?;
            let plist = plist::Value::from_reader(std::io::Cursor::new(bytes))
                .map_err(|_| AutoPkgError::InvalidReport)?;
            serde_json::to_value(plist).map_err(|_| AutoPkgError::InvalidReport)
        })
        .collect()
}

fn normalize_report_paths(value: &mut serde_json::Value, canonical_root: &Path) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                normalize_report_paths(value, canonical_root);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values_mut() {
                normalize_report_paths(value, canonical_root);
            }
        }
        serde_json::Value::String(value) => {
            let path = Path::new(value);
            if !path.is_absolute() {
                return;
            }
            let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            *value = canonical.strip_prefix(canonical_root).map_or_else(
                |_| "[redacted absolute path]".to_owned(),
                |relative| relative.to_string_lossy().into_owned(),
            );
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn append_search_directories(command: &mut Command, directories: &[PathBuf]) {
    for directory in directories {
        command.arg("--search-dir").arg(directory);
    }
}

fn isolated_autopkg_command(program: &Path, home: &Path, cache: &Path, work: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .env_clear()
        .env("HOME", home)
        .env("AUTOPKG_CACHE_DIR", cache)
        .env(
            "PATH",
            "/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin",
        )
        .current_dir(work)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

async fn log_redaction_needles(isolation_root: &Path) -> Result<Vec<Vec<u8>>, AutoPkgError> {
    let canonical = tokio::fs::canonicalize(isolation_root)
        .await
        .map_err(|_| AutoPkgError::IsolationFailed)?;
    let mut needles = vec![
        isolation_root.as_os_str().as_encoded_bytes().to_vec(),
        canonical.as_os_str().as_encoded_bytes().to_vec(),
    ];
    needles.retain(|needle| !needle.is_empty());
    needles.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
    needles.dedup();
    Ok(needles)
}

struct LogRedactor {
    needles: Vec<Vec<u8>>,
    pending: Vec<u8>,
    holdback: usize,
}

impl LogRedactor {
    fn new(needles: &[Vec<u8>]) -> Self {
        Self {
            needles: needles.to_vec(),
            pending: Vec::new(),
            holdback: needles
                .iter()
                .map(Vec::len)
                .max()
                .unwrap_or(1)
                .saturating_sub(1),
        }
    }

    fn push(&mut self, bytes: &[u8], end_of_stream: bool) -> Vec<u8> {
        self.pending.extend_from_slice(bytes);
        let target = if end_of_stream {
            self.pending.len()
        } else {
            self.pending.len().saturating_sub(self.holdback)
        };
        let mut output = Vec::with_capacity(target);
        let mut cursor = 0_usize;
        while cursor < target {
            let next = self
                .needles
                .iter()
                .filter_map(|needle| {
                    self.pending[cursor..]
                        .windows(needle.len())
                        .position(|window| window == needle)
                        .map(|offset| (cursor + offset, needle.len()))
                })
                .min_by_key(|(start, _length)| *start);
            let Some((start, length)) = next.filter(|(start, _length)| *start < target) else {
                output.extend_from_slice(&self.pending[cursor..target]);
                cursor = target;
                break;
            };
            output.extend_from_slice(&self.pending[cursor..start]);
            output.extend_from_slice(b"[attempt]");
            cursor = start + length;
        }
        self.pending.drain(..cursor);
        output
    }
}

async fn run_autopkg_process(
    command: &mut Command,
    logs: Option<mpsc::Sender<AutoPkgLogChunk>>,
    redactions: &[Vec<u8>],
) -> Result<(ExitStatus, u64, u64), AutoPkgError> {
    let mut child = command.spawn().map_err(|_| AutoPkgError::ExecutionFailed)?;
    let stdout = child.stdout.take().ok_or(AutoPkgError::ExecutionFailed)?;
    let stderr = child.stderr.take().ok_or(AutoPkgError::ExecutionFailed)?;
    tokio::try_join!(
        async {
            child
                .wait()
                .await
                .map_err(|_| AutoPkgError::ExecutionFailed)
        },
        drain_log_stream(stdout, AutoPkgLogStream::Stdout, logs.clone(), redactions),
        drain_log_stream(stderr, AutoPkgLogStream::Stderr, logs, redactions),
    )
}

async fn drain_log_stream<R: AsyncRead + Unpin>(
    mut reader: R,
    stream: AutoPkgLogStream,
    logs: Option<mpsc::Sender<AutoPkgLogChunk>>,
    redactions: &[Vec<u8>],
) -> Result<u64, AutoPkgError> {
    let mut total = 0_u64;
    let mut buffer = vec![0_u8; 16 * 1024];
    let mut redactor = LogRedactor::new(redactions);
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|_| AutoPkgError::ExecutionFailed)?;
        if read == 0 {
            if let Some(logs) = &logs {
                let bytes = redactor.push(&[], true);
                if !bytes.is_empty() {
                    logs.send(AutoPkgLogChunk { stream, bytes })
                        .await
                        .map_err(|_| AutoPkgError::ExecutionFailed)?;
                }
            }
            return Ok(total);
        }
        total = total
            .checked_add(u64::try_from(read).map_err(|_| AutoPkgError::ExecutionFailed)?)
            .ok_or(AutoPkgError::ExecutionFailed)?;
        if let Some(logs) = &logs {
            let bytes = redactor.push(&buffer[..read], false);
            if !bytes.is_empty() {
                logs.send(AutoPkgLogChunk { stream, bytes })
                    .await
                    .map_err(|_| AutoPkgError::ExecutionFailed)?;
            }
        }
    }
}

async fn materialize(source: &PinnedSource, destination: &PathBuf) -> Result<(), AutoPkgError> {
    async fn git(destination: &Path, arguments: &[&str]) -> Result<(), AutoPkgError> {
        let status = Command::new("git")
            .args(arguments)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "/usr/bin/false")
            .current_dir(destination)
            .status()
            .await
            .map_err(|_| AutoPkgError::MaterializationFailed)?;
        if status.success() {
            Ok(())
        } else {
            Err(AutoPkgError::MaterializationFailed)
        }
    }

    tokio::fs::create_dir_all(destination)
        .await
        .map_err(|_| AutoPkgError::MaterializationFailed)?;
    git(destination, &["init", "--quiet"]).await?;
    git(destination, &["remote", "add", "origin", &source.url]).await?;
    git(
        destination,
        &["fetch", "--quiet", "--depth", "1", "origin", &source.commit],
    )
    .await?;
    git(
        destination,
        &["checkout", "--quiet", "--detach", &source.commit],
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    const DOCUMENTED_AUTOPKG_RELEASE_REVISION: &str =
        include_str!("../../../docs/examples/autopkg/autopkg-release-revision.json");

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct DocumentedRevision {
        builder: String,
        definition: AutoPkgRecipe,
        required_capabilities: Vec<String>,
    }

    fn source() -> PinnedSource {
        PinnedSource {
            url: "https://example.test/recipes.git".into(),
            commit: "a".repeat(40),
        }
    }

    fn output() -> AutoPkgOutputSelectors {
        AutoPkgOutputSelectors {
            version_pointer: "/version".into(),
            recipe_trust_pointer: RECIPE_TRUST_POINTER.into(),
            variants: vec![AutoPkgVariantSelector {
                platform: Platform::MacOs,
                architecture: Architecture::Universal,
                minimum_macos: Some("13.0".parse().unwrap()),
                maximum_macos: None,
                resolution_priority: 0,
                artifacts: vec![AutoPkgArtifactSelector {
                    path_pointer: "/artifact".into(),
                    media_type: "application/vnd.apple.installer+xml".into(),
                    role: ArtifactRole::PrimaryInstaller,
                }],
            }],
            verification: vec![AutoPkgVerificationSelector {
                name: "signature".into(),
                pointer: "/signature_valid".into(),
                required: true,
            }],
        }
    }

    #[test]
    fn documented_autopkg_release_revision_is_a_valid_real_definition() {
        let revision: DocumentedRevision =
            serde_json::from_str(DOCUMENTED_AUTOPKG_RELEASE_REVISION).unwrap();
        assert_eq!(revision.builder, "autopkg");
        assert!(revision.required_capabilities.is_empty());
        revision.definition.validate().unwrap();
        assert_eq!(
            revision.definition.sources[0].commit,
            "6c092b47e9c6324aa48758832b2597a0f3ff932e"
        );
        assert_eq!(
            revision.definition.entrypoint,
            "com.github.autopkg.download.AutoPkg-Release"
        );
    }

    #[test]
    fn sources_require_https_and_full_lowercase_commits() {
        assert!(source().validate().is_ok());
        let mut invalid = source();
        invalid.url = "file:///private/recipes".into();
        assert!(matches!(
            invalid.validate(),
            Err(AutoPkgError::InvalidSource)
        ));
        let mut invalid = source();
        invalid.url = "https://user:secret@example.test/recipes.git".into();
        assert!(matches!(
            invalid.validate(),
            Err(AutoPkgError::InvalidSource)
        ));
        let mut invalid = source();
        invalid.url = "https://example.test/recipes.git?token=secret".into();
        assert!(matches!(
            invalid.validate(),
            Err(AutoPkgError::InvalidSource)
        ));
        let mut invalid = source();
        invalid.commit = "abc123".into();
        assert!(matches!(
            invalid.validate(),
            Err(AutoPkgError::InvalidCommit)
        ));
    }

    #[test]
    fn v01_rejects_secret_like_input_names() {
        let recipe = AutoPkgRecipe {
            sources: vec![source()],
            entrypoint: "Firefox.pkg.recipe".into(),
            inputs: [("API_TOKEN".into(), "never-store-this".into())].into(),
            output: output(),
        };
        assert!(matches!(
            recipe.validate(),
            Err(AutoPkgError::SensitiveInputRejected)
        ));
    }

    #[test]
    fn recipe_json_defaults_inputs_and_rejects_unknown_fields() {
        let recipe = AutoPkgRecipe {
            sources: vec![source()],
            entrypoint: "Firefox.pkg.recipe".into(),
            inputs: BTreeMap::new(),
            output: output(),
        };
        let mut value = serde_json::to_value(&recipe).unwrap();
        value.as_object_mut().unwrap().remove("inputs");
        assert_eq!(
            serde_json::from_value::<AutoPkgRecipe>(value.clone())
                .unwrap()
                .inputs,
            BTreeMap::new()
        );

        value
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), serde_json::Value::Null);
        assert!(serde_json::from_value::<AutoPkgRecipe>(value).is_err());
    }

    #[test]
    fn recipe_arguments_and_output_cardinality_are_bounded() {
        let mut recipe = AutoPkgRecipe {
            sources: vec![source()],
            entrypoint: "--recipe-search-dir".into(),
            inputs: BTreeMap::new(),
            output: output(),
        };
        assert!(matches!(
            recipe.validate(),
            Err(AutoPkgError::InvalidRecipe)
        ));

        recipe.entrypoint = "Firefox.pkg.recipe".into();
        recipe.inputs.insert("BAD=KEY".into(), "value".into());
        assert!(matches!(
            recipe.validate(),
            Err(AutoPkgError::InvalidRecipe)
        ));

        recipe.inputs.clear();
        recipe.output.variants = std::iter::repeat_with(|| output().variants.remove(0))
            .take(MAX_VARIANT_COUNT + 1)
            .collect();
        assert!(matches!(
            recipe.validate(),
            Err(AutoPkgError::InvalidSelector)
        ));

        recipe.output = output();
        recipe.output.verification.clear();
        assert!(matches!(
            recipe.validate(),
            Err(AutoPkgError::InvalidSelector)
        ));

        recipe.output = output();
        recipe.output.recipe_trust_pointer = "/recipe_claimed_trusted".into();
        assert!(matches!(
            recipe.validate(),
            Err(AutoPkgError::InvalidSelector)
        ));
    }

    #[test]
    fn capabilities_never_advertise_autopkg_when_detection_failed() {
        let availability = AutoPkgAvailability {
            autopkg_version: None,
            xcode_version: None,
        };
        assert!(
            !availability
                .capabilities()
                .iter()
                .any(|cap| cap.as_str() == "builder.autopkg")
        );
    }

    #[test]
    fn materialized_sources_use_the_supported_autopkg_search_option() {
        let mut command = Command::new("autopkg");
        append_search_directories(
            &mut command,
            &[
                PathBuf::from("/attempt/sources/0"),
                PathBuf::from("/attempt/sources/1"),
            ],
        );
        let arguments = command
            .as_std()
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            arguments,
            [
                "--search-dir",
                "/attempt/sources/0",
                "--search-dir",
                "/attempt/sources/1"
            ]
        );
        assert!(
            !arguments
                .iter()
                .any(|argument| argument == "--recipe-search-dir")
        );
    }

    #[test]
    fn recipe_discovery_distinguishes_pinned_bases_from_trusted_overrides() {
        let temporary = tempfile::tempdir().unwrap();
        std::fs::write(
            temporary.path().join("Base.download.recipe.yaml"),
            b"Identifier: com.example.download.Base\nInput: {}\nProcess: []\n",
        )
        .unwrap();
        std::fs::write(
            temporary.path().join("Override.download.recipe"),
            br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>Identifier</key><string>local.download.Base</string>
<key>ParentRecipe</key><string>com.example.download.Base</string>
<key>ParentRecipeTrustInfo</key><dict/>
</dict></plist>"#,
        )
        .unwrap();
        let directories = [temporary.path().to_path_buf()];
        assert_eq!(
            discover_recipe_trust_method(&directories, "com.example.download.Base").unwrap(),
            RecipeTrustMethod::PinnedSource
        );
        assert_eq!(
            discover_recipe_trust_method(&directories, "Override.download.recipe").unwrap(),
            RecipeTrustMethod::AutoPkgParentTrust
        );
    }

    #[test]
    fn receipts_are_bounded_and_worker_paths_are_normalized() {
        let temporary = tempfile::tempdir().unwrap();
        let receipts = temporary.path().join("cache/recipe/receipts");
        std::fs::create_dir_all(&receipts).unwrap();
        std::fs::write(
            receipts.join("run.plist"),
            br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><array><dict>
<key>Processor</key><string>Fixture</string>
</dict></array></plist>"#,
        )
        .unwrap();
        let captured = collect_receipts(&temporary.path().join("cache")).unwrap();
        assert_eq!(captured[0][0]["Processor"], "Fixture");

        let artifact = temporary.path().join("artifact.pkg");
        std::fs::write(&artifact, b"fixture").unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let mut report = serde_json::json!({
            "artifact": artifact,
            "outside": outside.path(),
            "url": "https://example.test/file.pkg"
        });
        let root = std::fs::canonicalize(temporary.path()).unwrap();
        normalize_report_paths(&mut report, &root);
        assert_eq!(report["artifact"], "artifact.pkg");
        assert_eq!(report["outside"], "[redacted absolute path]");
        assert_eq!(report["url"], "https://example.test/file.pkg");
    }

    #[tokio::test]
    async fn bounded_log_drain_preserves_exact_bytes() {
        let (mut writer, reader) = tokio::io::duplex(128);
        let (sender, mut receiver) = mpsc::channel(2);
        let expected = vec![0, 1, 2, 0xff, b'\n'];
        let written = expected.clone();
        let write = tokio::spawn(async move {
            writer.write_all(&written).await.unwrap();
            writer.shutdown().await.unwrap();
        });
        let count = drain_log_stream(reader, AutoPkgLogStream::Stdout, Some(sender), &[])
            .await
            .unwrap();
        write.await.unwrap();
        let chunk = receiver.recv().await.unwrap();
        assert_eq!(chunk.stream, AutoPkgLogStream::Stdout);
        assert_eq!(chunk.bytes, expected);
        assert_eq!(count, 5);
    }

    #[test]
    fn log_redaction_covers_matches_split_across_read_boundaries() {
        let needle = b"/private/worker/attempt".to_vec();
        let mut redactor = LogRedactor::new(std::slice::from_ref(&needle));
        let chunks = [
            redactor.push(b"path=/private/worker/att", false),
            redactor.push(b"empt/cache/file.pkg\n", false),
            redactor.push(&[], true),
        ];
        let output = chunks.concat();
        assert_eq!(output, b"path=[attempt]/cache/file.pkg\n");
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.windows(9).any(|window| window == b"[attempt]"))
        );
        assert!(!output.windows(needle.len()).any(|window| window == needle));
    }

    #[test]
    fn catalog_scan_normalizes_recipe_documents_and_safe_diagnostics() {
        let temporary = tempfile::tempdir().unwrap();
        std::fs::write(
            temporary.path().join("Alpha.download.recipe.yaml"),
            "Identifier: com.example.alpha\nParentRecipe: com.example.base\n",
        )
        .unwrap();
        std::fs::write(
            temporary.path().join("Beta.pkg.recipe"),
            br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>Identifier</key><string>com.example.beta</string></dict></plist>"#,
        )
        .unwrap();
        std::fs::write(temporary.path().join("Broken.recipe.yaml"), "[").unwrap();
        let (recipes, diagnostics) = scan_catalog_source(temporary.path()).unwrap();
        assert_eq!(
            recipes
                .iter()
                .map(|recipe| recipe.identifier.as_str())
                .collect::<Vec<_>>(),
            ["com.example.alpha", "com.example.beta"]
        );
        assert_eq!(recipes[0].parents, ["com.example.base"]);
        assert!(
            recipes[0]
                .required_capabilities
                .iter()
                .any(|capability| capability.as_str() == "builder.autopkg")
        );
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "invalid_recipe_documents");
        assert!(
            !diagnostics[0]
                .detail
                .contains(temporary.path().to_string_lossy().as_ref())
        );
    }

    #[tokio::test]
    async fn selectors_accept_regular_files_inside_attempt_root() {
        let temporary = tempfile::tempdir().unwrap();
        let artifact = temporary.path().join("output.pkg");
        tokio::fs::write(&artifact, b"artifact").await.unwrap();
        let recipe = AutoPkgRecipe {
            sources: vec![source()],
            entrypoint: "Firefox.pkg.recipe".into(),
            inputs: BTreeMap::new(),
            output: output(),
        };
        let report = serde_json::json!({
            "version": "128.0",
            "stabbur": {"recipe_trust_succeeded": true},
            "signature_valid": true,
            "artifact": "output.pkg"
        });
        let selected = AutoPkgAdapter::select_outputs(&recipe, &report, temporary.path())
            .await
            .unwrap();
        assert_eq!(selected.discovered_version.as_str(), "128.0");
        assert_eq!(
            selected.variants[0].artifacts[0].path,
            tokio::fs::canonicalize(artifact).await.unwrap()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn selectors_reject_symlinks_and_paths_outside_attempt_root() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let link = temporary.path().join("output.pkg");
        symlink(outside.path(), &link).unwrap();
        let recipe = AutoPkgRecipe {
            sources: vec![source()],
            entrypoint: "Firefox.pkg.recipe".into(),
            inputs: BTreeMap::new(),
            output: output(),
        };
        let report = serde_json::json!({
            "version": "128.0",
            "stabbur": {"recipe_trust_succeeded": true},
            "signature_valid": true,
            "artifact": link
        });
        assert!(matches!(
            AutoPkgAdapter::select_outputs(&recipe, &report, temporary.path()).await,
            Err(AutoPkgError::UnsafeArtifact)
        ));
    }
}
