//! Runtime-neutral identities, entities, and policy for Stabbur.

use std::{cmp::Ordering, fmt, str::FromStr};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;
use uuid::Uuid;

/// A validation or policy error in a domain value.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DomainError {
    /// A UUID is syntactically invalid or is not version 7.
    #[error("{kind} must be a UUIDv7")]
    InvalidId {
        /// Identity kind used in the diagnostic.
        kind: &'static str,
    },
    /// A software slug is invalid.
    #[error("slug must contain 1-63 lowercase ASCII letters, digits, or interior hyphens")]
    InvalidSlug,
    /// A version is empty or too long.
    #[error("version must contain 1-255 non-control characters")]
    InvalidVersion,
    /// A SHA-256 digest is malformed.
    #[error("digest must be exactly 64 lowercase hexadecimal characters")]
    InvalidDigest,
    /// An Apple-style operating-system version is malformed.
    #[error("macOS version must contain one to four numeric components")]
    InvalidMacOsVersion,
    /// A lifecycle transition is not allowed.
    #[error("release cannot transition from {from:?} to {to:?}")]
    InvalidLifecycleTransition {
        /// Current state.
        from: ReleaseState,
        /// Requested state.
        to: ReleaseState,
    },
    /// No variant satisfies a resolution request.
    #[error("no compatible variant exists")]
    NoCompatibleVariant,
    /// More than one equally preferred variant satisfies a resolution request.
    #[error("more than one equally preferred variant exists")]
    AmbiguousVariant,
}

macro_rules! typed_uuid {
    ($(#[$meta:meta])* $name:ident, $kind:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(Uuid);

        impl $name {
            /// Creates a new monotonically sortable `UUIDv7` identity.
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            /// Returns the underlying UUID value.
            #[must_use]
            pub const fn as_uuid(self) -> Uuid {
                self.0
            }

            /// Validates an existing UUID as a version 7 identity.
            pub fn from_uuid(value: Uuid) -> Result<Self, DomainError> {
                if value.get_version_num() == 7 {
                    Ok(Self(value))
                } else {
                    Err(DomainError::InvalidId { kind: $kind })
                }
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = DomainError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(value)
                    .ok()
                    .and_then(|uuid| Self::from_uuid(uuid).ok())
                    .ok_or(DomainError::InvalidId { kind: $kind })
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                value.parse().map_err(de::Error::custom)
            }
        }
    };
}

typed_uuid!(/// Identity of a software aggregate.
    SoftwareId, "software ID");
typed_uuid!(/// Identity of a release.
    ReleaseId, "release ID");
typed_uuid!(/// Identity of a release variant.
    VariantId, "variant ID");
typed_uuid!(/// Identity of an artifact store.
    StoreId, "store ID");
typed_uuid!(/// Identity of an artifact location.
    LocationId, "location ID");
typed_uuid!(/// Identity of a recipe.
    RecipeId, "recipe ID");
typed_uuid!(/// Identity of an immutable recipe revision.
    RecipeRevisionId, "recipe revision ID");
typed_uuid!(/// Identity of an immutable worker-published recipe catalog snapshot.
    RecipeCatalogSnapshotId, "recipe catalog snapshot ID");
typed_uuid!(/// Identity of a durable server-requested recipe catalog scan.
    RecipeCatalogScanId, "recipe catalog scan ID");
typed_uuid!(/// Identity of a persisted build target.
    BuildTargetId, "build target ID");
typed_uuid!(/// Identity of a build run.
    RunId, "run ID");
typed_uuid!(/// Identity of a worker.
    WorkerId, "worker ID");
typed_uuid!(/// Identity of a queued job.
    JobId, "job ID");
typed_uuid!(/// Identity of one attempt to execute a job.
    AttemptId, "attempt ID");
typed_uuid!(/// Identity of an audit event.
    AuditEventId, "audit event ID");
typed_uuid!(/// Identity of a lifecycle event.
    LifecycleEventId, "lifecycle event ID");
typed_uuid!(/// Identity of a channel promotion event.
    PromotionEventId, "promotion event ID");

/// A stable lowercase URL-safe software name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SoftwareSlug(String);

impl SoftwareSlug {
    /// Validates a new slug.
    pub fn new(value: impl Into<String>) -> Result<Self, DomainError> {
        let value = value.into();
        let valid_length = (1..=63).contains(&value.len());
        let valid_bytes = value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
        let valid_edges = value
            .as_bytes()
            .first()
            .zip(value.as_bytes().last())
            .is_some_and(|(first, last)| {
                first.is_ascii_alphanumeric() && last.is_ascii_alphanumeric()
            });
        if valid_length && valid_bytes && valid_edges && !value.contains("--") {
            Ok(Self(value))
        } else {
            Err(DomainError::InvalidSlug)
        }
    }

    /// Returns the normalized slug.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SoftwareSlug {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for SoftwareSlug {
    type Err = DomainError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for SoftwareSlug {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SoftwareSlug {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// An opaque, non-empty upstream version string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Version(String);

impl Version {
    /// Validates a version without assigning ordering semantics to it.
    pub fn new(value: impl Into<String>) -> Result<Self, DomainError> {
        let value = value.into();
        if (1..=255).contains(&value.len())
            && value.trim() == value
            && !value.chars().any(char::is_control)
        {
            Ok(Self(value))
        } else {
            Err(DomainError::InvalidVersion)
        }
    }

    /// Returns the exact upstream representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for Version {
    type Err = DomainError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for Version {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// A lowercase SHA-256 digest.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sha256Digest(String);

impl Sha256Digest {
    /// Validates a digest.
    pub fn new(value: impl Into<String>) -> Result<Self, DomainError> {
        let value = value.into();
        if value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            Ok(Self(value))
        } else {
            Err(DomainError::InvalidDigest)
        }
    }

    /// Returns the hexadecimal representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for Sha256Digest {
    type Err = DomainError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for Sha256Digest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// Supported operating-system families.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    /// Apple macOS.
    MacOs,
    /// Linux.
    Linux,
    /// Microsoft Windows.
    Windows,
}

/// Supported CPU architectures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Architecture {
    /// 64-bit Intel/AMD architecture.
    X86_64,
    /// 64-bit ARM architecture.
    Aarch64,
    /// A multi-architecture artifact.
    Universal,
}

/// A numeric Apple-style macOS version with one to four components.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MacOsVersion(Vec<u32>);

impl MacOsVersion {
    /// Returns the numeric components.
    #[must_use]
    pub fn components(&self) -> &[u32] {
        &self.0
    }

    fn normalized_component(&self, index: usize) -> u32 {
        self.0.get(index).copied().unwrap_or(0)
    }
}

impl FromStr for MacOsVersion {
    type Err = DomainError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut components = value
            .split('.')
            .map(str::parse::<u32>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| DomainError::InvalidMacOsVersion)?;
        if (1..=4).contains(&components.len()) && !value.is_empty() {
            while components.len() > 1 && components.last() == Some(&0) {
                components.pop();
            }
            Ok(Self(components))
        } else {
            Err(DomainError::InvalidMacOsVersion)
        }
    }
}

impl fmt::Display for MacOsVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = self
            .0
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(".");
        formatter.write_str(&value)
    }
}

impl Ord for MacOsVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        (0..4)
            .map(|index| {
                self.normalized_component(index)
                    .cmp(&other.normalized_component(index))
            })
            .find(|ordering| *ordering != Ordering::Equal)
            .unwrap_or(Ordering::Equal)
    }
}

impl PartialOrd for MacOsVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Serialize for MacOsVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for MacOsVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

/// Compatibility restrictions attached to a variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compatibility {
    /// Operating-system family.
    pub platform: Platform,
    /// CPU architecture.
    pub architecture: Architecture,
    /// Inclusive minimum macOS version, when applicable.
    pub minimum_macos: Option<MacOsVersion>,
    /// Inclusive maximum macOS version, when applicable.
    pub maximum_macos: Option<MacOsVersion>,
}

impl Compatibility {
    /// Returns whether a target satisfies these restrictions.
    #[must_use]
    pub fn matches(
        &self,
        platform: Platform,
        architecture: Architecture,
        macos: Option<&MacOsVersion>,
    ) -> bool {
        if self.platform != platform
            || (self.architecture != Architecture::Universal && self.architecture != architecture)
        {
            return false;
        }
        if platform != Platform::MacOs {
            return true;
        }
        let Some(macos) = macos else {
            return self.minimum_macos.is_none() && self.maximum_macos.is_none();
        };
        self.minimum_macos.as_ref().is_none_or(|min| macos >= min)
            && self.maximum_macos.as_ref().is_none_or(|max| macos <= max)
    }
}

/// An immutable software aggregate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Software {
    /// Domain identity.
    pub id: SoftwareId,
    /// Stable URL slug.
    pub slug: SoftwareSlug,
    /// Human-readable display name.
    pub name: String,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Optimistic-concurrency revision.
    pub revision: u64,
}

/// Platform-neutral installation and installed-state detection metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SoftwareInstallation {
    /// Parent software identity.
    pub software_id: SoftwareId,
    /// Declarative installation metadata consumed by clients and packagers.
    pub install: serde_json::Value,
    /// Declarative installed-state detection metadata.
    pub detection: serde_json::Value,
}

/// A release of one software item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    /// Publication eligibility, independent of the highest lifecycle stage.
    #[serde(default)]
    pub availability: ReleaseAvailability,
    /// Domain identity.
    pub id: ReleaseId,
    /// Parent software.
    pub software_id: SoftwareId,
    /// Opaque upstream version.
    pub version: Version,
    /// Highest achieved lifecycle state.
    pub state: ReleaseState,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Optimistic-concurrency revision.
    pub revision: u64,
}

/// A platform-specific release variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Variant {
    /// Domain identity.
    pub id: VariantId,
    /// Parent release.
    pub release_id: ReleaseId,
    /// Compatibility restrictions.
    pub compatibility: Compatibility,
    /// Explicit resolution priority; greater values win.
    pub resolution_priority: i32,
}

/// Immutable artifact metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    /// Content identity.
    pub digest: Sha256Digest,
    /// Exact byte length.
    pub size: u64,
    /// Media type supplied at ingestion.
    pub media_type: String,
    /// Ingestion time.
    pub created_at: DateTime<Utc>,
}

/// Semantic role of an artifact within a variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactRole {
    /// The one installable artifact selected by the resolver.
    PrimaryInstaller,
    /// A cryptographic signature.
    Signature,
    /// A software bill of materials.
    Sbom,
    /// Debug symbols.
    DebugSymbols,
    /// Auxiliary metadata.
    Metadata,
}

/// A durable artifact-location state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocationState {
    /// A location has been declared but not populated.
    Pending,
    /// Replication is in progress.
    Replicating,
    /// Verified bytes exist locally.
    Present,
    /// Bytes exist behind a remote redirect.
    Remote,
    /// Expected bytes are absent.
    Missing,
    /// Bytes failed verification.
    Corrupt,
    /// A location operation failed.
    Failed,
}

/// The highest lifecycle state achieved by a release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseState {
    /// Upstream version metadata has been discovered.
    Discovered,
    /// Builder output exists.
    Built,
    /// Builder output has been inspected.
    Inspected,
    /// Required verification checks succeeded.
    Verified,
    /// Automatically eligible for publication.
    Candidate,
    /// Explicitly promoted for testing.
    Testing,
    /// Explicitly promoted as stable.
    Stable,
    /// The build or verification failed.
    Failed,
    /// An operator rejected the release.
    Rejected,
}

impl ReleaseState {
    /// Checks and applies an allowed forward lifecycle transition.
    pub fn transition(self, to: Self) -> Result<Self, DomainError> {
        let allowed = matches!(
            (self, to),
            (
                Self::Discovered,
                Self::Built | Self::Failed | Self::Rejected
            ) | (Self::Built, Self::Inspected | Self::Failed | Self::Rejected)
                | (
                    Self::Inspected,
                    Self::Verified | Self::Failed | Self::Rejected
                )
                | (
                    Self::Verified,
                    Self::Candidate | Self::Failed | Self::Rejected
                )
                | (
                    Self::Candidate,
                    Self::Testing | Self::Stable | Self::Rejected
                )
                | (Self::Testing, Self::Stable | Self::Rejected)
        );
        if allowed {
            Ok(to)
        } else {
            Err(DomainError::InvalidLifecycleTransition { from: self, to })
        }
    }
}

/// Current permission to publish a release, independent of its attained lifecycle.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReleaseAvailability {
    /// The release has not been withdrawn.
    #[default]
    Available,
    /// An operator withdrew the release. History and immutable bytes remain available for audit.
    Withdrawn {
        /// Required, validated operator explanation.
        reason: WithdrawalReason,
        /// Time of the withdrawal.
        at: DateTime<Utc>,
    },
}

impl ReleaseAvailability {
    /// Whether channel publication is permitted.
    pub const fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }
}

/// Nonempty, bounded, single-line explanation accepted at the mutation boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct WithdrawalReason(String);

impl WithdrawalReason {
    /// Validates an operator explanation without retaining invalid input in an error.
    pub fn new(value: impl Into<String>) -> Result<Self, &'static str> {
        let value = value.into();
        if value.trim() != value
            || value.is_empty()
            || value.len() > 1024
            || value.chars().any(char::is_control)
        {
            return Err(
                "withdrawal reason must contain 1-1024 non-control bytes without surrounding whitespace",
            );
        }
        Ok(Self(value))
    }
    /// Returns the validated explanation.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for WithdrawalReason {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
impl From<WithdrawalReason> for String {
    fn from(value: WithdrawalReason) -> Self {
        value.0
    }
}

/// Immutable evidence of a lifecycle transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleEvent {
    /// Event identity.
    pub id: LifecycleEventId,
    /// Affected release.
    pub release_id: ReleaseId,
    /// State before the change, absent for creation.
    pub from: Option<ReleaseState>,
    /// State after the change.
    pub to: ReleaseState,
    /// Authenticated actor representation.
    pub actor: String,
    /// Human-readable reason.
    pub reason: Option<String>,
    /// Event time.
    pub occurred_at: DateTime<Utc>,
}

/// A target supplied to the deterministic variant resolver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionTarget {
    /// Operating-system family.
    pub platform: Platform,
    /// Requested CPU architecture.
    pub architecture: Architecture,
    /// Numeric macOS version.
    pub macos: Option<MacOsVersion>,
    /// An optional channel-pinned variant.
    pub pinned_variant: Option<VariantId>,
}

/// Resolves one compatible variant without guessing between equal candidates.
pub fn resolve_variant<'a>(
    variants: &'a [Variant],
    target: &ResolutionTarget,
) -> Result<&'a Variant, DomainError> {
    if let Some(pinned) = target.pinned_variant {
        return variants
            .iter()
            .find(|variant| variant.id == pinned)
            .filter(|variant| {
                variant.compatibility.matches(
                    target.platform,
                    target.architecture,
                    target.macos.as_ref(),
                )
            })
            .ok_or(DomainError::NoCompatibleVariant);
    }

    let mut matches = variants
        .iter()
        .filter(|variant| {
            variant.compatibility.matches(
                target.platform,
                target.architecture,
                target.macos.as_ref(),
            )
        })
        .collect::<Vec<_>>();
    matches.sort_by_key(|variant| {
        (
            variant.compatibility.architecture == target.architecture,
            variant.resolution_priority,
        )
    });
    let winner = matches.pop().ok_or(DomainError::NoCompatibleVariant)?;
    if matches.last().is_some_and(|runner_up| {
        runner_up.compatibility.architecture == winner.compatibility.architecture
            && runner_up.resolution_priority == winner.resolution_priority
    }) {
        Err(DomainError::AmbiguousVariant)
    } else {
        Ok(winner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variant(architecture: Architecture, priority: i32) -> Variant {
        Variant {
            id: VariantId::new(),
            release_id: ReleaseId::new(),
            compatibility: Compatibility {
                platform: Platform::MacOs,
                architecture,
                minimum_macos: Some("13.0".parse().unwrap()),
                maximum_macos: Some("15.9".parse().unwrap()),
            },
            resolution_priority: priority,
        }
    }

    #[test]
    fn ids_are_v7_and_reject_other_versions() {
        let id = SoftwareId::new();
        assert_eq!(id.as_uuid().get_version_num(), 7);
        assert!(SoftwareId::from_uuid(Uuid::nil()).is_err());
        assert_eq!(id.to_string().parse::<SoftwareId>().unwrap(), id);
    }

    #[test]
    fn slugs_are_normalized_at_the_boundary() {
        assert!(SoftwareSlug::new("firefox").is_ok());
        assert!(SoftwareSlug::new("firefox-esr").is_ok());
        for invalid in ["Firefox", "-firefox", "firefox-", "firefox--esr", ""] {
            assert!(SoftwareSlug::new(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn versions_are_opaque() {
        assert!(Version::new("2026.08-beta+vendor.2").is_ok());
        assert!(Version::new("").is_err());
        assert!(Version::new(" padded ").is_err());
    }

    #[test]
    fn macos_version_comparison_is_numeric_and_zero_extended() {
        let old: MacOsVersion = "14.9".parse().unwrap();
        let new: MacOsVersion = "14.10".parse().unwrap();
        let equivalent: MacOsVersion = "14.9.0".parse().unwrap();
        assert!(new > old);
        assert_eq!(old, equivalent);
    }

    #[test]
    fn lifecycle_allows_candidate_to_stable_but_never_regresses() {
        assert_eq!(
            ReleaseState::Candidate
                .transition(ReleaseState::Stable)
                .unwrap(),
            ReleaseState::Stable
        );
        assert!(
            ReleaseState::Stable
                .transition(ReleaseState::Testing)
                .is_err()
        );
        assert!(
            ReleaseState::Verified
                .transition(ReleaseState::Stable)
                .is_err()
        );
    }

    #[test]
    fn resolver_prefers_exact_architecture_then_priority() {
        let universal = variant(Architecture::Universal, 100);
        let exact = variant(Architecture::Aarch64, 0);
        let variants = [universal, exact.clone()];
        let target = ResolutionTarget {
            platform: Platform::MacOs,
            architecture: Architecture::Aarch64,
            macos: Some("15.0".parse().unwrap()),
            pinned_variant: None,
        };
        assert_eq!(resolve_variant(&variants, &target).unwrap().id, exact.id);
    }

    #[test]
    fn resolver_reports_ambiguity() {
        let variants = [
            variant(Architecture::Aarch64, 10),
            variant(Architecture::Aarch64, 10),
        ];
        let target = ResolutionTarget {
            platform: Platform::MacOs,
            architecture: Architecture::Aarch64,
            macos: Some("14.0".parse().unwrap()),
            pinned_variant: None,
        };
        assert_eq!(
            resolve_variant(&variants, &target),
            Err(DomainError::AmbiguousVariant)
        );
    }
    #[test]
    fn withdrawal_reason_cannot_be_deserialized_without_validation() {
        for value in ["", " padded ", "line\nbreak"] {
            assert!(serde_json::from_value::<WithdrawalReason>(serde_json::json!(value)).is_err());
        }
        let reason: WithdrawalReason =
            serde_json::from_value(serde_json::json!("Confirmed regression")).unwrap();
        assert_eq!(reason.as_str(), "Confirmed regression");
        assert!(WithdrawalReason::new("a".repeat(1025)).is_err());
    }
}
