//! One-statement library summaries with filtering before keyset pagination.

use chrono::{DateTime, Duration, Utc};
use stabbur_storage_core::{LibraryEntry, LibraryQuery, LibrarySort, LibraryView, StorageError};

use super::{
    Row, SqliteStorage, invalid_data, map_write, parse_value, query, run_state, software_from_row,
};

pub(super) async fn read(
    storage: &SqliteStorage,
    request: &LibraryQuery,
    now: DateTime<Utc>,
) -> Result<Vec<LibraryEntry>, StorageError> {
    // These fragments are selected only from enums. All user values remain bound parameters.
    let predicate = match request.view() {
        LibraryView::All => "1",
        LibraryView::Attention => {
            "blocked_targets > 0 OR review_count > 0 OR (latest_run_state = 'failed' AND outstanding_runs = 0)"
        }
        LibraryView::Failed => "latest_run_state = 'failed' AND outstanding_runs = 0",
        LibraryView::Blocked => "blocked_targets > 0",
        LibraryView::Review => "review_count > 0",
        LibraryView::NotBuilt => "latest_run_id IS NULL",
    };
    let (boundary, order) = match request.sort() {
        LibrarySort::Name => (
            "(?4 = '' OR name > ?5 OR (name = ?5 AND id > ?4))",
            "name, id",
        ),
        LibrarySort::Newest => ("(?4 = '' OR id < ?4)", "id DESC"),
    };
    let sql = format!(
        "WITH summaries AS (
            SELECT s.*,
              (SELECT id FROM runs WHERE software_id = s.id ORDER BY created_at DESC, id DESC LIMIT 1) AS latest_run_id,
              (SELECT state FROM runs WHERE software_id = s.id ORDER BY created_at DESC, id DESC LIMIT 1) AS latest_run_state,
              (SELECT MAX(completed_at) FROM runs WHERE software_id = s.id AND state = 'succeeded') AS last_success_at,
              (SELECT MIN(next_run_at) FROM build_targets WHERE software_id = s.id AND enabled = 1) AS next_run_at,
              (SELECT COUNT(*) FROM runs WHERE software_id = s.id AND state IN ('queued','running')) AS outstanding_runs,
              (SELECT COUNT(*) FROM releases WHERE software_id = s.id AND state = 'candidate' AND json_extract(availability_json, '$.kind') = 'available') AS review_count,
              (SELECT id FROM releases WHERE software_id = s.id AND state = 'candidate' AND json_extract(availability_json, '$.kind') = 'available' ORDER BY created_at DESC, id DESC LIMIT 1) AS review_release_id,
              (SELECT COUNT(*) FROM build_targets t JOIN recipe_revisions rr ON rr.id = t.recipe_revision_id
                WHERE t.software_id = s.id AND t.enabled = 1 AND NOT EXISTS (
                  SELECT 1 FROM workers w WHERE w.enabled = 1 AND w.draining = 0 AND w.last_seen_at >= ?2
                    AND NOT EXISTS (SELECT 1 FROM json_each(rr.required_capabilities_json) requirement
                      WHERE NOT EXISTS (SELECT 1 FROM worker_capabilities wc WHERE wc.worker_id = w.id AND wc.capability = requirement.value)))) AS blocked_targets
            FROM software s
            WHERE instr(lower(s.name), lower(?1)) > 0 OR instr(lower(s.slug), lower(?1)) > 0
        ), page AS (
            SELECT * FROM summaries WHERE ({predicate}) AND {boundary} ORDER BY {order} LIMIT ?3
        )
        SELECT page.*, (SELECT json_group_array(json_object('name', name, 'release_id', release_id, 'version', version, 'revision', revision)) FROM (
            SELECT c.name, c.release_id, r.version, c.revision FROM channels c JOIN releases r ON r.id = c.release_id
            WHERE c.software_id = page.id ORDER BY c.name
        )) AS channels_json FROM page ORDER BY {order}"
    );
    let rows = query(&sql)
        .bind(request.search().as_str())
        .bind(now - Duration::minutes(5))
        .bind(i64::from(request.limit()) + 1)
        .bind(
            request
                .after()
                .map(|p| p.id().to_string())
                .unwrap_or_default(),
        )
        .bind(
            request
                .after()
                .map_or("", stabbur_storage_core::LibraryPosition::name),
        )
        .fetch_all(&storage.pool)
        .await
        .map_err(map_write)?;
    rows.iter()
        .map(|row| {
            let count = |key| -> Result<u64, StorageError> {
                u64::try_from(row.try_get::<i64, _>(key).map_err(map_write)?)
                    .map_err(|_| invalid_data("invalid library count"))
            };
            Ok(LibraryEntry {
                software: software_from_row(row)?,
                channels: serde_json::from_str(row.try_get("channels_json").map_err(map_write)?)
                    .map_err(|_| invalid_data("invalid library channels"))?,
                latest_run_id: row
                    .try_get::<Option<String>, _>("latest_run_id")
                    .map_err(map_write)?
                    .map(|id| parse_value(&id, "run ID"))
                    .transpose()?,
                latest_run_state: row
                    .try_get::<Option<String>, _>("latest_run_state")
                    .map_err(map_write)?
                    .map(|state| run_state(&state))
                    .transpose()?,
                last_success_at: row.try_get("last_success_at").map_err(map_write)?,
                next_run_at: row.try_get("next_run_at").map_err(map_write)?,
                outstanding_runs: count("outstanding_runs")?,
                blocked_targets: count("blocked_targets")?,
                review_count: count("review_count")?,
                review_release_id: row
                    .try_get::<Option<String>, _>("review_release_id")
                    .map_err(map_write)?
                    .map(|id| parse_value(&id, "release ID"))
                    .transpose()?,
            })
        })
        .collect()
}
