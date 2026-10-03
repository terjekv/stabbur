//! Actix handlers and application services for the released `/api/v1` slice.

mod exports;
pub use exports::*;
mod operations;
pub use operations::*;

use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::{Arc, OnceLock},
};

use actix_web::{
    HttpMessage, HttpRequest, HttpResponse,
    body::SizedStream,
    http::{StatusCode, header},
    web,
};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use bytes::Bytes;
use chrono::{Duration, Utc};
use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use stabbur_auth_core::{
    Authorizer, PasswordHash, PasswordPolicy, Permission, Principal, PrincipalId, PrincipalKind,
    RoleName, SecretToken, TokenHash, generate_token,
};
use stabbur_builder_autopkg::{AutoPkgRecipe, PinnedSource};
use stabbur_builder_core::{
    BuildParameter, BuildRequest, BuilderExecutionFailure, BuilderExecutionResult, BuilderJob,
    RecipeCatalogDiagnostic, RecipeCatalogDiagnosticSeverity, RecipeCatalogEntry,
    RecipeCatalogManifest, RecipeCatalogScanExecutionFailure, RecipeCatalogScanExecutionResult,
    RecipeCatalogScanJob, RecipeCatalogSource,
};
use stabbur_domain::{
    Architecture, Artifact, ArtifactRole, AttemptId, AuditEventId, BuildTargetId, JobId,
    LocationId, LocationState, MacOsVersion, Platform, RecipeCatalogScanId,
    RecipeCatalogSnapshotId, RecipeId, RecipeRevisionId, Release, ReleaseId, ResolutionTarget,
    RunId, Sha256Digest, Software, SoftwareId, SoftwareInstallation, SoftwareSlug, StoreId,
    Variant, VariantId, WorkerId, resolve_variant,
};
use stabbur_jobs_core::{Capability, CapabilitySet, Job, JobState, JobSubject, Lease};
use stabbur_storage_core::{
    ApiTokenRecord, ArtifactLocation, AuditActor, AuditEvent, BuildDisposition, BuildTargetRecord,
    BuildTargetRunTrigger, BuildTargetSchedule, ChannelRecord, CompletionOutcome, JobSummaryRecord,
    MAX_BUILD_INTERVAL_SECONDS, MIN_BUILD_INTERVAL_SECONDS, NewRecipeRevision, NewRunLogEntry,
    PrincipalRecord, RecipeCatalogMatch, RecipeCatalogPublishOutcome, RecipeCatalogScanRecord,
    RecipeCatalogScanSummaryRecord, RecipeCatalogSnapshotRecord,
    RecipeCatalogSnapshotSummaryRecord, RecipeRecord, RecipeRevisionRecord, RoleRecord,
    RunLogAppendOutcome, RunLogRecord, RunLogStream, RunRecord, RunState, RunSummaryRecord,
    Storage, VariantArtifactRecord, WorkerRecord,
};
use stabbur_store_core::{ArtifactStore, ByteRange, ByteStream, StoreError, WriteOutcome};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::{
    error::{ApiError, Problem, ValidationError},
    scheduler::queued_target_build,
};

/// Shared application capabilities; all adapter types remain behind Stabbur-owned ports.
#[derive(Clone)]
pub struct AppState {
    storage: Arc<dyn Storage>,
    store: Arc<dyn ArtifactStore>,
    store_id: stabbur_domain::StoreId,
    bootstrap_secret_path: PathBuf,
}

impl AppState {
    /// Composes the HTTP application from persistence and artifact-store adapters.
    #[must_use]
    pub fn new(
        storage: Arc<dyn Storage>,
        store: Arc<dyn ArtifactStore>,
        store_id: stabbur_domain::StoreId,
        bootstrap_secret_path: PathBuf,
    ) -> Self {
        Self {
            storage,
            store,
            store_id,
            bootstrap_secret_path,
        }
    }

    /// Returns the backend-neutral persistence handle for server-owned background policy.
    #[must_use]
    pub(crate) fn storage(&self) -> Arc<dyn Storage> {
        self.storage.clone()
    }
}

/// Request correlation identity installed by middleware.
#[derive(Debug, Clone)]
pub struct RequestId(pub String);

/// Returns or creates the current request identity.
#[must_use]
pub fn request_id(request: &HttpRequest) -> String {
    if let Some(value) = request.extensions().get::<RequestId>() {
        return value.0.clone();
    }
    let value = request
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .map_or_else(|| Uuid::now_v7().to_string(), str::to_owned);
    request.extensions_mut().insert(RequestId(value.clone()));
    value
}

/// Registers the versioned API and JSON error conventions.
pub fn configure(configuration: &mut web::ServiceConfig) {
    configuration
        .app_data(
            web::JsonConfig::default()
                .limit(2 * 1024 * 1024)
                .error_handler(|error, request| {
                    ApiError::validation(
                        "The JSON request body is invalid.",
                        vec![ValidationError {
                            field: "body".into(),
                            code: "invalid_json".into(),
                            message: error.to_string(),
                        }],
                        request_id(request),
                    )
                    .into()
                }),
        )
        .service(healthz)
        .service(readyz)
        .service(
            web::scope("/api/v1")
                .service(openapi_document)
                .service(bootstrap)
                .service(login)
                .service(me)
                .service(list_principals)
                .service(create_principal)
                .service(get_principal)
                .service(update_principal)
                .service(change_password)
                .service(reset_principal_password)
                .service(revoke_principal_sessions)
                .service(list_api_tokens)
                .service(create_api_token)
                .service(revoke_api_token)
                .service(list_roles)
                .service(create_role)
                .service(list_exports)
                .service(create_export)
                .service(get_export)
                .service(update_export)
                .service(plan_export)
                .service(apply_export)
                .service(get_export_snapshot)
                .service(list_export_history)
                .service(create_export_reader)
                .service(revoke_export_readers)
                .service(export_repository)
                .service(list_software)
                .service(create_software)
                .service(get_software)
                .service(update_software)
                .service(list_releases)
                .service(get_release)
                .service(list_release_variants)
                .service(list_channels)
                .service(get_channel)
                .service(promote_channel)
                .service(reject_release)
                .service(withdraw_release)
                .service(drain_worker)
                .service(software_status)
                .service(operational_status)
                .service(resolve_software)
                .service(get_artifact)
                .service(list_artifact_locations)
                .service(list_stores)
                .service(get_store)
                .service(test_store)
                .service(list_audit_events)
                .service(upload_artifact_content)
                .service(provision_worker)
                .service(list_workers)
                .service(get_worker)
                .service(update_worker)
                .service(rotate_worker_token)
                .service(create_recipe)
                .service(list_recipes)
                .service(get_recipe)
                .service(create_recipe_revision)
                .service(list_recipe_revisions)
                .service(list_recipe_runs)
                .service(list_recipe_catalog_snapshots)
                .service(get_recipe_catalog_snapshot)
                .service(resolve_recipe_catalog_entry)
                .service(create_recipe_catalog_scan)
                .service(list_recipe_catalog_scans)
                .service(get_recipe_catalog_scan)
                .service(cancel_recipe_catalog_scan)
                .service(create_build_target)
                .service(list_build_targets)
                .service(get_build_target)
                .service(update_build_target)
                .service(trigger_build_target)
                .service(list_build_target_runs)
                .service(create_run)
                .service(list_runs)
                .service(get_run)
                .service(cancel_run)
                .service(list_run_logs)
                .service(stream_run_events)
                .service(list_jobs)
                .service(get_job)
                .service(register_worker)
                .service(publish_worker_recipe_catalog)
                .service(claim_worker_job)
                .service(heartbeat_worker_job)
                .service(append_worker_logs)
                .service(upload_worker_artifact)
                .service(complete_worker_job)
                .service(fail_worker_job)
                .route(
                    "/artifacts/{digest}/content",
                    web::get().to(download_artifact_content),
                )
                .route(
                    "/artifacts/{digest}/content",
                    web::head().to(head_artifact_content),
                ),
        );
}

/// Internal worker registration body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RegisterWorkerRequest {
    pub(crate) worker_id: WorkerId,
    pub(crate) capabilities: Vec<String>,
}

/// Internal worker claim body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ClaimWorkerJobRequest {
    pub(crate) lease_seconds: u32,
}

/// Internal worker heartbeat body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HeartbeatWorkerJobRequest {
    pub(crate) lease: Lease,
    pub(crate) lease_seconds: u32,
}

/// Internal exact-byte worker log entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WorkerLogEntryRequest {
    pub(crate) stream: String,
    pub(crate) message_base64: String,
}

/// Internal idempotent worker log batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AppendWorkerLogsRequest {
    pub(crate) lease: Lease,
    pub(crate) idempotency_key: String,
    pub(crate) entries: Vec<WorkerLogEntryRequest>,
}

/// Internal worker completion body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CompleteWorkerJobRequest {
    pub(crate) lease: Lease,
    pub(crate) idempotency_key: String,
    pub(crate) result: serde_json::Value,
}

/// Internal worker terminal failure body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FailWorkerJobRequest {
    pub(crate) lease: Lease,
    pub(crate) idempotency_key: String,
    pub(crate) failure: serde_json::Value,
}

fn capability_set(values: &[String], request_id: &str) -> Result<CapabilitySet, ApiError> {
    values
        .iter()
        .map(|value| Capability::new(value.clone()))
        .collect::<Result<Vec<_>, _>>()
        .map(CapabilitySet::new)
        .map_err(|error| {
            ApiError::validation(
                "A worker capability is invalid.",
                vec![ValidationError {
                    field: "capabilities".into(),
                    code: "invalid_capability".into(),
                    message: error.to_string(),
                }],
                request_id,
            )
        })
}

#[actix_web::post("/internal/workers/register")]
async fn register_worker(
    request: HttpRequest,
    state: web::Data<AppState>,
    body: web::Json<RegisterWorkerRequest>,
) -> Result<HttpResponse, ApiError> {
    let request_id = request_id(&request);
    let worker = authenticate_worker(&request, &state, body.worker_id).await?;
    let capabilities = capability_set(&body.capabilities, &request_id)?;
    if !worker.allowed_capabilities.satisfies(&capabilities) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "worker_capability_escalation",
            "Worker advertised a capability outside its server-defined ceiling.",
            &request_id,
        ));
    }
    state
        .storage
        .register_worker(body.worker_id, &worker.name, &capabilities, Utc::now())
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(HttpResponse::NoContent().finish())
}

#[actix_web::post("/internal/workers/{worker}/recipe-catalogs")]
async fn publish_worker_recipe_catalog(
    request: HttpRequest,
    state: web::Data<AppState>,
    worker: web::Path<String>,
    body: web::Json<RecipeCatalogManifest>,
) -> Result<HttpResponse, ApiError> {
    let request_id = request_id(&request);
    let worker_id = worker
        .parse::<WorkerId>()
        .map_err(|error| ApiError::domain(error, &request_id))?;
    let worker = authenticate_worker(&request, &state, worker_id).await?;
    body.validate().map_err(|error| {
        ApiError::validation(
            "The recipe catalog manifest is invalid.",
            vec![ValidationError {
                field: "body".into(),
                code: "invalid_recipe_catalog".into(),
                message: error.to_string(),
            }],
            &request_id,
        )
    })?;
    let producer_capability =
        Capability::new(format!("builder.{}", body.producer)).map_err(|_| {
            ApiError::validation(
                "The recipe catalog producer is invalid.",
                vec![ValidationError {
                    field: "producer".into(),
                    code: "invalid_catalog_producer".into(),
                    message: "The producer must map to a valid builder capability.".into(),
                }],
                &request_id,
            )
        })?;
    if !worker
        .advertised_capabilities
        .satisfies(&CapabilitySet::new([producer_capability]))
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "catalog_producer_not_advertised",
            "The worker did not advertise the catalog producer capability.",
            &request_id,
        ));
    }
    let manifest = body.into_inner();
    let snapshot = RecipeCatalogSnapshotRecord {
        id: RecipeCatalogSnapshotId::new(),
        worker_id,
        manifest_digest: manifest.canonical_digest().map_err(|error| {
            ApiError::validation(
                "The recipe catalog manifest is invalid.",
                vec![ValidationError {
                    field: "body".into(),
                    code: "invalid_recipe_catalog".into(),
                    message: error.to_string(),
                }],
                &request_id,
            )
        })?,
        manifest,
        observed_at: Utc::now(),
    };
    let publication = state
        .storage
        .publish_recipe_catalog(
            &snapshot,
            &AuditEvent {
                id: AuditEventId::new(),
                actor: AuditActor::Principal(
                    worker
                        .principal_id
                        .ok_or_else(|| ApiError::internal(&request_id))?,
                ),
                action: "recipe_catalog.publish".into(),
                resource_kind: "recipe_catalog_snapshot".into(),
                resource_id: Some(snapshot.id.to_string()),
                details: serde_json::json!({
                    "worker_id": worker_id,
                    "producer": snapshot.manifest.producer,
                    "source_locator": snapshot.manifest.source.locator,
                    "source_revision": snapshot.manifest.source.revision,
                    "manifest_digest": snapshot.manifest_digest,
                    "recipe_count": snapshot.manifest.recipes.len(),
                    "diagnostic_count": snapshot.manifest.diagnostics.len(),
                }),
                request_id: Some(request_id.clone()),
                occurred_at: snapshot.observed_at,
            },
        )
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    let status = match publication.outcome {
        RecipeCatalogPublishOutcome::Published => StatusCode::CREATED,
        RecipeCatalogPublishOutcome::Replayed => StatusCode::OK,
    };
    Ok(HttpResponse::build(status)
        .insert_header(("x-request-id", request_id))
        .json(RecipeCatalogPublicationResponse {
            snapshot: RecipeCatalogSnapshotResponse::from(publication.snapshot),
            outcome: publication.outcome,
        }))
}

#[actix_web::post("/internal/workers/{worker}/claim")]
async fn claim_worker_job(
    request: HttpRequest,
    state: web::Data<AppState>,
    worker: web::Path<String>,
    body: web::Json<ClaimWorkerJobRequest>,
) -> Result<HttpResponse, ApiError> {
    let request_id = request_id(&request);
    let worker = worker
        .parse::<WorkerId>()
        .map_err(|error| ApiError::domain(error, &request_id))?;
    if !(30..=300).contains(&body.lease_seconds) {
        return Err(ApiError::validation(
            "Lease duration must be between 30 and 300 seconds.",
            vec![ValidationError {
                field: "lease_seconds".into(),
                code: "out_of_range".into(),
                message: "Use a value from 30 through 300.".into(),
            }],
            &request_id,
        ));
    }
    let worker_record = authenticate_worker(&request, &state, worker).await?;
    let job = state
        .storage
        .claim_job(
            worker,
            &worker_record.advertised_capabilities,
            Utc::now(),
            body.lease_seconds,
        )
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    match job {
        Some(job) => Ok(HttpResponse::Ok().json(job)),
        None => Ok(HttpResponse::NoContent().finish()),
    }
}

#[actix_web::post("/internal/workers/{worker}/heartbeat")]
async fn heartbeat_worker_job(
    request: HttpRequest,
    state: web::Data<AppState>,
    worker: web::Path<String>,
    body: web::Json<HeartbeatWorkerJobRequest>,
) -> Result<web::Json<Lease>, ApiError> {
    let request_id = request_id(&request);
    let worker = worker
        .parse::<WorkerId>()
        .map_err(|error| ApiError::domain(error, &request_id))?;
    authenticate_worker(&request, &state, worker).await?;
    if !(30..=300).contains(&body.lease_seconds) {
        return Err(ApiError::validation(
            "Lease duration must be between 30 and 300 seconds.",
            vec![ValidationError {
                field: "lease_seconds".into(),
                code: "out_of_range".into(),
                message: "Use a value from 30 through 300.".into(),
            }],
            &request_id,
        ));
    }
    let lease = state
        .storage
        .heartbeat_job(worker, &body.lease, Utc::now(), body.lease_seconds)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(web::Json(lease))
}

#[actix_web::post("/internal/workers/{worker}/logs")]
async fn append_worker_logs(
    request: HttpRequest,
    state: web::Data<AppState>,
    worker: web::Path<String>,
    body: web::Json<AppendWorkerLogsRequest>,
) -> Result<HttpResponse, ApiError> {
    let request_id = request_id(&request);
    let worker = worker
        .parse::<WorkerId>()
        .map_err(|error| ApiError::domain(error, &request_id))?;
    authenticate_worker(&request, &state, worker).await?;
    let entries = body
        .entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let stream = match entry.stream.as_str() {
                "stdout" => RunLogStream::Stdout,
                "stderr" => RunLogStream::Stderr,
                "system" => RunLogStream::System,
                _ => {
                    return Err(ApiError::validation(
                        "Worker log stream is invalid.",
                        vec![ValidationError {
                            field: format!("entries[{index}].stream"),
                            code: "invalid_log_stream".into(),
                            message: "Use stdout, stderr, or system.".into(),
                        }],
                        &request_id,
                    ));
                }
            };
            let message = STANDARD.decode(&entry.message_base64).map_err(|_| {
                ApiError::validation(
                    "Worker log bytes are invalid.",
                    vec![ValidationError {
                        field: format!("entries[{index}].message_base64"),
                        code: "invalid_base64".into(),
                        message: "Use canonical base64-encoded exact bytes.".into(),
                    }],
                    &request_id,
                )
            })?;
            Ok(NewRunLogEntry { stream, message })
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    let outcome = state
        .storage
        .append_run_logs(
            worker,
            &body.lease,
            &body.idempotency_key,
            &entries,
            Utc::now(),
        )
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    let (status, receipt, replayed) = match outcome {
        RunLogAppendOutcome::Appended(receipt) => (StatusCode::CREATED, receipt, false),
        RunLogAppendOutcome::Replayed(receipt) => (StatusCode::OK, receipt, true),
    };
    Ok(HttpResponse::build(status).json(serde_json::json!({
        "first_sequence": receipt.first_sequence,
        "last_sequence": receipt.last_sequence,
        "count": receipt.count,
        "replayed": replayed,
    })))
}

fn parse_artifact_role(value: &str, request_id: &str) -> Result<ArtifactRole, ApiError> {
    match value {
        "primary_installer" => Ok(ArtifactRole::PrimaryInstaller),
        "signature" => Ok(ArtifactRole::Signature),
        "sbom" => Ok(ArtifactRole::Sbom),
        "debug_symbols" => Ok(ArtifactRole::DebugSymbols),
        "metadata" => Ok(ArtifactRole::Metadata),
        _ => Err(ApiError::validation(
            "The artifact role is invalid.",
            vec![ValidationError {
                field: "x-stabbur-artifact-role".into(),
                code: "invalid_artifact_role".into(),
                message: "Use a supported semantic artifact role.".into(),
            }],
            request_id,
        )),
    }
}

#[actix_web::put("/internal/workers/{worker}/attempts/{attempt}/artifacts/{digest}")]
async fn upload_worker_artifact(
    request: HttpRequest,
    state: web::Data<AppState>,
    path: web::Path<(String, String, String)>,
    payload: web::Payload,
) -> Result<HttpResponse, ApiError> {
    let request_id = request_id(&request);
    let (worker, attempt, digest) = path.into_inner();
    let worker = worker
        .parse::<WorkerId>()
        .map_err(|error| ApiError::domain(error, &request_id))?;
    let attempt = attempt
        .parse::<AttemptId>()
        .map_err(|error| ApiError::domain(error, &request_id))?;
    let digest = Sha256Digest::new(digest).map_err(|error| ApiError::domain(error, &request_id))?;
    let worker_record = authenticate_worker(&request, &state, worker).await?;
    let principal_id = worker_record.principal_id.ok_or_else(|| {
        ApiError::new(
            StatusCode::FORBIDDEN,
            "worker_principal_required",
            "Worker does not have a server-issued identity.",
            &request_id,
        )
    })?;
    let size = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| {
            ApiError::validation(
                "A valid Content-Length header is required.",
                vec![ValidationError {
                    field: "Content-Length".into(),
                    code: "required".into(),
                    message: "Content-Length must be an unsigned integer.".into(),
                }],
                &request_id,
            )
        })?;
    let media_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_owned();
    if media_type.trim() != media_type
        || media_type.len() > 255
        || media_type.chars().any(char::is_control)
    {
        return Err(ApiError::validation(
            "The Content-Type header is invalid.",
            vec![],
            &request_id,
        ));
    }
    let role = request
        .headers()
        .get("x-stabbur-artifact-role")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            ApiError::validation(
                "The X-Stabbur-Artifact-Role header is required.",
                vec![],
                &request_id,
            )
        })
        .and_then(|value| parse_artifact_role(value, &request_id))?;
    let outcome = state
        .store
        .write(&digest, size, bridge_payload(payload))
        .await
        .map_err(|error| ApiError::store(error, &request_id))?;
    let now = Utc::now();
    let artifact = Artifact {
        digest: digest.clone(),
        size,
        media_type,
        created_at: now,
    };
    let location = ArtifactLocation {
        id: LocationId::new(),
        digest: digest.clone(),
        store_id: state.store_id,
        state: LocationState::Present,
        verified_at: Some(now),
        last_error: None,
    };
    let run_id = state
        .storage
        .record_run_artifact(
            worker,
            attempt,
            &artifact,
            &location,
            role,
            &AuditEvent {
                id: AuditEventId::new(),
                actor: AuditActor::Principal(principal_id),
                action: "run.artifact.upload".into(),
                resource_kind: "run_artifact".into(),
                resource_id: Some(digest.to_string()),
                details: serde_json::json!({
                    "attempt_id": attempt,
                    "size": size,
                    "role": request.headers().get("x-stabbur-artifact-role").and_then(|v| v.to_str().ok()),
                    "reused": outcome == WriteOutcome::Reused,
                }),
                request_id: Some(request_id.clone()),
                occurred_at: now,
            },
            now,
        )
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    let response = UploadArtifactResponse {
        digest: digest.to_string(),
        size,
        reused: outcome == WriteOutcome::Reused,
    };
    let mut builder = if outcome == WriteOutcome::Created {
        HttpResponse::Created()
    } else {
        HttpResponse::Ok()
    };
    Ok(builder
        .insert_header(("x-stabbur-run-id", run_id.to_string()))
        .insert_header((header::ETAG, digest_etag(&digest)))
        .insert_header(("x-request-id", request_id))
        .json(response))
}

#[actix_web::post("/internal/workers/{worker}/complete")]
async fn complete_worker_job(
    request: HttpRequest,
    state: web::Data<AppState>,
    worker: web::Path<String>,
    body: web::Json<CompleteWorkerJobRequest>,
) -> Result<HttpResponse, ApiError> {
    let request_id = request_id(&request);
    let worker = worker
        .parse::<WorkerId>()
        .map_err(|error| ApiError::domain(error, &request_id))?;
    let worker_record = authenticate_worker(&request, &state, worker).await?;
    let leased_job = state
        .storage
        .job(body.lease.job_id)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "worker_job_not_found",
                "The completion lease does not identify a job.",
                &request_id,
            )
        })?;
    if let JobSubject::RecipeCatalogScan { scan_id } = leased_job.subject {
        let result: RecipeCatalogScanExecutionResult = serde_json::from_value(body.result.clone())
            .map_err(|_| {
                ApiError::validation(
                    "Worker result does not match the catalog scan result contract.",
                    vec![ValidationError {
                        field: "result".into(),
                        code: "invalid_catalog_scan_result".into(),
                        message: "Submit a versioned catalog scan result.".into(),
                    }],
                    &request_id,
                )
            })?;
        let envelope: RecipeCatalogScanJob =
            serde_json::from_value(leased_job.payload).map_err(|_| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_catalog_scan_job",
                    "The leased job does not contain a supported catalog scan envelope.",
                    &request_id,
                )
            })?;
        if result.scan_id != scan_id {
            return Err(ApiError::validation(
                "Worker result targets a different catalog scan than the leased job.",
                vec![],
                &request_id,
            ));
        }
        result.validate_for(&envelope.request).map_err(|error| {
            ApiError::validation(
                "Worker catalog observation differs from the immutable scan request.",
                vec![ValidationError {
                    field: "result.manifest".into(),
                    code: "catalog_scan_mismatch".into(),
                    message: error.to_string(),
                }],
                &request_id,
            )
        })?;
        let principal_id = worker_record.principal_id.ok_or_else(|| {
            ApiError::new(
                StatusCode::FORBIDDEN,
                "worker_principal_required",
                "Worker does not have a server-issued identity.",
                &request_id,
            )
        })?;
        let now = Utc::now();
        let completion = state
            .storage
            .complete_recipe_catalog_scan(
                worker,
                &body.lease,
                &body.idempotency_key,
                &result,
                &AuditEvent {
                    id: AuditEventId::new(),
                    actor: AuditActor::Principal(principal_id),
                    action: "recipe_catalog_scan.complete".into(),
                    resource_kind: "recipe_catalog_scan".into(),
                    resource_id: Some(scan_id.to_string()),
                    details: serde_json::json!({
                        "producer": result.manifest.producer,
                        "recipe_count": result.manifest.recipes.len(),
                        "diagnostic_count": result.manifest.diagnostics.len(),
                    }),
                    request_id: Some(request_id.clone()),
                    occurred_at: now,
                },
                now,
            )
            .await
            .map_err(|error| ApiError::storage(error, &request_id))?;
        let status = if completion.outcome == CompletionOutcome::Completed {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        };
        return Ok(HttpResponse::build(status).json(completion));
    }
    let result: BuilderExecutionResult =
        serde_json::from_value(body.result.clone()).map_err(|_| {
            ApiError::validation(
                "Worker result does not match the current typed result contract.",
                vec![ValidationError {
                    field: "result".into(),
                    code: "invalid_worker_result".into(),
                    message: "Submit a versioned builder execution result.".into(),
                }],
                &request_id,
            )
        })?;
    if result.schema_version != BuilderJob::SCHEMA_VERSION {
        return Err(ApiError::validation(
            "Worker result schema version is unsupported.",
            vec![ValidationError {
                field: "result.schema_version".into(),
                code: "unsupported_version".into(),
                message: "Use the server-advertised worker protocol version.".into(),
            }],
            &request_id,
        ));
    }
    validate_worker_result_run(&state, body.lease.job_id, result.run_id, &request_id).await?;
    let job = state
        .storage
        .job(body.lease.job_id)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "worker_job_not_found",
                "The completion lease does not identify a job.",
                &request_id,
            )
        })?;
    let envelope: BuilderJob = serde_json::from_value(job.payload).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_worker_job",
            "The leased job does not contain a supported builder envelope.",
            &request_id,
        )
    })?;
    if result.adapter != envelope.adapter {
        return Err(ApiError::validation(
            "Worker result adapter does not match the leased job.",
            vec![],
            &request_id,
        ));
    }
    if result.adapter == "autopkg" {
        let recipe: AutoPkgRecipe =
            serde_json::from_value(envelope.adapter_definition).map_err(|_| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_autopkg_definition",
                    "The leased AutoPkg definition is invalid.",
                    &request_id,
                )
            })?;
        let build_result = result.build_result.as_ref().ok_or_else(|| {
            ApiError::validation(
                "AutoPkg completion requires selected output and uploaded artifacts.",
                vec![],
                &request_id,
            )
        })?;
        recipe
            .validate_build_result(&result.raw_report, build_result)
            .map_err(|error| {
                ApiError::validation(
                    "Worker output does not match the immutable AutoPkg selectors.",
                    vec![ValidationError {
                        field: "result.build_result".into(),
                        code: "selector_mismatch".into(),
                        message: error.to_string(),
                    }],
                    &request_id,
                )
            })?;
        let principal_id = worker_record.principal_id.ok_or_else(|| {
            ApiError::new(
                StatusCode::FORBIDDEN,
                "worker_principal_required",
                "Worker does not have a server-issued identity.",
                &request_id,
            )
        })?;
        let now = Utc::now();
        let completion = state
            .storage
            .finalize_build(
                worker,
                &body.lease,
                &body.idempotency_key,
                &result,
                &AuditEvent {
                    id: AuditEventId::new(),
                    actor: AuditActor::Principal(principal_id),
                    action: "build.finalize".into(),
                    resource_kind: "run".into(),
                    resource_id: Some(result.run_id.to_string()),
                    details: serde_json::json!({"adapter": result.adapter}),
                    request_id: Some(request_id.clone()),
                    occurred_at: now,
                },
                now,
            )
            .await
            .map_err(|error| ApiError::storage(error, &request_id))?;
        let status = match (completion.outcome, completion.disposition) {
            (CompletionOutcome::Completed, BuildDisposition::ReleaseCreated) => StatusCode::CREATED,
            (
                CompletionOutcome::Completed,
                BuildDisposition::NoChange
                | BuildDisposition::EvidenceChanged
                | BuildDisposition::VerificationFailed
                | BuildDisposition::VersionContentConflict,
            )
            | (CompletionOutcome::Replayed, _) => StatusCode::OK,
        };
        return Ok(HttpResponse::build(status).json(completion));
    }
    let outcome = state
        .storage
        .complete_job(
            worker,
            &body.lease,
            &body.idempotency_key,
            &body.result,
            Utc::now(),
        )
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(match outcome {
        CompletionOutcome::Completed => HttpResponse::Created().finish(),
        CompletionOutcome::Replayed => HttpResponse::Ok().finish(),
    })
}

#[actix_web::post("/internal/workers/{worker}/fail")]
async fn fail_worker_job(
    request: HttpRequest,
    state: web::Data<AppState>,
    worker: web::Path<String>,
    body: web::Json<FailWorkerJobRequest>,
) -> Result<HttpResponse, ApiError> {
    let request_id = request_id(&request);
    let worker = worker
        .parse::<WorkerId>()
        .map_err(|error| ApiError::domain(error, &request_id))?;
    let worker_record = authenticate_worker(&request, &state, worker).await?;
    let leased_job = state
        .storage
        .job(body.lease.job_id)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "worker_job_not_found",
                "The failure lease does not identify a job.",
                &request_id,
            )
        })?;
    if let JobSubject::RecipeCatalogScan { scan_id } = leased_job.subject {
        let failure: RecipeCatalogScanExecutionFailure =
            serde_json::from_value(body.failure.clone()).map_err(|_| {
                ApiError::validation(
                    "Worker failure does not match the catalog scan failure contract.",
                    vec![ValidationError {
                        field: "failure".into(),
                        code: "invalid_catalog_scan_failure".into(),
                        message: "Submit a versioned catalog scan failure.".into(),
                    }],
                    &request_id,
                )
            })?;
        let envelope: RecipeCatalogScanJob =
            serde_json::from_value(leased_job.payload).map_err(|_| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_catalog_scan_job",
                    "The leased job does not contain a supported catalog scan envelope.",
                    &request_id,
                )
            })?;
        failure.validate_for(&envelope.request).map_err(|error| {
            ApiError::validation(
                "Worker failure differs from the immutable catalog scan request.",
                vec![ValidationError {
                    field: "failure".into(),
                    code: "catalog_scan_failure_mismatch".into(),
                    message: error.to_string(),
                }],
                &request_id,
            )
        })?;
        debug_assert_eq!(failure.scan_id, scan_id);
        let principal_id = worker_record.principal_id.ok_or_else(|| {
            ApiError::new(
                StatusCode::FORBIDDEN,
                "worker_principal_required",
                "Worker does not have a server-issued identity.",
                &request_id,
            )
        })?;
        let now = Utc::now();
        let outcome = state
            .storage
            .fail_recipe_catalog_scan(
                worker,
                &body.lease,
                &body.idempotency_key,
                &failure,
                &AuditEvent {
                    id: AuditEventId::new(),
                    actor: AuditActor::Principal(principal_id),
                    action: "recipe_catalog_scan.fail".into(),
                    resource_kind: "recipe_catalog_scan".into(),
                    resource_id: Some(scan_id.to_string()),
                    details: serde_json::json!({"code": failure.code}),
                    request_id: Some(request_id.clone()),
                    occurred_at: now,
                },
                now,
            )
            .await
            .map_err(|error| ApiError::storage(error, &request_id))?;
        return Ok(match outcome {
            CompletionOutcome::Completed => HttpResponse::Created().finish(),
            CompletionOutcome::Replayed => HttpResponse::Ok().finish(),
        });
    }
    let failure: BuilderExecutionFailure =
        serde_json::from_value(body.failure.clone()).map_err(|_| {
            ApiError::validation(
                "Worker failure does not match the current typed failure contract.",
                vec![ValidationError {
                    field: "failure".into(),
                    code: "invalid_worker_failure".into(),
                    message: "Submit a versioned builder execution failure.".into(),
                }],
                &request_id,
            )
        })?;
    if failure.schema_version != BuilderJob::SCHEMA_VERSION {
        return Err(ApiError::validation(
            "Worker failure schema version is unsupported.",
            vec![ValidationError {
                field: "failure.schema_version".into(),
                code: "unsupported_version".into(),
                message: "Use the server-advertised worker protocol version.".into(),
            }],
            &request_id,
        ));
    }
    validate_worker_result_run(&state, body.lease.job_id, failure.run_id, &request_id).await?;
    let outcome = state
        .storage
        .fail_job(
            worker,
            &body.lease,
            &body.idempotency_key,
            &body.failure,
            Utc::now(),
        )
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(match outcome {
        CompletionOutcome::Completed => HttpResponse::Created().finish(),
        CompletionOutcome::Replayed => HttpResponse::Ok().finish(),
    })
}

async fn validate_worker_result_run(
    state: &AppState,
    job_id: JobId,
    result_run_id: RunId,
    request_id: &str,
) -> Result<(), ApiError> {
    let job = state
        .storage
        .job(job_id)
        .await
        .map_err(|error| ApiError::storage(error, request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "job_not_found",
                "The leased job does not exist.",
                request_id,
            )
        })?;
    if job.subject.build_run_id() != Some(result_run_id) {
        return Err(ApiError::validation(
            "Worker result targets a different run than the leased job.",
            vec![ValidationError {
                field: "result.run_id".into(),
                code: "run_mismatch".into(),
                message: "Use the run identity carried by the claimed job.".into(),
            }],
            request_id,
        ));
    }
    Ok(())
}

/// Liveness response.
#[derive(Debug, Serialize, ToSchema)]
struct LivenessResponse {
    status: &'static str,
    version: &'static str,
}

#[utoipa::path(
    get,
    path = "/healthz",
    tag = "system",
    responses((status = 200, description = "Process is alive", body = LivenessResponse))
)]
#[actix_web::get("/healthz")]
pub(crate) async fn healthz() -> HttpResponse {
    HttpResponse::Ok()
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .json(LivenessResponse {
            status: "ok",
            version: env!("CARGO_PKG_VERSION"),
        })
}

/// Dependency readiness response.
#[derive(Debug, Serialize, ToSchema)]
struct ReadinessResponse {
    status: &'static str,
    version: &'static str,
}

#[utoipa::path(
    get,
    path = "/readyz",
    tag = "system",
    responses(
        (status = 200, description = "Server dependencies are ready", body = ReadinessResponse),
        (status = 503, description = "A required server dependency is unavailable", body = ReadinessResponse)
    )
)]
#[actix_web::get("/readyz")]
pub(crate) async fn readyz(state: web::Data<AppState>) -> HttpResponse {
    let checks = async {
        let (storage, store) = tokio::join!(
            state.storage.check_readiness(),
            state.store.check_readiness()
        );
        storage.is_ok() && store.is_ok()
    };
    let ready = tokio::time::timeout(std::time::Duration::from_secs(2), checks)
        .await
        .unwrap_or(false);
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    HttpResponse::build(status)
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .json(ReadinessResponse {
            status: if ready { "ready" } else { "not_ready" },
            version: env!("CARGO_PKG_VERSION"),
        })
}

#[utoipa::path(
    get,
    path = "/api/v1/openapi.json",
    tag = "system",
    responses((status = 200, description = "Released OpenAPI contract"))
)]
#[actix_web::get("/openapi.json")]
pub(crate) async fn openapi_document() -> HttpResponse {
    HttpResponse::Ok()
        .content_type("application/json")
        .body(crate::openapi::json())
}

/// First-administrator request using the one-time secret file contents.
#[derive(Debug, Deserialize, ToSchema)]
pub struct BootstrapRequest {
    /// One-time bootstrap secret.
    #[schema(value_type = String, format = Password)]
    secret: String,
    /// First administrator login name.
    username: String,
    /// First administrator password.
    #[schema(value_type = String, format = Password)]
    password: String,
}

/// Safe authenticated principal representation.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PrincipalResponse {
    /// UUIDv7 identity.
    id: String,
    /// Login or service name.
    name: String,
    /// `human`, `service`, or `worker`.
    kind: String,
    /// Assigned role names.
    roles: Vec<String>,
}

impl From<Principal> for PrincipalResponse {
    fn from(value: Principal) -> Self {
        let kind = match value.kind {
            PrincipalKind::Human => "human",
            PrincipalKind::Service => "service",
            PrincipalKind::Worker => "worker",
        };
        Self {
            id: value.id.to_string(),
            name: value.name,
            kind: kind.into(),
            roles: value
                .roles
                .into_iter()
                .map(|role| role.to_string())
                .collect(),
        }
    }
}

#[utoipa::path(
    post,
    path = "/api/v1/auth/bootstrap",
    tag = "auth",
    request_body = BootstrapRequest,
    responses(
        (status = 201, description = "First administrator created", body = PrincipalResponse),
        (status = 400, description = "Invalid request", body = Problem),
        (status = 410, description = "Bootstrap unavailable", body = Problem)
    )
)]
#[actix_web::post("/auth/bootstrap")]
pub(crate) async fn bootstrap(
    request: HttpRequest,
    state: web::Data<AppState>,
    body: web::Json<BootstrapRequest>,
) -> Result<HttpResponse, ApiError> {
    let request_id = request_id(&request);
    let password_hash = PasswordPolicy::default()
        .hash(&body.password)
        .map_err(|error| {
            ApiError::validation(
                "The bootstrap request is invalid.",
                vec![ValidationError {
                    field: "password".into(),
                    code: "weak_password".into(),
                    message: error.to_string(),
                }],
                &request_id,
            )
        })?;
    let principal = state
        .storage
        .bootstrap_admin(&body.secret, &body.username, &password_hash, Utc::now())
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    match tokio::fs::remove_file(&state.bootstrap_secret_path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(ApiError::internal(&request_id)),
    }
    Ok(HttpResponse::Created()
        .insert_header(("x-request-id", request_id))
        .json(PrincipalResponse::from(principal)))
}

/// Local human login request.
#[derive(Debug, Deserialize, ToSchema)]
pub struct LoginRequest {
    /// Human login name.
    username: String,
    /// Human password.
    #[schema(value_type = String, format = Password)]
    password: String,
}

/// Short-lived CLI session token.
#[derive(Serialize, ToSchema)]
pub struct LoginResponse {
    /// Raw bearer token, returned exactly once.
    #[schema(value_type = String, format = Password)]
    token: String,
    /// Session expiration.
    expires_at: chrono::DateTime<Utc>,
}

impl std::fmt::Debug for LoginResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LoginResponse")
            .field("token", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[utoipa::path(
    post,
    path = "/api/v1/auth/login",
    tag = "auth",
    request_body = LoginRequest,
    responses(
        (status = 200, description = "Short-lived CLI session", body = LoginResponse),
        (status = 401, description = "Invalid credentials", body = Problem)
    )
)]
#[actix_web::post("/auth/login")]
pub(crate) async fn login(
    request: HttpRequest,
    state: web::Data<AppState>,
    body: web::Json<LoginRequest>,
) -> Result<HttpResponse, ApiError> {
    let request_id = request_id(&request);
    if body.username.trim() != body.username
        || !(1..=128).contains(&body.username.len())
        || body.password.chars().count() > 1024
    {
        return Err(ApiError::unauthorized(&request_id));
    }
    let credential = state
        .storage
        .human_credential(&body.username)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    let policy = PasswordPolicy::default();
    let verified = match &credential {
        Some(credential) => policy
            .verify(&body.password, &credential.password_hash)
            .is_ok(),
        None => policy.verify(&body.password, dummy_password_hash()).is_ok(),
    };
    let credential = credential
        .filter(|_| verified)
        .ok_or_else(|| ApiError::unauthorized(&request_id))?;
    let (token, token_hash) = generate_token();
    let now = Utc::now();
    let expires_at = now + Duration::minutes(15);
    state
        .storage
        .create_credential(
            credential.principal.id,
            Some("cli-session"),
            "session",
            &token_hash,
            Some(expires_at),
            now,
        )
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(HttpResponse::Ok()
        .insert_header(("x-request-id", request_id))
        .json(LoginResponse {
            token: consume_secret(token),
            expires_at,
        }))
}

fn dummy_password_hash() -> &'static PasswordHash {
    static HASH: OnceLock<PasswordHash> = OnceLock::new();
    HASH.get_or_init(|| {
        PasswordPolicy::default()
            .hash("stabbur-invalid-login-sentinel")
            .expect("the static login sentinel satisfies password policy")
    })
}

fn consume_secret(token: SecretToken) -> String {
    token.expose_secret().to_owned()
}

async fn authenticate(
    request: &HttpRequest,
    state: &AppState,
    permission: Permission,
) -> Result<Principal, ApiError> {
    let principal = bearer_principal(request, state).await?;
    let request_id = request_id(request);
    let roles = state
        .storage
        .roles()
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .into_iter()
        .map(|record| (record.role.name.clone(), record.role))
        .collect::<BTreeMap<_, _>>();
    if Authorizer::allows(&principal, permission, &roles) {
        Ok(principal)
    } else {
        Err(ApiError::forbidden(&request_id))
    }
}

async fn bearer_principal(request: &HttpRequest, state: &AppState) -> Result<Principal, ApiError> {
    let request_id = request_id(request);
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::unauthorized(&request_id))?;
    let principal = state
        .storage
        .principal_by_token(&TokenHash::from_secret(token), Utc::now())
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| ApiError::unauthorized(&request_id))?;
    Ok(principal)
}

async fn authenticate_worker(
    request: &HttpRequest,
    state: &AppState,
    worker_id: WorkerId,
) -> Result<WorkerRecord, ApiError> {
    let request_id = request_id(request);
    let principal = bearer_principal(request, state).await?;
    if principal.kind != PrincipalKind::Worker {
        return Err(ApiError::forbidden(&request_id));
    }
    let worker = state
        .storage
        .worker_for_principal(principal.id)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| ApiError::forbidden(&request_id))?;
    if worker.id != worker_id || !worker.enabled || worker.principal_id != Some(principal.id) {
        return Err(ApiError::forbidden(&request_id));
    }
    Ok(worker)
}

#[utoipa::path(
    get,
    path = "/api/v1/auth/me",
    tag = "auth",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Authenticated principal", body = PrincipalResponse),
        (status = 401, description = "Authentication required", body = Problem)
    )
)]
#[actix_web::get("/auth/me")]
pub(crate) async fn me(
    request: HttpRequest,
    state: web::Data<AppState>,
) -> Result<web::Json<PrincipalResponse>, ApiError> {
    let principal = authenticate(&request, &state, Permission::SoftwareRead).await?;
    Ok(web::Json(principal.into()))
}

/// Administrative principal representation.
#[derive(Debug, Serialize, ToSchema)]
pub struct PrincipalAdminResponse {
    /// UUIDv7 principal identity.
    id: String,
    /// Unique login or service name.
    name: String,
    /// `human`, `service`, or `worker`.
    kind: String,
    /// Assigned roles.
    roles: Vec<String>,
    /// Whether authentication is enabled.
    enabled: bool,
    /// Creation time.
    created_at: chrono::DateTime<Utc>,
    /// Optimistic-concurrency revision.
    revision: u64,
}

impl From<PrincipalRecord> for PrincipalAdminResponse {
    fn from(value: PrincipalRecord) -> Self {
        let kind = match value.principal.kind {
            PrincipalKind::Human => "human",
            PrincipalKind::Service => "service",
            PrincipalKind::Worker => "worker",
        };
        Self {
            id: value.principal.id.to_string(),
            name: value.principal.name,
            kind: kind.into(),
            roles: value
                .principal
                .roles
                .into_iter()
                .map(|role| role.to_string())
                .collect(),
            enabled: value.principal.enabled,
            created_at: value.created_at,
            revision: value.revision,
        }
    }
}

/// Cursor page of administrative principals.
#[derive(Debug, Serialize, ToSchema)]
pub struct PrincipalPage {
    /// Current page.
    items: Vec<PrincipalAdminResponse>,
    /// Opaque next cursor.
    next_cursor: Option<String>,
}

/// Human or service principal creation request.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreatePrincipalRequest {
    /// Unique login or service name.
    name: String,
    /// `human` or `service`.
    kind: String,
    /// Initial human password; forbidden for services.
    #[schema(value_type = Option<String>, format = Password)]
    password: Option<String>,
    /// One or more existing roles.
    roles: Vec<String>,
}

/// Principal status or role-assignment replacement.
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdatePrincipalRequest {
    /// Complete replacement role set.
    roles: Option<Vec<String>>,
    /// New enabled state.
    enabled: Option<bool>,
}

fn parse_role_names(values: &[String], request_id: &str) -> Result<Vec<RoleName>, ApiError> {
    values
        .iter()
        .map(|value| RoleName::new(value.clone()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            ApiError::validation(
                "A role name is invalid.",
                vec![ValidationError {
                    field: "roles".into(),
                    code: "invalid_role".into(),
                    message: error.to_string(),
                }],
                request_id,
            )
        })
}

#[utoipa::path(
    get,
    path = "/api/v1/auth/principals",
    tag = "auth",
    params(PageQuery),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Principal page", body = PrincipalPage))
)]
#[actix_web::get("/auth/principals")]
pub(crate) async fn list_principals(
    request: HttpRequest,
    state: web::Data<AppState>,
    query: web::Query<PageQuery>,
) -> Result<web::Json<PrincipalPage>, ApiError> {
    authenticate(&request, &state, Permission::AuthManage).await?;
    let request_id = request_id(&request);
    let limit = query.limit.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return Err(ApiError::validation(
            "The pagination limit is invalid.",
            vec![],
            &request_id,
        ));
    }
    let cursor = query
        .cursor
        .as_deref()
        .map(|cursor| {
            let decoded = URL_SAFE_NO_PAD
                .decode(cursor)
                .ok()
                .and_then(|value| String::from_utf8(value).ok())
                .ok_or_else(|| {
                    ApiError::validation("The pagination cursor is invalid.", vec![], &request_id)
                })?;
            decoded.parse::<PrincipalId>().map_err(|_| {
                ApiError::validation("The pagination cursor is invalid.", vec![], &request_id)
            })?;
            Ok(decoded)
        })
        .transpose()?;
    let principals = state
        .storage
        .list_principals(cursor.as_deref(), limit)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    let next_cursor = (principals.len() == limit as usize)
        .then(|| {
            principals
                .last()
                .map(|record| URL_SAFE_NO_PAD.encode(record.principal.id.to_string()))
        })
        .flatten();
    Ok(web::Json(PrincipalPage {
        items: principals
            .into_iter()
            .map(PrincipalAdminResponse::from)
            .collect(),
        next_cursor,
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/auth/principals",
    tag = "auth",
    request_body = CreatePrincipalRequest,
    security(("bearer_auth" = [])),
    responses((status = 201, description = "Human or service principal", body = PrincipalAdminResponse))
)]
#[actix_web::post("/auth/principals")]
pub(crate) async fn create_principal(
    request: HttpRequest,
    state: web::Data<AppState>,
    body: web::Json<CreatePrincipalRequest>,
) -> Result<HttpResponse, ApiError> {
    let actor = authenticate(&request, &state, Permission::AuthManage).await?;
    let request_id = request_id(&request);
    let kind = match body.kind.as_str() {
        "human" => PrincipalKind::Human,
        "service" => PrincipalKind::Service,
        _ => {
            return Err(ApiError::validation(
                "Principal kind is invalid.",
                vec![],
                &request_id,
            ));
        }
    };
    let password_hash = match (kind, body.password.as_deref()) {
        (PrincipalKind::Human, Some(password)) => {
            Some(PasswordPolicy::default().hash(password).map_err(|error| {
                ApiError::validation(
                    "Human password is invalid.",
                    vec![ValidationError {
                        field: "password".into(),
                        code: "weak_password".into(),
                        message: error.to_string(),
                    }],
                    &request_id,
                )
            })?)
        }
        (PrincipalKind::Service, None) => None,
        _ => {
            return Err(ApiError::validation(
                "Humans require a password and services must not have one.",
                vec![],
                &request_id,
            ));
        }
    };
    let roles = parse_role_names(&body.roles, &request_id)?;
    let principal = Principal {
        id: PrincipalId::new(),
        name: body.name.clone(),
        kind,
        roles: roles.into_iter().collect(),
        enabled: true,
    };
    let now = Utc::now();
    let record = state
        .storage
        .create_principal(
            &principal,
            password_hash.as_ref(),
            &AuditEvent {
                id: AuditEventId::new(),
                actor: AuditActor::Principal(actor.id),
                action: "auth.principal.create".into(),
                resource_kind: "principal".into(),
                resource_id: Some(principal.id.to_string()),
                details: serde_json::json!({"name": body.name, "kind": body.kind, "roles": body.roles}),
                request_id: Some(request_id.clone()),
                occurred_at: now,
            },
            now,
        )
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(HttpResponse::Created()
        .insert_header((header::ETAG, revision_etag(record.revision)))
        .json(PrincipalAdminResponse::from(record)))
}

#[utoipa::path(
    get,
    path = "/api/v1/auth/principals/{principal}",
    tag = "auth",
    params(("principal" = String, Path, description = "Principal UUIDv7 or name")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Principal", body = PrincipalAdminResponse))
)]
#[actix_web::get("/auth/principals/{principal}")]
pub(crate) async fn get_principal(
    request: HttpRequest,
    state: web::Data<AppState>,
    identity: web::Path<String>,
) -> Result<HttpResponse, ApiError> {
    authenticate(&request, &state, Permission::AuthManage).await?;
    let request_id = request_id(&request);
    let record = state
        .storage
        .principal_record(&identity)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "principal_not_found",
                "Principal does not exist.",
                &request_id,
            )
        })?;
    Ok(HttpResponse::Ok()
        .insert_header((header::ETAG, revision_etag(record.revision)))
        .json(PrincipalAdminResponse::from(record)))
}

#[utoipa::path(
    patch,
    path = "/api/v1/auth/principals/{principal}",
    tag = "auth",
    params(
        ("principal" = String, Path, description = "Principal UUIDv7 or name"),
        ("If-Match" = String, Header, description = "Current ETag")
    ),
    request_body = UpdatePrincipalRequest,
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Updated principal", body = PrincipalAdminResponse))
)]
#[actix_web::patch("/auth/principals/{principal}")]
pub(crate) async fn update_principal(
    request: HttpRequest,
    state: web::Data<AppState>,
    identity: web::Path<String>,
    body: web::Json<UpdatePrincipalRequest>,
) -> Result<HttpResponse, ApiError> {
    let actor = authenticate(&request, &state, Permission::AuthManage).await?;
    let request_id = request_id(&request);
    let expected_revision = expected_revision(&request, &request_id)?;
    if body.roles.is_some() == body.enabled.is_some() {
        return Err(ApiError::validation(
            "Change exactly one of roles or enabled per request.",
            vec![],
            &request_id,
        ));
    }
    let current = state
        .storage
        .principal_record(&identity)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "principal_not_found",
                "Principal does not exist.",
                &request_id,
            )
        })?;
    let now = Utc::now();
    let audit = AuditEvent {
        id: AuditEventId::new(),
        actor: AuditActor::Principal(actor.id),
        action: if body.roles.is_some() {
            "auth.principal.roles.assign"
        } else {
            "auth.principal.status.set"
        }
        .into(),
        resource_kind: "principal".into(),
        resource_id: Some(current.principal.id.to_string()),
        details: serde_json::json!({"roles": body.roles, "enabled": body.enabled}),
        request_id: Some(request_id.clone()),
        occurred_at: now,
    };
    let record = if let Some(roles) = &body.roles {
        let roles = parse_role_names(roles, &request_id)?;
        state
            .storage
            .assign_roles(current.principal.id, &roles, expected_revision, &audit)
            .await
    } else {
        state
            .storage
            .set_principal_enabled(
                current.principal.id,
                body.enabled
                    .expect("exactly one update field was validated"),
                expected_revision,
                &audit,
                now,
            )
            .await
    }
    .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(HttpResponse::Ok()
        .insert_header((header::ETAG, revision_etag(record.revision)))
        .json(PrincipalAdminResponse::from(record)))
}

/// Self-service password change request.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ChangePasswordRequest {
    /// Current password.
    #[schema(value_type = String, format = Password)]
    current_password: String,
    /// New password.
    #[schema(value_type = String, format = Password)]
    new_password: String,
}

/// Administrative password reset request.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ResetPasswordRequest {
    /// New password.
    #[schema(value_type = String, format = Password)]
    new_password: String,
}

#[utoipa::path(
    post,
    path = "/api/v1/auth/password",
    tag = "auth",
    request_body = ChangePasswordRequest,
    security(("bearer_auth" = [])),
    responses((status = 204, description = "Password changed and sessions revoked"))
)]
#[actix_web::post("/auth/password")]
pub(crate) async fn change_password(
    request: HttpRequest,
    state: web::Data<AppState>,
    body: web::Json<ChangePasswordRequest>,
) -> Result<HttpResponse, ApiError> {
    let principal = bearer_principal(&request, &state).await?;
    let request_id = request_id(&request);
    if principal.kind != PrincipalKind::Human {
        return Err(ApiError::forbidden(&request_id));
    }
    let credential = state
        .storage
        .human_credential(&principal.name)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| ApiError::unauthorized(&request_id))?;
    PasswordPolicy::default()
        .verify(&body.current_password, &credential.password_hash)
        .map_err(|_| ApiError::unauthorized(&request_id))?;
    let hash = PasswordPolicy::default()
        .hash(&body.new_password)
        .map_err(|error| {
            ApiError::validation(
                "New password is invalid.",
                vec![ValidationError {
                    field: "new_password".into(),
                    code: "weak_password".into(),
                    message: error.to_string(),
                }],
                &request_id,
            )
        })?;
    state
        .storage
        .reset_password(
            &principal.name,
            &hash,
            AuditActor::Principal(principal.id),
            Utc::now(),
        )
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(HttpResponse::NoContent().finish())
}

#[utoipa::path(
    post,
    path = "/api/v1/auth/principals/{principal}/password",
    tag = "auth",
    params(("principal" = String, Path, description = "Human UUIDv7 or name")),
    request_body = ResetPasswordRequest,
    security(("bearer_auth" = [])),
    responses((status = 204, description = "Password reset and credentials revoked"))
)]
#[actix_web::post("/auth/principals/{principal}/password")]
pub(crate) async fn reset_principal_password(
    request: HttpRequest,
    state: web::Data<AppState>,
    identity: web::Path<String>,
    body: web::Json<ResetPasswordRequest>,
) -> Result<HttpResponse, ApiError> {
    let actor = authenticate(&request, &state, Permission::AuthManage).await?;
    let request_id = request_id(&request);
    let principal = state
        .storage
        .principal_record(&identity)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .filter(|record| record.principal.kind == PrincipalKind::Human)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "human_not_found",
                "Human principal does not exist.",
                &request_id,
            )
        })?;
    let hash = PasswordPolicy::default()
        .hash(&body.new_password)
        .map_err(|error| {
            ApiError::validation(
                "New password is invalid.",
                vec![ValidationError {
                    field: "new_password".into(),
                    code: "weak_password".into(),
                    message: error.to_string(),
                }],
                &request_id,
            )
        })?;
    state
        .storage
        .reset_password(
            &principal.principal.name,
            &hash,
            AuditActor::Principal(actor.id),
            Utc::now(),
        )
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(HttpResponse::NoContent().finish())
}

#[utoipa::path(
    post,
    path = "/api/v1/auth/principals/{principal}/revoke-sessions",
    tag = "auth",
    params(("principal" = String, Path, description = "Human UUIDv7 or name")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Credential revocation count"))
)]
#[actix_web::post("/auth/principals/{principal}/revoke-sessions")]
pub(crate) async fn revoke_principal_sessions(
    request: HttpRequest,
    state: web::Data<AppState>,
    identity: web::Path<String>,
) -> Result<web::Json<serde_json::Value>, ApiError> {
    let actor = authenticate(&request, &state, Permission::AuthManage).await?;
    let request_id = request_id(&request);
    let principal = state
        .storage
        .principal_record(&identity)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .filter(|record| record.principal.kind == PrincipalKind::Human)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "human_not_found",
                "Human principal does not exist.",
                &request_id,
            )
        })?;
    let count = state
        .storage
        .revoke_credentials(
            &principal.principal.name,
            AuditActor::Principal(actor.id),
            Utc::now(),
        )
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(web::Json(serde_json::json!({"credentials_revoked": count})))
}

/// API token metadata without recoverable secret material.
#[derive(Debug, Serialize, ToSchema)]
pub struct ApiTokenResponse {
    /// Token UUIDv7 identity.
    id: String,
    /// Owning principal identity.
    principal_id: String,
    /// Required token name.
    name: String,
    /// Creation time.
    created_at: chrono::DateTime<Utc>,
    /// Optional expiration.
    expires_at: Option<chrono::DateTime<Utc>>,
    /// Revocation time.
    revoked_at: Option<chrono::DateTime<Utc>>,
}

impl From<ApiTokenRecord> for ApiTokenResponse {
    fn from(value: ApiTokenRecord) -> Self {
        Self {
            id: value.id,
            principal_id: value.principal_id.to_string(),
            name: value.name,
            created_at: value.created_at,
            expires_at: value.expires_at,
            revoked_at: value.revoked_at,
        }
    }
}

/// API token collection.
#[derive(Debug, Serialize, ToSchema)]
pub struct ApiTokenList {
    /// Named tokens, including revoked metadata.
    items: Vec<ApiTokenResponse>,
}

/// Named long-lived API token request.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateApiTokenRequest {
    /// Unique operator-facing name within the principal.
    name: String,
    /// Optional expiration.
    expires_at: Option<chrono::DateTime<Utc>>,
}

/// One-time API token response.
#[derive(Serialize, ToSchema)]
pub struct CreatedApiTokenResponse {
    /// Token metadata.
    token: ApiTokenResponse,
    /// Raw bearer secret returned exactly once.
    #[schema(value_type = String, format = Password)]
    secret: String,
}

impl std::fmt::Debug for CreatedApiTokenResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CreatedApiTokenResponse")
            .field("token", &self.token)
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/auth/principals/{principal}/tokens",
    tag = "auth",
    params(("principal" = String, Path, description = "Principal UUIDv7 or name")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Named API tokens", body = ApiTokenList))
)]
#[actix_web::get("/auth/principals/{principal}/tokens")]
pub(crate) async fn list_api_tokens(
    request: HttpRequest,
    state: web::Data<AppState>,
    identity: web::Path<String>,
) -> Result<web::Json<ApiTokenList>, ApiError> {
    authenticate(&request, &state, Permission::AuthManage).await?;
    let request_id = request_id(&request);
    let principal = state
        .storage
        .principal_record(&identity)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "principal_not_found",
                "Principal does not exist.",
                &request_id,
            )
        })?;
    let tokens = state
        .storage
        .api_tokens(principal.principal.id)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(web::Json(ApiTokenList {
        items: tokens.into_iter().map(ApiTokenResponse::from).collect(),
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/auth/principals/{principal}/tokens",
    tag = "auth",
    params(("principal" = String, Path, description = "Principal UUIDv7 or name")),
    request_body = CreateApiTokenRequest,
    security(("bearer_auth" = [])),
    responses((status = 201, description = "One-time named API token", body = CreatedApiTokenResponse))
)]
#[actix_web::post("/auth/principals/{principal}/tokens")]
pub(crate) async fn create_api_token(
    request: HttpRequest,
    state: web::Data<AppState>,
    identity: web::Path<String>,
    body: web::Json<CreateApiTokenRequest>,
) -> Result<HttpResponse, ApiError> {
    let actor = authenticate(&request, &state, Permission::AuthManage).await?;
    let request_id = request_id(&request);
    let principal = state
        .storage
        .principal_record(&identity)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "principal_not_found",
                "Principal does not exist.",
                &request_id,
            )
        })?;
    let now = Utc::now();
    if body.expires_at.is_some_and(|expires| expires <= now) {
        return Err(ApiError::validation(
            "Token expiration must be in the future.",
            vec![],
            &request_id,
        ));
    }
    let (secret, hash) = generate_token();
    let record = ApiTokenRecord {
        id: Uuid::now_v7().to_string(),
        principal_id: principal.principal.id,
        name: body.name.clone(),
        created_at: now,
        expires_at: body.expires_at,
        revoked_at: None,
    };
    state
        .storage
        .create_api_token(
            &record,
            &hash,
            &AuditEvent {
                id: AuditEventId::new(),
                actor: AuditActor::Principal(actor.id),
                action: "auth.token.create".into(),
                resource_kind: "api_token".into(),
                resource_id: Some(record.id.clone()),
                details: serde_json::json!({"principal_id": record.principal_id, "name": record.name, "expires_at": record.expires_at}),
                request_id: Some(request_id.clone()),
                occurred_at: now,
            },
        )
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(HttpResponse::Created().json(CreatedApiTokenResponse {
        token: ApiTokenResponse::from(record),
        secret: consume_secret(secret),
    }))
}

#[utoipa::path(
    delete,
    path = "/api/v1/auth/tokens/{token}",
    tag = "auth",
    params(("token" = String, Path, description = "API token UUIDv7")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Revoked token metadata", body = ApiTokenResponse))
)]
#[actix_web::delete("/auth/tokens/{token}")]
pub(crate) async fn revoke_api_token(
    request: HttpRequest,
    state: web::Data<AppState>,
    token_id: web::Path<String>,
) -> Result<web::Json<ApiTokenResponse>, ApiError> {
    let actor = authenticate(&request, &state, Permission::AuthManage).await?;
    let request_id = request_id(&request);
    let token_id = token_id.into_inner();
    let token = state
        .storage
        .revoke_api_token(
            &token_id,
            &AuditEvent {
                id: AuditEventId::new(),
                actor: AuditActor::Principal(actor.id),
                action: "auth.token.revoke".into(),
                resource_kind: "api_token".into(),
                resource_id: Some(token_id.clone()),
                details: serde_json::json!({}),
                request_id: Some(request_id.clone()),
                occurred_at: Utc::now(),
            },
            Utc::now(),
        )
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(web::Json(ApiTokenResponse::from(token)))
}

/// Role and permission response.
#[derive(Debug, Serialize, ToSchema)]
pub struct RoleResponse {
    /// Role name.
    name: String,
    /// Sorted permission names.
    permissions: Vec<String>,
    /// Whether the role is immutable.
    built_in: bool,
    /// Optimistic-concurrency revision.
    revision: u64,
}

fn permission_name(permission: Permission) -> String {
    serde_json::to_value(permission)
        .expect("permission is serializable")
        .as_str()
        .expect("permission serializes as text")
        .to_owned()
}

impl From<RoleRecord> for RoleResponse {
    fn from(value: RoleRecord) -> Self {
        Self {
            name: value.role.name.to_string(),
            permissions: value
                .role
                .permissions
                .into_iter()
                .map(permission_name)
                .collect(),
            built_in: value.role.built_in,
            revision: value.revision,
        }
    }
}

/// Role collection.
#[derive(Debug, Serialize, ToSchema)]
pub struct RoleList {
    /// Built-in and custom roles.
    items: Vec<RoleResponse>,
}

/// Custom role request.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateRoleRequest {
    /// Valid lowercase role name.
    name: String,
    /// One or more exact permission names.
    permissions: Vec<String>,
}

fn parse_permissions(values: &[String], request_id: &str) -> Result<Vec<Permission>, ApiError> {
    values
        .iter()
        .map(|value| {
            serde_json::from_value(serde_json::Value::String(value.clone())).map_err(|_| {
                ApiError::validation(
                    "A permission name is invalid.",
                    vec![ValidationError {
                        field: "permissions".into(),
                        code: "invalid_permission".into(),
                        message: format!("Unknown permission: {value}"),
                    }],
                    request_id,
                )
            })
        })
        .collect()
}

#[utoipa::path(
    get,
    path = "/api/v1/auth/roles",
    tag = "auth",
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Role policy", body = RoleList))
)]
#[actix_web::get("/auth/roles")]
pub(crate) async fn list_roles(
    request: HttpRequest,
    state: web::Data<AppState>,
) -> Result<web::Json<RoleList>, ApiError> {
    authenticate(&request, &state, Permission::AuthManage).await?;
    let request_id = request_id(&request);
    let roles = state
        .storage
        .roles()
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(web::Json(RoleList {
        items: roles.into_iter().map(RoleResponse::from).collect(),
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/auth/roles",
    tag = "auth",
    request_body = CreateRoleRequest,
    security(("bearer_auth" = [])),
    responses((status = 201, description = "Custom role", body = RoleResponse))
)]
#[actix_web::post("/auth/roles")]
pub(crate) async fn create_role(
    request: HttpRequest,
    state: web::Data<AppState>,
    body: web::Json<CreateRoleRequest>,
) -> Result<HttpResponse, ApiError> {
    let actor = authenticate(&request, &state, Permission::AuthManage).await?;
    let request_id = request_id(&request);
    let name = RoleName::new(body.name.clone()).map_err(|error| {
        ApiError::validation(
            "Role name is invalid.",
            vec![ValidationError {
                field: "name".into(),
                code: "invalid_role".into(),
                message: error.to_string(),
            }],
            &request_id,
        )
    })?;
    let permissions = parse_permissions(&body.permissions, &request_id)?;
    let record = state
        .storage
        .create_role(
            &name,
            &permissions,
            &AuditEvent {
                id: AuditEventId::new(),
                actor: AuditActor::Principal(actor.id),
                action: "auth.role.create".into(),
                resource_kind: "role".into(),
                resource_id: Some(name.to_string()),
                details: serde_json::json!({"permissions": body.permissions}),
                request_id: Some(request_id.clone()),
                occurred_at: Utc::now(),
            },
        )
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(HttpResponse::Created()
        .insert_header((header::ETAG, revision_etag(record.revision)))
        .json(RoleResponse::from(record)))
}

/// Administrative worker provisioning request.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ProvisionWorkerRequest {
    /// Unique operator-facing worker name.
    name: String,
    /// Maximum capabilities this worker may advertise.
    allowed_capabilities: Vec<String>,
}

/// One-time worker credential file contents.
#[derive(Serialize, ToSchema)]
pub struct WorkerCredentialResponse {
    /// Server-assigned worker identity.
    worker_id: String,
    /// One-time bearer token written directly to an owner-only file.
    #[schema(value_type = String, format = Password)]
    token: String,
}

impl std::fmt::Debug for WorkerCredentialResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkerCredentialResponse")
            .field("worker_id", &self.worker_id)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

#[utoipa::path(
    post,
    path = "/api/v1/workers",
    tag = "workers",
    request_body = ProvisionWorkerRequest,
    security(("bearer_auth" = [])),
    responses(
        (status = 201, description = "One-time worker credential", body = WorkerCredentialResponse),
        (status = 409, description = "Worker name already exists", body = Problem)
    )
)]
#[actix_web::post("/workers")]
pub(crate) async fn provision_worker(
    request: HttpRequest,
    state: web::Data<AppState>,
    body: web::Json<ProvisionWorkerRequest>,
) -> Result<HttpResponse, ApiError> {
    let actor = authenticate(&request, &state, Permission::WorkerManage).await?;
    let request_id = request_id(&request);
    if body.name.trim() != body.name || !(1..=128).contains(&body.name.len()) {
        return Err(ApiError::validation(
            "Worker name is invalid.",
            vec![ValidationError {
                field: "name".into(),
                code: "invalid_name".into(),
                message: "Use 1-128 bytes without leading or trailing whitespace.".into(),
            }],
            &request_id,
        ));
    }
    let allowed_capabilities = capability_set(&body.allowed_capabilities, &request_id)?;
    let worker_id = WorkerId::new();
    let principal_id = PrincipalId::new();
    let now = Utc::now();
    let worker = WorkerRecord {
        draining: false,
        id: worker_id,
        principal_id: Some(principal_id),
        name: body.name.clone(),
        allowed_capabilities,
        advertised_capabilities: CapabilitySet::default(),
        enabled: true,
        last_seen_at: now,
        revision: 1,
    };
    let (token, token_hash) = generate_token();
    state
        .storage
        .provision_worker(
            &worker,
            &token_hash,
            &AuditEvent {
                id: AuditEventId::new(),
                actor: AuditActor::Principal(actor.id),
                action: "worker.provision".into(),
                resource_kind: "worker".into(),
                resource_id: Some(worker_id.to_string()),
                details: serde_json::json!({
                    "name": body.name,
                    "allowed_capabilities": body.allowed_capabilities,
                }),
                request_id: Some(request_id.clone()),
                occurred_at: now,
            },
        )
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(HttpResponse::Created()
        .insert_header(("x-request-id", request_id))
        .json(WorkerCredentialResponse {
            worker_id: worker_id.to_string(),
            token: consume_secret(token),
        }))
}

/// Administrative worker status and capabilities.
#[derive(Debug, Serialize, ToSchema)]
pub struct WorkerResponse {
    /// Whether new claims are paused while existing attempts finish.
    draining: bool,
    /// UUIDv7 worker identity.
    id: String,
    /// Operator-facing name.
    name: String,
    /// Server-defined capability ceiling.
    allowed_capabilities: Vec<String>,
    /// Latest detected advertisement.
    advertised_capabilities: Vec<String>,
    /// Whether the worker can authenticate.
    enabled: bool,
    /// Latest registration activity.
    last_seen_at: chrono::DateTime<Utc>,
    /// Optimistic-concurrency revision.
    revision: u64,
}

impl From<WorkerRecord> for WorkerResponse {
    fn from(value: WorkerRecord) -> Self {
        Self {
            draining: value.draining,
            id: value.id.to_string(),
            name: value.name,
            allowed_capabilities: value
                .allowed_capabilities
                .iter()
                .map(|value| value.to_string())
                .collect(),
            advertised_capabilities: value
                .advertised_capabilities
                .iter()
                .map(|value| value.to_string())
                .collect(),
            enabled: value.enabled,
            last_seen_at: value.last_seen_at,
            revision: value.revision,
        }
    }
}

/// Cursor page of workers.
#[derive(Debug, Serialize, ToSchema)]
pub struct WorkerPage {
    /// Current page.
    items: Vec<WorkerResponse>,
    /// Opaque next cursor.
    next_cursor: Option<String>,
}

/// Worker capability-ceiling or status update.
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateWorkerRequest {
    /// New enabled state.
    enabled: Option<bool>,
    /// Complete replacement capability ceiling.
    allowed_capabilities: Option<Vec<String>>,
}

/// One-time rotated worker credential and current metadata.
#[derive(Serialize, ToSchema)]
pub struct RotatedWorkerCredentialResponse {
    /// Updated worker metadata.
    worker: WorkerResponse,
    /// New raw credential returned exactly once.
    #[schema(value_type = String, format = Password)]
    token: String,
}

impl std::fmt::Debug for RotatedWorkerCredentialResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RotatedWorkerCredentialResponse")
            .field("worker", &self.worker)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/workers",
    tag = "workers",
    params(PageQuery),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Worker page", body = WorkerPage))
)]
#[actix_web::get("/workers")]
pub(crate) async fn list_workers(
    request: HttpRequest,
    state: web::Data<AppState>,
    query: web::Query<PageQuery>,
) -> Result<web::Json<WorkerPage>, ApiError> {
    authenticate(&request, &state, Permission::WorkerRead).await?;
    let request_id = request_id(&request);
    let limit = query.limit.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return Err(ApiError::validation(
            "The pagination limit is invalid.",
            vec![],
            &request_id,
        ));
    }
    let cursor = query
        .cursor
        .as_deref()
        .map(|cursor| {
            let decoded = URL_SAFE_NO_PAD
                .decode(cursor)
                .ok()
                .and_then(|value| String::from_utf8(value).ok())
                .ok_or_else(|| {
                    ApiError::validation("The pagination cursor is invalid.", vec![], &request_id)
                })?;
            decoded.parse::<WorkerId>().map_err(|_| {
                ApiError::validation("The pagination cursor is invalid.", vec![], &request_id)
            })?;
            Ok(decoded)
        })
        .transpose()?;
    let workers = state
        .storage
        .list_workers(cursor.as_deref(), limit)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    let next_cursor = (workers.len() == limit as usize)
        .then(|| {
            workers
                .last()
                .map(|worker| URL_SAFE_NO_PAD.encode(worker.id.to_string()))
        })
        .flatten();
    Ok(web::Json(WorkerPage {
        items: workers.into_iter().map(WorkerResponse::from).collect(),
        next_cursor,
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/workers/{worker}",
    tag = "workers",
    params(("worker" = String, Path, description = "Worker UUIDv7")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Worker", body = WorkerResponse))
)]
#[actix_web::get("/workers/{worker}")]
pub(crate) async fn get_worker(
    request: HttpRequest,
    state: web::Data<AppState>,
    worker: web::Path<String>,
) -> Result<HttpResponse, ApiError> {
    authenticate(&request, &state, Permission::WorkerRead).await?;
    let request_id = request_id(&request);
    let worker_id = worker
        .parse::<WorkerId>()
        .map_err(|error| ApiError::domain(error, &request_id))?;
    let worker = state
        .storage
        .worker(worker_id)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "worker_not_found",
                "Worker does not exist.",
                &request_id,
            )
        })?;
    Ok(HttpResponse::Ok()
        .insert_header((header::ETAG, revision_etag(worker.revision)))
        .json(WorkerResponse::from(worker)))
}

#[utoipa::path(
    patch,
    path = "/api/v1/workers/{worker}",
    tag = "workers",
    params(
        ("worker" = String, Path, description = "Worker UUIDv7"),
        ("If-Match" = String, Header, description = "Current ETag")
    ),
    request_body = UpdateWorkerRequest,
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Updated worker", body = WorkerResponse))
)]
#[actix_web::patch("/workers/{worker}")]
pub(crate) async fn update_worker(
    request: HttpRequest,
    state: web::Data<AppState>,
    worker: web::Path<String>,
    body: web::Json<UpdateWorkerRequest>,
) -> Result<HttpResponse, ApiError> {
    let actor = authenticate(&request, &state, Permission::WorkerManage).await?;
    let request_id = request_id(&request);
    let expected_revision = expected_revision(&request, &request_id)?;
    if body.enabled.is_some() == body.allowed_capabilities.is_some() {
        return Err(ApiError::validation(
            "Change exactly one worker field.",
            vec![],
            &request_id,
        ));
    }
    let worker_id = worker
        .parse::<WorkerId>()
        .map_err(|error| ApiError::domain(error, &request_id))?;
    let capabilities = body
        .allowed_capabilities
        .as_deref()
        .map(|values| capability_set(values, &request_id))
        .transpose()?;
    let now = Utc::now();
    let worker = state.storage.update_worker(
        worker_id,
        body.enabled,
        capabilities.as_ref(),
        expected_revision,
        &AuditEvent {
            id: AuditEventId::new(), actor: AuditActor::Principal(actor.id),
            action: "worker.update".into(), resource_kind: "worker".into(),
            resource_id: Some(worker_id.to_string()),
            details: serde_json::json!({"enabled": body.enabled, "allowed_capabilities": body.allowed_capabilities}),
            request_id: Some(request_id.clone()), occurred_at: now,
        },
        now,
    ).await.map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(HttpResponse::Ok()
        .insert_header((header::ETAG, revision_etag(worker.revision)))
        .json(WorkerResponse::from(worker)))
}

#[utoipa::path(
    post,
    path = "/api/v1/workers/{worker}/rotate-token",
    tag = "workers",
    params(
        ("worker" = String, Path, description = "Worker UUIDv7"),
        ("If-Match" = String, Header, description = "Current ETag")
    ),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "One-time rotated credential", body = RotatedWorkerCredentialResponse))
)]
#[actix_web::post("/workers/{worker}/rotate-token")]
pub(crate) async fn rotate_worker_token(
    request: HttpRequest,
    state: web::Data<AppState>,
    worker: web::Path<String>,
) -> Result<HttpResponse, ApiError> {
    let actor = authenticate(&request, &state, Permission::WorkerManage).await?;
    let request_id = request_id(&request);
    let expected_revision = expected_revision(&request, &request_id)?;
    let worker_id = worker
        .parse::<WorkerId>()
        .map_err(|error| ApiError::domain(error, &request_id))?;
    let (token, hash) = generate_token();
    let now = Utc::now();
    let worker = state
        .storage
        .rotate_worker_credential(
            worker_id,
            &hash,
            expected_revision,
            &AuditEvent {
                id: AuditEventId::new(),
                actor: AuditActor::Principal(actor.id),
                action: "worker.credential.rotate".into(),
                resource_kind: "worker".into(),
                resource_id: Some(worker_id.to_string()),
                details: serde_json::json!({}),
                request_id: Some(request_id.clone()),
                occurred_at: now,
            },
            now,
        )
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(HttpResponse::Ok()
        .insert_header((header::ETAG, revision_etag(worker.revision)))
        .json(RotatedWorkerCredentialResponse {
            worker: WorkerResponse::from(worker),
            token: consume_secret(token),
        }))
}

/// Recipe metadata creation request.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateRecipeRequest {
    /// Unique operator-facing recipe name.
    name: String,
}

/// Recipe metadata response.
#[derive(Debug, Serialize, ToSchema)]
pub struct RecipeResponse {
    /// UUIDv7 recipe identity.
    id: String,
    /// Unique operator-facing name.
    name: String,
    /// Creation time.
    created_at: chrono::DateTime<Utc>,
    /// Mutable metadata revision.
    revision: u64,
}

impl From<RecipeRecord> for RecipeResponse {
    fn from(value: RecipeRecord) -> Self {
        Self {
            id: value.id.to_string(),
            name: value.name,
            created_at: value.created_at,
            revision: value.revision,
        }
    }
}

#[utoipa::path(
    post,
    path = "/api/v1/recipes",
    tag = "recipes",
    request_body = CreateRecipeRequest,
    security(("bearer_auth" = [])),
    responses((status = 201, description = "Recipe created", body = RecipeResponse))
)]
#[actix_web::post("/recipes")]
pub(crate) async fn create_recipe(
    request: HttpRequest,
    state: web::Data<AppState>,
    body: web::Json<CreateRecipeRequest>,
) -> Result<HttpResponse, ApiError> {
    let principal = authenticate(&request, &state, Permission::RecipeWrite).await?;
    let request_id = request_id(&request);
    if body.name.trim() != body.name || !(1..=128).contains(&body.name.len()) {
        return Err(ApiError::validation(
            "Recipe name is invalid.",
            vec![ValidationError {
                field: "name".into(),
                code: "invalid_name".into(),
                message: "Use 1-128 bytes without leading or trailing whitespace.".into(),
            }],
            &request_id,
        ));
    }
    let now = Utc::now();
    let recipe = RecipeRecord {
        id: RecipeId::new(),
        name: body.name.clone(),
        created_at: now,
        revision: 1,
    };
    state
        .storage
        .create_recipe(
            &recipe,
            &AuditEvent {
                id: AuditEventId::new(),
                actor: AuditActor::Principal(principal.id),
                action: "recipe.create".into(),
                resource_kind: "recipe".into(),
                resource_id: Some(recipe.id.to_string()),
                details: serde_json::json!({"name": recipe.name}),
                request_id: Some(request_id.clone()),
                occurred_at: now,
            },
        )
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(HttpResponse::Created()
        .insert_header((header::ETAG, revision_etag(recipe.revision)))
        .insert_header(("x-request-id", request_id))
        .json(RecipeResponse::from(recipe)))
}

/// Cursor page of recipes.
#[derive(Debug, Serialize, ToSchema)]
pub struct RecipePage {
    /// Current page.
    items: Vec<RecipeResponse>,
    /// Opaque next cursor.
    next_cursor: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/v1/recipes",
    tag = "recipes",
    params(PageQuery),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Recipe page", body = RecipePage))
)]
#[actix_web::get("/recipes")]
pub(crate) async fn list_recipes(
    request: HttpRequest,
    state: web::Data<AppState>,
    query: web::Query<PageQuery>,
) -> Result<web::Json<RecipePage>, ApiError> {
    authenticate(&request, &state, Permission::RecipeRead).await?;
    let request_id = request_id(&request);
    let limit = query.limit.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return Err(ApiError::validation(
            "The pagination limit is invalid.",
            vec![],
            &request_id,
        ));
    }
    let cursor = query
        .cursor
        .as_deref()
        .map(|cursor| {
            let decoded = URL_SAFE_NO_PAD
                .decode(cursor)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .ok_or_else(|| {
                    ApiError::validation("The pagination cursor is invalid.", vec![], &request_id)
                })?;
            decoded.parse::<RecipeId>().map_err(|_| {
                ApiError::validation("The pagination cursor is invalid.", vec![], &request_id)
            })?;
            Ok(decoded)
        })
        .transpose()?;
    let recipes = state
        .storage
        .list_recipes(cursor.as_deref(), limit)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    let next_cursor = (recipes.len() == limit as usize)
        .then(|| {
            recipes
                .last()
                .map(|recipe| URL_SAFE_NO_PAD.encode(recipe.id.to_string()))
        })
        .flatten();
    Ok(web::Json(RecipePage {
        items: recipes.into_iter().map(RecipeResponse::from).collect(),
        next_cursor,
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/recipes/{recipe}",
    tag = "recipes",
    params(("recipe" = String, Path, description = "Recipe UUIDv7 or exact name")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Recipe", body = RecipeResponse))
)]
#[actix_web::get("/recipes/{recipe}")]
pub(crate) async fn get_recipe(
    request: HttpRequest,
    state: web::Data<AppState>,
    identity: web::Path<String>,
) -> Result<HttpResponse, ApiError> {
    authenticate(&request, &state, Permission::RecipeRead).await?;
    let request_id = request_id(&request);
    let recipe = state
        .storage
        .recipe(&identity)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "recipe_not_found",
                "Recipe does not exist.",
                &request_id,
            )
        })?;
    Ok(HttpResponse::Ok()
        .insert_header((header::ETAG, revision_etag(recipe.revision)))
        .json(RecipeResponse::from(recipe)))
}

/// Immutable builder-neutral recipe revision request.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateRecipeRevisionRequest {
    /// Optional next sequence; a stale plan fails atomically before append.
    #[schema(value_type = Option<u64>, minimum = 1)]
    expected_sequence: Option<std::num::NonZeroU64>,
    /// Stable server-supported builder adapter name.
    builder: String,
    /// Adapter-owned immutable definition. The server validates this for the selected builder.
    #[schema(value_type = Object)]
    definition: serde_json::Value,
    /// Additional scheduling constraints beyond those required by the selected builder.
    #[serde(default)]
    required_capabilities: Vec<String>,
}

/// Immutable recipe revision response.
#[derive(Debug, Serialize, ToSchema)]
pub struct RecipeRevisionResponse {
    /// UUIDv7 immutable revision identity.
    id: String,
    /// Parent recipe identity.
    recipe_id: String,
    /// Monotonic sequence within the recipe.
    sequence: u64,
    /// Stable builder adapter name.
    builder: String,
    /// Adapter definition represented as JSON.
    definition: serde_json::Value,
    /// Required worker capabilities.
    required_capabilities: Vec<String>,
    /// Creation time.
    created_at: chrono::DateTime<Utc>,
}

impl From<RecipeRevisionRecord> for RecipeRevisionResponse {
    fn from(value: RecipeRevisionRecord) -> Self {
        Self {
            id: value.id.to_string(),
            recipe_id: value.recipe_id.to_string(),
            sequence: value.sequence,
            builder: value.builder,
            definition: value.definition,
            required_capabilities: value
                .required_capabilities
                .iter()
                .map(|capability| capability.as_str().to_owned())
                .collect(),
            created_at: value.created_at,
        }
    }
}

/// Immutable recipe revision collection.
#[derive(Debug, Serialize, ToSchema)]
pub struct RecipeRevisionList {
    /// Revisions sorted by sequence.
    items: Vec<RecipeRevisionResponse>,
}

#[utoipa::path(
    get,
    path = "/api/v1/recipes/{recipe}/revisions",
    tag = "recipes",
    params(("recipe" = String, Path, description = "Recipe UUIDv7 or exact name")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Immutable recipe revisions", body = RecipeRevisionList))
)]
#[actix_web::get("/recipes/{recipe}/revisions")]
pub(crate) async fn list_recipe_revisions(
    request: HttpRequest,
    state: web::Data<AppState>,
    identity: web::Path<String>,
) -> Result<web::Json<RecipeRevisionList>, ApiError> {
    authenticate(&request, &state, Permission::RecipeRead).await?;
    let request_id = request_id(&request);
    let recipe = state
        .storage
        .recipe(&identity)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "recipe_not_found",
                "Recipe does not exist.",
                &request_id,
            )
        })?;
    let revisions = state
        .storage
        .recipe_revisions(recipe.id)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(web::Json(RecipeRevisionList {
        items: revisions
            .into_iter()
            .map(RecipeRevisionResponse::from)
            .collect(),
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/recipes/{recipe}/revisions",
    tag = "recipes",
    params(("recipe" = String, Path, description = "Recipe UUIDv7 or exact name")),
    request_body = CreateRecipeRevisionRequest,
    security(("bearer_auth" = [])),
    responses(
        (status = 201, description = "Immutable validated builder revision created", body = RecipeRevisionResponse),
        (status = 404, description = "Recipe not found", body = Problem)
    )
)]
#[actix_web::post("/recipes/{recipe}/revisions")]
pub(crate) async fn create_recipe_revision(
    request: HttpRequest,
    state: web::Data<AppState>,
    recipe_identity: web::Path<String>,
    body: web::Json<CreateRecipeRevisionRequest>,
) -> Result<HttpResponse, ApiError> {
    let principal = authenticate(&request, &state, Permission::RecipeWrite).await?;
    let request_id = request_id(&request);
    let recipe = state
        .storage
        .recipe(&recipe_identity)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "recipe_not_found",
                "Recipe does not exist.",
                &request_id,
            )
        })?;
    let mut capabilities = body.required_capabilities.clone();
    let definition_json = match body.builder.as_str() {
        "autopkg" => {
            let definition: AutoPkgRecipe = serde_json::from_value(body.definition.clone())
                .map_err(|error| {
                    ApiError::validation(
                        "AutoPkg recipe definition is invalid.",
                        vec![ValidationError {
                            field: "definition".into(),
                            code: "invalid_autopkg_recipe".into(),
                            message: error.to_string(),
                        }],
                        &request_id,
                    )
                })?;
            definition.validate().map_err(|error| {
                ApiError::validation(
                    "AutoPkg recipe definition is invalid.",
                    vec![ValidationError {
                        field: "definition".into(),
                        code: "invalid_autopkg_recipe".into(),
                        message: error.to_string(),
                    }],
                    &request_id,
                )
            })?;
            capabilities.extend(["os.macos".to_owned(), "builder.autopkg".to_owned()]);
            serde_json::to_value(definition).map_err(|error| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "recipe_serialization_failed",
                    error.to_string(),
                    &request_id,
                )
            })?
        }
        "fake" => {
            if body.definition != serde_json::json!({}) {
                return Err(ApiError::validation(
                    "The deterministic fake builder definition must be an empty object.",
                    vec![ValidationError {
                        field: "definition".into(),
                        code: "invalid_fake_recipe".into(),
                        message: "Use an empty JSON object.".into(),
                    }],
                    &request_id,
                ));
            }
            capabilities.extend(["runtime.portable".to_owned(), "builder.fake".to_owned()]);
            serde_json::json!({})
        }
        _ => {
            return Err(ApiError::validation(
                "The requested builder adapter is unsupported.",
                vec![ValidationError {
                    field: "builder".into(),
                    code: "unsupported_builder".into(),
                    message: "Supported builders are `autopkg` and `fake`.".into(),
                }],
                &request_id,
            ));
        }
    };
    let required_capabilities = capability_set(&capabilities, &request_id)?;
    let revision_id = RecipeRevisionId::new();
    let now = Utc::now();
    let revision = state
        .storage
        .create_recipe_revision(
            &NewRecipeRevision {
                expected_sequence: body.expected_sequence,
                id: revision_id,
                recipe_id: recipe.id,
                builder: body.builder.clone(),
                definition: definition_json,
                required_capabilities: required_capabilities.clone(),
                created_at: now,
            },
            &AuditEvent {
                id: AuditEventId::new(),
                actor: AuditActor::Principal(principal.id),
                action: "recipe.revision.create".into(),
                resource_kind: "recipe_revision".into(),
                resource_id: Some(revision_id.to_string()),
                details: serde_json::json!({
                    "recipe_id": recipe.id,
                    "builder": body.builder,
                    "required_capabilities": required_capabilities,
                }),
                request_id: Some(request_id.clone()),
                occurred_at: now,
            },
        )
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(HttpResponse::Created()
        .insert_header(("x-request-id", request_id))
        .json(RecipeRevisionResponse::from(revision)))
}

/// Pinned builder-neutral catalog source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct RecipeCatalogSourceResponse {
    /// Stable source locator.
    locator: String,
    /// Exact opaque source revision observed by the worker.
    revision: String,
}

impl From<RecipeCatalogSource> for RecipeCatalogSourceResponse {
    fn from(value: RecipeCatalogSource) -> Self {
        Self {
            locator: value.locator,
            revision: value.revision,
        }
    }
}

/// Builder-neutral recipe observed in a pinned catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct RecipeCatalogEntryResponse {
    /// Observed display hints, never a safety or readiness guarantee. Absent in legacy snapshots.
    #[serde(skip_serializing_if = "Option::is_none")]
    guidance: Option<RecipeCatalogGuidanceResponse>,
    /// Complete pinned source closure; absent when a recipe needs attention before import.
    #[serde(skip_serializing_if = "Option::is_none")]
    import_sources: Option<Vec<RecipeCatalogSourceResponse>>,
    /// Builder-owned stable identifier or entrypoint.
    identifier: String,
    /// Stable builder adapter selector.
    builder: String,
    /// Normalized parent identifiers.
    parents: Vec<String>,
    /// Capabilities required to execute this recipe.
    required_capabilities: Vec<String>,
}

/// Bounded recipe display metadata observed by a worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct RecipeCatalogGuidanceResponse {
    /// Display name derived from the recipe filename, never its input values.
    name: String,
    /// Observed processing intent across the parent chain.
    purpose: RecipePurposeResponse,
}

/// Observed recipe intent; custom processors may have additional effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecipePurposeResponse {
    /// Fetch a vendor artifact.
    FetchArtifact,
    /// Build or copy a package.
    BuildArtifact,
    /// Install on the worker.
    Install,
    /// Publish to another distribution system.
    Publish,
    /// Unknown intent.
    Unknown,
}

impl From<stabbur_builder_core::RecipePurpose> for RecipePurposeResponse {
    fn from(value: stabbur_builder_core::RecipePurpose) -> Self {
        use stabbur_builder_core::RecipePurpose;
        match value {
            RecipePurpose::FetchArtifact => Self::FetchArtifact,
            RecipePurpose::BuildArtifact => Self::BuildArtifact,
            RecipePurpose::Install => Self::Install,
            RecipePurpose::Publish => Self::Publish,
            RecipePurpose::Unknown => Self::Unknown,
        }
    }
}

impl From<RecipeCatalogEntry> for RecipeCatalogEntryResponse {
    fn from(value: RecipeCatalogEntry) -> Self {
        Self {
            guidance: value.guidance.map(|hint| RecipeCatalogGuidanceResponse {
                name: hint.name().to_owned(),
                purpose: hint.purpose().into(),
            }),
            import_sources: value
                .import_sources
                .map(|sources| sources.as_slice().iter().cloned().map(Into::into).collect()),
            identifier: value.identifier,
            builder: value.builder,
            parents: value.parents,
            required_capabilities: value
                .required_capabilities
                .iter()
                .map(|capability| capability.as_str().to_owned())
                .collect(),
        }
    }
}

/// Severity of one safe catalog diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecipeCatalogDiagnosticSeverityResponse {
    /// Informational source metadata.
    Info,
    /// An entry may require operator attention.
    Warning,
    /// An entry could not be normalized or validated.
    Error,
}

impl From<RecipeCatalogDiagnosticSeverity> for RecipeCatalogDiagnosticSeverityResponse {
    fn from(value: RecipeCatalogDiagnosticSeverity) -> Self {
        match value {
            RecipeCatalogDiagnosticSeverity::Info => Self::Info,
            RecipeCatalogDiagnosticSeverity::Warning => Self::Warning,
            RecipeCatalogDiagnosticSeverity::Error => Self::Error,
        }
    }
}

/// Safe catalog validation diagnostic without worker-local paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct RecipeCatalogDiagnosticResponse {
    /// Recipe identifier when the diagnostic belongs to one entry.
    identifier: Option<String>,
    /// Stable machine-readable code.
    code: String,
    /// Diagnostic severity.
    severity: RecipeCatalogDiagnosticSeverityResponse,
    /// Bounded operator-facing detail.
    detail: String,
}

impl From<RecipeCatalogDiagnostic> for RecipeCatalogDiagnosticResponse {
    fn from(value: RecipeCatalogDiagnostic) -> Self {
        Self {
            identifier: value.identifier,
            code: value.code,
            severity: value.severity.into(),
            detail: value.detail,
        }
    }
}

/// Complete versioned builder-neutral catalog manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct RecipeCatalogManifestResponse {
    /// Catalog protocol schema version.
    schema_version: u32,
    /// Stable producer adapter name.
    producer: String,
    /// Pinned source observation.
    source: RecipeCatalogSourceResponse,
    /// Sorted normalized recipe entries.
    recipes: Vec<RecipeCatalogEntryResponse>,
    /// Sorted safe validation diagnostics.
    diagnostics: Vec<RecipeCatalogDiagnosticResponse>,
}

impl From<RecipeCatalogManifest> for RecipeCatalogManifestResponse {
    fn from(value: RecipeCatalogManifest) -> Self {
        Self {
            schema_version: value.schema_version,
            producer: value.producer,
            source: value.source.into(),
            recipes: value.recipes.into_iter().map(Into::into).collect(),
            diagnostics: value.diagnostics.into_iter().map(Into::into).collect(),
        }
    }
}

/// Immutable catalog snapshot metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct RecipeCatalogSnapshotSummaryResponse {
    /// Snapshot UUIDv7.
    id: String,
    /// Authenticated publishing worker UUIDv7.
    worker_id: String,
    /// Stable producer adapter name.
    producer: String,
    /// Pinned source observation.
    source: RecipeCatalogSourceResponse,
    /// Lowercase SHA-256 digest of the canonical manifest.
    manifest_digest: String,
    /// Number of normalized recipes.
    recipe_count: u32,
    /// Number of safe validation diagnostics.
    diagnostic_count: u32,
    /// Server receipt time.
    observed_at: chrono::DateTime<Utc>,
}

impl From<&RecipeCatalogSnapshotRecord> for RecipeCatalogSnapshotSummaryResponse {
    fn from(value: &RecipeCatalogSnapshotRecord) -> Self {
        Self {
            id: value.id.to_string(),
            worker_id: value.worker_id.to_string(),
            producer: value.manifest.producer.clone(),
            source: value.manifest.source.clone().into(),
            manifest_digest: value.manifest_digest.to_string(),
            recipe_count: u32::try_from(value.manifest.recipes.len())
                .expect("validated recipe count fits u32"),
            diagnostic_count: u32::try_from(value.manifest.diagnostics.len())
                .expect("validated diagnostic count fits u32"),
            observed_at: value.observed_at,
        }
    }
}

impl From<&RecipeCatalogSnapshotSummaryRecord> for RecipeCatalogSnapshotSummaryResponse {
    fn from(value: &RecipeCatalogSnapshotSummaryRecord) -> Self {
        Self {
            id: value.id.to_string(),
            worker_id: value.worker_id.to_string(),
            producer: value.producer.clone(),
            source: RecipeCatalogSourceResponse {
                locator: value.source_locator.clone(),
                revision: value.source_revision.clone(),
            },
            manifest_digest: value.manifest_digest.to_string(),
            recipe_count: value.recipe_count,
            diagnostic_count: value.diagnostic_count,
            observed_at: value.observed_at,
        }
    }
}

/// Immutable catalog snapshot including its complete bounded manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct RecipeCatalogSnapshotResponse {
    /// Snapshot metadata.
    summary: RecipeCatalogSnapshotSummaryResponse,
    /// Complete canonical manifest.
    manifest: RecipeCatalogManifestResponse,
}

impl From<RecipeCatalogSnapshotRecord> for RecipeCatalogSnapshotResponse {
    fn from(value: RecipeCatalogSnapshotRecord) -> Self {
        Self {
            summary: (&value).into(),
            manifest: value.manifest.into(),
        }
    }
}

#[derive(Debug, Serialize)]
struct RecipeCatalogPublicationResponse {
    snapshot: RecipeCatalogSnapshotResponse,
    outcome: RecipeCatalogPublishOutcome,
}

/// Pinned source requested for a durable catalog scan.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RecipeCatalogScanSourceRequest {
    /// Absolute credential-free HTTPS Git repository URL.
    locator: String,
    /// Full lowercase 40-character Git commit hash.
    revision: String,
}

/// Creates one server-requested worker scan.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateRecipeCatalogScanRequest {
    /// Stable producer adapter. v0.0.1 supports `autopkg`.
    producer: String,
    /// Exact immutable source to inspect.
    source: RecipeCatalogScanSourceRequest,
}

/// Safe typed terminal scan failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct RecipeCatalogScanFailureResponse {
    /// Stable producer when the worker decoded the request.
    producer: Option<String>,
    /// Stable machine-readable failure code.
    code: String,
    /// Safe operator-facing detail.
    detail: String,
    /// Worker failure time.
    failed_at: chrono::DateTime<Utc>,
}

impl From<RecipeCatalogScanExecutionFailure> for RecipeCatalogScanFailureResponse {
    fn from(value: RecipeCatalogScanExecutionFailure) -> Self {
        Self {
            producer: value.producer,
            code: value.code,
            detail: value.detail,
            failed_at: value.failed_at,
        }
    }
}

/// Durable server-requested catalog scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct RecipeCatalogScanResponse {
    /// Scan UUIDv7.
    id: String,
    /// Durable worker job UUIDv7.
    job_id: String,
    /// Stable producer adapter selector.
    producer: String,
    /// Exact immutable source requested by the server.
    source: RecipeCatalogSourceResponse,
    /// Durable queue state.
    state: String,
    /// Published snapshot UUIDv7 after successful completion.
    snapshot_id: Option<String>,
    /// Safe typed worker failure after unsuccessful completion.
    failure: Option<RecipeCatalogScanFailureResponse>,
    /// Server request time.
    requested_at: chrono::DateTime<Utc>,
    /// Terminal completion time.
    completed_at: Option<chrono::DateTime<Utc>>,
}

impl From<RecipeCatalogScanRecord> for RecipeCatalogScanResponse {
    fn from(value: RecipeCatalogScanRecord) -> Self {
        Self {
            id: value.id.to_string(),
            job_id: value.job_id.to_string(),
            producer: value.producer,
            source: value.source.into(),
            state: job_state_response(value.state).to_owned(),
            snapshot_id: value.snapshot_id.map(|identity| identity.to_string()),
            failure: value.failure.map(Into::into),
            requested_at: value.requested_at,
            completed_at: value.completed_at,
        }
    }
}

impl From<RecipeCatalogScanSummaryRecord> for RecipeCatalogScanResponse {
    fn from(value: RecipeCatalogScanSummaryRecord) -> Self {
        Self {
            id: value.id.to_string(),
            job_id: value.job_id.to_string(),
            producer: value.producer,
            source: value.source.into(),
            state: job_state_response(value.state).to_owned(),
            snapshot_id: value.snapshot_id.map(|identity| identity.to_string()),
            failure: None,
            requested_at: value.requested_at,
            completed_at: value.completed_at,
        }
    }
}

/// Cursor page of durable catalog scans.
#[derive(Debug, Serialize, ToSchema)]
pub struct RecipeCatalogScanPage {
    /// Current page.
    items: Vec<RecipeCatalogScanResponse>,
    /// Opaque next cursor.
    next_cursor: Option<String>,
}

/// Cursor page of immutable catalog snapshot metadata.
#[derive(Debug, Serialize, ToSchema)]
pub struct RecipeCatalogSnapshotPage {
    /// Current page.
    items: Vec<RecipeCatalogSnapshotSummaryResponse>,
    /// Opaque next cursor.
    next_cursor: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/v1/recipe-catalogs",
    tag = "recipe-catalogs",
    params(PageQuery),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Immutable recipe catalog snapshot page", body = RecipeCatalogSnapshotPage))
)]
#[actix_web::get("/recipe-catalogs")]
pub(crate) async fn list_recipe_catalog_snapshots(
    request: HttpRequest,
    state: web::Data<AppState>,
    query: web::Query<PageQuery>,
) -> Result<web::Json<RecipeCatalogSnapshotPage>, ApiError> {
    authenticate(&request, &state, Permission::RecipeRead).await?;
    let request_id = request_id(&request);
    let limit = query.limit.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return Err(ApiError::validation(
            "The pagination limit is invalid.",
            vec![],
            &request_id,
        ));
    }
    let cursor = decode_cursor(query.cursor.as_deref(), &request_id)?;
    if let Some(cursor) = &cursor {
        cursor.parse::<RecipeCatalogSnapshotId>().map_err(|_| {
            ApiError::validation("The pagination cursor is invalid.", vec![], &request_id)
        })?;
    }
    let snapshots = state
        .storage
        .list_recipe_catalog_snapshots(cursor.as_deref(), limit)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    let next_cursor = (snapshots.len() == limit as usize)
        .then(|| {
            snapshots
                .last()
                .map(|snapshot| URL_SAFE_NO_PAD.encode(snapshot.id.to_string()))
        })
        .flatten();
    Ok(web::Json(RecipeCatalogSnapshotPage {
        items: snapshots.iter().map(Into::into).collect(),
        next_cursor,
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/recipe-catalogs/{snapshot}",
    tag = "recipe-catalogs",
    params(("snapshot" = String, Path, description = "Catalog snapshot UUIDv7")),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Immutable recipe catalog snapshot", body = RecipeCatalogSnapshotResponse),
        (status = 404, description = "Catalog snapshot not found", body = Problem)
    )
)]
#[actix_web::get("/recipe-catalogs/{snapshot}")]
pub(crate) async fn get_recipe_catalog_snapshot(
    request: HttpRequest,
    state: web::Data<AppState>,
    snapshot: web::Path<String>,
) -> Result<web::Json<RecipeCatalogSnapshotResponse>, ApiError> {
    authenticate(&request, &state, Permission::RecipeRead).await?;
    let request_id = request_id(&request);
    let snapshot_id = snapshot
        .parse::<RecipeCatalogSnapshotId>()
        .map_err(|error| ApiError::domain(error, &request_id))?;
    let snapshot = state
        .storage
        .recipe_catalog_snapshot(snapshot_id)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "recipe_catalog_not_found",
                "Recipe catalog snapshot does not exist.",
                &request_id,
            )
        })?;
    Ok(web::Json(snapshot.into()))
}

/// Exact catalog identifier lookup.
#[derive(Debug, Deserialize, IntoParams)]
pub struct RecipeCatalogEntryQuery {
    /// Exact builder-owned recipe identifier or entrypoint.
    identifier: String,
}

/// One exact identifier observed in a latest pinned-source snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct RecipeCatalogMatchResponse {
    /// Containing snapshot UUIDv7.
    snapshot_id: String,
    /// Publishing worker UUIDv7.
    worker_id: String,
    /// Stable producer adapter name.
    producer: String,
    /// Pinned source observation.
    source: RecipeCatalogSourceResponse,
    /// Normalized recipe entry.
    recipe: RecipeCatalogEntryResponse,
    /// Server receipt time.
    observed_at: chrono::DateTime<Utc>,
}

impl From<RecipeCatalogMatch> for RecipeCatalogMatchResponse {
    fn from(value: RecipeCatalogMatch) -> Self {
        Self {
            snapshot_id: value.snapshot_id.to_string(),
            worker_id: value.worker_id.to_string(),
            producer: value.producer,
            source: RecipeCatalogSourceResponse {
                locator: value.source_locator,
                revision: value.source_revision,
            },
            recipe: value.recipe.into(),
            observed_at: value.observed_at,
        }
    }
}

/// Exact catalog lookup result across latest source snapshots.
#[derive(Debug, Serialize, ToSchema)]
pub struct RecipeCatalogLookupResponse {
    /// Exact requested identifier.
    identifier: String,
    /// Whether any latest source snapshot contains the identifier.
    exists: bool,
    /// Matching latest source observations.
    matches: Vec<RecipeCatalogMatchResponse>,
}

#[utoipa::path(
    get,
    path = "/api/v1/recipe-catalog-entries",
    tag = "recipe-catalogs",
    params(RecipeCatalogEntryQuery),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Exact latest-source recipe catalog lookup", body = RecipeCatalogLookupResponse))
)]
#[actix_web::get("/recipe-catalog-entries")]
pub(crate) async fn resolve_recipe_catalog_entry(
    request: HttpRequest,
    state: web::Data<AppState>,
    query: web::Query<RecipeCatalogEntryQuery>,
) -> Result<web::Json<RecipeCatalogLookupResponse>, ApiError> {
    authenticate(&request, &state, Permission::RecipeRead).await?;
    let request_id = request_id(&request);
    let matches = state
        .storage
        .latest_recipe_catalog_matches(&query.identifier)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .into_iter()
        .map(Into::into)
        .collect::<Vec<_>>();
    Ok(web::Json(RecipeCatalogLookupResponse {
        identifier: query.identifier.clone(),
        exists: !matches.is_empty(),
        matches,
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/recipe-catalog-scans",
    tag = "recipe-catalogs",
    request_body = CreateRecipeCatalogScanRequest,
    security(("bearer_auth" = [])),
    responses(
        (status = 201, description = "Catalog scan queued", body = RecipeCatalogScanResponse),
        (status = 200, description = "Idempotently replayed catalog scan", body = RecipeCatalogScanResponse)
    )
)]
#[actix_web::post("/recipe-catalog-scans")]
pub(crate) async fn create_recipe_catalog_scan(
    request: HttpRequest,
    state: web::Data<AppState>,
    body: web::Json<CreateRecipeCatalogScanRequest>,
) -> Result<HttpResponse, ApiError> {
    let principal = authenticate(&request, &state, Permission::RecipeWrite).await?;
    let request_id = request_id(&request);
    let idempotency_key = required_idempotency_key(&request, &request_id)?;
    if body.producer != "autopkg" {
        return Err(ApiError::validation(
            "The catalog producer is unsupported.",
            vec![ValidationError {
                field: "producer".into(),
                code: "unsupported_catalog_producer".into(),
                message: "v0.0.1 supports the `autopkg` producer.".into(),
            }],
            &request_id,
        ));
    }
    let pinned = PinnedSource {
        url: body.source.locator.clone(),
        commit: body.source.revision.clone(),
    };
    pinned.validate().map_err(|error| {
        ApiError::validation(
            "The catalog source must be an exact immutable AutoPkg repository revision.",
            vec![ValidationError {
                field: "source".into(),
                code: "invalid_catalog_source".into(),
                message: error.to_string(),
            }],
            &request_id,
        )
    })?;
    let required_capabilities = capability_set(
        &["builder.autopkg".to_owned(), "os.macos".to_owned()],
        &request_id,
    )?;
    let scan_id = RecipeCatalogScanId::new();
    let job_id = JobId::new();
    let now = Utc::now();
    let source = RecipeCatalogSource {
        locator: pinned.url,
        revision: pinned.commit,
    };
    let payload = serde_json::to_value(RecipeCatalogScanJob::new(
        stabbur_builder_core::RecipeCatalogScanRequest {
            scan_id,
            producer: body.producer.clone(),
            source: source.clone(),
            required_capabilities: required_capabilities.clone(),
        },
    ))
    .map_err(|_| ApiError::internal(&request_id))?;
    let scan = RecipeCatalogScanRecord {
        id: scan_id,
        job_id,
        producer: body.producer.clone(),
        source,
        state: JobState::Queued,
        snapshot_id: None,
        failure: None,
        requested_at: now,
        completed_at: None,
    };
    let job = Job {
        id: job_id,
        subject: JobSubject::RecipeCatalogScan { scan_id },
        required_capabilities,
        payload,
        state: JobState::Queued,
        maximum_attempts: 3,
        attempt_count: 0,
        created_at: now,
    };
    let creation = state
        .storage
        .create_recipe_catalog_scan(
            &scan,
            &job,
            &format!("principal:{}:recipe_catalog_scan.create", principal.id),
            idempotency_key,
            &AuditEvent {
                id: AuditEventId::new(),
                actor: AuditActor::Principal(principal.id),
                action: "recipe_catalog_scan.create".into(),
                resource_kind: "recipe_catalog_scan".into(),
                resource_id: Some(scan_id.to_string()),
                details: serde_json::json!({
                    "producer": body.producer,
                    "source_locator": body.source.locator,
                    "source_revision": body.source.revision,
                }),
                request_id: Some(request_id.clone()),
                occurred_at: now,
            },
        )
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    let status = if creation.outcome == CompletionOutcome::Completed {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok(HttpResponse::build(status)
        .insert_header((
            header::LOCATION,
            format!("/api/v1/recipe-catalog-scans/{}", creation.scan.id),
        ))
        .insert_header(("x-request-id", request_id))
        .json(RecipeCatalogScanResponse::from(creation.scan)))
}

#[utoipa::path(
    get,
    path = "/api/v1/recipe-catalog-scans",
    tag = "recipe-catalogs",
    params(PageQuery),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Durable catalog scan page", body = RecipeCatalogScanPage))
)]
#[actix_web::get("/recipe-catalog-scans")]
pub(crate) async fn list_recipe_catalog_scans(
    request: HttpRequest,
    state: web::Data<AppState>,
    query: web::Query<PageQuery>,
) -> Result<web::Json<RecipeCatalogScanPage>, ApiError> {
    authenticate(&request, &state, Permission::RecipeRead).await?;
    let request_id = request_id(&request);
    let limit = query.limit.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return Err(ApiError::validation(
            "The pagination limit is invalid.",
            vec![],
            &request_id,
        ));
    }
    let cursor = decode_cursor(query.cursor.as_deref(), &request_id)?;
    if let Some(cursor) = &cursor {
        cursor.parse::<RecipeCatalogScanId>().map_err(|_| {
            ApiError::validation("The pagination cursor is invalid.", vec![], &request_id)
        })?;
    }
    let scans = state
        .storage
        .list_recipe_catalog_scans(cursor.as_deref(), limit)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    let next_cursor = (scans.len() == limit as usize)
        .then(|| {
            scans
                .last()
                .map(|scan| URL_SAFE_NO_PAD.encode(scan.id.to_string()))
        })
        .flatten();
    Ok(web::Json(RecipeCatalogScanPage {
        items: scans.into_iter().map(Into::into).collect(),
        next_cursor,
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/recipe-catalog-scans/{scan}",
    tag = "recipe-catalogs",
    params(("scan" = String, Path, description = "Catalog scan UUIDv7")),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Durable catalog scan", body = RecipeCatalogScanResponse),
        (status = 404, description = "Catalog scan not found", body = Problem)
    )
)]
#[actix_web::get("/recipe-catalog-scans/{scan}")]
pub(crate) async fn get_recipe_catalog_scan(
    request: HttpRequest,
    state: web::Data<AppState>,
    scan: web::Path<String>,
) -> Result<web::Json<RecipeCatalogScanResponse>, ApiError> {
    authenticate(&request, &state, Permission::RecipeRead).await?;
    let request_id = request_id(&request);
    let scan_id = scan
        .parse::<RecipeCatalogScanId>()
        .map_err(|error| ApiError::domain(error, &request_id))?;
    let scan = state
        .storage
        .recipe_catalog_scan(scan_id)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "recipe_catalog_scan_not_found",
                "Recipe catalog scan does not exist.",
                &request_id,
            )
        })?;
    Ok(web::Json(scan.into()))
}

#[utoipa::path(
    post,
    path = "/api/v1/recipe-catalog-scans/{scan}/cancel",
    tag = "recipe-catalogs",
    params(("scan" = String, Path, description = "Catalog scan UUIDv7")),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Cancelled catalog scan", body = RecipeCatalogScanResponse),
        (status = 404, description = "Catalog scan not found", body = Problem)
    )
)]
#[actix_web::post("/recipe-catalog-scans/{scan}/cancel")]
pub(crate) async fn cancel_recipe_catalog_scan(
    request: HttpRequest,
    state: web::Data<AppState>,
    scan: web::Path<String>,
) -> Result<web::Json<RecipeCatalogScanResponse>, ApiError> {
    let principal = authenticate(&request, &state, Permission::RecipeWrite).await?;
    let request_id = request_id(&request);
    let idempotency_key = required_idempotency_key(&request, &request_id)?;
    let scan_id = scan
        .parse::<RecipeCatalogScanId>()
        .map_err(|error| ApiError::domain(error, &request_id))?;
    let now = Utc::now();
    let scan = state
        .storage
        .cancel_recipe_catalog_scan(
            scan_id,
            idempotency_key,
            &AuditEvent {
                id: AuditEventId::new(),
                actor: AuditActor::Principal(principal.id),
                action: "recipe_catalog_scan.cancel".into(),
                resource_kind: "recipe_catalog_scan".into(),
                resource_id: Some(scan_id.to_string()),
                details: serde_json::json!({}),
                request_id: Some(request_id.clone()),
                occurred_at: now,
            },
            now,
        )
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(web::Json(scan.into()))
}

fn validated_build_parameters(
    values: &BTreeMap<String, serde_json::Value>,
    request_id: &str,
) -> Result<(serde_json::Value, BTreeMap<String, BuildParameter>), ApiError> {
    if values.keys().any(|key| {
        let key = key.to_ascii_lowercase();
        key.contains("password") || key.contains("token") || key.contains("secret")
    }) {
        return Err(ApiError::validation(
            "Sensitive build parameters are not supported.",
            vec![ValidationError {
                field: "parameters".into(),
                code: "sensitive_input_rejected".into(),
                message: "Use a future external secret provider instead.".into(),
            }],
            request_id,
        ));
    }
    let value = serde_json::to_value(values).map_err(|_| ApiError::internal(request_id))?;
    let typed = serde_json::from_value(value.clone()).map_err(|_| {
        ApiError::validation(
            "Build parameters contain an unsupported value.",
            vec![ValidationError {
                field: "parameters".into(),
                code: "invalid_parameter".into(),
                message: "Use strings, booleans, integers, or lists of those values.".into(),
            }],
            request_id,
        )
    })?;
    Ok((value, typed))
}

fn validate_build_target_name(name: &str, request_id: &str) -> Result<(), ApiError> {
    if name.trim() == name && (1..=128).contains(&name.len()) {
        Ok(())
    } else {
        Err(ApiError::validation(
            "The build target name is invalid.",
            vec![ValidationError {
                field: "name".into(),
                code: "invalid_name".into(),
                message: "Use 1-128 bytes without leading or trailing whitespace.".into(),
            }],
            request_id,
        ))
    }
}

/// Manual or fixed-interval target schedule.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BuildTargetSchedulePayload {
    /// Runs are created only through the explicit trigger endpoint.
    Manual,
    /// The server consumes a durable cursor at this fixed interval.
    Interval {
        /// Seconds between eligible cursors.
        #[schema(minimum = 60, maximum = 31_536_000)]
        every_seconds: u32,
    },
}

impl From<BuildTargetSchedule> for BuildTargetSchedulePayload {
    fn from(value: BuildTargetSchedule) -> Self {
        match value {
            BuildTargetSchedule::Manual => Self::Manual,
            BuildTargetSchedule::Interval { every_seconds } => Self::Interval { every_seconds },
        }
    }
}

fn persisted_target_schedule(
    schedule: BuildTargetSchedulePayload,
    next_run_at: Option<chrono::DateTime<Utc>>,
    default_cursor: chrono::DateTime<Utc>,
    request_id: &str,
) -> Result<(BuildTargetSchedule, Option<chrono::DateTime<Utc>>), ApiError> {
    match schedule {
        BuildTargetSchedulePayload::Manual if next_run_at.is_none() => {
            Ok((BuildTargetSchedule::Manual, None))
        }
        BuildTargetSchedulePayload::Manual => Err(ApiError::validation(
            "Manual targets cannot have a recurring cursor.",
            vec![ValidationError {
                field: "next_run_at".into(),
                code: "not_allowed".into(),
                message: "Omit next_run_at for a manual target.".into(),
            }],
            request_id,
        )),
        BuildTargetSchedulePayload::Interval { every_seconds }
            if (MIN_BUILD_INTERVAL_SECONDS..=MAX_BUILD_INTERVAL_SECONDS)
                .contains(&every_seconds) =>
        {
            Ok((
                BuildTargetSchedule::Interval { every_seconds },
                Some(next_run_at.unwrap_or(default_cursor)),
            ))
        }
        BuildTargetSchedulePayload::Interval { .. } => Err(ApiError::validation(
            "The recurring interval is outside the supported range.",
            vec![ValidationError {
                field: "schedule.every_seconds".into(),
                code: "out_of_range".into(),
                message: format!(
                    "Use {MIN_BUILD_INTERVAL_SECONDS}-{MAX_BUILD_INTERVAL_SECONDS} seconds."
                ),
            }],
            request_id,
        )),
    }
}

/// Build target creation request.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateBuildTargetRequest {
    /// Unique operator-facing target name.
    name: String,
    /// Software UUIDv7 or slug.
    software: String,
    /// Exact immutable recipe revision UUIDv7.
    recipe_revision: String,
    /// Declared non-secret builder parameters.
    #[serde(default)]
    parameters: BTreeMap<String, serde_json::Value>,
    /// Manual or recurring policy.
    schedule: BuildTargetSchedulePayload,
    /// Initial recurring cursor; defaults to creation time for interval targets.
    next_run_at: Option<chrono::DateTime<Utc>>,
    /// Whether triggering is allowed.
    #[serde(default = "default_true")]
    enabled: bool,
}

const fn default_true() -> bool {
    true
}

/// Build target configuration update.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateBuildTargetRequest {
    /// Replacement unique operator-facing name.
    name: Option<String>,
    /// Replacement exact immutable recipe revision UUIDv7.
    recipe_revision: Option<String>,
    /// Complete replacement non-secret builder parameters.
    parameters: Option<BTreeMap<String, serde_json::Value>>,
    /// Replacement manual or recurring policy.
    schedule: Option<BuildTargetSchedulePayload>,
    /// Replacement recurring cursor; requires an interval policy.
    next_run_at: Option<chrono::DateTime<Utc>>,
    /// Enable or disable triggering.
    enabled: Option<bool>,
}

/// Persisted desired build target.
#[derive(Debug, Serialize, ToSchema)]
pub struct BuildTargetResponse {
    /// Stable UUIDv7 target identity.
    id: String,
    /// Unique operator-facing name.
    name: String,
    /// Bound software identity.
    software_id: String,
    /// Exact immutable recipe revision identity.
    recipe_revision_id: String,
    /// Declared non-secret parameters.
    parameters: serde_json::Value,
    /// Manual or recurring trigger policy.
    schedule: BuildTargetSchedulePayload,
    /// Whether triggering is allowed.
    enabled: bool,
    /// Next recurring cursor, absent for manual targets.
    next_run_at: Option<chrono::DateTime<Utc>>,
    /// Creation time.
    created_at: chrono::DateTime<Utc>,
    /// Latest configuration or scheduler-cursor change.
    updated_at: chrono::DateTime<Utc>,
    /// Optimistic-concurrency revision.
    revision: u64,
}

impl From<BuildTargetRecord> for BuildTargetResponse {
    fn from(value: BuildTargetRecord) -> Self {
        Self {
            id: value.id.to_string(),
            name: value.name,
            software_id: value.software_id.to_string(),
            recipe_revision_id: value.recipe_revision_id.to_string(),
            parameters: value.parameters,
            schedule: value.schedule.into(),
            enabled: value.enabled,
            next_run_at: value.next_run_at,
            created_at: value.created_at,
            updated_at: value.updated_at,
            revision: value.revision,
        }
    }
}

/// Cursor page of desired build targets.
#[derive(Debug, Serialize, ToSchema)]
pub struct BuildTargetPage {
    /// Current page.
    items: Vec<BuildTargetResponse>,
    /// Opaque next cursor.
    next_cursor: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/v1/build-targets",
    tag = "build-targets",
    request_body = CreateBuildTargetRequest,
    security(("bearer_auth" = [])),
    responses(
        (status = 201, description = "Build target created", body = BuildTargetResponse),
        (status = 404, description = "Software or recipe revision not found", body = Problem),
        (status = 409, description = "Target name already exists", body = Problem)
    )
)]
#[actix_web::post("/build-targets")]
pub(crate) async fn create_build_target(
    request: HttpRequest,
    state: web::Data<AppState>,
    body: web::Json<CreateBuildTargetRequest>,
) -> Result<HttpResponse, ApiError> {
    let principal = authenticate(&request, &state, Permission::RecipeWrite).await?;
    let request_id = request_id(&request);
    validate_build_target_name(&body.name, &request_id)?;
    let software = state
        .storage
        .software(&body.software)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "software_not_found",
                "Software does not exist.",
                &request_id,
            )
        })?;
    let revision_id = body
        .recipe_revision
        .parse::<RecipeRevisionId>()
        .map_err(|error| ApiError::domain(error, &request_id))?;
    state
        .storage
        .recipe_revision(revision_id)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "recipe_revision_not_found",
                "Recipe revision does not exist.",
                &request_id,
            )
        })?;
    let (parameters, _) = validated_build_parameters(&body.parameters, &request_id)?;
    let now = Utc::now();
    let (schedule, next_run_at) =
        persisted_target_schedule(body.schedule, body.next_run_at, now, &request_id)?;
    let target = BuildTargetRecord {
        id: BuildTargetId::new(),
        name: body.name.clone(),
        software_id: software.id,
        recipe_revision_id: revision_id,
        parameters,
        schedule,
        enabled: body.enabled,
        next_run_at,
        created_at: now,
        updated_at: now,
        revision: 1,
    };
    let target = state
        .storage
        .create_build_target(
            &target,
            &AuditEvent {
                id: AuditEventId::new(),
                actor: AuditActor::Principal(principal.id),
                action: "build_target.create".into(),
                resource_kind: "build_target".into(),
                resource_id: Some(target.id.to_string()),
                details: serde_json::json!({
                    "software_id": target.software_id,
                    "recipe_revision_id": target.recipe_revision_id,
                    "schedule": body.schedule,
                    "enabled": target.enabled,
                }),
                request_id: Some(request_id.clone()),
                occurred_at: now,
            },
        )
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(HttpResponse::Created()
        .insert_header((header::ETAG, revision_etag(target.revision)))
        .json(BuildTargetResponse::from(target)))
}

#[utoipa::path(
    get,
    path = "/api/v1/build-targets",
    tag = "build-targets",
    params(PageQuery),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Build target page", body = BuildTargetPage))
)]
#[actix_web::get("/build-targets")]
pub(crate) async fn list_build_targets(
    request: HttpRequest,
    state: web::Data<AppState>,
    query: web::Query<PageQuery>,
) -> Result<web::Json<BuildTargetPage>, ApiError> {
    authenticate(&request, &state, Permission::RecipeRead).await?;
    let request_id = request_id(&request);
    let limit = query.limit.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return Err(ApiError::validation(
            "The pagination limit is invalid.",
            vec![],
            &request_id,
        ));
    }
    let cursor = query
        .cursor
        .as_deref()
        .map(|cursor| {
            let decoded = URL_SAFE_NO_PAD
                .decode(cursor)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .ok_or_else(|| {
                    ApiError::validation("The pagination cursor is invalid.", vec![], &request_id)
                })?;
            decoded.parse::<BuildTargetId>().map_err(|_| {
                ApiError::validation("The pagination cursor is invalid.", vec![], &request_id)
            })?;
            Ok(decoded)
        })
        .transpose()?;
    let targets = state
        .storage
        .list_build_targets(cursor.as_deref(), limit)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    let next_cursor = (targets.len() == limit as usize)
        .then(|| {
            targets
                .last()
                .map(|target| URL_SAFE_NO_PAD.encode(target.id.to_string()))
        })
        .flatten();
    Ok(web::Json(BuildTargetPage {
        items: targets.into_iter().map(BuildTargetResponse::from).collect(),
        next_cursor,
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/build-targets/{target}",
    tag = "build-targets",
    params(("target" = String, Path, description = "Build target UUIDv7 or exact name")),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Build target", body = BuildTargetResponse),
        (status = 404, description = "Build target not found", body = Problem)
    )
)]
#[actix_web::get("/build-targets/{target}")]
pub(crate) async fn get_build_target(
    request: HttpRequest,
    state: web::Data<AppState>,
    identity: web::Path<String>,
) -> Result<HttpResponse, ApiError> {
    authenticate(&request, &state, Permission::RecipeRead).await?;
    let request_id = request_id(&request);
    let target = state
        .storage
        .build_target(&identity)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "build_target_not_found",
                "Build target does not exist.",
                &request_id,
            )
        })?;
    Ok(HttpResponse::Ok()
        .insert_header((header::ETAG, revision_etag(target.revision)))
        .json(BuildTargetResponse::from(target)))
}

#[utoipa::path(
    patch,
    path = "/api/v1/build-targets/{target}",
    tag = "build-targets",
    params(
        ("target" = String, Path, description = "Build target UUIDv7 or exact name"),
        ("If-Match" = String, Header, description = "Current target ETag")
    ),
    request_body = UpdateBuildTargetRequest,
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Updated build target", body = BuildTargetResponse),
        (status = 412, description = "Stale target revision", body = Problem)
    )
)]
#[actix_web::patch("/build-targets/{target}")]
pub(crate) async fn update_build_target(
    request: HttpRequest,
    state: web::Data<AppState>,
    identity: web::Path<String>,
    body: web::Json<UpdateBuildTargetRequest>,
) -> Result<HttpResponse, ApiError> {
    let principal = authenticate(&request, &state, Permission::RecipeWrite).await?;
    let request_id = request_id(&request);
    let expected_revision = expected_revision(&request, &request_id)?;
    if body.name.is_none()
        && body.recipe_revision.is_none()
        && body.parameters.is_none()
        && body.schedule.is_none()
        && body.next_run_at.is_none()
        && body.enabled.is_none()
    {
        return Err(ApiError::validation(
            "At least one build target field is required.",
            vec![],
            &request_id,
        ));
    }
    let mut target = state
        .storage
        .build_target(&identity)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "build_target_not_found",
                "Build target does not exist.",
                &request_id,
            )
        })?;
    if let Some(name) = &body.name {
        validate_build_target_name(name, &request_id)?;
        target.name.clone_from(name);
    }
    if let Some(revision) = &body.recipe_revision {
        let revision_id = revision
            .parse::<RecipeRevisionId>()
            .map_err(|error| ApiError::domain(error, &request_id))?;
        state
            .storage
            .recipe_revision(revision_id)
            .await
            .map_err(|error| ApiError::storage(error, &request_id))?
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::NOT_FOUND,
                    "recipe_revision_not_found",
                    "Recipe revision does not exist.",
                    &request_id,
                )
            })?;
        target.recipe_revision_id = revision_id;
    }
    if let Some(parameters) = &body.parameters {
        target.parameters = validated_build_parameters(parameters, &request_id)?.0;
    }
    if let Some(schedule) = body.schedule {
        let (schedule, next_run_at) =
            persisted_target_schedule(schedule, body.next_run_at, Utc::now(), &request_id)?;
        target.schedule = schedule;
        target.next_run_at = next_run_at;
    } else if let Some(next_run_at) = body.next_run_at {
        if !matches!(target.schedule, BuildTargetSchedule::Interval { .. }) {
            return Err(ApiError::validation(
                "A recurring cursor requires an interval target.",
                vec![ValidationError {
                    field: "next_run_at".into(),
                    code: "not_allowed".into(),
                    message: "Set an interval schedule before setting its cursor.".into(),
                }],
                &request_id,
            ));
        }
        target.next_run_at = Some(next_run_at);
    }
    if let Some(enabled) = body.enabled {
        target.enabled = enabled;
    }
    let now = Utc::now();
    target.updated_at = now;
    let target = state
        .storage
        .update_build_target(
            &target,
            expected_revision,
            &AuditEvent {
                id: AuditEventId::new(),
                actor: AuditActor::Principal(principal.id),
                action: "build_target.update".into(),
                resource_kind: "build_target".into(),
                resource_id: Some(target.id.to_string()),
                details: serde_json::json!({
                    "name": body.name,
                    "recipe_revision": body.recipe_revision,
                    "parameters_changed": body.parameters.is_some(),
                    "schedule": body.schedule,
                    "next_run_at": body.next_run_at,
                    "enabled": body.enabled,
                }),
                request_id: Some(request_id.clone()),
                occurred_at: now,
            },
        )
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(HttpResponse::Ok()
        .insert_header((header::ETAG, revision_etag(target.revision)))
        .json(BuildTargetResponse::from(target)))
}

#[utoipa::path(
    get,
    path = "/api/v1/build-targets/{target}/runs",
    tag = "build-targets",
    params(
        ("target" = String, Path, description = "Build target UUIDv7 or exact name"),
        PageQuery
    ),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Runs created from this target", body = RunPage),
        (status = 404, description = "Build target not found", body = Problem)
    )
)]
#[actix_web::get("/build-targets/{target}/runs")]
pub(crate) async fn list_build_target_runs(
    request: HttpRequest,
    state: web::Data<AppState>,
    identity: web::Path<String>,
    query: web::Query<PageQuery>,
) -> Result<web::Json<RunPage>, ApiError> {
    authenticate(&request, &state, Permission::RecipeRead).await?;
    let request_id = request_id(&request);
    let target = state
        .storage
        .build_target(&identity)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "build_target_not_found",
                "Build target does not exist.",
                &request_id,
            )
        })?;
    let limit = query.limit.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return Err(ApiError::validation(
            "The pagination limit is invalid.",
            vec![],
            &request_id,
        ));
    }
    let cursor = decode_run_cursor(query.cursor.as_deref(), &request_id)?;
    let runs = state
        .storage
        .list_build_target_runs(target.id, cursor.as_deref(), limit)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    let next_cursor = (runs.len() == limit as usize)
        .then(|| {
            runs.last()
                .map(|run| URL_SAFE_NO_PAD.encode(run.id.to_string()))
        })
        .flatten();
    Ok(web::Json(RunPage {
        items: runs.into_iter().map(RunSummaryResponse::from).collect(),
        next_cursor,
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/build-targets/{target}/runs",
    tag = "build-targets",
    params(
        ("target" = String, Path, description = "Build target UUIDv7 or exact name"),
        ("Idempotency-Key" = String, Header, description = "Stable manual trigger identity")
    ),
    security(("bearer_auth" = [])),
    responses(
        (status = 201, description = "Target run and job queued", body = RunResponse),
        (status = 200, description = "Prior manual trigger replayed", body = RunResponse),
        (status = 409, description = "Target is disabled", body = Problem)
    )
)]
#[actix_web::post("/build-targets/{target}/runs")]
pub(crate) async fn trigger_build_target(
    request: HttpRequest,
    state: web::Data<AppState>,
    identity: web::Path<String>,
) -> Result<HttpResponse, ApiError> {
    let principal = authenticate(&request, &state, Permission::RecipeExecute).await?;
    let request_id = request_id(&request);
    let idempotency_key = required_idempotency_key(&request, &request_id)?;
    let target = state
        .storage
        .build_target(&identity)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "build_target_not_found",
                "Build target does not exist.",
                &request_id,
            )
        })?;
    let revision = state
        .storage
        .recipe_revision(target.recipe_revision_id)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| ApiError::internal(&request_id))?;
    let now = Utc::now();
    let (run, job) = queued_target_build(&target, &revision, now)
        .map_err(|_| ApiError::internal(&request_id))?;
    let creation = state
        .storage
        .create_build_target_run(
            target.id,
            BuildTargetRunTrigger::Manual,
            &run,
            &job,
            idempotency_key,
            &AuditEvent {
                id: AuditEventId::new(),
                actor: AuditActor::Principal(principal.id),
                action: "build_target.run.create".into(),
                resource_kind: "build_target".into(),
                resource_id: Some(target.id.to_string()),
                details: serde_json::json!({"run_id": run.id, "job_id": job.id}),
                request_id: Some(request_id.clone()),
                occurred_at: now,
            },
            now,
        )
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    let mut response = match creation.outcome {
        CompletionOutcome::Completed => HttpResponse::Created(),
        CompletionOutcome::Replayed => HttpResponse::Ok(),
    };
    Ok(response.json(RunResponse::from(creation.run)))
}

/// Queued run creation request.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateRunRequest {
    /// Software UUIDv7 or slug.
    software: String,
    /// Immutable recipe revision UUIDv7.
    recipe_revision: String,
    /// Declared non-secret builder parameters.
    #[serde(default)]
    parameters: BTreeMap<String, serde_json::Value>,
}

/// Durable run response.
#[derive(Debug, Serialize, ToSchema)]
pub struct RunResponse {
    /// UUIDv7 run identity.
    id: String,
    /// Immutable recipe revision identity.
    recipe_revision_id: String,
    /// Software identity.
    software_id: String,
    /// Durable lifecycle state.
    state: String,
    /// Declared non-secret parameters.
    parameters: serde_json::Value,
    /// Terminal typed worker result or failure.
    result: Option<serde_json::Value>,
    /// Creation time.
    created_at: chrono::DateTime<Utc>,
    /// Terminal completion time.
    completed_at: Option<chrono::DateTime<Utc>>,
}

const fn run_state_response(state: RunState) -> &'static str {
    match state {
        RunState::Queued => "queued",
        RunState::Running => "running",
        RunState::Succeeded => "succeeded",
        RunState::Failed => "failed",
        RunState::Cancelled => "cancelled",
    }
}

impl From<RunRecord> for RunResponse {
    fn from(value: RunRecord) -> Self {
        Self {
            id: value.id.to_string(),
            recipe_revision_id: value.recipe_revision_id.to_string(),
            software_id: value.software_id.to_string(),
            state: run_state_response(value.state).to_owned(),
            parameters: value.parameters,
            result: value.result,
            created_at: value.created_at,
            completed_at: value.completed_at,
        }
    }
}

/// Lightweight run representation used by bounded collection responses.
#[derive(Debug, Serialize, ToSchema)]
pub struct RunSummaryResponse {
    /// UUIDv7 run identity.
    id: String,
    /// Immutable recipe revision identity.
    recipe_revision_id: String,
    /// Software identity.
    software_id: String,
    /// Durable lifecycle state.
    state: String,
    /// Creation time.
    created_at: chrono::DateTime<Utc>,
    /// Terminal completion time.
    completed_at: Option<chrono::DateTime<Utc>>,
}

impl From<RunSummaryRecord> for RunSummaryResponse {
    fn from(value: RunSummaryRecord) -> Self {
        Self {
            id: value.id.to_string(),
            recipe_revision_id: value.recipe_revision_id.to_string(),
            software_id: value.software_id.to_string(),
            state: run_state_response(value.state).to_owned(),
            created_at: value.created_at,
            completed_at: value.completed_at,
        }
    }
}

/// Cursor page of durable runs.
#[derive(Debug, Serialize, ToSchema)]
pub struct RunPage {
    /// Current page.
    items: Vec<RunSummaryResponse>,
    /// Opaque next cursor.
    next_cursor: Option<String>,
}

fn decode_run_cursor(cursor: Option<&str>, request_id: &str) -> Result<Option<String>, ApiError> {
    cursor
        .map(|cursor| {
            let decoded = URL_SAFE_NO_PAD
                .decode(cursor)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .ok_or_else(|| {
                    ApiError::validation("The pagination cursor is invalid.", vec![], request_id)
                })?;
            decoded.parse::<RunId>().map_err(|_| {
                ApiError::validation("The pagination cursor is invalid.", vec![], request_id)
            })?;
            Ok(decoded)
        })
        .transpose()
}

async fn run_page(
    state: &AppState,
    recipe_id: Option<RecipeId>,
    query: &PageQuery,
    request_id: &str,
) -> Result<RunPage, ApiError> {
    let limit = query.limit.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return Err(ApiError::validation(
            "The pagination limit is invalid.",
            vec![],
            request_id,
        ));
    }
    let cursor = decode_run_cursor(query.cursor.as_deref(), request_id)?;
    let runs = state
        .storage
        .list_runs(recipe_id, None, cursor.as_deref(), limit)
        .await
        .map_err(|error| ApiError::storage(error, request_id))?;
    let next_cursor = (runs.len() == limit as usize)
        .then(|| {
            runs.last()
                .map(|run| URL_SAFE_NO_PAD.encode(run.id.to_string()))
        })
        .flatten();
    Ok(RunPage {
        items: runs.into_iter().map(RunSummaryResponse::from).collect(),
        next_cursor,
    })
}

#[utoipa::path(
    get,
    path = "/api/v1/runs",
    tag = "runs",
    params(PageQuery),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Run page", body = RunPage))
)]
#[actix_web::get("/runs")]
pub(crate) async fn list_runs(
    request: HttpRequest,
    state: web::Data<AppState>,
    query: web::Query<PageQuery>,
) -> Result<web::Json<RunPage>, ApiError> {
    authenticate(&request, &state, Permission::RecipeRead).await?;
    let request_id = request_id(&request);
    Ok(web::Json(
        run_page(&state, None, &query, &request_id).await?,
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/recipes/{recipe}/runs",
    tag = "runs",
    params(("recipe" = String, Path, description = "Recipe UUIDv7 or exact name"), PageQuery),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Recipe run page", body = RunPage))
)]
#[actix_web::get("/recipes/{recipe}/runs")]
pub(crate) async fn list_recipe_runs(
    request: HttpRequest,
    state: web::Data<AppState>,
    identity: web::Path<String>,
    query: web::Query<PageQuery>,
) -> Result<web::Json<RunPage>, ApiError> {
    authenticate(&request, &state, Permission::RecipeRead).await?;
    let request_id = request_id(&request);
    let recipe = state
        .storage
        .recipe(&identity)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "recipe_not_found",
                "Recipe does not exist.",
                &request_id,
            )
        })?;
    Ok(web::Json(
        run_page(&state, Some(recipe.id), &query, &request_id).await?,
    ))
}

fn required_idempotency_key<'a>(
    request: &'a HttpRequest,
    request_id: &str,
) -> Result<&'a str, ApiError> {
    request
        .headers()
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty() && value.len() <= 255)
        .ok_or_else(|| {
            ApiError::validation(
                "A valid Idempotency-Key header is required.",
                vec![ValidationError {
                    field: "Idempotency-Key".into(),
                    code: "required".into(),
                    message: "Use a stable 1-255 byte key for retries.".into(),
                }],
                request_id,
            )
        })
}

#[utoipa::path(
    post,
    path = "/api/v1/runs",
    tag = "runs",
    request_body = CreateRunRequest,
    security(("bearer_auth" = [])),
    responses(
        (status = 201, description = "Run and compatible job queued", body = RunResponse),
        (status = 200, description = "Prior idempotent run creation replayed", body = RunResponse),
        (status = 404, description = "Software or revision not found", body = Problem)
    ),
    params(("Idempotency-Key" = String, Header, description = "Stable retry identity"))
)]
#[actix_web::post("/runs")]
pub(crate) async fn create_run(
    request: HttpRequest,
    state: web::Data<AppState>,
    body: web::Json<CreateRunRequest>,
) -> Result<HttpResponse, ApiError> {
    let principal = authenticate(&request, &state, Permission::RecipeExecute).await?;
    let request_id = request_id(&request);
    let idempotency_key = required_idempotency_key(&request, &request_id)?;
    let software = state
        .storage
        .software(&body.software)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "software_not_found",
                "Software does not exist.",
                &request_id,
            )
        })?;
    let revision_id = body
        .recipe_revision
        .parse::<RecipeRevisionId>()
        .map_err(|error| ApiError::domain(error, &request_id))?;
    let revision = state
        .storage
        .recipe_revision(revision_id)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "recipe_revision_not_found",
                "Recipe revision does not exist.",
                &request_id,
            )
        })?;
    if body.parameters.keys().any(|key| {
        let key = key.to_ascii_lowercase();
        key.contains("password") || key.contains("token") || key.contains("secret")
    }) {
        return Err(ApiError::validation(
            "Sensitive run parameters are not supported.",
            vec![ValidationError {
                field: "parameters".into(),
                code: "sensitive_input_rejected".into(),
                message: "Use a future external secret provider instead.".into(),
            }],
            &request_id,
        ));
    }
    let parameters_value = serde_json::to_value(&body.parameters).map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "parameter_serialization_failed",
            error.to_string(),
            &request_id,
        )
    })?;
    let parameters: BTreeMap<String, BuildParameter> =
        serde_json::from_value(parameters_value.clone()).map_err(|_| {
            ApiError::validation(
                "Run parameters contain an unsupported value.",
                vec![ValidationError {
                    field: "parameters".into(),
                    code: "invalid_parameter".into(),
                    message: "Use strings, booleans, integers, or lists of those values.".into(),
                }],
                &request_id,
            )
        })?;
    let run_id = RunId::new();
    let now = Utc::now();
    let build_request = BuildRequest {
        run_id,
        software: software.id,
        recipe_revision: revision.id,
        parameters,
        required_capabilities: revision.required_capabilities.clone(),
    };
    let payload = serde_json::to_value(BuilderJob::new(
        build_request,
        &revision.builder,
        revision.definition.clone(),
    ))
    .map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "job_serialization_failed",
            error.to_string(),
            &request_id,
        )
    })?;
    let run = RunRecord {
        id: run_id,
        recipe_revision_id: revision.id,
        software_id: software.id,
        state: RunState::Queued,
        parameters: parameters_value,
        result: None,
        created_at: now,
        completed_at: None,
    };
    let job = Job {
        id: JobId::new(),
        subject: JobSubject::BuildRun { run_id },
        required_capabilities: revision.required_capabilities,
        payload,
        state: JobState::Queued,
        maximum_attempts: 3,
        attempt_count: 0,
        created_at: now,
    };
    let creation = state
        .storage
        .create_run(
            &run,
            &job,
            &format!("principal:{}:run.create", principal.id),
            idempotency_key,
            &AuditEvent {
                id: AuditEventId::new(),
                actor: AuditActor::Principal(principal.id),
                action: "run.create".into(),
                resource_kind: "run".into(),
                resource_id: Some(run_id.to_string()),
                details: serde_json::json!({
                    "job_id": job.id,
                    "software_id": software.id,
                    "recipe_revision_id": revision.id,
                }),
                request_id: Some(request_id.clone()),
                occurred_at: now,
            },
        )
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    let mut response = match creation.outcome {
        CompletionOutcome::Completed => HttpResponse::Created(),
        CompletionOutcome::Replayed => HttpResponse::Ok(),
    };
    Ok(response
        .insert_header(("x-request-id", request_id))
        .json(RunResponse::from(creation.run)))
}

#[utoipa::path(
    post,
    path = "/api/v1/runs/{run}/cancel",
    tag = "runs",
    params(
        ("run" = String, Path, description = "Run UUIDv7"),
        ("Idempotency-Key" = String, Header, description = "Stable retry identity")
    ),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Run cancelled and active lease invalidated", body = RunResponse),
        (status = 404, description = "Run not found", body = Problem),
        (status = 409, description = "Run is already terminal", body = Problem)
    )
)]
#[actix_web::post("/runs/{run}/cancel")]
pub(crate) async fn cancel_run(
    request: HttpRequest,
    state: web::Data<AppState>,
    run: web::Path<String>,
) -> Result<web::Json<RunResponse>, ApiError> {
    let principal = authenticate(&request, &state, Permission::RecipeExecute).await?;
    let request_id = request_id(&request);
    let idempotency_key = required_idempotency_key(&request, &request_id)?;
    let run_id = run
        .parse::<RunId>()
        .map_err(|error| ApiError::domain(error, &request_id))?;
    let now = Utc::now();
    let run = state
        .storage
        .cancel_run(
            run_id,
            idempotency_key,
            &AuditEvent {
                id: AuditEventId::new(),
                actor: AuditActor::Principal(principal.id),
                action: "run.cancel".into(),
                resource_kind: "run".into(),
                resource_id: Some(run_id.to_string()),
                details: serde_json::json!({}),
                request_id: Some(request_id.clone()),
                occurred_at: now,
            },
            now,
        )
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(web::Json(RunResponse::from(run)))
}

#[utoipa::path(
    get,
    path = "/api/v1/runs/{run}",
    tag = "runs",
    params(("run" = String, Path, description = "Run UUIDv7")),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Durable run", body = RunResponse),
        (status = 404, description = "Run not found", body = Problem)
    )
)]
#[actix_web::get("/runs/{run}")]
pub(crate) async fn get_run(
    request: HttpRequest,
    state: web::Data<AppState>,
    run: web::Path<String>,
) -> Result<web::Json<RunResponse>, ApiError> {
    authenticate(&request, &state, Permission::RecipeRead).await?;
    let request_id = request_id(&request);
    let run_id = run
        .parse::<RunId>()
        .map_err(|error| ApiError::domain(error, &request_id))?;
    let run = state
        .storage
        .run(run_id)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "run_not_found",
                "Run does not exist.",
                &request_id,
            )
        })?;
    Ok(web::Json(RunResponse::from(run)))
}

/// Durable builder-neutral job response.
#[derive(Debug, Serialize, ToSchema)]
pub struct JobResponse {
    /// UUIDv7 job identity.
    id: String,
    /// Owning build run identity, absent for non-build work.
    run_id: Option<String>,
    /// Owning recipe catalog scan identity, absent for build work.
    recipe_catalog_scan_id: Option<String>,
    /// Required worker capability names.
    required_capabilities: Vec<String>,
    /// Builder-neutral request envelope.
    payload: serde_json::Value,
    /// Durable scheduling state.
    state: String,
    /// Maximum permitted attempts.
    maximum_attempts: u32,
    /// Attempts already issued.
    attempt_count: u32,
    /// Creation time.
    created_at: chrono::DateTime<Utc>,
}

const fn job_state_response(state: JobState) -> &'static str {
    match state {
        JobState::Queued => "queued",
        JobState::Leased => "leased",
        JobState::Succeeded => "succeeded",
        JobState::Failed => "failed",
        JobState::Cancelled => "cancelled",
    }
}

impl From<Job> for JobResponse {
    fn from(job: Job) -> Self {
        Self {
            id: job.id.to_string(),
            run_id: job.subject.build_run_id().map(|value| value.to_string()),
            recipe_catalog_scan_id: job
                .subject
                .recipe_catalog_scan_id()
                .map(|value| value.to_string()),
            required_capabilities: job
                .required_capabilities
                .iter()
                .map(|capability| capability.as_str().to_owned())
                .collect(),
            payload: job.payload,
            state: job_state_response(job.state).into(),
            maximum_attempts: job.maximum_attempts,
            attempt_count: job.attempt_count,
            created_at: job.created_at,
        }
    }
}

/// Lightweight job representation used by bounded collection responses.
#[derive(Debug, Serialize, ToSchema)]
pub struct JobSummaryResponse {
    /// UUIDv7 job identity.
    id: String,
    /// Owning build run identity, absent for non-build work.
    run_id: Option<String>,
    /// Owning recipe catalog scan identity, absent for build work.
    recipe_catalog_scan_id: Option<String>,
    /// Required worker capability names.
    required_capabilities: Vec<String>,
    /// Durable scheduling state.
    state: String,
    /// Maximum permitted attempts.
    maximum_attempts: u32,
    /// Attempts already issued.
    attempt_count: u32,
    /// Creation time.
    created_at: chrono::DateTime<Utc>,
}

impl From<JobSummaryRecord> for JobSummaryResponse {
    fn from(job: JobSummaryRecord) -> Self {
        Self {
            id: job.id.to_string(),
            run_id: job.subject.build_run_id().map(|value| value.to_string()),
            recipe_catalog_scan_id: job
                .subject
                .recipe_catalog_scan_id()
                .map(|value| value.to_string()),
            required_capabilities: job
                .required_capabilities
                .iter()
                .map(|capability| capability.as_str().to_owned())
                .collect(),
            state: job_state_response(job.state).into(),
            maximum_attempts: job.maximum_attempts,
            attempt_count: job.attempt_count,
            created_at: job.created_at,
        }
    }
}

/// Cursor page of durable jobs.
#[derive(Debug, Serialize, ToSchema)]
pub struct JobPage {
    /// Current page.
    items: Vec<JobSummaryResponse>,
    /// Opaque next cursor.
    next_cursor: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/v1/jobs",
    tag = "jobs",
    params(PageQuery),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Durable job page", body = JobPage))
)]
#[actix_web::get("/jobs")]
pub(crate) async fn list_jobs(
    request: HttpRequest,
    state: web::Data<AppState>,
    query: web::Query<PageQuery>,
) -> Result<web::Json<JobPage>, ApiError> {
    authenticate(&request, &state, Permission::WorkerRead).await?;
    let request_id = request_id(&request);
    let limit = query.limit.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return Err(ApiError::validation(
            "The pagination limit is invalid.",
            vec![],
            &request_id,
        ));
    }
    let cursor = query
        .cursor
        .as_deref()
        .map(|cursor| {
            let decoded = URL_SAFE_NO_PAD
                .decode(cursor)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .ok_or_else(|| {
                    ApiError::validation("The pagination cursor is invalid.", vec![], &request_id)
                })?;
            decoded.parse::<JobId>().map_err(|_| {
                ApiError::validation("The pagination cursor is invalid.", vec![], &request_id)
            })?;
            Ok(decoded)
        })
        .transpose()?;
    let jobs = state
        .storage
        .list_jobs(cursor.as_deref(), limit)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    let next_cursor = (jobs.len() == limit as usize)
        .then(|| {
            jobs.last()
                .map(|job| URL_SAFE_NO_PAD.encode(job.id.to_string()))
        })
        .flatten();
    Ok(web::Json(JobPage {
        items: jobs.into_iter().map(JobSummaryResponse::from).collect(),
        next_cursor,
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/jobs/{job}",
    tag = "jobs",
    params(("job" = String, Path, description = "Job UUIDv7")),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Durable job", body = JobResponse),
        (status = 404, description = "Job not found", body = Problem)
    )
)]
#[actix_web::get("/jobs/{job}")]
pub(crate) async fn get_job(
    request: HttpRequest,
    state: web::Data<AppState>,
    job: web::Path<String>,
) -> Result<web::Json<JobResponse>, ApiError> {
    authenticate(&request, &state, Permission::WorkerRead).await?;
    let request_id = request_id(&request);
    let job_id = job
        .parse::<JobId>()
        .map_err(|error| ApiError::domain(error, &request_id))?;
    let job = state
        .storage
        .job(job_id)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "job_not_found",
                "Job does not exist.",
                &request_id,
            )
        })?;
    Ok(web::Json(JobResponse::from(job)))
}

/// Cursor query for ordered run logs.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RunLogQuery {
    /// Opaque exclusive sequence cursor.
    cursor: Option<String>,
    /// Page size, default 50 and maximum 200.
    #[param(minimum = 1, maximum = 200)]
    limit: Option<u32>,
}

/// Exact-byte ordered run-log entry.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RunLogResponse {
    /// Emitting attempt UUIDv7.
    attempt_id: String,
    /// Monotonic sequence across the run.
    sequence: u64,
    /// `stdout`, `stderr`, or `system`.
    stream: String,
    /// Exact entry bytes encoded with standard base64.
    message_base64: String,
    /// Server receipt time.
    occurred_at: chrono::DateTime<Utc>,
}

impl From<RunLogRecord> for RunLogResponse {
    fn from(value: RunLogRecord) -> Self {
        Self {
            attempt_id: value.attempt_id.to_string(),
            sequence: value.sequence,
            stream: match value.stream {
                RunLogStream::Stdout => "stdout",
                RunLogStream::Stderr => "stderr",
                RunLogStream::System => "system",
            }
            .to_owned(),
            message_base64: STANDARD.encode(value.message),
            occurred_at: value.occurred_at,
        }
    }
}

/// Cursor-paginated ordered run logs.
#[derive(Debug, Serialize, ToSchema)]
pub struct RunLogPage {
    /// Current ordered page.
    items: Vec<RunLogResponse>,
    /// Opaque exclusive cursor for the next page.
    next_cursor: Option<String>,
}

fn decode_run_log_cursor(cursor: Option<&str>, request_id: &str) -> Result<Option<u64>, ApiError> {
    cursor
        .map(|cursor| {
            let decoded = URL_SAFE_NO_PAD
                .decode(cursor)
                .ok()
                .and_then(|value| String::from_utf8(value).ok())
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| {
                    ApiError::validation(
                        "The run log cursor is invalid.",
                        vec![ValidationError {
                            field: "cursor".into(),
                            code: "invalid_cursor".into(),
                            message: "Use the opaque cursor returned by the server.".into(),
                        }],
                        request_id,
                    )
                })?;
            Ok(decoded)
        })
        .transpose()
}

#[utoipa::path(
    get,
    path = "/api/v1/runs/{run}/logs",
    tag = "runs",
    params(
        ("run" = String, Path, description = "Run UUIDv7"),
        RunLogQuery
    ),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Ordered exact-byte run logs", body = RunLogPage),
        (status = 404, description = "Run not found", body = Problem)
    )
)]
#[actix_web::get("/runs/{run}/logs")]
pub(crate) async fn list_run_logs(
    request: HttpRequest,
    state: web::Data<AppState>,
    run: web::Path<String>,
    query: web::Query<RunLogQuery>,
) -> Result<web::Json<RunLogPage>, ApiError> {
    authenticate(&request, &state, Permission::RecipeRead).await?;
    let request_id = request_id(&request);
    let run_id = run
        .parse::<RunId>()
        .map_err(|error| ApiError::domain(error, &request_id))?;
    ensure_run_exists(&state, run_id, &request_id).await?;
    let limit = query.limit.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return Err(ApiError::validation(
            "The pagination limit is invalid.",
            vec![ValidationError {
                field: "limit".into(),
                code: "out_of_range".into(),
                message: "Limit must be between 1 and 200.".into(),
            }],
            &request_id,
        ));
    }
    let cursor = decode_run_log_cursor(query.cursor.as_deref(), &request_id)?;
    let logs = state
        .storage
        .run_logs(run_id, cursor, limit)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    let next_cursor = (logs.len() == limit as usize)
        .then(|| {
            logs.last()
                .map(|log| URL_SAFE_NO_PAD.encode(log.sequence.to_string()))
        })
        .flatten();
    Ok(web::Json(RunLogPage {
        items: logs.into_iter().map(Into::into).collect(),
        next_cursor,
    }))
}

async fn ensure_run_exists(
    state: &AppState,
    run_id: RunId,
    request_id: &str,
) -> Result<RunRecord, ApiError> {
    state
        .storage
        .run(run_id)
        .await
        .map_err(|error| ApiError::storage(error, request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "run_not_found",
                "Run does not exist.",
                request_id,
            )
        })
}

struct RunEventState {
    storage: Arc<dyn Storage>,
    run_id: RunId,
    cursor: Option<u64>,
    pending: VecDeque<Bytes>,
    finished: bool,
}

#[utoipa::path(
    get,
    path = "/api/v1/runs/{run}/events",
    tag = "runs",
    params(
        ("run" = String, Path, description = "Run UUIDv7"),
        ("cursor" = Option<String>, Query, description = "Opaque exclusive log cursor"),
        ("Last-Event-ID" = Option<u64>, Header, description = "Numeric last delivered log sequence")
    ),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Replay followed by live run events", content_type = "text/event-stream"),
        (status = 404, description = "Run not found", body = Problem)
    )
)]
#[actix_web::get("/runs/{run}/events")]
pub(crate) async fn stream_run_events(
    request: HttpRequest,
    state: web::Data<AppState>,
    run: web::Path<String>,
    query: web::Query<RunLogQuery>,
) -> Result<HttpResponse, ApiError> {
    authenticate(&request, &state, Permission::RecipeRead).await?;
    let request_id = request_id(&request);
    let run_id = run
        .parse::<RunId>()
        .map_err(|error| ApiError::domain(error, &request_id))?;
    ensure_run_exists(&state, run_id, &request_id).await?;
    let header_cursor = request
        .headers()
        .get("last-event-id")
        .and_then(|value| value.to_str().ok());
    let cursor =
        if let Some(value) = header_cursor {
            Some(value.parse::<u64>().map_err(|_| {
                ApiError::validation("Last-Event-ID is invalid.", vec![], &request_id)
            })?)
        } else {
            decode_run_log_cursor(query.cursor.as_deref(), &request_id)?
        };
    let events = stream::unfold(
        RunEventState {
            storage: state.storage.clone(),
            run_id,
            cursor,
            pending: VecDeque::new(),
            finished: false,
        },
        next_run_event,
    );
    Ok(HttpResponse::Ok()
        .insert_header((header::CONTENT_TYPE, "text/event-stream"))
        .insert_header((header::CACHE_CONTROL, "no-cache"))
        .insert_header(("x-accel-buffering", "no"))
        .streaming(events))
}

async fn next_run_event(
    mut state: RunEventState,
) -> Option<(Result<Bytes, actix_web::Error>, RunEventState)> {
    if let Some(event) = state.pending.pop_front() {
        return Some((Ok(event), state));
    }
    if state.finished {
        return None;
    }
    match state.storage.run_logs(state.run_id, state.cursor, 32).await {
        Ok(logs) if !logs.is_empty() => {
            for log in logs {
                state.cursor = Some(log.sequence);
                let response = RunLogResponse::from(log);
                let data = serde_json::to_string(&response).expect("run log response serializes");
                state.pending.push_back(Bytes::from(format!(
                    "id: {}\nevent: log\ndata: {data}\n\n",
                    response.sequence
                )));
            }
            state.pending.pop_front().map(|event| (Ok(event), state))
        }
        Ok(_) => match state.storage.run(state.run_id).await {
            Ok(Some(run))
                if matches!(
                    run.state,
                    RunState::Succeeded | RunState::Failed | RunState::Cancelled
                ) =>
            {
                state.finished = true;
                let data = serde_json::json!({
                    "run_id": run.id,
                    "state": run_state_response(run.state),
                });
                Some((
                    Ok(Bytes::from(format!("event: complete\ndata: {data}\n\n"))),
                    state,
                ))
            }
            Ok(Some(_)) => {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                Some((Ok(Bytes::from_static(b": keep-alive\n\n")), state))
            }
            Ok(None) => None,
            Err(_) => {
                state.finished = true;
                Some((
                    Err(actix_web::error::ErrorInternalServerError(
                        "run event persistence failure",
                    )),
                    state,
                ))
            }
        },
        Err(_) => {
            state.finished = true;
            Some((
                Err(actix_web::error::ErrorInternalServerError(
                    "run event persistence failure",
                )),
                state,
            ))
        }
    }
}

/// Software creation request.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateSoftwareRequest {
    /// Unique lowercase URL slug.
    slug: String,
    /// Human-readable name.
    name: String,
    /// Optional declarative installation and detection metadata.
    installation: Option<InstallationMetadata>,
}

/// Declarative, platform-neutral installation and detection metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct InstallationMetadata {
    /// Installation metadata interpreted by a client or packager.
    install: serde_json::Value,
    /// Installed-state detection metadata interpreted by a client or packager.
    detection: serde_json::Value,
}

/// Mutable software fields; exactly one field changes per request.
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateSoftwareRequest {
    /// Replacement human-readable display name.
    name: Option<String>,
    /// Complete replacement installation and detection metadata.
    installation: Option<InstallationMetadata>,
}

/// Software representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SoftwareResponse {
    /// UUIDv7 identity.
    id: String,
    /// Unique lowercase URL slug.
    slug: String,
    /// Human-readable name.
    name: String,
    /// Creation time.
    created_at: chrono::DateTime<Utc>,
    /// Optimistic-concurrency revision.
    revision: u64,
    /// Declarative installation and installed-state detection metadata.
    installation: Option<InstallationMetadata>,
}

impl From<Software> for SoftwareResponse {
    fn from(value: Software) -> Self {
        Self {
            id: value.id.to_string(),
            slug: value.slug.to_string(),
            name: value.name,
            created_at: value.created_at,
            revision: value.revision,
            installation: None,
        }
    }
}

fn software_response(
    software: Software,
    installation: Option<SoftwareInstallation>,
) -> SoftwareResponse {
    let mut response = SoftwareResponse::from(software);
    response.installation = installation.map(|installation| InstallationMetadata {
        install: installation.install,
        detection: installation.detection,
    });
    response
}

fn validate_installation_metadata(
    software_id: SoftwareId,
    value: &InstallationMetadata,
    request_id: &str,
) -> Result<SoftwareInstallation, ApiError> {
    fn contains_secret_key(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::Object(values) => values.iter().any(|(key, value)| {
                let key = key.to_ascii_lowercase();
                key.contains("password")
                    || key.contains("token")
                    || key.contains("secret")
                    || key.contains("credential")
                    || contains_secret_key(value)
            }),
            serde_json::Value::Array(values) => values.iter().any(contains_secret_key),
            _ => false,
        }
    }

    let encoded = serde_json::to_vec(value).map_err(|_| ApiError::internal(request_id))?;
    if !value.install.is_object()
        || !value.detection.is_object()
        || encoded.len() > 256 * 1024
        || contains_secret_key(&value.install)
        || contains_secret_key(&value.detection)
    {
        return Err(ApiError::validation(
            "Installation metadata is invalid or contains secret-like fields.",
            vec![ValidationError {
                field: "installation".into(),
                code: "invalid_metadata".into(),
                message: "Use two JSON objects totaling at most 256 KiB without credential fields."
                    .into(),
            }],
            request_id,
        ));
    }
    Ok(SoftwareInstallation {
        software_id,
        install: value.install.clone(),
        detection: value.detection.clone(),
    })
}

/// Cursor-paginated software collection.
#[derive(Debug, Serialize, ToSchema)]
pub struct SoftwarePage {
    /// Current page of resources.
    items: Vec<SoftwareResponse>,
    /// Opaque cursor for the next page.
    next_cursor: Option<String>,
}

/// Common cursor-page query.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PageQuery {
    /// Opaque cursor from a previous response.
    cursor: Option<String>,
    /// Page size, default 50 and maximum 200.
    #[param(minimum = 1, maximum = 200)]
    limit: Option<u32>,
}

/// Append-only audit event response.
#[derive(Debug, Serialize, ToSchema)]
pub struct AuditEventResponse {
    /// UUIDv7 event identity.
    id: String,
    /// Authenticated, bootstrap, or local break-glass actor.
    actor: serde_json::Value,
    /// Stable operation name.
    action: String,
    /// Stable resource kind.
    resource_kind: String,
    /// Optional domain resource identity.
    resource_id: Option<String>,
    /// Safe operation details.
    details: serde_json::Value,
    /// Request correlation identity.
    request_id: Option<String>,
    /// Event time.
    occurred_at: chrono::DateTime<Utc>,
}

/// Cursor-paginated audit collection.
#[derive(Debug, Serialize, ToSchema)]
pub struct AuditPage {
    /// Current page of append-only events.
    items: Vec<AuditEventResponse>,
    /// Opaque cursor for the next page.
    next_cursor: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/v1/audit",
    tag = "audit",
    params(PageQuery),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Append-only audit page", body = AuditPage),
        (status = 400, description = "Invalid cursor or limit", body = Problem),
        (status = 403, description = "Audit permission required", body = Problem)
    )
)]
#[actix_web::get("/audit")]
pub(crate) async fn list_audit_events(
    request: HttpRequest,
    state: web::Data<AppState>,
    query: web::Query<PageQuery>,
) -> Result<web::Json<AuditPage>, ApiError> {
    authenticate(&request, &state, Permission::AuditRead).await?;
    let request_id = request_id(&request);
    let limit = query.limit.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return Err(ApiError::validation(
            "The pagination limit is invalid.",
            vec![ValidationError {
                field: "limit".into(),
                code: "out_of_range".into(),
                message: "Limit must be between 1 and 200.".into(),
            }],
            &request_id,
        ));
    }
    let cursor = query
        .cursor
        .as_deref()
        .map(|cursor| {
            let decoded = URL_SAFE_NO_PAD
                .decode(cursor)
                .ok()
                .and_then(|decoded| String::from_utf8(decoded).ok())
                .ok_or_else(|| {
                    ApiError::validation("The pagination cursor is invalid.", vec![], &request_id)
                })?;
            decoded
                .parse::<stabbur_domain::AuditEventId>()
                .map_err(|_| {
                    ApiError::validation("The pagination cursor is invalid.", vec![], &request_id)
                })?;
            Ok(decoded)
        })
        .transpose()?;
    let events = state
        .storage
        .audit_events(cursor.as_deref(), limit)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    let next_cursor = (events.len() == limit as usize)
        .then(|| {
            events
                .last()
                .map(|event| URL_SAFE_NO_PAD.encode(event.id.to_string()))
        })
        .flatten();
    let items = events
        .into_iter()
        .map(|event| {
            let actor =
                serde_json::to_value(event.actor).map_err(|_| ApiError::internal(&request_id))?;
            Ok(AuditEventResponse {
                id: event.id.to_string(),
                actor,
                action: event.action,
                resource_kind: event.resource_kind,
                resource_id: event.resource_id,
                details: event.details,
                request_id: event.request_id,
                occurred_at: event.occurred_at,
            })
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    Ok(web::Json(AuditPage { items, next_cursor }))
}

fn decode_cursor(cursor: Option<&str>, request_id: &str) -> Result<Option<String>, ApiError> {
    cursor
        .map(|cursor| {
            let decoded = URL_SAFE_NO_PAD.decode(cursor).map_err(|_| {
                ApiError::validation(
                    "The pagination cursor is invalid.",
                    vec![ValidationError {
                        field: "cursor".into(),
                        code: "invalid_cursor".into(),
                        message: "Cursor is not valid opaque cursor data.".into(),
                    }],
                    request_id,
                )
            })?;
            let value = String::from_utf8(decoded).map_err(|_| {
                ApiError::validation("The pagination cursor is invalid.", vec![], request_id)
            })?;
            value.parse::<SoftwareId>().map_err(|_| {
                ApiError::validation("The pagination cursor is invalid.", vec![], request_id)
            })?;
            Ok(value)
        })
        .transpose()
}

#[utoipa::path(
    get,
    path = "/api/v1/software",
    tag = "software",
    params(PageQuery),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Cursor page", body = SoftwarePage),
        (status = 400, description = "Invalid cursor or limit", body = Problem),
        (status = 401, description = "Authentication required", body = Problem)
    )
)]
#[actix_web::get("/software")]
pub(crate) async fn list_software(
    request: HttpRequest,
    state: web::Data<AppState>,
    query: web::Query<PageQuery>,
) -> Result<web::Json<SoftwarePage>, ApiError> {
    authenticate(&request, &state, Permission::SoftwareRead).await?;
    let request_id = request_id(&request);
    let limit = query.limit.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return Err(ApiError::validation(
            "The pagination limit is invalid.",
            vec![ValidationError {
                field: "limit".into(),
                code: "out_of_range".into(),
                message: "Limit must be between 1 and 200.".into(),
            }],
            request_id,
        ));
    }
    let cursor = decode_cursor(query.cursor.as_deref(), &request_id)?;
    let software = state
        .storage
        .list_software(cursor.as_deref(), limit)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    let next_cursor = (software.len() == limit as usize)
        .then(|| {
            software
                .last()
                .map(|item| URL_SAFE_NO_PAD.encode(item.id.to_string()))
        })
        .flatten();
    Ok(web::Json(SoftwarePage {
        items: software.into_iter().map(Into::into).collect(),
        next_cursor,
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/software",
    tag = "software",
    request_body = CreateSoftwareRequest,
    security(("bearer_auth" = [])),
    responses(
        (status = 201, description = "Software created", body = SoftwareResponse),
        (status = 400, description = "Invalid software", body = Problem),
        (status = 409, description = "Slug already exists", body = Problem)
    )
)]
#[actix_web::post("/software")]
pub(crate) async fn create_software(
    request: HttpRequest,
    state: web::Data<AppState>,
    body: web::Json<CreateSoftwareRequest>,
) -> Result<HttpResponse, ApiError> {
    let principal = authenticate(&request, &state, Permission::SoftwareWrite).await?;
    let request_id = request_id(&request);
    let slug = SoftwareSlug::new(&body.slug)
        .map_err(|error| ApiError::domain(error, &request_id).with_field("slug"))?;
    if body.name.trim() != body.name || !(1..=255).contains(&body.name.len()) {
        return Err(ApiError::validation(
            "The software name is invalid.",
            vec![ValidationError {
                field: "name".into(),
                code: "invalid_length".into(),
                message: "Name must contain 1-255 non-padding bytes.".into(),
            }],
            request_id,
        ));
    }
    let software = Software {
        id: SoftwareId::new(),
        slug,
        name: body.name.clone(),
        created_at: Utc::now(),
        revision: 1,
    };
    let installation = body
        .installation
        .as_ref()
        .map(|value| validate_installation_metadata(software.id, value, &request_id))
        .transpose()?;
    let mut transaction = state
        .storage
        .begin()
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    transaction
        .create_software(&software)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    if let Some(installation) = &installation {
        transaction
            .set_software_installation(installation)
            .await
            .map_err(|error| ApiError::storage(error, &request_id))?;
    }
    transaction
        .append_audit(&AuditEvent {
            id: AuditEventId::new(),
            actor: AuditActor::Principal(principal.id),
            action: "software.create".into(),
            resource_kind: "software".into(),
            resource_id: Some(software.id.to_string()),
            details: serde_json::json!({"slug": software.slug.as_str()}),
            request_id: Some(request_id.clone()),
            occurred_at: Utc::now(),
        })
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    transaction
        .commit()
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(HttpResponse::Created()
        .insert_header((header::ETAG, revision_etag(software.revision)))
        .insert_header(("x-request-id", request_id))
        .json(software_response(software, installation)))
}

#[utoipa::path(
    get,
    path = "/api/v1/software/{software}",
    tag = "software",
    params(("software" = String, Path, description = "UUIDv7 identity or lowercase slug")),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Software", body = SoftwareResponse),
        (status = 404, description = "Software not found", body = Problem)
    )
)]
#[actix_web::get("/software/{software}")]
pub(crate) async fn get_software(
    request: HttpRequest,
    state: web::Data<AppState>,
    identity: web::Path<String>,
) -> Result<HttpResponse, ApiError> {
    authenticate(&request, &state, Permission::SoftwareRead).await?;
    let request_id = request_id(&request);
    let software = state
        .storage
        .software(&identity)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "software_not_found",
                "Software does not exist.",
                &request_id,
            )
        })?;
    let installation = state
        .storage
        .software_installation(software.id)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(HttpResponse::Ok()
        .insert_header((header::ETAG, revision_etag(software.revision)))
        .insert_header(("x-request-id", request_id))
        .json(software_response(software, installation)))
}

#[utoipa::path(
    patch,
    path = "/api/v1/software/{software}",
    tag = "software",
    params(
        ("software" = String, Path, description = "UUIDv7 identity or lowercase slug"),
        ("If-Match" = String, Header, description = "Current ETag")
    ),
    request_body = UpdateSoftwareRequest,
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Updated software", body = SoftwareResponse),
        (status = 412, description = "Stale software revision", body = Problem)
    )
)]
#[actix_web::patch("/software/{software}")]
pub(crate) async fn update_software(
    request: HttpRequest,
    state: web::Data<AppState>,
    identity: web::Path<String>,
    body: web::Json<UpdateSoftwareRequest>,
) -> Result<HttpResponse, ApiError> {
    let actor = authenticate(&request, &state, Permission::SoftwareWrite).await?;
    let request_id = request_id(&request);
    let expected_revision = expected_revision(&request, &request_id)?;
    if body.name.is_none() == body.installation.is_none() {
        return Err(ApiError::validation(
            "Change exactly one of name or installation.",
            vec![],
            &request_id,
        ));
    }
    let current = state
        .storage
        .software(&identity)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "software_not_found",
                "Software does not exist.",
                &request_id,
            )
        })?;
    if body
        .name
        .as_deref()
        .is_some_and(|name| name.trim() != name || !(1..=255).contains(&name.len()))
    {
        return Err(ApiError::validation(
            "The software name is invalid.",
            vec![],
            &request_id,
        ));
    }
    let installation = body
        .installation
        .as_ref()
        .map(|value| validate_installation_metadata(current.id, value, &request_id))
        .transpose()?;
    let now = Utc::now();
    let software = state
        .storage
        .update_software(
            current.id,
            body.name.as_deref(),
            installation.as_ref(),
            expected_revision,
            &AuditEvent {
                id: AuditEventId::new(),
                actor: AuditActor::Principal(actor.id),
                action: "software.update".into(),
                resource_kind: "software".into(),
                resource_id: Some(current.id.to_string()),
                details: serde_json::json!({
                    "name_changed": body.name.is_some(),
                    "installation_changed": body.installation.is_some(),
                }),
                request_id: Some(request_id.clone()),
                occurred_at: now,
            },
        )
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    let installation = if let Some(installation) = installation {
        Some(installation)
    } else {
        state
            .storage
            .software_installation(software.id)
            .await
            .map_err(|error| ApiError::storage(error, &request_id))?
    };
    Ok(HttpResponse::Ok()
        .insert_header((header::ETAG, revision_etag(software.revision)))
        .json(software_response(software, installation)))
}

fn revision_etag(revision: u64) -> String {
    format!("\"rev-{revision}\"")
}

fn expected_revision(request: &HttpRequest, request_id: &str) -> Result<u64, ApiError> {
    let value = request
        .headers()
        .get(header::IF_MATCH)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::PRECONDITION_REQUIRED,
                "if_match_required",
                "Supply the current strong ETag in If-Match; use \"rev-0\" to create a channel.",
                request_id,
            )
        })?;
    value
        .strip_prefix("\"rev-")
        .and_then(|value| value.strip_suffix('"'))
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| {
            ApiError::validation(
                "The If-Match header is invalid.",
                vec![ValidationError {
                    field: "If-Match".into(),
                    code: "invalid_etag".into(),
                    message: "Use a strong revision ETag returned by the API.".into(),
                }],
                request_id,
            )
        })
}

fn release_state_response(state: stabbur_domain::ReleaseState) -> &'static str {
    match state {
        stabbur_domain::ReleaseState::Discovered => "discovered",
        stabbur_domain::ReleaseState::Built => "built",
        stabbur_domain::ReleaseState::Inspected => "inspected",
        stabbur_domain::ReleaseState::Verified => "verified",
        stabbur_domain::ReleaseState::Candidate => "candidate",
        stabbur_domain::ReleaseState::Testing => "testing",
        stabbur_domain::ReleaseState::Stable => "stable",
        stabbur_domain::ReleaseState::Failed => "failed",
        stabbur_domain::ReleaseState::Rejected => "rejected",
    }
}

/// One immutable release with its highest achieved lifecycle state.
#[derive(Debug, Serialize, ToSchema)]
pub struct ReleaseResponse {
    /// Publication eligibility independent of attained lifecycle.
    availability: ReleaseAvailabilityResponse,
    /// UUIDv7 release identity.
    id: String,
    /// Parent software identity.
    software_id: String,
    /// Exact opaque upstream version.
    version: String,
    /// Highest achieved lifecycle state.
    state: String,
    /// Creation time.
    created_at: chrono::DateTime<Utc>,
    /// Optimistic-concurrency revision.
    revision: u64,
}

/// Publication eligibility of an immutable release.
#[derive(Debug, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReleaseAvailabilityResponse {
    /// The release may be published if its verification/lifecycle permits it.
    Available,
    /// The release was withdrawn and cannot be promoted or resolved.
    Withdrawn {
        /// Operator explanation.
        reason: String,
        /// Withdrawal timestamp.
        at: chrono::DateTime<Utc>,
    },
}
impl From<stabbur_domain::ReleaseAvailability> for ReleaseAvailabilityResponse {
    fn from(value: stabbur_domain::ReleaseAvailability) -> Self {
        match value {
            stabbur_domain::ReleaseAvailability::Available => Self::Available,
            stabbur_domain::ReleaseAvailability::Withdrawn { reason, at } => Self::Withdrawn {
                reason: reason.as_str().to_owned(),
                at,
            },
        }
    }
}

impl From<Release> for ReleaseResponse {
    fn from(value: Release) -> Self {
        Self {
            availability: value.availability.into(),
            id: value.id.to_string(),
            software_id: value.software_id.to_string(),
            version: value.version.to_string(),
            state: release_state_response(value.state).into(),
            created_at: value.created_at,
            revision: value.revision,
        }
    }
}

/// Cursor-paginated releases for one software item.
#[derive(Debug, Serialize, ToSchema)]
pub struct ReleasePage {
    /// Current page.
    items: Vec<ReleaseResponse>,
    /// Opaque cursor for the next page.
    next_cursor: Option<String>,
}

fn decode_release_cursor(
    cursor: Option<&str>,
    request_id: &str,
) -> Result<Option<String>, ApiError> {
    cursor
        .map(|cursor| {
            let decoded = URL_SAFE_NO_PAD
                .decode(cursor)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .ok_or_else(|| {
                    ApiError::validation("The pagination cursor is invalid.", vec![], request_id)
                })?;
            decoded.parse::<ReleaseId>().map_err(|_| {
                ApiError::validation("The pagination cursor is invalid.", vec![], request_id)
            })?;
            Ok(decoded)
        })
        .transpose()
}

#[utoipa::path(
    get,
    path = "/api/v1/software/{software}/releases",
    tag = "releases",
    params(("software" = String, Path, description = "Software UUIDv7 or slug"), PageQuery),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Release page", body = ReleasePage),
        (status = 404, description = "Software not found", body = Problem)
    )
)]
#[actix_web::get("/software/{software}/releases")]
pub(crate) async fn list_releases(
    request: HttpRequest,
    state: web::Data<AppState>,
    identity: web::Path<String>,
    query: web::Query<PageQuery>,
) -> Result<web::Json<ReleasePage>, ApiError> {
    authenticate(&request, &state, Permission::SoftwareRead).await?;
    let request_id = request_id(&request);
    let software = state
        .storage
        .software(&identity)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "software_not_found",
                "Software does not exist.",
                &request_id,
            )
        })?;
    let limit = query.limit.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return Err(ApiError::validation(
            "The pagination limit is invalid.",
            vec![],
            &request_id,
        ));
    }
    let cursor = decode_release_cursor(query.cursor.as_deref(), &request_id)?;
    let releases = state
        .storage
        .list_releases(software.id, cursor.as_deref(), limit)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    let next_cursor = (releases.len() == limit as usize)
        .then(|| {
            releases
                .last()
                .map(|release| URL_SAFE_NO_PAD.encode(release.id.to_string()))
        })
        .flatten();
    Ok(web::Json(ReleasePage {
        items: releases.into_iter().map(ReleaseResponse::from).collect(),
        next_cursor,
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/releases/{release}",
    tag = "releases",
    params(("release" = String, Path, description = "Release UUIDv7")),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Release", body = ReleaseResponse),
        (status = 404, description = "Release not found", body = Problem)
    )
)]
#[actix_web::get("/releases/{release}")]
pub(crate) async fn get_release(
    request: HttpRequest,
    state: web::Data<AppState>,
    release: web::Path<String>,
) -> Result<HttpResponse, ApiError> {
    authenticate(&request, &state, Permission::SoftwareRead).await?;
    let request_id = request_id(&request);
    let release_id = release
        .parse::<ReleaseId>()
        .map_err(|error| ApiError::domain(error, &request_id))?;
    let release = state
        .storage
        .release(release_id)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "release_not_found",
                "Release does not exist.",
                &request_id,
            )
        })?;
    Ok(HttpResponse::Ok()
        .insert_header((header::ETAG, revision_etag(release.revision)))
        .json(ReleaseResponse::from(release)))
}

fn platform_response(platform: Platform) -> &'static str {
    match platform {
        Platform::MacOs => "mac_os",
        Platform::Linux => "linux",
        Platform::Windows => "windows",
    }
}

fn architecture_response(architecture: Architecture) -> &'static str {
    match architecture {
        Architecture::X86_64 => "x86_64",
        Architecture::Aarch64 => "aarch64",
        Architecture::Universal => "universal",
    }
}

/// An artifact and its role within a variant.
#[derive(Debug, Serialize, ToSchema)]
pub struct VariantArtifactResponse {
    /// SHA-256 content identity.
    digest: String,
    /// Exact byte size.
    size: u64,
    /// Media type.
    media_type: String,
    /// Semantic artifact role.
    role: String,
    /// Whether content can currently be served.
    readable: bool,
}

fn artifact_role_response(role: ArtifactRole) -> &'static str {
    match role {
        ArtifactRole::PrimaryInstaller => "primary_installer",
        ArtifactRole::Signature => "signature",
        ArtifactRole::Sbom => "sbom",
        ArtifactRole::DebugSymbols => "debug_symbols",
        ArtifactRole::Metadata => "metadata",
    }
}

impl From<VariantArtifactRecord> for VariantArtifactResponse {
    fn from(value: VariantArtifactRecord) -> Self {
        Self {
            digest: value.artifact.digest.to_string(),
            size: value.artifact.size,
            media_type: value.artifact.media_type,
            role: artifact_role_response(value.role).into(),
            readable: value.readable,
        }
    }
}

/// One compatibility-specific release variant.
#[derive(Debug, Serialize, ToSchema)]
pub struct VariantResponse {
    /// UUIDv7 variant identity.
    id: String,
    /// Parent release identity.
    release_id: String,
    /// Target platform.
    platform: String,
    /// Target architecture.
    architecture: String,
    /// Inclusive minimum macOS version.
    minimum_macos: Option<String>,
    /// Inclusive maximum macOS version.
    maximum_macos: Option<String>,
    /// Explicit resolver priority.
    resolution_priority: i32,
    /// Attached immutable artifacts.
    artifacts: Vec<VariantArtifactResponse>,
}

fn variant_response(variant: Variant, artifacts: Vec<VariantArtifactRecord>) -> VariantResponse {
    VariantResponse {
        id: variant.id.to_string(),
        release_id: variant.release_id.to_string(),
        platform: platform_response(variant.compatibility.platform).into(),
        architecture: architecture_response(variant.compatibility.architecture).into(),
        minimum_macos: variant
            .compatibility
            .minimum_macos
            .map(|value| value.to_string()),
        maximum_macos: variant
            .compatibility
            .maximum_macos
            .map(|value| value.to_string()),
        resolution_priority: variant.resolution_priority,
        artifacts: artifacts
            .into_iter()
            .map(VariantArtifactResponse::from)
            .collect(),
    }
}

/// Release variant collection.
#[derive(Debug, Serialize, ToSchema)]
pub struct VariantList {
    /// All variants for the release.
    items: Vec<VariantResponse>,
}

#[utoipa::path(
    get,
    path = "/api/v1/releases/{release}/variants",
    tag = "variants",
    params(("release" = String, Path, description = "Release UUIDv7")),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Release variants", body = VariantList),
        (status = 404, description = "Release not found", body = Problem)
    )
)]
#[actix_web::get("/releases/{release}/variants")]
pub(crate) async fn list_release_variants(
    request: HttpRequest,
    state: web::Data<AppState>,
    release: web::Path<String>,
) -> Result<web::Json<VariantList>, ApiError> {
    authenticate(&request, &state, Permission::SoftwareRead).await?;
    let request_id = request_id(&request);
    let release_id = release
        .parse::<ReleaseId>()
        .map_err(|error| ApiError::domain(error, &request_id))?;
    if state
        .storage
        .release(release_id)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .is_none()
    {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "release_not_found",
            "Release does not exist.",
            &request_id,
        ));
    }
    let variants = state
        .storage
        .release_variants(release_id)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    let mut items = Vec::with_capacity(variants.len());
    for variant in variants {
        let artifacts = state
            .storage
            .variant_artifacts(variant.id)
            .await
            .map_err(|error| ApiError::storage(error, &request_id))?;
        items.push(variant_response(variant, artifacts));
    }
    Ok(web::Json(VariantList { items }))
}

/// Current channel binding.
#[derive(Debug, Serialize, ToSchema)]
pub struct ChannelResponse {
    /// Parent software identity.
    software_id: String,
    /// Channel name.
    name: String,
    /// Current release target.
    release_id: String,
    /// Optional pinned variant.
    pinned_variant_id: Option<String>,
    /// Optimistic-concurrency revision.
    revision: u64,
}

impl From<ChannelRecord> for ChannelResponse {
    fn from(value: ChannelRecord) -> Self {
        Self {
            software_id: value.software_id.to_string(),
            name: value.name,
            release_id: value.release_id.to_string(),
            pinned_variant_id: value.pinned_variant_id.map(|value| value.to_string()),
            revision: value.revision,
        }
    }
}

/// Current channel collection.
#[derive(Debug, Serialize, ToSchema)]
pub struct ChannelList {
    /// Current bindings sorted by name.
    items: Vec<ChannelResponse>,
}

async fn software_for_catalog(
    state: &AppState,
    identity: &str,
    request_id: &str,
) -> Result<Software, ApiError> {
    state
        .storage
        .software(identity)
        .await
        .map_err(|error| ApiError::storage(error, request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "software_not_found",
                "Software does not exist.",
                request_id,
            )
        })
}

#[utoipa::path(
    get,
    path = "/api/v1/software/{software}/channels",
    tag = "channels",
    params(("software" = String, Path, description = "Software UUIDv7 or slug")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Current channels", body = ChannelList))
)]
#[actix_web::get("/software/{software}/channels")]
pub(crate) async fn list_channels(
    request: HttpRequest,
    state: web::Data<AppState>,
    identity: web::Path<String>,
) -> Result<web::Json<ChannelList>, ApiError> {
    authenticate(&request, &state, Permission::SoftwareRead).await?;
    let request_id = request_id(&request);
    let software = software_for_catalog(&state, &identity, &request_id).await?;
    let channels = state
        .storage
        .channels(software.id)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(web::Json(ChannelList {
        items: channels.into_iter().map(ChannelResponse::from).collect(),
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/software/{software}/channels/{channel}",
    tag = "channels",
    params(
        ("software" = String, Path, description = "Software UUIDv7 or slug"),
        ("channel" = String, Path, description = "Channel name")
    ),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Current channel", body = ChannelResponse),
        (status = 404, description = "Channel not found", body = Problem)
    )
)]
#[actix_web::get("/software/{software}/channels/{channel}")]
pub(crate) async fn get_channel(
    request: HttpRequest,
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
) -> Result<HttpResponse, ApiError> {
    authenticate(&request, &state, Permission::SoftwareRead).await?;
    let request_id = request_id(&request);
    let (identity, name) = path.into_inner();
    let software = software_for_catalog(&state, &identity, &request_id).await?;
    let channel = state
        .storage
        .channel(software.id, &name)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "channel_not_found",
                "Channel does not exist.",
                &request_id,
            )
        })?;
    Ok(HttpResponse::Ok()
        .insert_header((header::ETAG, revision_etag(channel.revision)))
        .json(ChannelResponse::from(channel)))
}

/// Explicit channel promotion request.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PromoteChannelRequest {
    /// Release UUIDv7 target.
    release_id: String,
    /// Optional variant UUIDv7 pin.
    pinned_variant_id: Option<String>,
    /// Optional safe operator reason.
    reason: Option<String>,
}

#[utoipa::path(
    put,
    path = "/api/v1/software/{software}/channels/{channel}",
    tag = "channels",
    params(
        ("software" = String, Path, description = "Software UUIDv7 or slug"),
        ("channel" = String, Path, description = "`testing` or `stable`"),
        ("If-Match" = String, Header, description = "Current ETag or `\"rev-0\"` for creation")
    ),
    request_body = PromoteChannelRequest,
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Channel advanced", body = ChannelResponse),
        (status = 201, description = "Channel created", body = ChannelResponse),
        (status = 412, description = "Stale ETag", body = Problem)
    )
)]
#[actix_web::put("/software/{software}/channels/{channel}")]
pub(crate) async fn promote_channel(
    request: HttpRequest,
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
    body: web::Json<PromoteChannelRequest>,
) -> Result<HttpResponse, ApiError> {
    let principal = authenticate(&request, &state, Permission::ReleasePromote).await?;
    let request_id = request_id(&request);
    let expected_revision = expected_revision(&request, &request_id)?;
    let (identity, name) = path.into_inner();
    let software = software_for_catalog(&state, &identity, &request_id).await?;
    let release_id = body
        .release_id
        .parse::<ReleaseId>()
        .map_err(|error| ApiError::domain(error, &request_id))?;
    let pinned_variant_id = body
        .pinned_variant_id
        .as_deref()
        .map(str::parse::<VariantId>)
        .transpose()
        .map_err(|error| ApiError::domain(error, &request_id))?;
    let now = Utc::now();
    let channel = state
        .storage
        .promote_channel(
            software.id,
            &name,
            release_id,
            pinned_variant_id,
            expected_revision,
            body.reason.as_deref(),
            &AuditEvent {
                id: AuditEventId::new(),
                actor: AuditActor::Principal(principal.id),
                action: "release.promote".into(),
                resource_kind: "channel".into(),
                resource_id: Some(format!("{}:{name}", software.id)),
                details: serde_json::json!({
                    "release_id": release_id,
                    "pinned_variant_id": pinned_variant_id,
                    "reason": body.reason,
                }),
                request_id: Some(request_id.clone()),
                occurred_at: now,
            },
            now,
        )
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    let mut response = if expected_revision == 0 {
        HttpResponse::Created()
    } else {
        HttpResponse::Ok()
    };
    Ok(response
        .insert_header((header::ETAG, revision_etag(channel.revision)))
        .json(ChannelResponse::from(channel)))
}

/// Explicit release rejection request.
#[derive(Debug, Deserialize, ToSchema)]
pub struct RejectReleaseRequest {
    /// Required safe operator reason.
    reason: String,
}

#[utoipa::path(
    post,
    path = "/api/v1/releases/{release}/reject",
    tag = "releases",
    params(
        ("release" = String, Path, description = "Release UUIDv7"),
        ("If-Match" = String, Header, description = "Current release ETag")
    ),
    request_body = RejectReleaseRequest,
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Release rejected and channels removed", body = ReleaseResponse),
        (status = 412, description = "Stale ETag", body = Problem)
    )
)]
#[actix_web::post("/releases/{release}/reject")]
pub(crate) async fn reject_release(
    request: HttpRequest,
    state: web::Data<AppState>,
    release: web::Path<String>,
    body: web::Json<RejectReleaseRequest>,
) -> Result<HttpResponse, ApiError> {
    let principal = authenticate(&request, &state, Permission::ReleaseReject).await?;
    let request_id = request_id(&request);
    let expected_revision = expected_revision(&request, &request_id)?;
    let release_id = release
        .parse::<ReleaseId>()
        .map_err(|error| ApiError::domain(error, &request_id))?;
    let now = Utc::now();
    let release = state
        .storage
        .reject_release(
            release_id,
            expected_revision,
            &body.reason,
            &AuditEvent {
                id: AuditEventId::new(),
                actor: AuditActor::Principal(principal.id),
                action: "release.reject".into(),
                resource_kind: "release".into(),
                resource_id: Some(release_id.to_string()),
                details: serde_json::json!({"reason": body.reason}),
                request_id: Some(request_id.clone()),
                occurred_at: now,
            },
            now,
        )
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(HttpResponse::Ok()
        .insert_header((header::ETAG, revision_etag(release.revision)))
        .json(ReleaseResponse::from(release)))
}

/// Compatibility and channel inputs to deterministic resolution.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ResolveQuery {
    /// Channel name, default `stable`.
    channel: Option<String>,
    /// Platform: `mac_os`, `linux`, or `windows`.
    platform: String,
    /// Architecture: `x86_64` or `aarch64`.
    architecture: String,
    /// Numeric Apple-style macOS version.
    macos: Option<String>,
}

/// Deterministic resolver result.
#[derive(Debug, Serialize, ToSchema)]
pub struct ResolutionResponse {
    /// Channel used for resolution.
    channel: String,
    /// Selected release.
    release: ReleaseResponse,
    /// Selected variant.
    variant: VariantResponse,
    /// SHA-256 primary installer identity.
    artifact_digest: String,
    /// Exact primary installer size.
    artifact_size: u64,
    /// Authenticated immutable content endpoint.
    content_path: String,
}

fn parse_platform(value: &str, request_id: &str) -> Result<Platform, ApiError> {
    match value {
        "mac_os" => Ok(Platform::MacOs),
        "linux" => Ok(Platform::Linux),
        "windows" => Ok(Platform::Windows),
        _ => Err(ApiError::validation(
            "The target platform is invalid.",
            vec![],
            request_id,
        )),
    }
}

fn parse_architecture(value: &str, request_id: &str) -> Result<Architecture, ApiError> {
    match value {
        "x86_64" => Ok(Architecture::X86_64),
        "aarch64" => Ok(Architecture::Aarch64),
        _ => Err(ApiError::validation(
            "The target architecture is invalid.",
            vec![],
            request_id,
        )),
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/software/{software}/resolve",
    tag = "resolver",
    params(("software" = String, Path, description = "Software UUIDv7 or slug"), ResolveQuery),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "One readable primary installer", body = ResolutionResponse),
        (status = 404, description = "No compatible variant or channel", body = Problem),
        (status = 409, description = "Ambiguous variant or unavailable artifact", body = Problem)
    )
)]
#[actix_web::get("/software/{software}/resolve")]
pub(crate) async fn resolve_software(
    request: HttpRequest,
    state: web::Data<AppState>,
    identity: web::Path<String>,
    query: web::Query<ResolveQuery>,
) -> Result<web::Json<ResolutionResponse>, ApiError> {
    authenticate(&request, &state, Permission::SoftwareRead).await?;
    let request_id = request_id(&request);
    let software = software_for_catalog(&state, &identity, &request_id).await?;
    let channel_name = query.channel.as_deref().unwrap_or("stable");
    let channel = state
        .storage
        .channel(software.id, channel_name)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "channel_not_found",
                "Requested channel does not exist.",
                &request_id,
            )
        })?;
    let release = state
        .storage
        .release(channel.release_id)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| ApiError::internal(&request_id))?;
    if !release.availability.is_available() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "release_withdrawn",
            "The release is withdrawn.",
            &request_id,
        ));
    }
    let variants = state
        .storage
        .release_variants(release.id)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    let target = ResolutionTarget {
        platform: parse_platform(&query.platform, &request_id)?,
        architecture: parse_architecture(&query.architecture, &request_id)?,
        macos: query
            .macos
            .as_deref()
            .map(str::parse::<MacOsVersion>)
            .transpose()
            .map_err(|error| ApiError::domain(error, &request_id))?,
        pinned_variant: channel.pinned_variant_id,
    };
    let selected = resolve_variant(&variants, &target)
        .map_err(|error| ApiError::domain(error, &request_id))?
        .clone();
    let artifacts = state
        .storage
        .variant_artifacts(selected.id)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    let primary = artifacts
        .iter()
        .filter(|artifact| artifact.role == ArtifactRole::PrimaryInstaller && artifact.readable)
        .collect::<Vec<_>>();
    if primary.len() != 1 {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "primary_artifact_unavailable",
            "Resolved variant does not have exactly one readable primary installer.",
            &request_id,
        ));
    }
    let primary = primary[0];
    let digest = primary.artifact.digest.to_string();
    let artifact_size = primary.artifact.size;
    Ok(web::Json(ResolutionResponse {
        channel: channel.name,
        release: ReleaseResponse::from(release),
        variant: variant_response(selected, artifacts),
        artifact_digest: digest.clone(),
        artifact_size,
        content_path: format!("/api/v1/artifacts/{digest}/content"),
    }))
}

/// Immutable artifact metadata response.
#[derive(Debug, Serialize, ToSchema)]
pub struct ArtifactResponse {
    /// Lowercase SHA-256 digest.
    digest: String,
    /// Exact byte length.
    size: u64,
    /// Media type.
    media_type: String,
    /// Ingestion time.
    created_at: chrono::DateTime<Utc>,
    /// Independently tracked location states.
    locations: Vec<ArtifactLocationResponse>,
}

/// One artifact location response.
#[derive(Debug, Serialize, ToSchema)]
pub struct ArtifactLocationResponse {
    /// UUIDv7 location identity.
    id: String,
    /// UUIDv7 store identity.
    store_id: String,
    /// Durable location state.
    state: String,
    /// Last server verification time.
    verified_at: Option<chrono::DateTime<Utc>>,
    /// Safe adapter error summary, when the location is unhealthy.
    last_error: Option<String>,
}

impl From<ArtifactLocation> for ArtifactLocationResponse {
    fn from(location: ArtifactLocation) -> Self {
        Self {
            id: location.id.to_string(),
            store_id: location.store_id.to_string(),
            state: location_state_response(location.state).into(),
            verified_at: location.verified_at,
            last_error: location.last_error,
        }
    }
}

fn location_state_response(state: LocationState) -> &'static str {
    match state {
        LocationState::Pending => "pending",
        LocationState::Replicating => "replicating",
        LocationState::Present => "present",
        LocationState::Remote => "remote",
        LocationState::Missing => "missing",
        LocationState::Corrupt => "corrupt",
        LocationState::Failed => "failed",
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/artifacts/{digest}",
    tag = "artifacts",
    params(("digest" = String, Path, description = "Lowercase SHA-256 digest")),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Artifact metadata", body = ArtifactResponse),
        (status = 404, description = "Artifact not found", body = Problem)
    )
)]
#[actix_web::get("/artifacts/{digest}")]
pub(crate) async fn get_artifact(
    request: HttpRequest,
    state: web::Data<AppState>,
    digest: web::Path<String>,
) -> Result<web::Json<ArtifactResponse>, ApiError> {
    authenticate(&request, &state, Permission::ArtifactRead).await?;
    let request_id = request_id(&request);
    let digest = Sha256Digest::new(digest.into_inner())
        .map_err(|error| ApiError::domain(error, &request_id))?;
    let artifact = state
        .storage
        .artifact(&digest)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "artifact_not_found",
                "Artifact does not exist.",
                &request_id,
            )
        })?;
    let locations = state
        .storage
        .artifact_locations(&digest)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(web::Json(ArtifactResponse {
        digest: artifact.digest.to_string(),
        size: artifact.size,
        media_type: artifact.media_type,
        created_at: artifact.created_at,
        locations: locations
            .into_iter()
            .map(ArtifactLocationResponse::from)
            .collect(),
    }))
}

/// Independently tracked artifact locations.
#[derive(Debug, Serialize, ToSchema)]
pub struct ArtifactLocationList {
    /// All known locations for this digest.
    items: Vec<ArtifactLocationResponse>,
}

#[utoipa::path(
    get,
    path = "/api/v1/artifacts/{digest}/locations",
    tag = "artifacts",
    params(("digest" = String, Path, description = "Lowercase SHA-256 digest")),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Artifact locations", body = ArtifactLocationList),
        (status = 404, description = "Artifact not found", body = Problem)
    )
)]
#[actix_web::get("/artifacts/{digest}/locations")]
pub(crate) async fn list_artifact_locations(
    request: HttpRequest,
    state: web::Data<AppState>,
    digest: web::Path<String>,
) -> Result<web::Json<ArtifactLocationList>, ApiError> {
    authenticate(&request, &state, Permission::ArtifactRead).await?;
    let request_id = request_id(&request);
    let digest = Sha256Digest::new(digest.into_inner())
        .map_err(|error| ApiError::domain(error, &request_id))?;
    state
        .storage
        .artifact(&digest)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "artifact_not_found",
                "Artifact does not exist.",
                &request_id,
            )
        })?;
    let locations = state
        .storage
        .artifact_locations(&digest)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(web::Json(ArtifactLocationList {
        items: locations
            .into_iter()
            .map(ArtifactLocationResponse::from)
            .collect(),
    }))
}

/// Configured artifact store and its application-facing capabilities.
#[derive(Debug, Serialize, ToSchema)]
pub struct StoreResponse {
    /// UUIDv7 store identity.
    id: String,
    /// Unique operator-facing name.
    name: String,
    /// Placement role.
    role: String,
    /// Stable adapter kind.
    kind: String,
    /// Whether placement and serving may use this store.
    enabled: bool,
    /// Optimistic-concurrency revision.
    revision: u64,
    /// Whether full object reads are supported.
    read: bool,
    /// Whether streaming writes are supported.
    write: bool,
    /// Whether deletion is supported.
    delete: bool,
    /// Whether single byte ranges are supported.
    range: bool,
    /// Whether multipart ingestion is supported.
    multipart: bool,
    /// Whether redirects or presigned URLs are supported.
    redirect: bool,
}

fn store_role_response(role: stabbur_store_core::StoreRole) -> &'static str {
    match role {
        stabbur_store_core::StoreRole::Primary => "primary",
        stabbur_store_core::StoreRole::Replica => "replica",
        stabbur_store_core::StoreRole::Cache => "cache",
        stabbur_store_core::StoreRole::ReadOnly => "read_only",
    }
}

fn store_response(record: stabbur_storage_core::StoreRecord, state: &AppState) -> StoreResponse {
    let capabilities = state.store.capabilities();
    StoreResponse {
        id: record.id.to_string(),
        name: record.name,
        role: store_role_response(record.role).into(),
        kind: record.kind,
        enabled: record.enabled,
        revision: record.revision,
        read: capabilities.read,
        write: capabilities.write,
        delete: capabilities.delete,
        range: capabilities.range,
        multipart: capabilities.multipart,
        redirect: capabilities.redirect,
    }
}

/// Configured store collection.
#[derive(Debug, Serialize, ToSchema)]
pub struct StoreList {
    /// Stores known to the control plane.
    items: Vec<StoreResponse>,
}

#[utoipa::path(
    get,
    path = "/api/v1/stores",
    tag = "stores",
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Configured artifact stores", body = StoreList))
)]
#[actix_web::get("/stores")]
pub(crate) async fn list_stores(
    request: HttpRequest,
    state: web::Data<AppState>,
) -> Result<web::Json<StoreList>, ApiError> {
    authenticate(&request, &state, Permission::StorageManage).await?;
    let request_id = request_id(&request);
    let stores = state
        .storage
        .stores()
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(web::Json(StoreList {
        items: stores
            .into_iter()
            .map(|store| store_response(store, &state))
            .collect(),
    }))
}

async fn load_store(
    state: &AppState,
    identity: &str,
    request_id: &str,
) -> Result<stabbur_storage_core::StoreRecord, ApiError> {
    let store_id = identity
        .parse::<StoreId>()
        .map_err(|error| ApiError::domain(error, request_id))?;
    state
        .storage
        .store(store_id)
        .await
        .map_err(|error| ApiError::storage(error, request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "store_not_found",
                "Artifact store does not exist.",
                request_id,
            )
        })
}

#[utoipa::path(
    get,
    path = "/api/v1/stores/{store}",
    tag = "stores",
    params(("store" = String, Path, description = "Store UUIDv7")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Artifact store", body = StoreResponse))
)]
#[actix_web::get("/stores/{store}")]
pub(crate) async fn get_store(
    request: HttpRequest,
    state: web::Data<AppState>,
    identity: web::Path<String>,
) -> Result<HttpResponse, ApiError> {
    authenticate(&request, &state, Permission::StorageManage).await?;
    let request_id = request_id(&request);
    let store = load_store(&state, &identity, &request_id).await?;
    Ok(HttpResponse::Ok()
        .insert_header((header::ETAG, revision_etag(store.revision)))
        .json(store_response(store, &state)))
}

/// Non-mutating store adapter health result.
#[derive(Debug, Serialize, ToSchema)]
pub struct StoreTestResponse {
    /// Whether the configured adapter responded as expected.
    healthy: bool,
    /// Safe human-readable result.
    detail: String,
}

#[utoipa::path(
    post,
    path = "/api/v1/stores/{store}/test",
    tag = "stores",
    params(("store" = String, Path, description = "Store UUIDv7")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Non-mutating adapter probe", body = StoreTestResponse))
)]
#[actix_web::post("/stores/{store}/test")]
pub(crate) async fn test_store(
    request: HttpRequest,
    state: web::Data<AppState>,
    identity: web::Path<String>,
) -> Result<web::Json<StoreTestResponse>, ApiError> {
    authenticate(&request, &state, Permission::StorageManage).await?;
    let request_id = request_id(&request);
    let store = load_store(&state, &identity, &request_id).await?;
    if store.id != state.store_id {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "store_adapter_unavailable",
            "The store adapter is not attached to this server process.",
            &request_id,
        ));
    }
    let probe =
        Sha256Digest::new("0".repeat(64)).map_err(|error| ApiError::domain(error, &request_id))?;
    match state.store.head(&probe).await {
        Ok(_) | Err(StoreError::NotFound) => Ok(web::Json(StoreTestResponse {
            healthy: true,
            detail: "The adapter accepted a non-mutating metadata probe.".into(),
        })),
        Err(error) => Err(ApiError::store(error, &request_id)),
    }
}

/// Verified artifact ingestion response.
#[derive(Debug, Serialize, ToSchema)]
pub struct UploadArtifactResponse {
    /// Lowercase SHA-256 digest.
    digest: String,
    /// Exact verified size.
    size: u64,
    /// Whether existing verified content was reused.
    reused: bool,
}

#[utoipa::path(
    put,
    path = "/api/v1/artifacts/{digest}/content",
    tag = "artifacts",
    params(("digest" = String, Path, description = "Declared lowercase SHA-256 digest")),
    request_body(content = String, content_type = "application/octet-stream"),
    security(("bearer_auth" = [])),
    responses(
        (status = 201, description = "Content verified and published", body = UploadArtifactResponse),
        (status = 200, description = "Existing verified content reused", body = UploadArtifactResponse),
        (status = 422, description = "Digest or size mismatch", body = Problem)
    )
)]
#[actix_web::put("/artifacts/{digest}/content")]
pub(crate) async fn upload_artifact_content(
    request: HttpRequest,
    state: web::Data<AppState>,
    digest: web::Path<String>,
    payload: web::Payload,
) -> Result<HttpResponse, ApiError> {
    let principal = authenticate(&request, &state, Permission::ArtifactWrite).await?;
    let request_id = request_id(&request);
    let digest = Sha256Digest::new(digest.into_inner())
        .map_err(|error| ApiError::domain(error, &request_id))?;
    let size = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| {
            ApiError::validation(
                "A valid Content-Length header is required.",
                vec![ValidationError {
                    field: "Content-Length".into(),
                    code: "required".into(),
                    message: "Content-Length must be an unsigned integer.".into(),
                }],
                &request_id,
            )
        })?;
    let media_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_owned();
    if media_type.len() > 255 || media_type.chars().any(char::is_control) {
        return Err(ApiError::validation(
            "The Content-Type header is invalid.",
            vec![],
            &request_id,
        ));
    }
    let stream = bridge_payload(payload);
    let outcome = state
        .store
        .write(&digest, size, stream)
        .await
        .map_err(|error| ApiError::store(error, &request_id))?;
    let now = Utc::now();
    let artifact = Artifact {
        digest: digest.clone(),
        size,
        media_type,
        created_at: now,
    };
    let location = ArtifactLocation {
        id: LocationId::new(),
        digest: digest.clone(),
        store_id: state.store_id,
        state: LocationState::Present,
        verified_at: Some(now),
        last_error: None,
    };
    let mut transaction = state
        .storage
        .begin()
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    transaction
        .record_artifact_location(&artifact, &location)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    transaction
        .append_audit(&AuditEvent {
            id: AuditEventId::new(),
            actor: AuditActor::Principal(principal.id),
            action: "artifact.ingest".into(),
            resource_kind: "artifact".into(),
            resource_id: Some(digest.to_string()),
            details: serde_json::json!({"size": size, "reused": outcome == WriteOutcome::Reused}),
            request_id: Some(request_id.clone()),
            occurred_at: now,
        })
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    transaction
        .commit()
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    let response = UploadArtifactResponse {
        digest: digest.to_string(),
        size,
        reused: outcome == WriteOutcome::Reused,
    };
    let mut builder = if outcome == WriteOutcome::Created {
        HttpResponse::Created()
    } else {
        HttpResponse::Ok()
    };
    Ok(builder
        .insert_header((header::ETAG, digest_etag(&digest)))
        .insert_header(("x-request-id", request_id))
        .json(response))
}

#[utoipa::path(
    get,
    path = "/api/v1/artifacts/{digest}/content",
    tag = "artifacts",
    params(
        ("digest" = String, Path, description = "Lowercase SHA-256 digest"),
        ("Range" = Option<String>, Header, description = "One closed, open-ended, or suffix byte range"),
        ("If-None-Match" = Option<String>, Header, description = "Strong digest ETag")
    ),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Complete immutable artifact content", content_type = "application/octet-stream"),
        (status = 206, description = "Single byte range", content_type = "application/octet-stream"),
        (status = 304, description = "Digest ETag matched"),
        (status = 416, description = "Invalid, unsatisfiable, or multiple ranges", body = Problem)
    )
)]
pub(crate) async fn download_artifact_content(
    request: HttpRequest,
    state: web::Data<AppState>,
    digest: web::Path<String>,
) -> Result<HttpResponse, ApiError> {
    serve_artifact_content(request, state, digest.into_inner(), false).await
}

#[utoipa::path(
    head,
    path = "/api/v1/artifacts/{digest}/content",
    tag = "artifacts",
    params(
        ("digest" = String, Path, description = "Lowercase SHA-256 digest"),
        ("Range" = Option<String>, Header, description = "One closed, open-ended, or suffix byte range"),
        ("If-None-Match" = Option<String>, Header, description = "Strong digest ETag")
    ),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Complete immutable artifact metadata"),
        (status = 206, description = "Single byte-range metadata"),
        (status = 304, description = "Digest ETag matched"),
        (status = 416, description = "Invalid, unsatisfiable, or multiple ranges", body = Problem)
    )
)]
pub(crate) async fn head_artifact_content(
    request: HttpRequest,
    state: web::Data<AppState>,
    digest: web::Path<String>,
) -> Result<HttpResponse, ApiError> {
    serve_artifact_content(request, state, digest.into_inner(), true).await
}

async fn serve_artifact_content(
    request: HttpRequest,
    state: web::Data<AppState>,
    digest: String,
    head_only: bool,
) -> Result<HttpResponse, ApiError> {
    authenticate(&request, &state, Permission::ArtifactRead).await?;
    let request_id = request_id(&request);
    let digest = Sha256Digest::new(digest).map_err(|error| ApiError::domain(error, &request_id))?;
    let artifact = state
        .storage
        .artifact(&digest)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "artifact_not_found",
                "Artifact does not exist.",
                &request_id,
            )
        })?;
    let locations = state
        .storage
        .artifact_locations(&digest)
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    if !locations.iter().any(|location| {
        location.store_id == state.store_id && location.state == LocationState::Present
    }) {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "artifact_unavailable",
            "Artifact has no readable present location.",
            &request_id,
        ));
    }
    let etag = digest_etag(&digest);
    if request
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .map(str::trim)
                .any(|tag| tag == etag || tag == "*")
        })
    {
        return Ok(HttpResponse::NotModified()
            .insert_header((header::ETAG, etag))
            .insert_header((header::CACHE_CONTROL, "public, max-age=31536000, immutable"))
            .insert_header(("x-request-id", request_id))
            .finish());
    }
    let range = parse_range(request.headers().get(header::RANGE), &request_id)?;
    let read = state
        .store
        .read(&digest, range)
        .await
        .map_err(|error| ApiError::store(error, &request_id))?;
    let (status, content_length, content_range) =
        read.range
            .map_or((StatusCode::OK, read.object.size, None), |range| {
                (
                    StatusCode::PARTIAL_CONTENT,
                    range.length(),
                    Some(format!(
                        "bytes {}-{}/{}",
                        range.start(),
                        range.end_inclusive(),
                        range.object_size()
                    )),
                )
            });
    let mut builder = HttpResponse::build(status);
    builder
        .insert_header((header::ETAG, etag))
        .insert_header((header::ACCEPT_RANGES, "bytes"))
        .insert_header((header::CACHE_CONTROL, "public, max-age=31536000, immutable"))
        .insert_header((header::CONTENT_LENGTH, content_length.to_string()))
        .insert_header((header::CONTENT_TYPE, artifact.media_type))
        .insert_header(("x-request-id", request_id));
    if let Some(content_range) = content_range {
        builder.insert_header((header::CONTENT_RANGE, content_range));
    }
    if head_only {
        Ok(builder.body(SizedStream::new(
            content_length,
            stream::empty::<Result<Bytes, actix_web::Error>>(),
        )))
    } else {
        Ok(builder.streaming(read.stream))
    }
}

fn digest_etag(digest: &Sha256Digest) -> String {
    format!("\"{}\"", digest.as_str())
}

fn parse_range(
    value: Option<&header::HeaderValue>,
    request_id: &str,
) -> Result<Option<ByteRange>, ApiError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_| range_error(request_id))?;
    let value = value
        .strip_prefix("bytes=")
        .ok_or_else(|| range_error(request_id))?;
    if value.contains(',') || value.is_empty() {
        return Err(range_error(request_id));
    }
    let (start, end) = value
        .split_once('-')
        .ok_or_else(|| range_error(request_id))?;
    match (start.is_empty(), end.is_empty()) {
        (true, false) => end
            .parse()
            .map(|length| Some(ByteRange::Suffix { length }))
            .map_err(|_| range_error(request_id)),
        (false, true) => start
            .parse()
            .map(|start| Some(ByteRange::From { start }))
            .map_err(|_| range_error(request_id)),
        (false, false) => Ok(Some(ByteRange::Inclusive {
            start: start.parse().map_err(|_| range_error(request_id))?,
            end: end.parse().map_err(|_| range_error(request_id))?,
        })),
        (true, true) => Err(range_error(request_id)),
    }
}

fn range_error(request_id: &str) -> ApiError {
    ApiError::new(
        StatusCode::RANGE_NOT_SATISFIABLE,
        "range_not_satisfiable",
        "Only one valid bytes range is supported.",
        request_id,
    )
}

fn bridge_payload(mut payload: web::Payload) -> ByteStream {
    // Actix payloads are local to an HTTP worker while store adapters are Send. A bounded channel
    // preserves backpressure and makes that runtime boundary explicit without buffering uploads.
    let (sender, receiver) = tokio::sync::mpsc::channel(8);
    actix_web::rt::spawn(async move {
        while let Some(chunk) = payload.next().await {
            let chunk = chunk.map_err(|_| StoreError::Backend {
                message: "request body stream failed".into(),
            });
            let failed = chunk.is_err();
            if sender.send(chunk).await.is_err() || failed {
                break;
            }
        }
    });
    Box::pin(futures_util::stream::unfold(
        receiver,
        |mut receiver| async { receiver.recv().await.map(|item| (item, receiver)) },
    ))
}

#[cfg(test)]
mod tests;
