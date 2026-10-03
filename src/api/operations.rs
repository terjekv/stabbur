//! Operator actions with explicit publication and claim eligibility.
use super::*;

/// Change whether a worker may claim new work while preserving active leases.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DrainWorkerRequest {
    /// True pauses new claims; false resumes claims.
    draining: bool,
}

#[utoipa::path(post, path = "/api/v1/workers/{worker}/drain", tag = "workers",
    params(("worker" = String, Path), ("If-Match" = String, Header)),
    request_body = DrainWorkerRequest, security(("bearer_auth" = [])),
    responses((status = 200, description = "Worker claim policy updated", body = WorkerResponse),
              (status = 412, description = "Stale revision", body = Problem)))]
#[actix_web::post("/workers/{worker}/drain")]
pub(crate) async fn drain_worker(
    request: HttpRequest,
    state: web::Data<AppState>,
    worker: web::Path<String>,
    body: web::Json<DrainWorkerRequest>,
) -> Result<HttpResponse, ApiError> {
    let principal = authenticate(&request, &state, Permission::WorkerManage).await?;
    let request_id = request_id(&request);
    let revision = expected_revision(&request, &request_id)?;
    let worker_id = worker
        .parse::<WorkerId>()
        .map_err(|error| ApiError::domain(error, &request_id))?;
    let value = state
        .storage
        .set_worker_draining(
            worker_id,
            body.draining,
            revision,
            &AuditEvent {
                id: AuditEventId::new(),
                actor: AuditActor::Principal(principal.id),
                action: "worker.drain".into(),
                resource_kind: "worker".into(),
                resource_id: Some(worker_id.to_string()),
                details: serde_json::json!({"draining": body.draining}),
                request_id: Some(request_id.clone()),
                occurred_at: Utc::now(),
            },
        )
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(HttpResponse::Ok()
        .insert_header((header::ETAG, revision_etag(value.revision)))
        .json(WorkerResponse::from(value)))
}

#[utoipa::path(post, path = "/api/v1/releases/{release}/withdraw", tag = "releases",
    params(("release" = String, Path), ("If-Match" = String, Header)),
    request_body = RejectReleaseRequest, security(("bearer_auth" = [])),
    responses((status = 200, description = "Release withdrawn; lifecycle and bytes preserved", body = ReleaseResponse),
              (status = 409, description = "Already withdrawn", body = Problem),
              (status = 412, description = "Stale revision", body = Problem)))]
#[actix_web::post("/releases/{release}/withdraw")]
pub(crate) async fn withdraw_release(
    request: HttpRequest,
    state: web::Data<AppState>,
    release: web::Path<String>,
    body: web::Json<RejectReleaseRequest>,
) -> Result<HttpResponse, ApiError> {
    let principal = authenticate(&request, &state, Permission::ReleaseReject).await?;
    let request_id = request_id(&request);
    let revision = expected_revision(&request, &request_id)?;
    let release_id = release
        .parse::<ReleaseId>()
        .map_err(|error| ApiError::domain(error, &request_id))?;
    let reason = stabbur_domain::WithdrawalReason::new(body.reason.clone())
        .map_err(|detail| ApiError::validation(detail, vec![], &request_id))?;
    let now = Utc::now();
    let value = state
        .storage
        .withdraw_release(
            release_id,
            &reason,
            revision,
            &AuditEvent {
                id: AuditEventId::new(),
                actor: AuditActor::Principal(principal.id),
                action: "release.withdraw".into(),
                resource_kind: "release".into(),
                resource_id: Some(release_id.to_string()),
                details: serde_json::json!({"reason": reason.as_str()}),
                request_id: Some(request_id.clone()),
                occurred_at: now,
            },
            now,
        )
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(HttpResponse::Ok()
        .insert_header((header::ETAG, revision_etag(value.revision)))
        .json(ReleaseResponse::from(value)))
}

/// Published channel with the exact opaque version and concurrency revision.
#[derive(Debug, Serialize, ToSchema)]
pub struct SoftwareChannelStatusResponse {
    /// Channel name.
    name: String,
    /// Release identity.
    release_id: String,
    /// Opaque version.
    version: String,
    /// Channel concurrency revision.
    revision: u64,
}

impl From<stabbur_storage_core::SoftwareChannelSummary> for SoftwareChannelStatusResponse {
    fn from(value: stabbur_storage_core::SoftwareChannelSummary) -> Self {
        Self {
            name: value.name,
            release_id: value.release_id.to_string(),
            version: value.version.to_string(),
            revision: value.revision,
        }
    }
}
/// Target waiting for one recently observed worker matching all its capabilities.
#[derive(Debug, Serialize, ToSchema)]
pub struct BlockedTargetResponse {
    /// Target identity.
    id: String,
    /// Target name.
    name: String,
    /// Required capabilities.
    required_capabilities: Vec<String>,
}
/// Bounded operator view of execution and publication for one software item.
#[derive(Debug, Serialize, ToSchema)]
pub struct SoftwareStatusResponse {
    /// Software identity.
    software_id: String,
    /// Current published channels.
    channels: Vec<SoftwareChannelStatusResponse>,
    /// Latest run observation.
    latest_run: Option<RunSummaryResponse>,
    /// Last successful check, including no-change observations.
    last_success_at: Option<chrono::DateTime<Utc>>,
    /// Next recurring check.
    next_run_at: Option<chrono::DateTime<Utc>>,
    /// Enabled target count.
    enabled_targets: u64,
    /// Queued and running builds.
    outstanding_runs: u64,
    /// First 200 enabled targets without a matching worker observed in the last five minutes.
    blocked_targets: Vec<BlockedTargetResponse>,
}

#[utoipa::path(get, path = "/api/v1/software/{software}/status", tag = "software",
    params(("software" = String, Path)), security(("bearer_auth" = [])),
    responses((status = 200, description = "Software execution and publication status", body = SoftwareStatusResponse)))]
#[actix_web::get("/software/{software}/status")]
pub(crate) async fn software_status(
    request: HttpRequest,
    state: web::Data<AppState>,
    identity: web::Path<String>,
) -> Result<web::Json<SoftwareStatusResponse>, ApiError> {
    authenticate(&request, &state, Permission::SoftwareRead).await?;
    let request_id = request_id(&request);
    let software = software_for_catalog(&state, &identity, &request_id).await?;
    let value = state
        .storage
        .software_status(software.id, Utc::now())
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(web::Json(SoftwareStatusResponse {
        software_id: value.software_id.to_string(),
        channels: value
            .channels
            .into_iter()
            .map(|value| SoftwareChannelStatusResponse {
                name: value.name,
                release_id: value.release_id.to_string(),
                version: value.version.to_string(),
                revision: value.revision,
            })
            .collect(),
        latest_run: value.latest_run.map(RunSummaryResponse::from),
        last_success_at: value.last_success_at,
        next_run_at: value.next_run_at,
        enabled_targets: value.enabled_targets,
        outstanding_runs: value.outstanding_runs,
        blocked_targets: value
            .blocked_targets
            .into_iter()
            .map(|value| BlockedTargetResponse {
                id: value.id.to_string(),
                name: value.name,
                required_capabilities: value
                    .required_capabilities
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
            })
            .collect(),
    }))
}

/// Durable queue and worker measurements; contains no backend paths or credentials.
#[derive(Debug, Serialize, ToSchema)]
pub struct OperationalStatusResponse {
    /// First 200 queued capability groups ordered by oldest work.
    capability_queues: Vec<CapabilityQueueResponse>,
    /// Further groups exist outside this bounded response.
    capability_queues_truncated: bool,
    /// Jobs awaiting a worker.
    queued_jobs: u64,
    /// Jobs with a leased attempt.
    running_jobs: u64,
    /// Historical terminal job failures.
    failed_jobs: u64,
    /// Historical expired attempts.
    expired_attempts: u64,
    /// Enabled workers paused for maintenance.
    draining_workers: u64,
    /// Creation time of the oldest queued job.
    oldest_queued_at: Option<chrono::DateTime<Utc>>,
}
#[utoipa::path(get, path = "/api/v1/operations/status", tag = "operations",
    security(("bearer_auth" = [])), responses((status = 200, description = "Operational measurements", body = OperationalStatusResponse)))]
#[actix_web::get("/operations/status")]
pub(crate) async fn operational_status(
    request: HttpRequest,
    state: web::Data<AppState>,
) -> Result<web::Json<OperationalStatusResponse>, ApiError> {
    authenticate(&request, &state, Permission::WorkerRead).await?;
    let request_id = request_id(&request);
    let value = state
        .storage
        .operational_status()
        .await
        .map_err(|error| ApiError::storage(error, &request_id))?;
    Ok(web::Json(OperationalStatusResponse {
        capability_queues: value
            .capability_queues
            .into_iter()
            .map(|group| CapabilityQueueResponse {
                required_capabilities: group
                    .required_capabilities
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
                queued_jobs: group.queued_jobs,
                matching_workers: group.matching_workers,
                workers_with_active_leases: group.workers_with_active_leases,
                oldest_queued_at: group.oldest_queued_at,
            })
            .collect(),
        capability_queues_truncated: value.capability_queues_truncated,
        queued_jobs: value.queued_jobs,
        running_jobs: value.running_jobs,
        failed_jobs: value.failed_jobs,
        expired_attempts: value.expired_attempts,
        draining_workers: value.draining_workers,
        oldest_queued_at: value.oldest_queued_at,
    }))
}

/// Jobs grouped by exact capabilities, separating unavailable workers from active leases.
#[derive(Debug, Serialize, ToSchema)]
pub struct CapabilityQueueResponse {
    /// Complete requirements matched on one worker.
    required_capabilities: Vec<String>,
    /// Queued jobs.
    queued_jobs: u64,
    /// Recent enabled non-draining compatible workers.
    matching_workers: u64,
    /// Compatible workers holding an unexpired lease; this is not configured capacity.
    workers_with_active_leases: u64,
    /// Oldest job in this group.
    oldest_queued_at: chrono::DateTime<Utc>,
}
