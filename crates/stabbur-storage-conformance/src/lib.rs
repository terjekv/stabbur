//! Reusable semantic acceptance tests for every selectable relational storage backend.

use std::{collections::BTreeMap, sync::Arc};

use chrono::{DateTime, Duration, Utc};
use stabbur_auth_core::{
    PasswordPolicy, Permission, Principal, PrincipalId, PrincipalKind, RoleName, generate_token,
};
use stabbur_builder_core::{
    BuildRequest, BuildResult, BuilderExecutionResult, BuilderJob, BuiltVariant, Provenance,
    RecipeCatalogDiagnostic, RecipeCatalogDiagnosticSeverity, RecipeCatalogEntry,
    RecipeCatalogManifest, RecipeCatalogScanExecutionResult, RecipeCatalogScanJob,
    RecipeCatalogScanRequest, RecipeCatalogSource, VariantArtifact, VerificationResult,
};
use stabbur_domain::{
    Architecture, Artifact, ArtifactRole, AuditEventId, BuildTargetId, JobId, LocationId,
    LocationState, Platform, RecipeCatalogScanId, RecipeCatalogSnapshotId, RecipeId,
    RecipeRevisionId, ReleaseState, RunId, Sha256Digest, Software, SoftwareId,
    SoftwareInstallation, SoftwareSlug, StoreId, WorkerId,
};
use stabbur_jobs_core::{Capability, CapabilitySet, Job, JobState, JobSubject};
use stabbur_storage_core::{
    ApiTokenRecord, ArtifactLocation, AuditActor, AuditEvent, BootstrapPreparation,
    BuildDisposition, BuildTargetRecord, BuildTargetRunTrigger, BuildTargetSchedule, ClaimedJob,
    CompletionOutcome, NewRecipeRevision, NewRunLogEntry, RecipeCatalogPublishOutcome,
    RecipeCatalogScanRecord, RecipeCatalogSnapshotRecord, RecipeRecord, RunLogAppendOutcome,
    RunLogStream, RunRecord, RunState, Storage, StorageError, WorkerRecord,
};

/// Exercises portable transaction, identity, artifact, worker, lease, idempotency, and audit
/// semantics against a fresh, migrated backend.
///
/// Adapter test suites should construct an isolated empty database, erase it after the test, and
/// pass the complete adapter behind [`Storage`]. Native adapter tests remain responsible for SQL
/// dialect, migration, locking, pool, TLS, and restart behavior.
pub async fn assert_storage_contract(storage: Arc<dyn Storage>) {
    let now = Utc::now();
    storage
        .check_readiness()
        .await
        .expect("backend must be ready for normal requests");
    let health = storage.doctor().await.expect("backend must be queryable");
    assert!(
        !health.backend.is_empty(),
        "backend identity must be stable"
    );
    assert!(health.database_ready, "migrations must be ready");

    let principal = assert_identity_contract(&storage, now).await;
    assert_identity_admin_contract(&storage, &principal, now + Duration::seconds(1)).await;
    let (software, audit_id) = assert_resource_contract(&storage, &principal, now).await;
    assert_rollback_contract(&storage, &software, now).await;
    assert_job_contract(&storage, now).await;
    let (worker, capabilities) =
        assert_recipe_run_worker_contract(&storage, &principal, &software, now).await;
    assert_recipe_catalog_contract(&storage, &worker, &capabilities, now + Duration::seconds(9))
        .await;
    assert_recipe_catalog_scan_contract(
        &storage,
        &principal,
        &worker,
        &capabilities,
        now + Duration::seconds(10),
    )
    .await;
    assert_build_catalog_contract(
        &storage,
        &principal,
        &software,
        &worker,
        &capabilities,
        now + Duration::seconds(12),
    )
    .await;
    assert_build_target_contract(
        &storage,
        &principal,
        &software,
        &capabilities,
        now + Duration::seconds(20),
    )
    .await;

    let events = storage
        .audit_events(None, 200)
        .await
        .expect("audit read must succeed");
    assert!(events.iter().any(|event| event.id == audit_id));
}

/// Exercises exclusive local first-administrator bootstrap against both a pristine backend and a
/// backend whose HTTP bootstrap secret was prepared but its raw file was lost.
///
/// Adapter suites must pass two independent, empty, migrated backend instances. This is separate
/// from [`assert_storage_contract`] so the main scenario can continue exercising successful
/// secret-authenticated bootstrap.
pub async fn assert_local_bootstrap_contract(
    pristine: Arc<dyn Storage>,
    pending: Arc<dyn Storage>,
) {
    let now = Utc::now();
    let password = PasswordPolicy::default()
        .hash("correct horse battery staple")
        .expect("static test password must satisfy policy");

    let pristine_admin = pristine
        .bootstrap_admin_local("local-pristine-admin", &password, now)
        .await
        .expect("local bootstrap must initialize a pristine backend");
    assert_local_bootstrap_result(&pristine, &pristine_admin, now).await;

    let (_, pending_hash) = generate_token();
    assert_eq!(
        pending
            .prepare_bootstrap(&pending_hash)
            .await
            .expect("HTTP bootstrap preparation must succeed"),
        BootstrapPreparation::Created
    );
    let pending_admin = pending
        .bootstrap_admin_local("local-pending-admin", &password, now)
        .await
        .expect("local bootstrap must recover a pending bootstrap without its raw secret");
    assert_local_bootstrap_result(&pending, &pending_admin, now).await;
}

async fn assert_local_bootstrap_result(
    storage: &Arc<dyn Storage>,
    administrator: &Principal,
    now: DateTime<Utc>,
) {
    assert!(
        administrator
            .roles
            .contains(&RoleName::new("admin").expect("built-in role name is valid"))
    );
    assert_eq!(
        storage
            .human_credential(&administrator.name)
            .await
            .expect("human lookup must succeed")
            .expect("local bootstrap must create a human")
            .principal,
        *administrator
    );
    let another_password = PasswordPolicy::default()
        .hash("another correct password")
        .expect("static test password must satisfy policy");
    assert_eq!(
        storage
            .bootstrap_admin_local("second-local-admin", &another_password, now)
            .await,
        Err(StorageError::BootstrapUnavailable)
    );
    let events = storage
        .audit_events(None, 20)
        .await
        .expect("local bootstrap audit must be readable");
    let administrator_id = administrator.id.to_string();
    assert!(events.iter().any(|event| {
        event.actor == AuditActor::LocalBreakGlass
            && event.action == "auth.bootstrap"
            && event.resource_id.as_deref() == Some(administrator_id.as_str())
    }));
}

#[allow(clippy::too_many_lines)] // One sequential scenario verifies atomic cross-operation invariants.
async fn assert_identity_admin_contract(
    storage: &Arc<dyn Storage>,
    administrator: &Principal,
    now: DateTime<Utc>,
) {
    let role_name = RoleName::new("contract-auditor").expect("static role name is valid");
    let role = storage
        .create_role(
            &role_name,
            &[Permission::AuditRead, Permission::SoftwareRead],
            &contract_audit(
                administrator,
                "auth.role.create",
                "role",
                role_name.to_string(),
                now,
            ),
        )
        .await
        .expect("custom role creation must succeed");
    assert!(!role.role.built_in);
    assert!(
        storage
            .roles()
            .await
            .expect("role listing must succeed")
            .iter()
            .any(|record| record.role.name == role_name)
    );
    let service = Principal {
        id: PrincipalId::new(),
        name: "contract-service".to_owned(),
        kind: PrincipalKind::Service,
        roles: [role_name.clone()].into(),
        enabled: true,
    };
    let service = storage
        .create_principal(
            &service,
            None,
            &contract_audit(
                administrator,
                "auth.principal.create",
                "principal",
                service.id.to_string(),
                now,
            ),
            now,
        )
        .await
        .expect("service principal creation must succeed");
    assert_eq!(service.revision, 1);
    let listed = storage
        .list_principals(None, 200)
        .await
        .expect("principal listing must succeed");
    assert!(
        listed
            .iter()
            .any(|record| record.principal.id == service.principal.id)
    );
    let (secret, token_hash) = generate_token();
    let token = ApiTokenRecord {
        id: uuid::Uuid::now_v7().to_string(),
        principal_id: service.principal.id,
        name: "contract-token".to_owned(),
        created_at: now,
        expires_at: Some(now + Duration::hours(1)),
        revoked_at: None,
    };
    storage
        .create_api_token(
            &token,
            &token_hash,
            &contract_audit(
                administrator,
                "auth.token.create",
                "api_token",
                token.id.clone(),
                now,
            ),
        )
        .await
        .expect("named API token creation must succeed");
    assert_eq!(
        storage
            .principal_by_token(
                &stabbur_auth_core::TokenHash::from_secret(secret.expose_secret()),
                now,
            )
            .await
            .expect("named token authentication must succeed")
            .expect("named token must authenticate")
            .id,
        service.principal.id
    );
    let assigned = storage
        .assign_roles(
            service.principal.id,
            &[
                role_name,
                RoleName::new("reader").expect("built-in role name is valid"),
            ],
            service.revision,
            &contract_audit(
                administrator,
                "auth.principal.roles.assign",
                "principal",
                service.principal.id.to_string(),
                now,
            ),
        )
        .await
        .expect("role assignment must succeed");
    assert_eq!(assigned.revision, 2);
    let disabled = storage
        .set_principal_enabled(
            service.principal.id,
            false,
            assigned.revision,
            &contract_audit(
                administrator,
                "auth.principal.status.set",
                "principal",
                service.principal.id.to_string(),
                now,
            ),
            now,
        )
        .await
        .expect("service disable must succeed");
    assert!(!disabled.principal.enabled);
    assert!(
        storage
            .principal_by_token(&token_hash, now)
            .await
            .expect("disabled token lookup must succeed")
            .is_none()
    );
    assert_eq!(
        storage
            .set_principal_enabled(
                administrator.id,
                false,
                1,
                &contract_audit(
                    administrator,
                    "auth.principal.status.set",
                    "principal",
                    administrator.id.to_string(),
                    now,
                ),
                now,
            )
            .await,
        Err(StorageError::Conflict),
        "the last enabled administrator must be protected"
    );
}

#[allow(clippy::too_many_lines)] // Keeping the shared scenario intact makes backend ordering failures reproducible.
async fn assert_recipe_run_worker_contract(
    storage: &Arc<dyn Storage>,
    principal: &Principal,
    software: &Software,
    now: DateTime<Utc>,
) -> (WorkerRecord, CapabilitySet) {
    let (worker, capabilities) = provision_contract_worker(storage, principal, now).await;
    let run = create_contract_recipe_run(storage, principal, software, &capabilities, now).await;
    let drain_audit = || {
        contract_audit(
            principal,
            "worker.drain",
            "worker",
            worker.id.to_string(),
            now,
        )
    };
    let draining = storage
        .set_worker_draining(worker.id, true, worker.revision, &drain_audit())
        .await
        .expect("worker can drain");
    assert!(draining.draining);
    assert!(
        storage
            .claim_job(worker.id, &capabilities, now, 60)
            .await
            .unwrap()
            .is_none(),
        "draining workers cannot claim queued work"
    );
    assert_eq!(
        storage
            .set_worker_draining(worker.id, false, worker.revision, &drain_audit())
            .await,
        Err(StorageError::StaleRevision)
    );
    let worker = storage
        .set_worker_draining(worker.id, false, draining.revision, &drain_audit())
        .await
        .expect("worker can resume");
    let claimed = storage
        .claim_job(worker.id, &capabilities, now, 60)
        .await
        .expect("run job claim must succeed")
        .expect("compatible run job must exist");
    assert_eq!(
        storage
            .run(run.id)
            .await
            .expect("running run lookup must succeed")
            .expect("run must exist")
            .state,
        RunState::Running
    );
    let draining = storage
        .set_worker_draining(worker.id, true, worker.revision, &drain_audit())
        .await
        .unwrap();
    let log_entries = [
        NewRunLogEntry {
            stream: RunLogStream::System,
            message: b"attempt started".to_vec(),
        },
        NewRunLogEntry {
            stream: RunLogStream::Stdout,
            message: b"builder output".to_vec(),
        },
    ];
    let receipt = storage
        .append_run_logs(
            worker.id,
            &claimed.lease,
            "contract-log-batch",
            &log_entries,
            now + Duration::milliseconds(500),
        )
        .await
        .expect("leased worker must append logs");
    let RunLogAppendOutcome::Appended(receipt) = receipt else {
        panic!("first log append must not be a replay");
    };
    assert_eq!(receipt.first_sequence, 0);
    assert_eq!(receipt.last_sequence, 1);
    assert!(matches!(
        storage
            .append_run_logs(
                worker.id,
                &claimed.lease,
                "contract-log-batch",
                &log_entries,
                now + Duration::milliseconds(750),
            )
            .await
            .expect("log replay must succeed"),
        RunLogAppendOutcome::Replayed(replayed) if replayed == receipt
    ));
    let logs = storage
        .run_logs(run.id, None, 50)
        .await
        .expect("run log replay must succeed");
    assert_eq!(logs.len(), 2);
    assert_eq!(logs[0].sequence, 0);
    assert_eq!(logs[1].message, b"builder output");
    assert_eq!(
        storage
            .fail_job(
                worker.id,
                &claimed.lease,
                "contract-failure",
                &serde_json::json!({"code": "contract_failure"}),
                now + Duration::seconds(1),
            )
            .await
            .expect("terminal failure must commit"),
        CompletionOutcome::Completed
    );
    let failed = storage
        .run(run.id)
        .await
        .expect("failed run lookup must succeed")
        .expect("failed run must exist");
    assert_eq!(failed.state, RunState::Failed);
    assert_eq!(
        failed.result,
        Some(serde_json::json!({"code": "contract_failure"}))
    );
    let worker = storage
        .set_worker_draining(worker.id, false, draining.revision, &drain_audit())
        .await
        .unwrap();
    let cancelled = RunRecord {
        id: RunId::new(),
        recipe_revision_id: run.recipe_revision_id,
        software_id: software.id,
        state: RunState::Queued,
        parameters: serde_json::json!({}),
        result: None,
        created_at: now + Duration::seconds(2),
        completed_at: None,
    };
    let cancelled_job = Job {
        id: JobId::new(),
        subject: JobSubject::BuildRun {
            run_id: cancelled.id,
        },
        required_capabilities: capabilities.clone(),
        payload: serde_json::json!({"schema_version": 1}),
        state: JobState::Queued,
        maximum_attempts: 2,
        attempt_count: 0,
        created_at: cancelled.created_at,
    };
    storage
        .create_run(
            &cancelled,
            &cancelled_job,
            "contract:cancelled-run",
            "create",
            &contract_audit(
                principal,
                "run.create",
                "run",
                cancelled.id.to_string(),
                cancelled.created_at,
            ),
        )
        .await
        .expect("cancellation run must be created");
    let cancelled_claim = storage
        .claim_job(worker.id, &capabilities, cancelled.created_at, 60)
        .await
        .expect("cancellation job claim must succeed")
        .expect("cancellation job must be claimable");
    let cancelled = storage
        .cancel_run(
            cancelled.id,
            "contract-cancel",
            &contract_audit(
                principal,
                "run.cancel",
                "run",
                cancelled.id.to_string(),
                cancelled.created_at + Duration::seconds(1),
            ),
            cancelled.created_at + Duration::seconds(1),
        )
        .await
        .expect("active run cancellation must succeed");
    assert_eq!(cancelled.state, RunState::Cancelled);
    assert_eq!(
        storage
            .heartbeat_job(
                worker.id,
                &cancelled_claim.lease,
                cancelled.created_at + Duration::seconds(2),
                60,
            )
            .await,
        Err(StorageError::InvalidLease),
        "cancellation must be immediately worker-visible"
    );
    (worker, capabilities)
}

fn contract_target_run(
    target: &BuildTargetRecord,
    revision: &stabbur_storage_core::RecipeRevisionRecord,
    now: DateTime<Utc>,
) -> (RunRecord, Job) {
    let run = RunRecord {
        id: RunId::new(),
        recipe_revision_id: target.recipe_revision_id,
        software_id: target.software_id,
        state: RunState::Queued,
        parameters: target.parameters.clone(),
        result: None,
        created_at: now,
        completed_at: None,
    };
    let parameters = serde_json::from_value(target.parameters.clone())
        .expect("contract target parameters are builder-neutral");
    let payload = serde_json::to_value(BuilderJob::new(
        BuildRequest {
            run_id: run.id,
            software: target.software_id,
            recipe_revision: target.recipe_revision_id,
            parameters,
            required_capabilities: revision.required_capabilities.clone(),
        },
        &revision.builder,
        revision.definition.clone(),
    ))
    .expect("contract target job must serialize");
    let job = Job {
        id: JobId::new(),
        subject: JobSubject::BuildRun { run_id: run.id },
        required_capabilities: revision.required_capabilities.clone(),
        payload,
        state: JobState::Queued,
        maximum_attempts: 3,
        attempt_count: 0,
        created_at: now,
    };
    (run, job)
}

#[allow(clippy::too_many_lines)] // CRUD, cursor consumption, replay, and manual triggering form one contract.
async fn assert_build_target_contract(
    storage: &Arc<dyn Storage>,
    principal: &Principal,
    software: &Software,
    capabilities: &CapabilitySet,
    now: DateTime<Utc>,
) {
    let recipe = RecipeRecord {
        id: RecipeId::new(),
        name: "contract-target-recipe".to_owned(),
        created_at: now,
        revision: 1,
    };
    storage
        .create_recipe(
            &recipe,
            &contract_audit(
                principal,
                "recipe.create",
                "recipe",
                recipe.id.to_string(),
                now,
            ),
        )
        .await
        .expect("target recipe creation must succeed");
    let revision_id = RecipeRevisionId::new();
    let revision = storage
        .create_recipe_revision(
            &NewRecipeRevision {
                expected_sequence: None,
                id: revision_id,
                recipe_id: recipe.id,
                builder: "fake".to_owned(),
                definition: serde_json::json!({}),
                required_capabilities: capabilities.clone(),
                created_at: now,
            },
            &contract_audit(
                principal,
                "recipe.revision.create",
                "recipe_revision",
                revision_id.to_string(),
                now,
            ),
        )
        .await
        .expect("target recipe revision must succeed");
    let target = BuildTargetRecord {
        id: BuildTargetId::new(),
        name: "contract-recurring-target".to_owned(),
        software_id: software.id,
        recipe_revision_id: revision.id,
        parameters: serde_json::json!({"CHANNEL": "contract"}),
        schedule: BuildTargetSchedule::Interval { every_seconds: 60 },
        enabled: true,
        next_run_at: Some(now),
        created_at: now,
        updated_at: now,
        revision: 1,
    };
    let target = storage
        .create_build_target(
            &target,
            &contract_audit(
                principal,
                "build_target.create",
                "build_target",
                target.id.to_string(),
                now,
            ),
        )
        .await
        .expect("build target creation must succeed");
    assert_eq!(
        storage
            .build_target(&target.name)
            .await
            .expect("target name lookup must succeed"),
        Some(target.clone())
    );
    assert_eq!(
        storage
            .list_build_targets(None, 200)
            .await
            .expect("target listing must succeed"),
        vec![target.clone()]
    );
    assert_eq!(
        storage
            .due_build_targets(now, 200)
            .await
            .expect("due target listing must succeed"),
        vec![target.clone()]
    );

    let mut disabled = target.clone();
    disabled.enabled = false;
    disabled.updated_at = now + Duration::seconds(1);
    let disabled = storage
        .update_build_target(
            &disabled,
            1,
            &contract_audit(
                principal,
                "build_target.update",
                "build_target",
                target.id.to_string(),
                disabled.updated_at,
            ),
        )
        .await
        .expect("target disabling must succeed");
    assert_eq!(disabled.revision, 2);
    assert!(
        storage
            .due_build_targets(disabled.updated_at, 200)
            .await
            .expect("disabled due lookup must succeed")
            .is_empty()
    );
    assert_eq!(
        storage
            .update_build_target(
                &disabled,
                1,
                &contract_audit(
                    principal,
                    "build_target.update",
                    "build_target",
                    target.id.to_string(),
                    disabled.updated_at,
                ),
            )
            .await,
        Err(StorageError::StaleRevision)
    );
    let mut enabled = disabled;
    enabled.enabled = true;
    enabled.updated_at = now + Duration::seconds(2);
    let enabled = storage
        .update_build_target(
            &enabled,
            2,
            &contract_audit(
                principal,
                "build_target.update",
                "build_target",
                target.id.to_string(),
                enabled.updated_at,
            ),
        )
        .await
        .expect("target re-enabling must succeed");
    assert_eq!(enabled.revision, 3);

    let observed_at = now + Duration::seconds(2);
    let due_at = enabled.next_run_at.expect("interval target has a cursor");
    let next_run_at = now + Duration::seconds(60);
    let (scheduled_run, scheduled_job) = contract_target_run(&enabled, &revision, observed_at);
    let scheduled = storage
        .create_build_target_run(
            enabled.id,
            BuildTargetRunTrigger::Scheduled {
                target_revision: enabled.revision,
                due_at,
                next_run_at,
            },
            &scheduled_run,
            &scheduled_job,
            "contract-due-cursor",
            &AuditEvent {
                id: AuditEventId::new(),
                actor: AuditActor::System,
                action: "build_target.schedule".to_owned(),
                resource_kind: "build_target".to_owned(),
                resource_id: Some(enabled.id.to_string()),
                details: serde_json::json!({"run_id": scheduled_run.id}),
                request_id: None,
                occurred_at: observed_at,
            },
            observed_at,
        )
        .await
        .expect("due cursor must atomically create a run");
    assert_eq!(scheduled.outcome, CompletionOutcome::Completed);
    assert_eq!(scheduled.run, scheduled_run);
    let after_schedule = storage
        .build_target(&enabled.id.to_string())
        .await
        .expect("scheduled target lookup must succeed")
        .expect("scheduled target must remain present");
    assert_eq!(after_schedule.next_run_at, Some(next_run_at));
    assert_eq!(after_schedule.revision, enabled.revision + 1);
    assert!(
        storage
            .due_build_targets(observed_at, 200)
            .await
            .expect("post-schedule due lookup must succeed")
            .is_empty()
    );
    assert_eq!(
        storage
            .create_build_target_run(
                enabled.id,
                BuildTargetRunTrigger::Scheduled {
                    target_revision: enabled.revision,
                    due_at,
                    next_run_at,
                },
                &scheduled_run,
                &scheduled_job,
                "contract-due-cursor",
                &AuditEvent {
                    id: AuditEventId::new(),
                    actor: AuditActor::System,
                    action: "build_target.schedule".to_owned(),
                    resource_kind: "build_target".to_owned(),
                    resource_id: Some(enabled.id.to_string()),
                    details: serde_json::json!({}),
                    request_id: None,
                    occurred_at: observed_at,
                },
                observed_at,
            )
            .await
            .expect("scheduled cursor replay must succeed")
            .outcome,
        CompletionOutcome::Replayed
    );
    storage
        .cancel_run(
            scheduled_run.id,
            "cancel-scheduled-target-run",
            &contract_audit(
                principal,
                "run.cancel",
                "run",
                scheduled_run.id.to_string(),
                observed_at,
            ),
            observed_at,
        )
        .await
        .expect("scheduled target run cancellation must succeed");

    let manual_at = now + Duration::seconds(3);
    let (manual_run, manual_job) = contract_target_run(&after_schedule, &revision, manual_at);
    let manual = storage
        .create_build_target_run(
            enabled.id,
            BuildTargetRunTrigger::Manual,
            &manual_run,
            &manual_job,
            "contract-manual-trigger",
            &contract_audit(
                principal,
                "build_target.run.create",
                "build_target",
                enabled.id.to_string(),
                manual_at,
            ),
            manual_at,
        )
        .await
        .expect("manual target trigger must succeed");
    assert_eq!(manual.outcome, CompletionOutcome::Completed);
    assert_eq!(manual.run, manual_run);
    storage
        .cancel_run(
            manual_run.id,
            "cancel-manual-target-run",
            &contract_audit(
                principal,
                "run.cancel",
                "run",
                manual_run.id.to_string(),
                manual_at,
            ),
            manual_at,
        )
        .await
        .expect("manual target run cancellation must succeed");
    let target_runs = storage
        .list_build_target_runs(enabled.id, None, 200)
        .await
        .expect("target run history must be readable");
    assert_eq!(target_runs.len(), 2);
    assert!(target_runs.iter().any(|run| run.id == scheduled_run.id));
    assert!(target_runs.iter().any(|run| run.id == manual_run.id));
}

#[allow(clippy::too_many_arguments)]
async fn prepare_build_attempt(
    storage: &Arc<dyn Storage>,
    principal: &Principal,
    software: &Software,
    revision_id: RecipeRevisionId,
    worker: &WorkerRecord,
    capabilities: &CapabilitySet,
    store_id: StoreId,
    artifact: &Artifact,
    now: DateTime<Utc>,
) -> (RunRecord, ClaimedJob) {
    let run = RunRecord {
        id: RunId::new(),
        recipe_revision_id: revision_id,
        software_id: software.id,
        state: RunState::Queued,
        parameters: serde_json::json!({}),
        result: None,
        created_at: now,
        completed_at: None,
    };
    let job = Job {
        id: JobId::new(),
        subject: JobSubject::BuildRun { run_id: run.id },
        required_capabilities: capabilities.clone(),
        payload: serde_json::json!({"schema_version": 1}),
        state: JobState::Queued,
        maximum_attempts: 2,
        attempt_count: 0,
        created_at: now,
    };
    storage
        .create_run(
            &run,
            &job,
            &format!("contract:build-run:{}", run.id),
            "create",
            &contract_audit(principal, "run.create", "run", run.id.to_string(), now),
        )
        .await
        .expect("rebuild run must be created");
    let claimed = storage
        .claim_job(worker.id, capabilities, now, 60)
        .await
        .expect("rebuild job claim must succeed")
        .expect("rebuild job must be claimable");
    storage
        .record_run_artifact(
            worker.id,
            claimed.lease.attempt_id,
            artifact,
            &ArtifactLocation {
                id: LocationId::new(),
                digest: artifact.digest.clone(),
                store_id,
                state: LocationState::Present,
                verified_at: Some(now),
                last_error: None,
            },
            ArtifactRole::PrimaryInstaller,
            &contract_audit(
                principal,
                "run.artifact.upload",
                "run_artifact",
                artifact.digest.to_string(),
                now,
            ),
            now,
        )
        .await
        .expect("leased rebuild artifact must be recorded");
    (run, claimed)
}

#[allow(clippy::too_many_lines)] // Publication, replay, and latest-source replacement are one contract.
async fn assert_recipe_catalog_contract(
    storage: &Arc<dyn Storage>,
    worker: &WorkerRecord,
    capabilities: &CapabilitySet,
    now: DateTime<Utc>,
) {
    let manifest = RecipeCatalogManifest {
        schema_version: RecipeCatalogManifest::SCHEMA_VERSION,
        producer: "contract-run".into(),
        source: RecipeCatalogSource {
            locator: "https://example.test/recipes.git".into(),
            revision: "a".repeat(40),
        },
        recipes: vec![
            RecipeCatalogEntry {
                import_sources: None,
                identifier: "com.example.alpha".into(),
                builder: "contract-run".into(),
                parents: vec![],
                required_capabilities: capabilities.clone(),
            },
            RecipeCatalogEntry {
                import_sources: None,
                identifier: "com.example.beta".into(),
                builder: "contract-run".into(),
                parents: vec!["com.example.alpha".into()],
                required_capabilities: capabilities.clone(),
            },
        ],
        diagnostics: vec![RecipeCatalogDiagnostic {
            identifier: Some("com.example.beta".into()),
            code: "parent_observed".into(),
            severity: RecipeCatalogDiagnosticSeverity::Info,
            detail: "A normalized parent relationship was observed.".into(),
        }],
    };
    let snapshot = RecipeCatalogSnapshotRecord {
        id: RecipeCatalogSnapshotId::new(),
        worker_id: worker.id,
        manifest_digest: manifest.canonical_digest().expect("manifest must hash"),
        manifest,
        observed_at: now,
    };
    let audit = |snapshot_id: RecipeCatalogSnapshotId, occurred_at: DateTime<Utc>| AuditEvent {
        id: AuditEventId::new(),
        actor: AuditActor::Principal(worker.principal_id.expect("worker principal must exist")),
        action: "recipe_catalog.publish".into(),
        resource_kind: "recipe_catalog_snapshot".into(),
        resource_id: Some(snapshot_id.to_string()),
        details: serde_json::json!({"worker_id": worker.id}),
        request_id: Some("storage-conformance".into()),
        occurred_at,
    };
    assert_eq!(
        storage
            .publish_recipe_catalog(&snapshot, &audit(snapshot.id, now))
            .await
            .expect("catalog publication must succeed")
            .outcome,
        RecipeCatalogPublishOutcome::Published
    );
    let replay = RecipeCatalogSnapshotRecord {
        id: RecipeCatalogSnapshotId::new(),
        observed_at: now + Duration::seconds(1),
        ..snapshot.clone()
    };
    assert_eq!(
        storage
            .publish_recipe_catalog(&replay, &audit(replay.id, replay.observed_at))
            .await
            .expect("catalog publication must replay by worker and digest")
            .outcome,
        RecipeCatalogPublishOutcome::Replayed
    );
    assert_eq!(
        storage
            .recipe_catalog_snapshot(snapshot.id)
            .await
            .expect("catalog snapshot lookup must succeed"),
        Some(snapshot.clone())
    );
    let summaries = storage
        .list_recipe_catalog_snapshots(None, 50)
        .await
        .expect("catalog snapshot list must succeed");
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].id, snapshot.id);
    assert_eq!(summaries[0].recipe_count, 2);
    assert_eq!(summaries[0].diagnostic_count, 1);
    let alpha = storage
        .latest_recipe_catalog_matches("com.example.alpha")
        .await
        .expect("catalog identifier lookup must succeed");
    assert_eq!(alpha.len(), 1);
    assert_eq!(alpha[0].snapshot_id, snapshot.id);

    let replacement_manifest = RecipeCatalogManifest {
        source: RecipeCatalogSource {
            revision: "b".repeat(40),
            ..snapshot.manifest.source.clone()
        },
        recipes: vec![snapshot.manifest.recipes[1].clone()],
        diagnostics: vec![],
        ..snapshot.manifest.clone()
    };
    let replacement = RecipeCatalogSnapshotRecord {
        id: RecipeCatalogSnapshotId::new(),
        worker_id: worker.id,
        manifest_digest: replacement_manifest
            .canonical_digest()
            .expect("replacement manifest must hash"),
        manifest: replacement_manifest,
        observed_at: now + Duration::seconds(2),
    };
    assert_eq!(
        storage
            .publish_recipe_catalog(
                &replacement,
                &audit(replacement.id, replacement.observed_at),
            )
            .await
            .expect("replacement catalog publication must succeed")
            .outcome,
        RecipeCatalogPublishOutcome::Published
    );
    assert!(
        storage
            .latest_recipe_catalog_matches("com.example.alpha")
            .await
            .expect("latest catalog lookup must succeed")
            .is_empty(),
        "entries removed from the latest pinned source must not remain discoverable"
    );
    let beta = storage
        .latest_recipe_catalog_matches("com.example.beta")
        .await
        .expect("replacement catalog lookup must succeed");
    assert_eq!(beta.len(), 1);
    assert_eq!(beta[0].snapshot_id, replacement.id);
}

#[allow(clippy::too_many_lines)] // Creation, claim, publication, and lookup form one adapter contract.
async fn assert_recipe_catalog_scan_contract(
    storage: &Arc<dyn Storage>,
    principal: &Principal,
    worker: &WorkerRecord,
    capabilities: &CapabilitySet,
    now: DateTime<Utc>,
) {
    let scan_id = RecipeCatalogScanId::new();
    let source = RecipeCatalogSource {
        locator: "https://example.test/scanned-recipes.git".into(),
        revision: "c".repeat(40),
    };
    let request = RecipeCatalogScanRequest {
        scan_id,
        producer: "contract-run".into(),
        source: source.clone(),
        required_capabilities: capabilities.clone(),
    };
    let payload = serde_json::to_value(RecipeCatalogScanJob::new(request))
        .expect("catalog scan job must serialize");
    let job = Job {
        id: JobId::new(),
        subject: JobSubject::RecipeCatalogScan { scan_id },
        required_capabilities: capabilities.clone(),
        payload,
        state: JobState::Queued,
        maximum_attempts: 2,
        attempt_count: 0,
        created_at: now,
    };
    let scan = RecipeCatalogScanRecord {
        id: scan_id,
        job_id: job.id,
        producer: "contract-run".into(),
        source: source.clone(),
        state: JobState::Queued,
        snapshot_id: None,
        failure: None,
        requested_at: now,
        completed_at: None,
    };
    let audit = contract_audit(
        principal,
        "recipe_catalog_scan.create",
        "recipe_catalog_scan",
        scan_id.to_string(),
        now,
    );
    let creation = storage
        .create_recipe_catalog_scan(&scan, &job, "contract:catalog-scan", "create", &audit)
        .await
        .expect("catalog scan creation must succeed");
    assert_eq!(creation.outcome, CompletionOutcome::Completed);
    let replay = storage
        .create_recipe_catalog_scan(&scan, &job, "contract:catalog-scan", "create", &audit)
        .await
        .expect("catalog scan creation replay must succeed");
    assert_eq!(replay.outcome, CompletionOutcome::Replayed);
    let claimed = storage
        .claim_job(worker.id, capabilities, now + Duration::seconds(1), 60)
        .await
        .expect("catalog scan claim must succeed")
        .expect("catalog scan must be claimable");
    assert_eq!(
        claimed.job.subject,
        JobSubject::RecipeCatalogScan { scan_id }
    );
    let manifest = RecipeCatalogManifest {
        schema_version: RecipeCatalogManifest::SCHEMA_VERSION,
        producer: "contract-run".into(),
        source,
        recipes: vec![RecipeCatalogEntry {
            import_sources: None,
            identifier: "com.example.scanned".into(),
            builder: "contract-run".into(),
            parents: vec![],
            required_capabilities: capabilities.clone(),
        }],
        diagnostics: vec![],
    };
    let result = RecipeCatalogScanExecutionResult {
        schema_version: RecipeCatalogScanJob::SCHEMA_VERSION,
        scan_id,
        manifest,
        completed_at: now + Duration::seconds(2),
    };
    let completion = storage
        .complete_recipe_catalog_scan(
            worker.id,
            &claimed.lease,
            "complete",
            &result,
            &contract_audit(
                principal,
                "recipe_catalog_scan.complete",
                "recipe_catalog_scan",
                scan_id.to_string(),
                now + Duration::seconds(2),
            ),
            now + Duration::seconds(2),
        )
        .await
        .expect("catalog scan completion must succeed");
    assert_eq!(completion.outcome, CompletionOutcome::Completed);
    let completed = storage
        .recipe_catalog_scan(scan_id)
        .await
        .expect("catalog scan lookup must succeed")
        .expect("catalog scan must exist");
    assert_eq!(completed.state, JobState::Succeeded);
    assert_eq!(
        completed.snapshot_id,
        Some(completion.publication.snapshot.id)
    );
    assert_eq!(
        storage
            .list_recipe_catalog_scans(None, 50)
            .await
            .expect("catalog scan list must succeed")[0]
            .id,
        scan_id
    );
    assert_catalog_scan_exhaustion(
        storage,
        principal,
        worker,
        capabilities,
        now + Duration::seconds(3),
    )
    .await;
}

async fn assert_catalog_scan_exhaustion(
    storage: &Arc<dyn Storage>,
    principal: &Principal,
    worker: &WorkerRecord,
    capabilities: &CapabilitySet,
    now: DateTime<Utc>,
) {
    let scan_id = RecipeCatalogScanId::new();
    let source = RecipeCatalogSource {
        locator: "https://example.test/exhausted-recipes.git".into(),
        revision: "d".repeat(40),
    };
    let request = RecipeCatalogScanRequest {
        scan_id,
        producer: "contract-run".into(),
        source: source.clone(),
        required_capabilities: capabilities.clone(),
    };
    let job = Job {
        id: JobId::new(),
        subject: JobSubject::RecipeCatalogScan { scan_id },
        required_capabilities: capabilities.clone(),
        payload: serde_json::to_value(RecipeCatalogScanJob::new(request))
            .expect("catalog scan job must serialize"),
        state: JobState::Queued,
        maximum_attempts: 1,
        attempt_count: 0,
        created_at: now,
    };
    let scan = RecipeCatalogScanRecord {
        id: scan_id,
        job_id: job.id,
        producer: "contract-run".into(),
        source,
        state: JobState::Queued,
        snapshot_id: None,
        failure: None,
        requested_at: now,
        completed_at: None,
    };
    storage
        .create_recipe_catalog_scan(
            &scan,
            &job,
            "contract:catalog-scan-exhaustion",
            "create",
            &contract_audit(
                principal,
                "recipe_catalog_scan.create",
                "recipe_catalog_scan",
                scan_id.to_string(),
                now,
            ),
        )
        .await
        .expect("exhaustion scan creation must succeed");
    storage
        .claim_job(worker.id, capabilities, now + Duration::seconds(1), 60)
        .await
        .expect("exhaustion scan claim must succeed")
        .expect("exhaustion scan must be claimable");
    assert!(
        storage
            .claim_job(worker.id, capabilities, now + Duration::seconds(62), 60)
            .await
            .expect("expired catalog scan recovery must succeed")
            .is_none(),
        "an exhausted scan must not be leased again"
    );
    let exhausted = storage
        .recipe_catalog_scan(scan_id)
        .await
        .expect("exhausted catalog scan lookup must succeed")
        .expect("exhausted catalog scan must remain queryable");
    assert_eq!(exhausted.state, JobState::Failed);
    assert!(exhausted.completed_at.is_some());
    let failure = exhausted
        .failure
        .expect("exhausted catalog scan must have typed terminal history");
    assert_eq!(failure.code, "attempts_exhausted");
    assert_eq!(failure.scan_id, scan_id);
}

#[allow(clippy::too_many_lines)] // Publication, rebuild policy, and promotion form one contract.
async fn assert_build_catalog_contract(
    storage: &Arc<dyn Storage>,
    principal: &Principal,
    software: &Software,
    worker: &WorkerRecord,
    capabilities: &CapabilitySet,
    now: DateTime<Utc>,
) {
    let recipe = RecipeRecord {
        id: RecipeId::new(),
        name: "contract-build-recipe".to_owned(),
        created_at: now,
        revision: 1,
    };
    storage
        .create_recipe(
            &recipe,
            &contract_audit(
                principal,
                "recipe.create",
                "recipe",
                recipe.id.to_string(),
                now,
            ),
        )
        .await
        .expect("build recipe must be created");
    let revision_id = RecipeRevisionId::new();
    storage
        .create_recipe_revision(
            &NewRecipeRevision {
                expected_sequence: None,
                id: revision_id,
                recipe_id: recipe.id,
                builder: "contract".to_owned(),
                definition: serde_json::json!({"immutable": true}),
                required_capabilities: capabilities.clone(),
                created_at: now,
            },
            &contract_audit(
                principal,
                "recipe.revision.create",
                "recipe_revision",
                revision_id.to_string(),
                now,
            ),
        )
        .await
        .expect("build recipe revision must be created");
    let run = RunRecord {
        id: RunId::new(),
        recipe_revision_id: revision_id,
        software_id: software.id,
        state: RunState::Queued,
        parameters: serde_json::json!({}),
        result: None,
        created_at: now,
        completed_at: None,
    };
    let job = Job {
        id: JobId::new(),
        subject: JobSubject::BuildRun { run_id: run.id },
        required_capabilities: capabilities.clone(),
        payload: serde_json::json!({"schema_version": 1}),
        state: JobState::Queued,
        maximum_attempts: 2,
        attempt_count: 0,
        created_at: now,
    };
    storage
        .create_run(
            &run,
            &job,
            "contract:build-run",
            "create",
            &contract_audit(principal, "run.create", "run", run.id.to_string(), now),
        )
        .await
        .expect("build run must be created");
    let claimed = storage
        .claim_job(worker.id, capabilities, now, 60)
        .await
        .expect("build job claim must succeed")
        .expect("build job must be claimable");
    let digest = Sha256Digest::new("b".repeat(64)).expect("static digest is valid");
    let artifact = Artifact {
        digest: digest.clone(),
        size: 128,
        media_type: "application/vnd.test.installer".to_owned(),
        created_at: now,
    };
    let store = storage
        .ensure_local_primary_store(StoreId::new(), "contract-primary")
        .await
        .expect("primary store must be reusable");
    storage
        .record_run_artifact(
            worker.id,
            claimed.lease.attempt_id,
            &artifact,
            &ArtifactLocation {
                id: LocationId::new(),
                digest: digest.clone(),
                store_id: store.id,
                state: LocationState::Present,
                verified_at: Some(now),
                last_error: None,
            },
            ArtifactRole::PrimaryInstaller,
            &contract_audit(
                principal,
                "run.artifact.upload",
                "run_artifact",
                digest.to_string(),
                now,
            ),
            now,
        )
        .await
        .expect("leased build artifact must be recorded");
    let result = BuildResult {
        discovered_version: "contract-1".parse().expect("static version is valid"),
        variants: vec![BuiltVariant {
            variant_id: None,
            platform: Platform::MacOs,
            architecture: Architecture::Universal,
            minimum_macos: Some("13".parse().expect("static macOS version is valid")),
            maximum_macos: None,
            resolution_priority: 0,
            artifacts: vec![VariantArtifact {
                digest: digest.clone(),
                size: artifact.size,
                role: ArtifactRole::PrimaryInstaller,
            }],
        }],
        uploaded_artifacts: vec![digest.clone()],
        provenance: Provenance {
            builder: "contract/1".to_owned(),
            worker_version: "0.0.1".to_owned(),
            operating_system: "contract".to_owned(),
            tools: BTreeMap::new(),
            sources: vec![],
            recipe_trust_succeeded: true,
            raw_report: serde_json::json!({}),
            captured_at: now,
        },
        verification_results: vec![VerificationResult {
            check: "contract".to_owned(),
            required: true,
            succeeded: true,
            detail: None,
        }],
    };
    let execution = BuilderExecutionResult {
        schema_version: BuilderJob::SCHEMA_VERSION,
        run_id: run.id,
        adapter: "contract".to_owned(),
        sources: vec![],
        tools: BTreeMap::new(),
        raw_report: serde_json::json!({}),
        build_result: Some(result),
        completed_at: now,
    };
    let audit = contract_audit(principal, "build.finalize", "run", run.id.to_string(), now);
    let completion = storage
        .finalize_build(
            worker.id,
            &claimed.lease,
            "contract-build-completion",
            &execution,
            &audit,
            now,
        )
        .await
        .expect("build completion must publish atomically");
    assert_eq!(completion.outcome, CompletionOutcome::Completed);
    assert_eq!(completion.disposition, BuildDisposition::ReleaseCreated);
    assert_eq!(completion.release.state, ReleaseState::Candidate);
    let replay = storage
        .finalize_build(
            worker.id,
            &claimed.lease,
            "contract-build-completion",
            &execution,
            &audit,
            now + Duration::seconds(1),
        )
        .await
        .expect("build completion replay must succeed");
    assert_eq!(replay.outcome, CompletionOutcome::Replayed);
    assert_eq!(replay.disposition, BuildDisposition::ReleaseCreated);
    let variants = storage
        .release_variants(completion.release.id)
        .await
        .expect("published variants must be readable");
    assert_eq!(variants.len(), 1);
    let artifacts = storage
        .variant_artifacts(variants[0].id)
        .await
        .expect("published variant artifacts must be readable");
    assert_eq!(artifacts.len(), 1);
    assert!(artifacts[0].readable);
    let candidate = storage
        .channel(software.id, "candidate")
        .await
        .expect("candidate channel lookup must succeed")
        .expect("candidate channel must advance automatically");
    assert_eq!(candidate.release_id, completion.release.id);

    let exact_time = now + Duration::seconds(2);
    let (exact_run, exact_claim) = prepare_build_attempt(
        storage,
        principal,
        software,
        revision_id,
        worker,
        capabilities,
        store.id,
        &artifact,
        exact_time,
    )
    .await;
    let mut exact_execution = execution.clone();
    exact_execution.run_id = exact_run.id;
    exact_execution.completed_at = exact_time;
    exact_execution
        .build_result
        .as_mut()
        .expect("contract execution contains a build result")
        .provenance
        .captured_at = exact_time;
    let exact_audit = contract_audit(
        principal,
        "build.finalize",
        "run",
        exact_run.id.to_string(),
        exact_time,
    );
    let exact_completion = storage
        .finalize_build(
            worker.id,
            &exact_claim.lease,
            "contract-exact-rebuild",
            &exact_execution,
            &exact_audit,
            exact_time,
        )
        .await
        .expect("an evidence-identical rebuild must close successfully");
    assert_eq!(exact_completion.outcome, CompletionOutcome::Completed);
    assert_eq!(exact_completion.disposition, BuildDisposition::NoChange);
    assert_eq!(exact_completion.release.id, completion.release.id);
    let exact_stored_run = storage
        .run(exact_run.id)
        .await
        .expect("exact rebuild lookup must succeed")
        .expect("exact rebuild must remain stored");
    assert_eq!(exact_stored_run.state, RunState::Succeeded);
    assert_eq!(
        exact_stored_run
            .result
            .as_ref()
            .and_then(|value| value.pointer("/publication/disposition"))
            .and_then(serde_json::Value::as_str),
        Some("no_change")
    );
    assert_eq!(
        storage
            .channel(software.id, "candidate")
            .await
            .expect("candidate channel lookup after exact rebuild must succeed")
            .expect("candidate channel must remain bound after exact rebuild"),
        candidate
    );

    let evidence_time = now + Duration::seconds(3);
    let (evidence_run, evidence_claim) = prepare_build_attempt(
        storage,
        principal,
        software,
        revision_id,
        worker,
        capabilities,
        store.id,
        &artifact,
        evidence_time,
    )
    .await;
    let mut evidence_execution = exact_execution.clone();
    evidence_execution.run_id = evidence_run.id;
    evidence_execution
        .build_result
        .as_mut()
        .unwrap()
        .provenance
        .worker_version = "new-evidence".into();
    let evidence = storage
        .finalize_build(
            worker.id,
            &evidence_claim.lease,
            "changed-evidence",
            &evidence_execution,
            &contract_audit(
                principal,
                "build.finalize",
                "run",
                evidence_run.id.to_string(),
                evidence_time,
            ),
            evidence_time,
        )
        .await
        .unwrap();
    assert_eq!(evidence.disposition, BuildDisposition::EvidenceChanged);
    assert_eq!(evidence.release.id, completion.release.id);
    assert_eq!(
        storage.run(evidence_run.id).await.unwrap().unwrap().state,
        RunState::Succeeded
    );

    let conflict_time = now + Duration::seconds(4);
    let conflict_digest =
        Sha256Digest::new("c".repeat(64)).expect("static conflicting digest is valid");
    let conflict_artifact = Artifact {
        digest: conflict_digest.clone(),
        size: 129,
        media_type: artifact.media_type.clone(),
        created_at: conflict_time,
    };
    let (conflict_run, conflict_claim) = prepare_build_attempt(
        storage,
        principal,
        software,
        revision_id,
        worker,
        capabilities,
        store.id,
        &conflict_artifact,
        conflict_time,
    )
    .await;
    let mut conflict_execution = execution.clone();
    conflict_execution.run_id = conflict_run.id;
    conflict_execution.completed_at = conflict_time;
    let conflict_result = conflict_execution
        .build_result
        .as_mut()
        .expect("contract execution contains a build result");
    conflict_result.variants[0].artifacts[0].digest = conflict_digest.clone();
    conflict_result.variants[0].artifacts[0].size = conflict_artifact.size;
    conflict_result.uploaded_artifacts = vec![conflict_digest];
    let conflict_audit = contract_audit(
        principal,
        "build.finalize",
        "run",
        conflict_run.id.to_string(),
        conflict_time,
    );
    let conflict_completion = storage
        .finalize_build(
            worker.id,
            &conflict_claim.lease,
            "contract-conflicting-rebuild",
            &conflict_execution,
            &conflict_audit,
            conflict_time,
        )
        .await
        .expect("a conflicting rebuild must close with an explicit disposition");
    assert_eq!(
        conflict_completion.disposition,
        BuildDisposition::VersionContentConflict
    );
    assert_eq!(conflict_completion.release.id, completion.release.id);
    let conflict_stored_run = storage
        .run(conflict_run.id)
        .await
        .expect("conflicting rebuild lookup must succeed")
        .expect("conflicting rebuild must remain stored");
    assert_eq!(conflict_stored_run.state, RunState::Failed);
    assert_eq!(
        conflict_stored_run
            .result
            .as_ref()
            .and_then(|value| value.pointer("/publication/disposition"))
            .and_then(serde_json::Value::as_str),
        Some("version_content_conflict")
    );
    let conflict_replay = storage
        .finalize_build(
            worker.id,
            &conflict_claim.lease,
            "contract-conflicting-rebuild",
            &conflict_execution,
            &conflict_audit,
            conflict_time + Duration::seconds(1),
        )
        .await
        .expect("conflicting rebuild replay must succeed");
    assert_eq!(conflict_replay.outcome, CompletionOutcome::Replayed);
    assert_eq!(
        conflict_replay.disposition,
        BuildDisposition::VersionContentConflict
    );
    assert_eq!(
        storage
            .list_releases(software.id, None, 200)
            .await
            .expect("same-version release list must succeed")
            .len(),
        1,
        "same-version rebuilds must never fork release identity"
    );
    assert_eq!(
        storage
            .channel(software.id, "candidate")
            .await
            .expect("candidate channel lookup after conflict must succeed")
            .expect("candidate channel must remain bound after conflict"),
        candidate
    );

    let testing = storage
        .promote_channel(
            software.id,
            "testing",
            completion.release.id,
            Some(variants[0].id),
            0,
            Some("contract promotion"),
            &contract_audit(
                principal,
                "release.promote",
                "channel",
                "testing".to_owned(),
                now,
            ),
            now,
        )
        .await
        .expect("candidate must promote to testing");
    assert_eq!(testing.revision, 1);
    let testing_release = storage
        .release(completion.release.id)
        .await
        .expect("promoted release lookup must succeed")
        .expect("promoted release must exist");
    assert_eq!(testing_release.state, ReleaseState::Testing);
    assert_eq!(testing_release.revision, 2);
    assert_eq!(
        storage
            .promote_channel(
                software.id,
                "testing",
                completion.release.id,
                None,
                0,
                None,
                &contract_audit(
                    principal,
                    "release.promote",
                    "channel",
                    "testing".to_owned(),
                    now,
                ),
                now,
            )
            .await,
        Err(StorageError::StaleRevision)
    );
    let promotion_audit = contract_audit(
        principal,
        "release.promote",
        "channel",
        "stable".into(),
        now,
    );
    storage
        .promote_channel(
            software.id,
            "stable",
            completion.release.id,
            None,
            0,
            None,
            &promotion_audit,
            now,
        )
        .await
        .expect("testing promotes to stable");
    let stable = storage
        .release(completion.release.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stable.state, ReleaseState::Stable);
    let reason = stabbur_domain::WithdrawalReason::new("Confirmed installer regression").unwrap();
    let audit = contract_audit(
        principal,
        "release.withdraw",
        "release",
        stable.id.to_string(),
        now,
    );
    assert_eq!(
        storage
            .withdraw_release(stable.id, &reason, stable.revision - 1, &audit, now)
            .await,
        Err(StorageError::StaleRevision)
    );
    let withdrawn = storage
        .withdraw_release(stable.id, &reason, stable.revision, &audit, now)
        .await
        .unwrap();
    assert_eq!(
        withdrawn.state,
        ReleaseState::Stable,
        "withdrawal preserves the attained lifecycle"
    );
    assert!(!withdrawn.availability.is_available());
    assert_eq!(withdrawn.revision, stable.revision + 1);
    assert!(storage.channels(software.id).await.unwrap().is_empty());
    assert!(
        storage
            .promote_channel(
                software.id,
                "stable",
                stable.id,
                None,
                0,
                None,
                &promotion_audit,
                now
            )
            .await
            .is_err()
    );
    assert_eq!(
        storage.release_variants(stable.id).await.unwrap(),
        variants,
        "withdrawal preserves immutable history"
    );
    let summary = storage.software_status(software.id, now).await.unwrap();
    assert!(summary.channels.is_empty());
    assert!(summary.last_success_at.is_some());
    let status = storage.operational_status().await.unwrap();
    assert!(status.failed_jobs > 0);
}

async fn provision_contract_worker(
    storage: &Arc<dyn Storage>,
    principal: &Principal,
    now: DateTime<Utc>,
) -> (WorkerRecord, CapabilitySet) {
    let capability = Capability::new("builder.contract-run").expect("static capability is valid");
    let capabilities = CapabilitySet::new([capability]);
    let worker = WorkerRecord {
        draining: false,
        id: WorkerId::new(),
        principal_id: Some(PrincipalId::new()),
        name: "contract-provisioned-worker".to_owned(),
        allowed_capabilities: capabilities.clone(),
        advertised_capabilities: CapabilitySet::default(),
        enabled: true,
        last_seen_at: now,
        revision: 1,
    };
    let (_, worker_token_hash) = generate_token();
    storage
        .provision_worker(
            &worker,
            &worker_token_hash,
            &contract_audit(
                principal,
                "worker.provision",
                "worker",
                worker.id.to_string(),
                now,
            ),
        )
        .await
        .expect("worker provisioning must be atomic");
    let worker_principal = storage
        .principal_by_token(&worker_token_hash, now)
        .await
        .expect("worker token lookup must succeed")
        .expect("worker token must resolve");
    assert_eq!(Some(worker_principal.id), worker.principal_id);
    assert_eq!(
        storage
            .worker_for_principal(worker_principal.id)
            .await
            .expect("worker principal lookup must succeed")
            .expect("worker binding must exist")
            .id,
        worker.id
    );
    storage
        .register_worker(worker.id, &worker.name, &capabilities, now)
        .await
        .expect("provisioned worker registration must succeed");
    (worker, capabilities)
}

#[allow(clippy::too_many_lines)] // Creation and retry assertions intentionally share one backend scenario.
async fn create_contract_recipe_run(
    storage: &Arc<dyn Storage>,
    principal: &Principal,
    software: &Software,
    capabilities: &CapabilitySet,
    now: DateTime<Utc>,
) -> RunRecord {
    let recipe = RecipeRecord {
        id: RecipeId::new(),
        name: "contract-recipe".to_owned(),
        created_at: now,
        revision: 1,
    };
    storage
        .create_recipe(
            &recipe,
            &contract_audit(
                principal,
                "recipe.create",
                "recipe",
                recipe.id.to_string(),
                now,
            ),
        )
        .await
        .expect("recipe and audit must commit atomically");
    assert_eq!(
        storage
            .recipe("contract-recipe")
            .await
            .expect("recipe lookup must succeed"),
        Some(recipe.clone())
    );
    let revision_id = RecipeRevisionId::new();
    let revision = storage
        .create_recipe_revision(
            &NewRecipeRevision {
                expected_sequence: None,
                id: revision_id,
                recipe_id: recipe.id,
                builder: "contract".to_owned(),
                definition: serde_json::json!({"immutable": true}),
                required_capabilities: capabilities.clone(),
                created_at: now,
            },
            &contract_audit(
                principal,
                "recipe.revision.create",
                "recipe_revision",
                revision_id.to_string(),
                now,
            ),
        )
        .await
        .expect("immutable recipe revision must be created");
    assert_eq!(revision.sequence, 1);
    let stale_append = NewRecipeRevision {
        expected_sequence: std::num::NonZeroU64::new(1),
        id: RecipeRevisionId::new(),
        recipe_id: recipe.id,
        builder: "contract".into(),
        definition: serde_json::json!({"immutable":true}),
        required_capabilities: capabilities.clone(),
        created_at: now,
    };
    assert_eq!(
        storage
            .create_recipe_revision(
                &stale_append,
                &contract_audit(
                    principal,
                    "recipe.revision.create",
                    "recipe",
                    recipe.id.to_string(),
                    now
                )
            )
            .await,
        Err(StorageError::StaleRevision)
    );

    let run = RunRecord {
        id: RunId::new(),
        recipe_revision_id: revision.id,
        software_id: software.id,
        state: RunState::Queued,
        parameters: serde_json::json!({}),
        result: None,
        created_at: now,
        completed_at: None,
    };
    let job = Job {
        id: JobId::new(),
        subject: JobSubject::BuildRun { run_id: run.id },
        required_capabilities: capabilities.clone(),
        payload: serde_json::json!({"schema_version": 1}),
        state: JobState::Queued,
        maximum_attempts: 2,
        attempt_count: 0,
        created_at: now,
    };
    let creation = storage
        .create_run(
            &run,
            &job,
            "contract:run-create",
            "create",
            &contract_audit(principal, "run.create", "run", run.id.to_string(), now),
        )
        .await
        .expect("run, job, and audit must commit atomically");
    assert_eq!(creation.outcome, CompletionOutcome::Completed);
    assert_eq!(creation.run, run);
    let replay_run = RunRecord {
        id: RunId::new(),
        created_at: now + Duration::seconds(1),
        ..run.clone()
    };
    let replay_job = Job {
        id: JobId::new(),
        subject: JobSubject::BuildRun {
            run_id: replay_run.id,
        },
        created_at: replay_run.created_at,
        ..job.clone()
    };
    let replay = storage
        .create_run(
            &replay_run,
            &replay_job,
            "contract:run-create",
            "create",
            &contract_audit(
                principal,
                "run.create",
                "run",
                replay_run.id.to_string(),
                replay_run.created_at,
            ),
        )
        .await
        .expect("same run creation request must replay");
    assert_eq!(replay.outcome, CompletionOutcome::Replayed);
    assert_eq!(replay.run, run);
    let conflicting_run = RunRecord {
        parameters: serde_json::json!({"different": true}),
        ..replay_run
    };
    assert_eq!(
        storage
            .create_run(
                &conflicting_run,
                &replay_job,
                "contract:run-create",
                "create",
                &contract_audit(
                    principal,
                    "run.create",
                    "run",
                    conflicting_run.id.to_string(),
                    conflicting_run.created_at,
                ),
            )
            .await
            .expect_err("changed request under the same idempotency key must conflict"),
        StorageError::Conflict
    );
    assert_eq!(
        storage.job(job.id).await.expect("job lookup must succeed"),
        Some(job)
    );
    run
}

fn contract_audit(
    principal: &Principal,
    action: &str,
    resource_kind: &str,
    resource_id: String,
    now: DateTime<Utc>,
) -> AuditEvent {
    AuditEvent {
        id: AuditEventId::new(),
        actor: AuditActor::Principal(principal.id),
        action: action.to_owned(),
        resource_kind: resource_kind.to_owned(),
        resource_id: Some(resource_id),
        details: serde_json::json!({}),
        request_id: Some("storage-contract".to_owned()),
        occurred_at: now,
    }
}

async fn assert_identity_contract(storage: &Arc<dyn Storage>, now: DateTime<Utc>) -> Principal {
    let (bootstrap_secret, bootstrap_hash) = generate_token();
    assert_eq!(
        storage
            .prepare_bootstrap(&bootstrap_hash)
            .await
            .expect("bootstrap preparation must succeed"),
        BootstrapPreparation::Created
    );
    assert_eq!(
        storage
            .prepare_bootstrap(&bootstrap_hash)
            .await
            .expect("bootstrap preparation must be idempotent"),
        BootstrapPreparation::Pending
    );
    let password = PasswordPolicy::default()
        .hash("correct horse battery staple")
        .expect("static test password must satisfy policy");
    let principal = storage
        .bootstrap_admin(
            bootstrap_secret.expose_secret(),
            "contract-admin",
            &password,
            now,
        )
        .await
        .expect("bootstrap must commit the first principal");
    assert_eq!(
        storage
            .prepare_bootstrap(&bootstrap_hash)
            .await
            .expect("disabled bootstrap must remain readable"),
        BootstrapPreparation::Disabled
    );
    assert_eq!(
        storage
            .human_credential("CONTRACT-ADMIN")
            .await
            .expect("human lookup must succeed")
            .expect("bootstrapped human must exist")
            .principal,
        principal
    );

    let (_, token_hash) = generate_token();
    storage
        .create_credential(
            principal.id,
            Some("conformance"),
            "session",
            &token_hash,
            Some(now + Duration::hours(1)),
            now,
        )
        .await
        .expect("credential creation must succeed");
    assert_eq!(
        storage
            .principal_by_token(&token_hash, now)
            .await
            .expect("token lookup must succeed"),
        Some(principal.clone())
    );
    principal
}

#[allow(clippy::too_many_lines)] // Creation, metadata, concurrency, and audit form one aggregate contract.
async fn assert_resource_contract(
    storage: &Arc<dyn Storage>,
    principal: &Principal,
    now: DateTime<Utc>,
) -> (Software, AuditEventId) {
    let software = Software {
        id: SoftwareId::new(),
        slug: SoftwareSlug::new("storage-contract").expect("static slug is valid"),
        name: "Storage contract".to_owned(),
        created_at: now,
        revision: 1,
    };
    let digest = Sha256Digest::new("a".repeat(64)).expect("static digest is valid");
    let artifact = Artifact {
        digest: digest.clone(),
        size: 42,
        media_type: "application/octet-stream".to_owned(),
        created_at: now,
    };
    let store = storage
        .ensure_local_primary_store(StoreId::new(), "contract-primary")
        .await
        .expect("store registration must succeed");
    let location = ArtifactLocation {
        id: LocationId::new(),
        digest: digest.clone(),
        store_id: store.id,
        state: LocationState::Present,
        verified_at: Some(now),
        last_error: None,
    };
    let audit = AuditEvent {
        id: AuditEventId::new(),
        actor: AuditActor::Principal(principal.id),
        action: "storage.contract.commit".to_owned(),
        resource_kind: "software".to_owned(),
        resource_id: Some(software.id.to_string()),
        details: serde_json::json!({}),
        request_id: Some("storage-contract".to_owned()),
        occurred_at: now,
    };
    let installation = SoftwareInstallation {
        software_id: software.id,
        install: serde_json::json!({"kind": "package"}),
        detection: serde_json::json!({"bundle_id": "test.contract"}),
    };
    let mut transaction = storage.begin().await.expect("transaction must start");
    transaction
        .create_software(&software)
        .await
        .expect("software write must succeed");
    transaction
        .set_software_installation(&installation)
        .await
        .expect("installation metadata must share software creation");
    transaction
        .record_artifact_location(&artifact, &location)
        .await
        .expect("artifact and location write must succeed");
    transaction
        .append_audit(&audit)
        .await
        .expect("audit write must share the transaction");
    transaction.commit().await.expect("transaction must commit");
    assert_eq!(
        storage
            .software("storage-contract")
            .await
            .expect("software lookup must succeed"),
        Some(software.clone())
    );
    assert_eq!(
        storage
            .software_installation(software.id)
            .await
            .expect("installation lookup must succeed"),
        Some(installation)
    );
    assert_eq!(
        storage
            .artifact(&digest)
            .await
            .expect("artifact lookup must succeed"),
        Some(artifact)
    );
    assert_eq!(
        storage
            .artifact_locations(&digest)
            .await
            .expect("location lookup must succeed"),
        vec![location]
    );
    let updated = storage
        .update_software(
            software.id,
            Some("Storage contract renamed"),
            None,
            1,
            &contract_audit(
                principal,
                "software.update",
                "software",
                software.id.to_string(),
                now + Duration::milliseconds(1),
            ),
        )
        .await
        .expect("software update must be atomic");
    assert_eq!(updated.name, "Storage contract renamed");
    assert_eq!(updated.revision, 2);
    assert_eq!(
        storage
            .update_software(
                software.id,
                Some("Stale name"),
                None,
                1,
                &contract_audit(
                    principal,
                    "software.update",
                    "software",
                    software.id.to_string(),
                    now + Duration::milliseconds(2),
                ),
            )
            .await
            .expect_err("stale software update must fail"),
        StorageError::StaleRevision
    );
    (updated, audit.id)
}

async fn assert_rollback_contract(
    storage: &Arc<dyn Storage>,
    software: &Software,
    now: DateTime<Utc>,
) {
    let rolled_back = Software {
        id: SoftwareId::new(),
        slug: SoftwareSlug::new("rolled-back").expect("static slug is valid"),
        name: "Rolled back".to_owned(),
        created_at: now,
        revision: 1,
    };
    let mut transaction = storage.begin().await.expect("transaction must start");
    transaction
        .create_software(&rolled_back)
        .await
        .expect("rollback fixture write must succeed");
    transaction.rollback().await.expect("rollback must succeed");
    assert_eq!(
        storage
            .software("rolled-back")
            .await
            .expect("rollback lookup must succeed"),
        None
    );

    let mut duplicate = storage.begin().await.expect("transaction must start");
    assert_eq!(
        duplicate.create_software(software).await,
        Err(StorageError::Conflict)
    );
    duplicate
        .rollback()
        .await
        .expect("failed transaction must remain rollback-capable");
}

async fn assert_job_contract(storage: &Arc<dyn Storage>, now: DateTime<Utc>) {
    let capability = Capability::new("builder.contract").expect("static capability is valid");
    let capabilities = CapabilitySet::new([capability]);
    let worker_id = WorkerId::new();
    storage
        .register_worker(worker_id, "contract-worker", &capabilities, now)
        .await
        .expect("worker registration must succeed");
    let job = Job {
        id: stabbur_domain::JobId::new(),
        subject: JobSubject::BuildRun {
            run_id: stabbur_domain::RunId::new(),
        },
        required_capabilities: capabilities.clone(),
        payload: serde_json::json!({"contract": true}),
        state: JobState::Queued,
        maximum_attempts: 2,
        attempt_count: 0,
        created_at: now,
    };
    storage
        .enqueue_job(&job)
        .await
        .expect("job enqueue must succeed");
    let claimed = storage
        .claim_job(worker_id, &capabilities, now, 60)
        .await
        .expect("job claim must succeed")
        .expect("compatible job must be claimed");
    let lease = storage
        .heartbeat_job(worker_id, &claimed.lease, now + Duration::seconds(1), 60)
        .await
        .expect("lease heartbeat must succeed");
    assert_eq!(
        storage
            .complete_job(
                worker_id,
                &lease,
                "contract-completion",
                &serde_json::json!({"succeeded": true}),
                now + Duration::seconds(2),
            )
            .await
            .expect("job completion must succeed"),
        CompletionOutcome::Completed
    );
    assert_eq!(
        storage
            .complete_job(
                worker_id,
                &lease,
                "contract-completion",
                &serde_json::json!({"succeeded": true}),
                now + Duration::seconds(3),
            )
            .await
            .expect("idempotent replay must succeed"),
        CompletionOutcome::Replayed
    );
}
