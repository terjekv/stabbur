//! Builder-neutral build-target composition and durable due-cursor scheduling.

use std::{collections::BTreeMap, sync::Arc, time::Duration as StdDuration};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use stabbur_builder_core::{BuildParameter, BuildRequest, BuilderJob};
use stabbur_domain::{AuditEventId, JobId, RunId};
use stabbur_jobs_core::{Job, JobState, JobSubject};
use stabbur_storage_core::{
    AuditActor, AuditEvent, BuildTargetRecord, BuildTargetRunTrigger, BuildTargetSchedule,
    CompletionOutcome, RecipeRevisionRecord, RunRecord, RunState, Storage, StorageError,
};
use tracing::{info, warn};

const SCHEDULER_BATCH_SIZE: u32 = 50;

/// Builds the ordinary queued run and builder-neutral job used by manual and scheduled execution.
pub(crate) fn queued_target_build(
    target: &BuildTargetRecord,
    revision: &RecipeRevisionRecord,
    now: DateTime<Utc>,
) -> Result<(RunRecord, Job)> {
    if revision.id != target.recipe_revision_id {
        bail!("build target references a different recipe revision");
    }
    let parameters =
        serde_json::from_value::<BTreeMap<String, BuildParameter>>(target.parameters.clone())
            .context("decoding validated build target parameters")?;
    let run_id = RunId::new();
    let request = BuildRequest {
        run_id,
        software: target.software_id,
        recipe_revision: revision.id,
        parameters,
        required_capabilities: revision.required_capabilities.clone(),
    };
    let payload = serde_json::to_value(BuilderJob::new(
        request,
        &revision.builder,
        revision.definition.clone(),
    ))
    .context("serializing scheduled builder job")?;
    Ok((
        RunRecord {
            id: run_id,
            recipe_revision_id: revision.id,
            software_id: target.software_id,
            state: RunState::Queued,
            parameters: target.parameters.clone(),
            result: None,
            created_at: now,
            completed_at: None,
        },
        Job {
            id: JobId::new(),
            subject: JobSubject::BuildRun { run_id },
            required_capabilities: revision.required_capabilities.clone(),
            payload,
            state: JobState::Queued,
            maximum_attempts: 3,
            attempt_count: 0,
            created_at: now,
        },
    ))
}

fn next_cursor_after(
    due_at: DateTime<Utc>,
    every_seconds: u32,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>> {
    let elapsed = now.signed_duration_since(due_at).num_seconds().max(0);
    let elapsed = u64::try_from(elapsed).context("converting scheduler cursor lag")?;
    let steps = elapsed / u64::from(every_seconds) + 1;
    let advance = u64::from(every_seconds)
        .checked_mul(steps)
        .and_then(|seconds| i64::try_from(seconds).ok())
        .context("build target cursor advance overflow")?;
    due_at
        .checked_add_signed(Duration::seconds(advance))
        .context("build target cursor exceeds supported time range")
}

/// Consumes one bounded snapshot of due targets, safe across concurrent server instances.
pub async fn schedule_due_once(storage: &Arc<dyn Storage>, now: DateTime<Utc>) -> Result<u32> {
    let targets = storage
        .due_build_targets(now, SCHEDULER_BATCH_SIZE)
        .await
        .context("loading due build targets")?;
    let mut scheduled = 0_u32;
    for target in targets {
        let BuildTargetSchedule::Interval { every_seconds } = target.schedule else {
            continue;
        };
        let Some(due_at) = target.next_run_at else {
            continue;
        };
        let revision = match storage.recipe_revision(target.recipe_revision_id).await {
            Ok(Some(revision)) => revision,
            Ok(None) => {
                warn!(target_id = %target.id, "due build target recipe revision is missing");
                continue;
            }
            Err(error) => {
                warn!(%error, target_id = %target.id, "loading due target recipe revision failed");
                continue;
            }
        };
        let next_run_at = match next_cursor_after(due_at, every_seconds, now) {
            Ok(next_run_at) => next_run_at,
            Err(error) => {
                warn!(%error, target_id = %target.id, "advancing due target cursor failed");
                continue;
            }
        };
        let (run, job) = match queued_target_build(&target, &revision, now) {
            Ok(unit) => unit,
            Err(error) => {
                warn!(%error, target_id = %target.id, "composing due target run failed");
                continue;
            }
        };
        let idempotency_key = due_at.to_rfc3339_opts(SecondsFormat::Nanos, true);
        let result = storage
            .create_build_target_run(
                target.id,
                BuildTargetRunTrigger::Scheduled {
                    target_revision: target.revision,
                    due_at,
                    next_run_at,
                },
                &run,
                &job,
                &idempotency_key,
                &AuditEvent {
                    id: AuditEventId::new(),
                    actor: AuditActor::System,
                    action: "build_target.schedule".to_owned(),
                    resource_kind: "build_target".to_owned(),
                    resource_id: Some(target.id.to_string()),
                    details: serde_json::json!({
                        "run_id": run.id,
                        "job_id": job.id,
                        "scheduled_for": due_at,
                        "next_run_at": next_run_at,
                    }),
                    request_id: None,
                    occurred_at: now,
                },
                now,
            )
            .await;
        match result {
            Ok(creation) if creation.outcome == CompletionOutcome::Completed => {
                scheduled += 1;
            }
            Ok(_) | Err(StorageError::Conflict | StorageError::StaleRevision) => {}
            Err(error) => {
                warn!(%error, target_id = %target.id, "scheduling due target failed");
            }
        }
    }
    Ok(scheduled)
}

/// Polls durable target cursors until the process is stopped.
pub async fn run_scheduler(storage: Arc<dyn Storage>, poll_seconds: u64) {
    let mut interval = tokio::time::interval(StdDuration::from_secs(poll_seconds));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        match schedule_due_once(&storage, Utc::now()).await {
            Ok(0) => {}
            Ok(count) => info!(count, "scheduled due build targets"),
            Err(error) => warn!(%error, "build target scheduler cycle failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_cursor_skips_missed_intervals_without_drift() {
        let due = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let now = due + Duration::seconds(185);
        assert_eq!(
            next_cursor_after(due, 60, now).unwrap(),
            due + Duration::seconds(240)
        );
    }
}
