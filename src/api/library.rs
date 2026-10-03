//! Searchable, bounded operator library and attention views.

use sha2::{Digest, Sha256};
use stabbur_storage_core::{
    LibraryEntry, LibraryPosition, LibraryQuery, LibrarySearch, LibrarySort, LibraryView,
};

use super::*;

/// Library search, attention filter, and stable ordering applied before pagination.
#[derive(Debug, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct LibraryPageQuery {
    /// Literal name/slug substring; ASCII case-insensitive, at most 200 UTF-8 bytes.
    q: Option<String>,
    /// all (default), attention, failed, blocked, review, or not_built.
    #[param(value_type = String)]
    view: Option<LibraryView>,
    /// name (default, code-point order) or newest (creation identity descending). Never version order.
    #[param(value_type = String)]
    sort: Option<LibrarySort>,
    /// Opaque keyset cursor bound to the search, view, and sort.
    cursor: Option<String>,
    /// Page size, default 50, maximum 200.
    #[param(minimum = 1, maximum = 200)]
    limit: Option<u32>,
}

/// Library summary with separate execution, approval, and publication observations.
#[derive(Debug, Serialize, ToSchema)]
pub struct LibraryEntryResponse {
    /// Software identity.
    id: String,
    /// Stable software slug.
    slug: String,
    /// Display name.
    name: String,
    /// Software concurrency revision.
    revision: u64,
    /// Software creation time.
    created_at: chrono::DateTime<Utc>,
    /// Current channel selections, not evidence of installation on a device.
    channels: Vec<SoftwareChannelStatusResponse>,
    /// Latest run identity.
    latest_run_id: Option<String>,
    /// Latest run state; historical failures do not overwrite later success.
    latest_run_state: Option<String>,
    /// Most recent successful check.
    last_success_at: Option<chrono::DateTime<Utc>>,
    /// Next enabled recurring check.
    next_run_at: Option<chrono::DateTime<Utc>>,
    /// Queued or running builds.
    outstanding_runs: u64,
    /// Enabled targets without a recent compatible worker.
    blocked_targets: u64,
    /// Available candidate releases awaiting review.
    review_count: u64,
    /// Most recently created available candidate.
    review_release_id: Option<String>,
}

impl From<LibraryEntry> for LibraryEntryResponse {
    fn from(value: LibraryEntry) -> Self {
        Self {
            id: value.software.id.to_string(),
            slug: value.software.slug.to_string(),
            name: value.software.name,
            revision: value.software.revision,
            created_at: value.software.created_at,
            channels: value
                .channels
                .into_iter()
                .map(SoftwareChannelStatusResponse::from)
                .collect(),
            latest_run_id: value.latest_run_id.map(|id| id.to_string()),
            latest_run_state: value
                .latest_run_state
                .map(|state| run_state_response(state).to_owned()),
            last_success_at: value.last_success_at,
            next_run_at: value.next_run_at,
            outstanding_runs: value.outstanding_runs,
            blocked_targets: value.blocked_targets,
            review_count: value.review_count,
            review_release_id: value.review_release_id.map(|id| id.to_string()),
        }
    }
}

/// A bounded library page. No false next-page cursor when the collection is exhausted.
#[derive(Debug, Serialize, ToSchema)]
pub struct LibraryPageResponse {
    /// Current summary rows.
    items: Vec<LibraryEntryResponse>,
    /// Continuation for the same query; null at exhaustion.
    next_cursor: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LibraryCursor {
    fingerprint: String,
    position: LibraryPosition,
}

#[utoipa::path(get, path = "/api/v1/library/software", tag = "software", params(LibraryPageQuery),
    security(("bearer_auth" = [])), responses(
    (status = 200, description = "Filtered software summaries and actionable attention", body = LibraryPageResponse),
    (status = 400, description = "Invalid query or cursor from a different query", body = Problem),
    (status = 403, description = "Software read permission required", body = Problem)))]
#[actix_web::get("/library/software")]
pub(crate) async fn software_library(
    request: HttpRequest,
    state: web::Data<AppState>,
    query: web::Query<LibraryPageQuery>,
) -> Result<web::Json<LibraryPageResponse>, ApiError> {
    authenticate(&request, &state, Permission::SoftwareRead).await?;
    let rid = request_id(&request);
    let invalid = |message| ApiError::validation(message, vec![], &rid);
    let search = LibrarySearch::new(query.q.clone().unwrap_or_default()).map_err(|_| {
        invalid("Search must contain at most 200 bytes without control characters.")
    })?;
    let view = query.view.unwrap_or_default();
    let sort = query.sort.unwrap_or_default();
    let fingerprint = hex::encode(Sha256::digest(
        serde_json::to_vec(&("library-v1", &search, view, sort))
            .map_err(|_| ApiError::internal(&rid))?,
    ));
    let after = query
        .cursor
        .as_ref()
        .map(|cursor| {
            if cursor.len() > 2048 {
                return Err(invalid("The library cursor is invalid."));
            }
            let decoded = URL_SAFE_NO_PAD
                .decode(cursor)
                .map_err(|_| invalid("The library cursor is invalid."))?;
            let value: LibraryCursor = serde_json::from_slice(&decoded)
                .map_err(|_| invalid("The library cursor is invalid."))?;
            if value.fingerprint != fingerprint {
                return Err(invalid(
                    "The library cursor belongs to a different search, view, or sort.",
                ));
            }
            Ok(value.position)
        })
        .transpose()?;
    let selection = LibraryQuery::new(search, view, sort, after, query.limit.unwrap_or(50))
        .map_err(|_| invalid("Limit must be between 1 and 200."))?;
    let mut values = state
        .storage
        .software_library(&selection, Utc::now())
        .await
        .map_err(|error| ApiError::storage(error, &rid))?;
    let more = values.len() > selection.limit() as usize;
    values.truncate(selection.limit() as usize);
    let next_cursor = if more {
        let last = values.last().ok_or_else(|| ApiError::internal(&rid))?;
        let position = LibraryPosition::new(last.software.name.clone(), last.software.id)
            .map_err(|_| ApiError::internal(&rid))?;
        Some(
            URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(&LibraryCursor {
                    fingerprint,
                    position,
                })
                .map_err(|_| ApiError::internal(&rid))?,
            ),
        )
    } else {
        None
    };
    Ok(web::Json(LibraryPageResponse {
        items: values.into_iter().map(LibraryEntryResponse::from).collect(),
        next_cursor,
    }))
}
