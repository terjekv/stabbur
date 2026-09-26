//! Builder-neutral requests, results, verification evidence, and provenance.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use stabbur_domain::{
    Architecture, ArtifactRole, MacOsVersion, Platform, RecipeCatalogScanId, RecipeRevisionId,
    RunId, Sha256Digest, SoftwareId, VariantId, Version,
};
use stabbur_jobs_core::{Capability, CapabilitySet};
use thiserror::Error;

const MAX_CATALOG_RECIPES: usize = 50_000;
const MAX_CATALOG_DIAGNOSTICS: usize = 2_000;
const MAX_CATALOG_TEXT_BYTES: usize = 2_048;
const MAX_CATALOG_MANIFEST_BYTES: usize = 1024 * 1024;

/// One builder-neutral, pinned catalog source observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeCatalogSource {
    /// Stable source locator, such as an HTTPS repository URL.
    pub locator: String,
    /// Exact opaque source revision observed by the producer.
    pub revision: String,
}

/// Validated, canonical source closure for importing a discovered recipe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    try_from = "Vec<RecipeCatalogSource>",
    into = "Vec<RecipeCatalogSource>"
)]
pub struct RecipeImportSources(Vec<RecipeCatalogSource>);

impl RecipeImportSources {
    /// Validates a nonempty, bounded set of exact source observations.
    pub fn new(mut sources: Vec<RecipeCatalogSource>) -> Result<Self, RecipeCatalogError> {
        if sources.is_empty() || sources.len() > 16 {
            return Err(RecipeCatalogError::TooLarge);
        }
        for source in &sources {
            validate_catalog_text(&source.locator)?;
            validate_catalog_text(&source.revision)?;
        }
        sources.sort_by(|a, b| a.locator.cmp(&b.locator));
        if sources
            .windows(2)
            .any(|pair| pair[0].locator == pair[1].locator)
        {
            return Err(RecipeCatalogError::NonCanonical);
        }
        Ok(Self(sources))
    }

    /// Returns the immutable pinned observations.
    pub fn as_slice(&self) -> &[RecipeCatalogSource] {
        &self.0
    }
}

impl TryFrom<Vec<RecipeCatalogSource>> for RecipeImportSources {
    type Error = RecipeCatalogError;
    fn try_from(value: Vec<RecipeCatalogSource>) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
impl From<RecipeImportSources> for Vec<RecipeCatalogSource> {
    fn from(value: RecipeImportSources) -> Self {
        value.0
    }
}

/// One normalized recipe observed in a catalog source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeCatalogEntry {
    /// Complete pinned source closure, absent when discovery cannot prove reproducibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_sources: Option<RecipeImportSources>,
    /// Builder-owned stable recipe identifier or entrypoint.
    pub identifier: String,
    /// Stable builder adapter selector, such as `autopkg`.
    pub builder: String,
    /// Optional normalized parent-recipe identifiers.
    #[serde(default)]
    pub parents: Vec<String>,
    /// Capabilities required to execute this recipe.
    pub required_capabilities: CapabilitySet,
}

/// Severity of one catalog validation diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecipeCatalogDiagnosticSeverity {
    /// Informational source metadata.
    Info,
    /// A recipe may require operator attention.
    Warning,
    /// A recipe could not be normalized or validated.
    Error,
}

/// Safe validation information produced while scanning a catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeCatalogDiagnostic {
    /// Recipe identifier when the diagnostic belongs to one entry.
    pub identifier: Option<String>,
    /// Stable machine-readable diagnostic code.
    pub code: String,
    /// Severity independent of builder implementation types.
    pub severity: RecipeCatalogDiagnosticSeverity,
    /// Bounded operator-facing detail without worker paths or secrets.
    pub detail: String,
}

/// Versioned builder-neutral snapshot published by an authenticated worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeCatalogManifest {
    /// Catalog protocol schema version.
    pub schema_version: u32,
    /// Stable producer adapter name, such as `autopkg`.
    pub producer: String,
    /// Pinned source that was inspected.
    pub source: RecipeCatalogSource,
    /// Sorted normalized recipes discovered at the exact source revision.
    pub recipes: Vec<RecipeCatalogEntry>,
    /// Sorted safe validation diagnostics.
    #[serde(default)]
    pub diagnostics: Vec<RecipeCatalogDiagnostic>,
}

impl RecipeCatalogManifest {
    /// Current catalog manifest schema version.
    pub const SCHEMA_VERSION: u32 = 1;

    /// Validates bounds and canonical ordering before hashing or persistence.
    pub fn validate(&self) -> Result<(), RecipeCatalogError> {
        if self.schema_version != Self::SCHEMA_VERSION {
            return Err(RecipeCatalogError::UnsupportedSchema);
        }
        validate_catalog_token(&self.producer, 64)?;
        validate_catalog_text(&self.source.locator)?;
        validate_catalog_text(&self.source.revision)?;
        if self.recipes.len() > MAX_CATALOG_RECIPES
            || self.diagnostics.len() > MAX_CATALOG_DIAGNOSTICS
        {
            return Err(RecipeCatalogError::TooLarge);
        }
        let mut previous_identifier: Option<&str> = None;
        for recipe in &self.recipes {
            validate_catalog_text(&recipe.identifier)?;
            validate_catalog_token(&recipe.builder, 64)?;
            let builder_capability = Capability::new(format!("builder.{}", recipe.builder))
                .map_err(|_| RecipeCatalogError::InvalidToken)?;
            if recipe.builder != self.producer
                || !recipe
                    .required_capabilities
                    .satisfies(&CapabilitySet::new([builder_capability]))
            {
                return Err(RecipeCatalogError::BuilderMismatch);
            }
            if previous_identifier.is_some_and(|previous| previous >= recipe.identifier.as_str()) {
                return Err(RecipeCatalogError::NonCanonical);
            }
            previous_identifier = Some(&recipe.identifier);
            let mut previous_parent: Option<&str> = None;
            for parent in &recipe.parents {
                validate_catalog_text(parent)?;
                if previous_parent.is_some_and(|previous| previous >= parent.as_str()) {
                    return Err(RecipeCatalogError::NonCanonical);
                }
                previous_parent = Some(parent);
            }
        }
        let mut previous_diagnostic: Option<(&str, &str)> = None;
        for diagnostic in &self.diagnostics {
            if let Some(identifier) = &diagnostic.identifier {
                validate_catalog_text(identifier)?;
            }
            validate_catalog_token(&diagnostic.code, 128)?;
            validate_catalog_text(&diagnostic.detail)?;
            let key = (
                diagnostic.identifier.as_deref().unwrap_or(""),
                diagnostic.code.as_str(),
            );
            if previous_diagnostic.is_some_and(|previous| previous >= key) {
                return Err(RecipeCatalogError::NonCanonical);
            }
            previous_diagnostic = Some(key);
        }
        Ok(())
    }

    /// Returns the lowercase SHA-256 digest of canonical validated JSON.
    pub fn canonical_digest(&self) -> Result<Sha256Digest, RecipeCatalogError> {
        self.validate()?;
        let json = serde_json::to_vec(self).map_err(|_| RecipeCatalogError::Serialization)?;
        if json.len() > MAX_CATALOG_MANIFEST_BYTES {
            return Err(RecipeCatalogError::TooLarge);
        }
        Sha256Digest::new(hex::encode(Sha256::digest(json)))
            .map_err(|_| RecipeCatalogError::Serialization)
    }
}

fn validate_catalog_text(value: &str) -> Result<(), RecipeCatalogError> {
    if value.trim() == value
        && (1..=MAX_CATALOG_TEXT_BYTES).contains(&value.len())
        && !value.chars().any(char::is_control)
    {
        Ok(())
    } else {
        Err(RecipeCatalogError::InvalidText)
    }
}

fn validate_catalog_token(value: &str, maximum: usize) -> Result<(), RecipeCatalogError> {
    if (1..=maximum).contains(&value.len())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
    {
        Ok(())
    } else {
        Err(RecipeCatalogError::InvalidToken)
    }
}

/// Validation failure for a worker-published catalog manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RecipeCatalogError {
    /// The producer uses an unsupported wire schema.
    #[error("catalog manifest schema version is unsupported")]
    UnsupportedSchema,
    /// A bounded source, recipe, or diagnostic value is malformed.
    #[error("catalog manifest text is invalid")]
    InvalidText,
    /// A producer, builder, or diagnostic code is malformed.
    #[error("catalog manifest token is invalid")]
    InvalidToken,
    /// Recipes or diagnostics exceed protocol bounds.
    #[error("catalog manifest exceeds protocol bounds")]
    TooLarge,
    /// Entries, parents, or diagnostics are not strictly sorted and unique.
    #[error("catalog manifest is not in canonical order")]
    NonCanonical,
    /// An entry does not belong to the producer or omit its builder capability.
    #[error("catalog recipe builder does not match its producer capability")]
    BuilderMismatch,
    /// A validated manifest could not be represented canonically.
    #[error("catalog manifest serialization failed")]
    Serialization,
}

/// Builder-neutral request for one pinned recipe catalog observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeCatalogScanRequest {
    /// Durable scan identity receiving the terminal observation.
    pub scan_id: RecipeCatalogScanId,
    /// Stable worker adapter selector, such as `autopkg`.
    pub producer: String,
    /// Exact immutable source to inspect.
    pub source: RecipeCatalogSource,
    /// Capabilities required to perform the scan.
    pub required_capabilities: CapabilitySet,
}

impl RecipeCatalogScanRequest {
    /// Validates source metadata and the producer capability invariant.
    pub fn validate(&self) -> Result<(), RecipeCatalogError> {
        validate_catalog_token(&self.producer, 64)?;
        validate_catalog_text(&self.source.locator)?;
        validate_catalog_text(&self.source.revision)?;
        let producer = Capability::new(format!("builder.{}", self.producer))
            .map_err(|_| RecipeCatalogError::InvalidToken)?;
        if !self
            .required_capabilities
            .satisfies(&CapabilitySet::new([producer]))
        {
            return Err(RecipeCatalogError::BuilderMismatch);
        }
        Ok(())
    }
}

/// Versioned catalog-scan envelope dispatched through the durable worker queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeCatalogScanJob {
    /// Worker protocol schema version.
    pub schema_version: u32,
    /// Builder-neutral scan request.
    pub request: RecipeCatalogScanRequest,
    /// Server-issued wall-clock execution deadline.
    pub execution_timeout_seconds: u32,
}

impl RecipeCatalogScanJob {
    /// Current catalog-scan worker protocol version.
    pub const SCHEMA_VERSION: u32 = 1;
    /// v0.0.1 control-plane scan deadline.
    pub const DEFAULT_EXECUTION_TIMEOUT_SECONDS: u32 = 30 * 60;

    /// Creates the current versioned catalog-scan envelope.
    #[must_use]
    pub const fn new(request: RecipeCatalogScanRequest) -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            request,
            execution_timeout_seconds: Self::DEFAULT_EXECUTION_TIMEOUT_SECONDS,
        }
    }

    /// Validates the version and builder-neutral request.
    pub fn validate(&self) -> Result<(), RecipeCatalogError> {
        if self.schema_version != Self::SCHEMA_VERSION || self.execution_timeout_seconds == 0 {
            return Err(RecipeCatalogError::UnsupportedSchema);
        }
        self.request.validate()
    }
}

/// Typed successful catalog observation submitted by a leased worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeCatalogScanExecutionResult {
    /// Worker protocol schema version.
    pub schema_version: u32,
    /// Durable scan receiving this observation.
    pub scan_id: RecipeCatalogScanId,
    /// Canonical builder-neutral manifest generated from the pinned source.
    pub manifest: RecipeCatalogManifest,
    /// Worker completion time.
    pub completed_at: DateTime<Utc>,
}

impl RecipeCatalogScanExecutionResult {
    /// Validates the result against the immutable scan request.
    pub fn validate_for(
        &self,
        request: &RecipeCatalogScanRequest,
    ) -> Result<(), RecipeCatalogError> {
        if self.schema_version != RecipeCatalogScanJob::SCHEMA_VERSION {
            return Err(RecipeCatalogError::UnsupportedSchema);
        }
        self.manifest.validate()?;
        if self.scan_id != request.scan_id
            || self.manifest.producer != request.producer
            || self.manifest.source != request.source
        {
            return Err(RecipeCatalogError::BuilderMismatch);
        }
        Ok(())
    }
}

/// Typed safe terminal failure for a catalog scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeCatalogScanExecutionFailure {
    /// Worker protocol schema version.
    pub schema_version: u32,
    /// Durable scan receiving this failure.
    pub scan_id: RecipeCatalogScanId,
    /// Stable producer when the envelope was readable.
    pub producer: Option<String>,
    /// Stable machine-readable failure code.
    pub code: String,
    /// Safe operator-facing detail without native paths or credentials.
    pub detail: String,
    /// Worker failure time.
    pub failed_at: DateTime<Utc>,
}

impl RecipeCatalogScanExecutionFailure {
    /// Validates safe bounded failure metadata against the immutable request.
    pub fn validate_for(
        &self,
        request: &RecipeCatalogScanRequest,
    ) -> Result<(), RecipeCatalogError> {
        if self.schema_version != RecipeCatalogScanJob::SCHEMA_VERSION {
            return Err(RecipeCatalogError::UnsupportedSchema);
        }
        if self.scan_id != request.scan_id
            || self
                .producer
                .as_ref()
                .is_some_and(|producer| producer != &request.producer)
        {
            return Err(RecipeCatalogError::BuilderMismatch);
        }
        if let Some(producer) = &self.producer {
            validate_catalog_token(producer, 64)?;
        }
        validate_catalog_token(&self.code, 128)?;
        validate_catalog_text(&self.detail)
    }
}

/// A deliberately non-secret recipe input value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BuildParameter {
    /// Text input.
    String(String),
    /// Boolean input.
    Boolean(bool),
    /// Signed numeric input.
    Integer(i64),
    /// Ordered list of non-secret inputs.
    List(Vec<BuildParameter>),
}

/// A builder-neutral immutable request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BuildRequest {
    /// Run receiving logs and the terminal result.
    pub run_id: RunId,
    /// Software aggregate being built.
    pub software: SoftwareId,
    /// Pinned immutable recipe revision.
    pub recipe_revision: RecipeRevisionId,
    /// Declared non-secret inputs.
    pub parameters: BTreeMap<String, BuildParameter>,
    /// Capabilities the executing worker must advertise.
    pub required_capabilities: CapabilitySet,
}

/// Versioned durable job envelope dispatched without leaking an adapter implementation type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BuilderJob {
    /// Worker protocol schema version.
    pub schema_version: u32,
    /// Builder-neutral build request.
    pub request: BuildRequest,
    /// Stable adapter selector such as `autopkg`.
    pub adapter: String,
    /// Immutable adapter definition owned by the selected worker adapter.
    pub adapter_definition: serde_json::Value,
    /// Server-issued wall-clock execution deadline.
    pub execution_timeout_seconds: u32,
}

impl BuilderJob {
    /// Current worker job schema version.
    pub const SCHEMA_VERSION: u32 = 1;
    /// v0.0.1 control-plane execution deadline.
    pub const DEFAULT_EXECUTION_TIMEOUT_SECONDS: u32 = 6 * 60 * 60;

    /// Creates the current versioned worker envelope.
    #[must_use]
    pub fn new(
        request: BuildRequest,
        adapter: impl Into<String>,
        adapter_definition: serde_json::Value,
    ) -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            request,
            adapter: adapter.into(),
            adapter_definition,
            execution_timeout_seconds: Self::DEFAULT_EXECUTION_TIMEOUT_SECONDS,
        }
    }
}

/// Typed successful worker execution submitted after artifact upload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BuilderExecutionResult {
    /// Worker protocol schema version.
    pub schema_version: u32,
    /// Run receiving this result.
    pub run_id: RunId,
    /// Stable builder adapter name.
    pub adapter: String,
    /// Fully pinned source provenance.
    pub sources: Vec<SourceProvenance>,
    /// Detected tool versions.
    pub tools: BTreeMap<String, String>,
    /// Parsed raw builder report before server-side selectors are applied.
    pub raw_report: serde_json::Value,
    /// Builder-neutral selected output, including uploaded artifact identities.
    ///
    /// This is absent only for the deterministic fake adapter, which exercises job mechanics
    /// without publishing a release. Production builders must submit a value.
    pub build_result: Option<BuildResult>,
    /// Worker completion time.
    pub completed_at: DateTime<Utc>,
}

/// Typed terminal worker failure safe to persist and show to operators.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuilderExecutionFailure {
    /// Worker protocol schema version.
    pub schema_version: u32,
    /// Run receiving this failure.
    pub run_id: RunId,
    /// Stable builder adapter name when the envelope was readable.
    pub adapter: Option<String>,
    /// Stable failure code without native paths or secrets.
    pub code: String,
    /// Safe operator-facing detail.
    pub detail: String,
    /// Worker failure time.
    pub failed_at: DateTime<Utc>,
}

/// Compatibility and artifact roles discovered for one result variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuiltVariant {
    /// Optional identity when updating an already-declared variant.
    pub variant_id: Option<VariantId>,
    /// Target operating-system family.
    pub platform: Platform,
    /// Target CPU architecture.
    pub architecture: Architecture,
    /// Inclusive minimum supported macOS version.
    pub minimum_macos: Option<MacOsVersion>,
    /// Inclusive maximum supported macOS version.
    pub maximum_macos: Option<MacOsVersion>,
    /// Explicit resolver priority.
    pub resolution_priority: i32,
    /// Artifacts associated with this variant.
    pub artifacts: Vec<VariantArtifact>,
}

/// An uploaded artifact and its semantic role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VariantArtifact {
    /// Server-verified digest.
    pub digest: Sha256Digest,
    /// Exact server-verified size.
    pub size: u64,
    /// Role within a variant.
    pub role: ArtifactRole,
}

/// Result of one required or advisory verification check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationResult {
    /// Stable check name.
    pub check: String,
    /// Whether candidate publication requires success.
    pub required: bool,
    /// Whether the check succeeded.
    pub succeeded: bool,
    /// Safe diagnostic summary.
    pub detail: Option<String>,
}

/// One pinned source included in provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceProvenance {
    /// Canonical source URL.
    pub url: String,
    /// Full immutable commit hash.
    pub commit: String,
}

/// Reproducibility and trust evidence captured by a builder adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    /// Stable builder adapter name and version.
    pub builder: String,
    /// Worker runtime version.
    pub worker_version: String,
    /// Host operating-system version.
    pub operating_system: String,
    /// Tool versions used during execution.
    pub tools: BTreeMap<String, String>,
    /// Fully pinned materialized sources.
    pub sources: Vec<SourceProvenance>,
    /// Whether recipe trust and signature policy succeeded.
    pub recipe_trust_succeeded: bool,
    /// Adapter-specific raw report represented without implementation types.
    pub raw_report: serde_json::Value,
    /// Capture time.
    pub captured_at: DateTime<Utc>,
}

/// Builder-neutral terminal result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BuildResult {
    /// Exact upstream version discovered by the recipe.
    pub discovered_version: Version,
    /// Produced variants.
    pub variants: Vec<BuiltVariant>,
    /// Digests uploaded before result submission.
    pub uploaded_artifacts: Vec<Sha256Digest>,
    /// Reproducibility and trust evidence.
    pub provenance: Provenance,
    /// Verification outcomes.
    pub verification_results: Vec<VerificationResult>,
}

impl BuildResult {
    /// Returns whether result-level evidence permits automatic candidate advancement.
    ///
    /// The application must additionally verify a readable `present` location in a primary
    /// store transactionally before publication.
    #[must_use]
    pub fn passes_candidate_gate(&self) -> bool {
        self.provenance.recipe_trust_succeeded
            && !self.uploaded_artifacts.is_empty()
            && self
                .verification_results
                .iter()
                .filter(|result| result.required)
                .all(|result| result.succeeded)
            && self.variants.iter().all(|variant| {
                variant
                    .artifacts
                    .iter()
                    .filter(|artifact| artifact.role == ArtifactRole::PrimaryInstaller)
                    .count()
                    == 1
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog_manifest() -> RecipeCatalogManifest {
        RecipeCatalogManifest {
            schema_version: RecipeCatalogManifest::SCHEMA_VERSION,
            producer: "autopkg".into(),
            source: RecipeCatalogSource {
                locator: "https://example.test/recipes.git".into(),
                revision: "a".repeat(40),
            },
            recipes: vec![RecipeCatalogEntry {
                import_sources: None,
                identifier: "com.example.firefox".into(),
                builder: "autopkg".into(),
                parents: vec!["com.example.download.firefox".into()],
                required_capabilities: CapabilitySet::new([
                    Capability::new("builder.autopkg").unwrap(),
                    Capability::new("os.macos").unwrap(),
                ]),
            }],
            diagnostics: vec![],
        }
    }

    fn result() -> BuildResult {
        let digest = Sha256Digest::new("a".repeat(64)).unwrap();
        BuildResult {
            discovered_version: Version::new("128.0").unwrap(),
            variants: vec![BuiltVariant {
                variant_id: None,
                platform: Platform::MacOs,
                architecture: Architecture::Universal,
                minimum_macos: Some("13.0".parse().unwrap()),
                maximum_macos: None,
                resolution_priority: 0,
                artifacts: vec![VariantArtifact {
                    digest: digest.clone(),
                    size: 42,
                    role: ArtifactRole::PrimaryInstaller,
                }],
            }],
            uploaded_artifacts: vec![digest],
            provenance: Provenance {
                builder: "fake/1".into(),
                worker_version: "0.0.1".into(),
                operating_system: "test".into(),
                tools: BTreeMap::new(),
                sources: vec![],
                recipe_trust_succeeded: true,
                raw_report: serde_json::json!({}),
                captured_at: Utc::now(),
            },
            verification_results: vec![VerificationResult {
                check: "digest".into(),
                required: true,
                succeeded: true,
                detail: None,
            }],
        }
    }

    #[test]
    fn candidate_gate_requires_trust_checks_uploads_and_one_installer() {
        let mut result = result();
        assert!(result.passes_candidate_gate());
        result.verification_results[0].succeeded = false;
        assert!(!result.passes_candidate_gate());
        result.verification_results[0].succeeded = true;
        result.provenance.recipe_trust_succeeded = false;
        assert!(!result.passes_candidate_gate());
    }

    #[test]
    fn catalog_manifests_are_bounded_canonical_and_content_addressed() {
        let manifest = catalog_manifest();
        assert!(manifest.validate().is_ok());
        assert_eq!(
            manifest.canonical_digest().unwrap(),
            manifest.canonical_digest().unwrap()
        );

        let mut duplicate = manifest.clone();
        duplicate.recipes.push(duplicate.recipes[0].clone());
        assert_eq!(duplicate.validate(), Err(RecipeCatalogError::NonCanonical));

        let mut mismatched = manifest.clone();
        mismatched.recipes[0].builder = "fake".into();
        assert_eq!(
            mismatched.validate(),
            Err(RecipeCatalogError::BuilderMismatch)
        );

        let mut unsafe_diagnostic = manifest;
        unsafe_diagnostic.diagnostics.push(RecipeCatalogDiagnostic {
            identifier: None,
            code: "worker_path".into(),
            severity: RecipeCatalogDiagnosticSeverity::Error,
            detail: "private/path\nleak".into(),
        });
        assert_eq!(
            unsafe_diagnostic.validate(),
            Err(RecipeCatalogError::InvalidText)
        );
    }

    #[test]
    fn catalog_scan_failures_are_bounded_and_match_the_request() {
        let request = RecipeCatalogScanRequest {
            scan_id: RecipeCatalogScanId::new(),
            producer: "autopkg".into(),
            source: catalog_manifest().source,
            required_capabilities: CapabilitySet::new([
                Capability::new("builder.autopkg").unwrap(),
                Capability::new("os.macos").unwrap(),
            ]),
        };
        let mut failure = RecipeCatalogScanExecutionFailure {
            schema_version: RecipeCatalogScanJob::SCHEMA_VERSION,
            scan_id: request.scan_id,
            producer: Some("autopkg".into()),
            code: "source_checkout_failed".into(),
            detail: "The pinned catalog source could not be materialized.".into(),
            failed_at: Utc::now(),
        };
        assert!(failure.validate_for(&request).is_ok());
        failure.detail = "private/path\nleak".into();
        assert_eq!(
            failure.validate_for(&request),
            Err(RecipeCatalogError::InvalidText)
        );
    }
}

#[cfg(test)]
mod import_source_tests {
    use super::*;
    #[test]
    fn source_proof_deserialization_cannot_bypass_validation() {
        assert!(serde_json::from_str::<RecipeImportSources>("[]").is_err());
        assert!(
            serde_json::from_str::<RecipeImportSources>(r#"[{"locator":" ","revision":"exact"}]"#)
                .is_err()
        );
        let source = RecipeCatalogSource {
            locator: "https://example.test/repo".into(),
            revision: "opaque".into(),
        };
        assert!(RecipeImportSources::new(vec![source.clone(), source]).is_err());
    }
}
