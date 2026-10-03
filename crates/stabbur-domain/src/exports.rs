//! Validated saved exports. Versions remain opaque; selection always names a channel or release.
use crate::{
    Architecture, ExportId, MacOsVersion, ReleaseId, Sha256Digest, SoftwareId, SoftwareSlug,
    VariantId, Version,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Delivery mechanism for an export snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportDestination {
    /// Serve the selected snapshot as a protected repository.
    Hosted,
    /// Produce files for an independently managed repository.
    Download,
}
/// Stable selection policy, with no version ordering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExportSource {
    /// Follow an explicitly selected channel.
    Channel {
        /// Channel name.
        channel: SoftwareSlug,
    },
    /// Retain one exact release, until the operator changes it.
    Release {
        /// Release identity.
        release: ReleaseId,
    },
}
/// Supported installer transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallerFormat {
    /// Flat macOS installer package.
    Pkg,
    /// Application copied from a disk image.
    DmgApp,
}
impl InstallerFormat {
    /// Safe filename extension.
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Pkg => "pkg",
            Self::DmgApp => "dmg",
        }
    }
}
/// Raw installed-state declaration; validated as part of `ExportSettings`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InstalledState {
    /// Application bundle installed in /Applications.
    Application {
        /// Filename ending in .app, without path components.
        name: String,
        /// Bundle identifier.
        bundle_id: String,
    },
    /// Package receipt matching the release's exact opaque version.
    Receipt {
        /// Package identifier.
        package_id: String,
    },
}
/// Input representation; not a validation proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawExportSettings {
    /// Installer representation.
    pub format: InstallerFormat,
    /// Installation detection policy.
    pub detection: InstalledState,
    /// Optional presentation override.
    #[serde(default)]
    pub display_name: String,
    /// Human description shown in the destination.
    #[serde(default)]
    pub description: String,
    /// Destination category.
    #[serde(default)]
    pub category: String,
}
/// Validated reusable installation and presentation settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawExportSettings", into = "RawExportSettings")]
pub struct ExportSettings(RawExportSettings);
impl ExportSettings {
    /// Read-only validated settings.
    pub const fn data(&self) -> &RawExportSettings {
        &self.0
    }
}
impl From<ExportSettings> for RawExportSettings {
    fn from(value: ExportSettings) -> Self {
        value.0
    }
}
impl TryFrom<RawExportSettings> for ExportSettings {
    type Error = &'static str;
    fn try_from(value: RawExportSettings) -> Result<Self, Self::Error> {
        let identifier = |s: &str| {
            !s.is_empty()
                && s.len() <= 255
                && s.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b".-_".contains(&c))
        };
        match &value.detection {
            InstalledState::Application { name, bundle_id } => {
                if name.len() <= 4
                    || name.len() > 200
                    || !name.to_ascii_lowercase().ends_with(".app")
                    || name.contains(['/', '\\', ':'])
                    || name.chars().any(char::is_control)
                    || !identifier(bundle_id)
                {
                    return Err(
                        "Application detection needs a .app filename and bundle identifier.",
                    );
                }
            }
            InstalledState::Receipt { package_id } => {
                if value.format != InstallerFormat::Pkg || !identifier(package_id) {
                    return Err("Receipt detection needs a PKG installer and package identifier.");
                }
            }
        }
        for (text, max) in [
            (&value.display_name, 200),
            (&value.description, 4000),
            (&value.category, 100),
        ] {
            if text.len() > max || text.chars().any(|c| c.is_control() && c != '\n') {
                return Err("Export presentation text is invalid or too long.");
            }
        }
        Ok(Self(value))
    }
}
/// One selected library application and its reusable destination policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportSelection {
    /// Library identity; never a free-form path.
    pub software: SoftwareId,
    /// Channel-following or exact release selection.
    pub source: ExportSource,
    /// Empty selects all compatible Mac variants; otherwise filters hardware architectures.
    #[serde(default)]
    pub architectures: Vec<Architecture>,
    /// Missing settings are saved as a draft and block publication until reviewed.
    pub settings: Option<ExportSettings>,
}
/// Input representation for a complete saved export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawExportDefinition {
    /// Stable human-friendly identity.
    pub slug: SoftwareSlug,
    /// Display name.
    pub name: String,
    /// Destination mode.
    pub destination: ExportDestination,
    /// Munki catalog name, independent of selected Stabbur channels.
    pub catalog: SoftwareSlug,
    /// Explicit application membership.
    pub selections: Vec<ExportSelection>,
}
/// Bounded, unique saved export definition. Deserialization always validates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawExportDefinition", into = "RawExportDefinition")]
pub struct ExportDefinition(RawExportDefinition);
impl ExportDefinition {
    /// Read-only definition.
    pub const fn data(&self) -> &RawExportDefinition {
        &self.0
    }
}
impl From<ExportDefinition> for RawExportDefinition {
    fn from(value: ExportDefinition) -> Self {
        value.0
    }
}
impl TryFrom<RawExportDefinition> for ExportDefinition {
    type Error = &'static str;
    fn try_from(mut value: RawExportDefinition) -> Result<Self, Self::Error> {
        if value.name.trim().is_empty()
            || value.name.len() > 200
            || value.name.chars().any(char::is_control)
        {
            return Err("Choose a nonempty export name of at most 200 characters.");
        }
        if value.selections.len() > 100 {
            return Err("An export supports at most 100 applications.");
        }
        let mut seen = BTreeSet::new();
        for selection in &mut value.selections {
            if !seen.insert(selection.software) {
                return Err("Select each application only once.");
            }
            if selection.architectures.len() > 2
                || selection
                    .architectures
                    .iter()
                    .any(|a| !matches!(a, Architecture::Aarch64 | Architecture::X86_64))
            {
                return Err("Choose Apple silicon, Intel, or both architectures.");
            }
            selection.architectures.sort_by_key(|a| format!("{a:?}"));
            selection.architectures.dedup();
        }
        value.selections.sort_by_key(|s| s.software);
        Ok(Self(value))
    }
}
/// Facts whose mutable revisions are rechecked atomically by the persistence adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportBinding {
    /// Selected library application.
    pub software: SoftwareId,
    /// Observed software revision.
    pub software_revision: u64,
    /// Exact selected release.
    pub release: ReleaseId,
    /// Observed release revision.
    pub release_revision: u64,
    /// Channel and its observed revision, absent for pinned releases.
    pub channel: Option<(SoftwareSlug, u64)>,
}
/// One immutable installer entry in an export snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportItem {
    /// Software identity.
    pub software: SoftwareId,
    /// Destination package name.
    pub slug: SoftwareSlug,
    /// Display name.
    pub name: String,
    /// Exact release identity.
    pub release: ReleaseId,
    /// Exact opaque version.
    pub version: Version,
    /// Variant identity.
    pub variant: VariantId,
    /// Actual artifact architecture.
    pub architecture: Architecture,
    /// Hardware architectures included by this export selection.
    pub architectures: Vec<Architecture>,
    /// Minimum macOS bound.
    pub minimum_macos: Option<MacOsVersion>,
    /// Maximum macOS bound.
    pub maximum_macos: Option<MacOsVersion>,
    /// Verified content identity.
    pub digest: Sha256Digest,
    /// Declared artifact size.
    pub size: u64,
    /// Reviewed reusable installation settings.
    pub settings: ExportSettings,
}
/// Validated all-or-nothing publication input. Mutable facts are fenced in storage.
#[derive(Debug, Clone)]
pub struct PreparedExport {
    id: ExportId,
    revision: u64,
    generation: u64,
    items: Vec<ExportItem>,
    bindings: Vec<ExportBinding>,
}
impl PreparedExport {
    /// Validates the bounded publication and its complete release bindings.
    pub fn new(
        id: ExportId,
        revision: u64,
        generation: u64,
        items: Vec<ExportItem>,
        bindings: Vec<ExportBinding>,
    ) -> Result<Self, &'static str> {
        if revision == 0 || items.len() > 400 || bindings.len() > 100 {
            return Err("Export publication is too large or has an invalid revision.");
        }
        let mut software = BTreeSet::new();
        for binding in &bindings {
            if !software.insert(binding.software)
                || binding.software_revision == 0
                || binding.release_revision == 0
                || binding
                    .channel
                    .as_ref()
                    .is_some_and(|(_, revision)| *revision == 0)
                || !items
                    .iter()
                    .any(|i| i.software == binding.software && i.release == binding.release)
            {
                return Err("Export contains an invalid, duplicate, or empty binding.");
            }
        }
        let mut variants = BTreeSet::new();
        for item in &items {
            let hardware = match item.architecture {
                Architecture::Universal => vec![Architecture::Aarch64, Architecture::X86_64],
                arch => vec![arch],
            };
            if item.architectures.is_empty()
                || item.architectures.len() > 2
                || item
                    .architectures
                    .iter()
                    .any(|arch| !hardware.contains(arch))
                || (item.architectures.len() == 2 && item.architectures[0] == item.architectures[1])
                || matches!((&item.minimum_macos, &item.maximum_macos), (Some(min), Some(max)) if min > max)
            {
                return Err("Export contains invalid compatibility facts.");
            }
            if item.size == 0
                || item.size > 4 * 1024 * 1024 * 1024
                || !variants.insert(item.variant)
                || !bindings.iter().any(|b| {
                    b.software == item.software
                        && b.release == item.release
                        && b.software_revision > 0
                        && b.release_revision > 0
                })
            {
                return Err("Export contains an invalid or unbound installer.");
            }
        }
        Ok(Self {
            id,
            revision,
            generation,
            items,
            bindings,
        })
    }
    /// Export identity.
    pub const fn id(&self) -> ExportId {
        self.id
    }
    /// Definition revision being published.
    pub const fn revision(&self) -> u64 {
        self.revision
    }
    /// Generation that was reviewed before publication.
    pub const fn generation(&self) -> u64 {
        self.generation
    }
    /// Immutable selected installers.
    pub fn items(&self) -> &[ExportItem] {
        &self.items
    }
    /// Complete concurrency preconditions.
    pub fn bindings(&self) -> &[ExportBinding] {
        &self.bindings
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn draft_definitions_validate_duplicates_paths_and_opaque_pins() {
        let software = SoftwareId::new();
        let good = serde_json::json!({"slug":"company","name":"Company Munki","destination":"hosted","catalog":"production","selections":[{"software":software,"source":{"kind":"release","release":ReleaseId::new()},"settings":null}]});
        assert!(serde_json::from_value::<ExportDefinition>(good.clone()).is_ok());
        let mut duplicate = good;
        let selection = duplicate["selections"][0].clone();
        duplicate["selections"]
            .as_array_mut()
            .unwrap()
            .push(selection);
        assert!(serde_json::from_value::<ExportDefinition>(duplicate).is_err());
        let settings = serde_json::json!({"format":"dmg_app","detection":{"kind":"application","name":"../App.app","bundle_id":"org.example.app"}});
        assert!(serde_json::from_value::<ExportSettings>(settings).is_err());
    }
}
