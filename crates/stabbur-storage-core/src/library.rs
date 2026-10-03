//! Validated library queries and bounded operator summaries.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use stabbur_domain::{ReleaseId, RunId, Software, SoftwareId};

use crate::{RunState, SoftwareChannelSummary, StorageError};

/// A literal, bounded search over software display names and slugs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct LibrarySearch(String);

impl LibrarySearch {
    /// Normalizes surrounding whitespace and rejects control characters and oversized input.
    pub fn new(value: String) -> Result<Self, StorageError> {
        if value.len() > 200 || value.chars().any(char::is_control) {
            return Err(StorageError::InvalidData {
                message: "search must contain at most 200 bytes without control characters".into(),
            });
        }
        if value.trim().len() == value.len() {
            Ok(Self(value))
        } else {
            Ok(Self(value.trim().to_owned()))
        }
    }

    /// Literal query text. Wildcards have no special meaning.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for LibrarySearch {
    type Error = StorageError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<LibrarySearch> for String {
    fn from(value: LibrarySearch) -> Self {
        value.0
    }
}

/// Server-evaluated views; historical failures alone do not create an incident.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibraryView {
    /// All matching software.
    #[default]
    All,
    /// A current failure, unavailable worker, or candidate awaiting review.
    Attention,
    /// Latest run failed and no build is currently outstanding.
    Failed,
    /// At least one enabled target lacks a recently observed compatible worker.
    Blocked,
    /// At least one available candidate awaits review.
    Review,
    /// No run has ever been created.
    NotBuilt,
}

/// Stable library ordering, independent of opaque software versions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibrarySort {
    /// Display name using Unicode code-point ordering, then stable identity.
    #[default]
    Name,
    /// Most recently created identity first.
    Newest,
}

/// A validated keyset position. The HTTP layer binds it to the original query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "LibraryPositionData", into = "LibraryPositionData")]
pub struct LibraryPosition {
    name: String,
    id: SoftwareId,
}

#[derive(Debug, Serialize, Deserialize)]
struct LibraryPositionData {
    name: String,
    id: SoftwareId,
}

impl LibraryPosition {
    /// Accepts only a bounded persisted display name and typed identity.
    pub fn new(name: String, id: SoftwareId) -> Result<Self, StorageError> {
        if name.is_empty() || name.len() > 255 {
            return Err(StorageError::InvalidData {
                message: "invalid library cursor name".into(),
            });
        }
        Ok(Self { name, id })
    }
    /// Display name at the page boundary.
    pub fn name(&self) -> &str {
        &self.name
    }
    /// Stable tie-breaker identity.
    pub fn id(&self) -> SoftwareId {
        self.id
    }
}

impl TryFrom<LibraryPositionData> for LibraryPosition {
    type Error = StorageError;
    fn try_from(value: LibraryPositionData) -> Result<Self, Self::Error> {
        Self::new(value.name, value.id)
    }
}
impl From<LibraryPosition> for LibraryPositionData {
    fn from(value: LibraryPosition) -> Self {
        Self {
            name: value.name,
            id: value.id,
        }
    }
}

/// A bounded library read; validated once before crossing the storage boundary.
#[derive(Debug, Clone)]
pub struct LibraryQuery {
    search: LibrarySearch,
    view: LibraryView,
    sort: LibrarySort,
    after: Option<LibraryPosition>,
    limit: u32,
}

impl LibraryQuery {
    /// Constructs a page with 1–200 items plus one adapter lookahead row.
    pub fn new(
        search: LibrarySearch,
        view: LibraryView,
        sort: LibrarySort,
        after: Option<LibraryPosition>,
        limit: u32,
    ) -> Result<Self, StorageError> {
        if !(1..=200).contains(&limit) {
            return Err(StorageError::InvalidData {
                message: "library limit must be between 1 and 200".into(),
            });
        }
        Ok(Self {
            search,
            view,
            sort,
            after,
            limit,
        })
    }
    /// Search text.
    pub fn search(&self) -> &LibrarySearch {
        &self.search
    }
    /// Attention predicate.
    pub fn view(&self) -> LibraryView {
        self.view
    }
    /// Ordering.
    pub fn sort(&self) -> LibrarySort {
        self.sort
    }
    /// Keyset boundary.
    pub fn after(&self) -> Option<&LibraryPosition> {
        self.after.as_ref()
    }
    /// Requested number of rows, excluding lookahead.
    pub fn limit(&self) -> u32 {
        self.limit
    }
}

/// Small, aggregate-shaped library entry; excludes logs, definitions, and artifact graphs.
#[derive(Debug, Clone)]
pub struct LibraryEntry {
    /// Software identity and display metadata.
    pub software: Software,
    /// Current channel bindings.
    pub channels: Vec<SoftwareChannelSummary>,
    /// Most recent run identity.
    pub latest_run_id: Option<RunId>,
    /// Most recent run state.
    pub latest_run_state: Option<RunState>,
    /// Last successful check.
    pub last_success_at: Option<DateTime<Utc>>,
    /// Next enabled scheduled check.
    pub next_run_at: Option<DateTime<Utc>>,
    /// Queued/running builds.
    pub outstanding_runs: u64,
    /// Enabled targets without a recently observed compatible worker.
    pub blocked_targets: u64,
    /// Available candidate releases.
    pub review_count: u64,
    /// Most recently created available candidate, for direct review navigation.
    pub review_release_id: Option<ReleaseId>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deserialization_cannot_bypass_search_validation() {
        assert!(serde_json::from_str::<LibrarySearch>("\"bad\\nquery\"").is_err());
        assert!(LibrarySearch::new("x".repeat(201)).is_err());
        assert_eq!(LibrarySearch::new("  %_  ".into()).unwrap().as_str(), "%_");
        assert!(
            LibraryQuery::new(
                LibrarySearch::default(),
                LibraryView::All,
                LibrarySort::Name,
                None,
                201
            )
            .is_err()
        );
    }
}
