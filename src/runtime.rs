//! Service composition, secure bootstrap-file handling, and Actix supervision.

use std::{
    collections::BTreeMap,
    future::Future,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::Arc,
};

use actix_web::{
    App, HttpMessage, HttpServer,
    dev::Service,
    http::header::{HeaderName, HeaderValue},
    web,
};
use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use stabbur_auth_core::generate_token;
use stabbur_builder_autopkg::{
    AutoPkgAdapter, AutoPkgCatalogGenerator, AutoPkgError, AutoPkgLogChunk, AutoPkgLogStream,
    AutoPkgRecipe, PinnedSource,
};
use stabbur_builder_core::{
    BuildParameter, BuildResult, BuilderExecutionFailure, BuilderExecutionResult, BuilderJob,
    BuiltVariant, Provenance, RecipeCatalogManifest, RecipeCatalogScanExecutionFailure,
    RecipeCatalogScanExecutionResult, RecipeCatalogScanJob, VariantArtifact,
};
use stabbur_domain::{ArtifactRole, RecipeCatalogScanId, Sha256Digest, StoreId, WorkerId};
use stabbur_jobs_core::{Capability, CapabilitySet, JobSubject};
use stabbur_storage_core::{BootstrapPreparation, ClaimedJob, Storage};
use stabbur_storage_runtime::open as open_storage;
use stabbur_store_core::StoreRole;
use stabbur_store_fs::FsArtifactStore;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;
use tracing::{info, warn};

use crate::{
    api::{
        AppState, AppendWorkerLogsRequest, ClaimWorkerJobRequest, CompleteWorkerJobRequest,
        FailWorkerJobRequest, HeartbeatWorkerJobRequest, RegisterWorkerRequest, RequestId,
        WorkerLogEntryRequest, configure,
    },
    config::ServiceConfig,
};

/// Fully initialized service adapters.
pub struct ServiceRuntime {
    /// Backend-neutral persistence handle shared with an embedded worker.
    pub storage: Arc<dyn Storage>,
    /// HTTP application state.
    pub state: AppState,
    /// Stable identity used only by the embedded portable worker.
    pub embedded_worker_id: WorkerId,
}

/// Initializes durable directories, migrations, the local primary store, and bootstrap state.
pub async fn initialize(config: &ServiceConfig, prepare_bootstrap: bool) -> Result<ServiceRuntime> {
    create_private_directory(&config.data_dir).await?;
    let embedded_worker_dir = config.data_dir.join("embedded-worker");
    create_private_directory(&embedded_worker_dir).await?;
    let embedded_worker_id = worker_identity(&embedded_worker_dir).await?;
    let storage_handle = open_storage(&config.storage_settings(false))
        .await
        .context("opening Stabbur persistence")?;
    info!(
        backend = storage_handle.backend(),
        "storage backend initialized"
    );
    let storage = storage_handle.storage();
    let store_record = storage
        .ensure_local_primary_store(StoreId::new(), "local-primary")
        .await
        .context("initializing local primary store metadata")?;
    let store = Arc::new(
        FsArtifactStore::open(config.store_path(), StoreRole::Primary)
            .await
            .context("opening local primary artifact store")?,
    );
    if prepare_bootstrap {
        prepare_bootstrap_file(storage.as_ref(), config).await?;
    }
    let state = AppState::new(
        storage.clone(),
        store,
        store_record.id,
        config.bootstrap_secret_path(),
    );
    Ok(ServiceRuntime {
        storage,
        state,
        embedded_worker_id,
    })
}

/// Runs the Actix HTTP role until graceful shutdown.
pub async fn serve(config: ServiceConfig, state: AppState) -> Result<()> {
    let bind = config.bind;
    info!(%bind, "starting Stabbur API");
    let scheduler_storage = state.storage();
    let server = HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(state.clone()))
            .configure(configure)
            .wrap_fn(|request, service| {
                let request_id = request
                    .headers()
                    .get("x-request-id")
                    .and_then(|value| value.to_str().ok())
                    .filter(|value| !value.is_empty() && value.len() <= 128)
                    .map_or_else(|| uuid::Uuid::now_v7().to_string(), str::to_owned);
                request
                    .extensions_mut()
                    .insert(RequestId(request_id.clone()));
                let response = service.call(request);
                async move {
                    let mut response = response.await?;
                    if let Ok(value) = HeaderValue::from_str(&request_id) {
                        response
                            .headers_mut()
                            .insert(HeaderName::from_static("x-request-id"), value);
                    }
                    Ok(response)
                }
            })
    })
    .bind(bind)
    .with_context(|| format!("binding Stabbur API to {bind}"))?
    .shutdown_timeout(15)
    .run();
    let scheduler = (!config.disable_scheduler).then(|| {
        info!(
            poll_seconds = config.scheduler_poll_seconds,
            "starting build target scheduler"
        );
        tokio::spawn(crate::scheduler::run_scheduler(
            scheduler_storage,
            config.scheduler_poll_seconds,
        ))
    });
    let result = server.await.context("running Stabbur API");
    if let Some(scheduler) = scheduler {
        scheduler.abort();
    }
    result
}

/// Runs the embedded cross-platform fake builder used by `all` and deterministic tests.
pub async fn run_embedded_worker(storage: Arc<dyn Storage>, worker_id: WorkerId) {
    // The direct-storage embedded loop implements only the deterministic fake adapter. In
    // particular, it must not advertise detected AutoPkg tooling and then submit a fake result for
    // an AutoPkg job. Real builders always use the fully validated outbound worker protocol.
    let capabilities = embedded_capabilities();
    if let Err(error) = storage
        .register_worker(
            worker_id,
            "embedded-worker",
            &capabilities,
            chrono::Utc::now(),
        )
        .await
    {
        warn!(%error, "embedded worker registration failed");
        return;
    }
    loop {
        match storage
            .claim_job(worker_id, &capabilities, chrono::Utc::now(), 60)
            .await
        {
            Ok(Some(claimed)) => {
                let result = fake_result(&claimed);
                if let Err(error) = storage
                    .complete_job(
                        worker_id,
                        &claimed.lease,
                        &claimed.lease.attempt_id.to_string(),
                        &result,
                        chrono::Utc::now(),
                    )
                    .await
                {
                    warn!(%error, job = %claimed.job.id, "embedded worker completion failed");
                }
            }
            Ok(None) => tokio::time::sleep(std::time::Duration::from_secs(1)).await,
            Err(error) => {
                warn!(%error, "embedded worker claim failed");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    }
}

/// Runs a standalone outbound-only worker over the versioned internal REST protocol.
pub async fn run_outbound_worker(
    server_url: &str,
    token_file: &Path,
    data_dir: &Path,
    autopkg_program: Option<&Path>,
    catalog_manifest: Option<&Path>,
) -> Result<()> {
    let base = reqwest::Url::parse(server_url).context("parsing STABBUR_SERVER_URL")?;
    if !matches!(base.scheme(), "http" | "https")
        || base.host_str().is_none()
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
    {
        bail!("worker server URL must be an absolute HTTP(S) URL");
    }
    let host = base.host_str().expect("validated URL has a host");
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if base.scheme() != "https" && !loopback {
        bail!("remote workers require HTTPS; plain HTTP is accepted only for loopback testing");
    }
    let credential_json = read_owner_only(token_file).await?;
    let credential: WorkerCredentialFile = serde_json::from_str(&credential_json)
        .context("worker credential file must contain server-issued JSON")?;
    if credential.token.is_empty() || credential.token.trim() != credential.token {
        bail!("worker credential file contains an invalid token");
    }
    create_private_directory(data_dir).await?;
    let autopkg_program = resolve_autopkg_program(autopkg_program).await?;
    let capabilities = detected_worker_capabilities(autopkg_program.as_deref()).await;
    let capabilities = capabilities
        .iter()
        .map(|capability| capability.as_str().to_owned())
        .collect::<Vec<_>>();
    let catalog_manifest = match catalog_manifest {
        Some(path) => Some(read_recipe_catalog_manifest(path).await?),
        None => None,
    };
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .read_timeout(std::time::Duration::from_secs(30))
        .user_agent(concat!("stabbur-worker/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building outbound worker HTTP client")?;
    let base = server_url.trim_end_matches('/').to_owned();
    info!(worker_id = %credential.worker_id, server = %base, "starting outbound Stabbur worker");
    let mut catalog_published = false;

    loop {
        let cycle = outbound_cycle(
            &client,
            &base,
            &credential,
            &capabilities,
            data_dir,
            autopkg_program.as_deref(),
            catalog_manifest.as_ref(),
            &mut catalog_published,
        );
        tokio::select! {
            result = cycle => {
                if let Err(error) = result {
                    warn!(%error, "outbound worker cycle failed");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                } else {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
            signal = tokio::signal::ctrl_c() => {
                signal.context("waiting for worker shutdown signal")?;
                return Ok(());
            }
        }
    }
}

#[allow(clippy::too_many_arguments)] // HTTP, credentials, execution inputs, and publication state are independent worker boundaries.
async fn outbound_cycle(
    client: &reqwest::Client,
    base: &str,
    credential: &WorkerCredentialFile,
    capabilities: &[String],
    data_dir: &Path,
    autopkg_program: Option<&Path>,
    catalog_manifest: Option<&RecipeCatalogManifest>,
    catalog_published: &mut bool,
) -> Result<()> {
    let worker_id = credential.worker_id;
    let response = client
        .post(format!("{base}/api/v1/internal/workers/register"))
        .bearer_auth(&credential.token)
        .json(&RegisterWorkerRequest {
            worker_id,
            capabilities: capabilities.to_vec(),
        })
        .send()
        .await
        .context("registering outbound worker")?;
    worker_response(response, "worker registration").await?;
    if !*catalog_published && let Some(manifest) = catalog_manifest {
        let response = client
            .post(format!(
                "{base}/api/v1/internal/workers/{worker_id}/recipe-catalogs"
            ))
            .bearer_auth(&credential.token)
            .json(manifest)
            .send()
            .await
            .context("publishing worker recipe catalog")?;
        worker_response(response, "worker recipe catalog publication").await?;
        *catalog_published = true;
    }
    let response = client
        .post(format!("{base}/api/v1/internal/workers/{worker_id}/claim"))
        .bearer_auth(&credential.token)
        .json(&ClaimWorkerJobRequest { lease_seconds: 60 })
        .send()
        .await
        .context("claiming outbound worker job")?;
    if response.status() == reqwest::StatusCode::NO_CONTENT {
        return Ok(());
    }
    let claimed: ClaimedJob = worker_response(response, "worker claim")
        .await?
        .json()
        .await
        .context("decoding claimed job")?;
    execute_claimed_job(client, base, credential, data_dir, autopkg_program, claimed).await?;
    Ok(())
}

async fn read_recipe_catalog_manifest(path: &Path) -> Result<RecipeCatalogManifest> {
    const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

    let metadata = tokio::fs::metadata(path).await.with_context(|| {
        format!(
            "reading recipe catalog manifest metadata {}",
            path.display()
        )
    })?;
    if !metadata.is_file() || metadata.len() > MAX_MANIFEST_BYTES {
        bail!("recipe catalog manifest must be a regular file no larger than 1 MiB");
    }
    let json = tokio::fs::read(path)
        .await
        .with_context(|| format!("reading recipe catalog manifest {}", path.display()))?;
    let manifest: RecipeCatalogManifest = serde_json::from_slice(&json)
        .context("decoding builder-neutral recipe catalog manifest")?;
    manifest
        .canonical_digest()
        .context("validating builder-neutral recipe catalog manifest")?;
    Ok(manifest)
}

fn fake_result(claimed: &ClaimedJob) -> serde_json::Value {
    let adapter = serde_json::from_value::<BuilderJob>(claimed.job.payload.clone())
        .map_or_else(|_| "fake".to_owned(), |job| job.adapter);
    serde_json::to_value(BuilderExecutionResult {
        schema_version: BuilderJob::SCHEMA_VERSION,
        run_id: claimed
            .job
            .subject
            .build_run_id()
            .expect("fake worker only claims build jobs"),
        adapter,
        sources: vec![],
        tools: BTreeMap::new(),
        raw_report: serde_json::json!({
            "builder": "fake",
            "job_id": claimed.job.id,
            "payload": claimed.job.payload,
        }),
        build_result: None,
        completed_at: chrono::Utc::now(),
    })
    .expect("builder execution result is serializable")
}

#[derive(Deserialize)]
struct WorkerCredentialFile {
    worker_id: WorkerId,
    token: String,
}

impl std::fmt::Debug for WorkerCredentialFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkerCredentialFile")
            .field("worker_id", &self.worker_id)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

async fn execute_recipe_catalog_scan(
    client: &reqwest::Client,
    base: &str,
    credential: &WorkerCredentialFile,
    data_dir: &Path,
    scan_id: RecipeCatalogScanId,
    claimed: ClaimedJob,
) -> Result<()> {
    let envelope = match serde_json::from_value::<RecipeCatalogScanJob>(claimed.job.payload.clone())
    {
        Ok(envelope)
            if envelope.validate().is_ok()
                && envelope.request.scan_id == scan_id
                && envelope.request.required_capabilities == claimed.job.required_capabilities =>
        {
            envelope
        }
        _ => {
            return submit_catalog_scan_failure(
                client,
                base,
                credential,
                &claimed,
                catalog_scan_failure(
                    scan_id,
                    None,
                    "invalid_catalog_scan_envelope",
                    "The claimed catalog scan does not match the supported worker contract.",
                ),
            )
            .await;
        }
    };
    if envelope.request.producer != "autopkg"
        || !(60..=24 * 60 * 60).contains(&envelope.execution_timeout_seconds)
    {
        return submit_catalog_scan_failure(
            client,
            base,
            credential,
            &claimed,
            catalog_scan_failure(
                scan_id,
                Some(envelope.request.producer),
                "unsupported_catalog_scan",
                "This worker does not support the requested catalog producer or deadline.",
            ),
        )
        .await;
    }
    let producer = envelope.request.producer.clone();
    let source = PinnedSource {
        url: envelope.request.source.locator,
        commit: envelope.request.source.revision,
    };
    let isolation_root = data_dir
        .join("attempts")
        .join(claimed.lease.attempt_id.to_string());
    let deadline = std::time::Duration::from_secs(u64::from(envelope.execution_timeout_seconds));
    let mut lease = claimed.lease.clone();
    let generated = await_with_heartbeats(
        client,
        base,
        credential,
        &mut lease,
        tokio::time::timeout(
            deadline,
            AutoPkgCatalogGenerator::generate(&source, &isolation_root),
        ),
    )
    .await?;
    let claimed = ClaimedJob { lease, ..claimed };
    match generated {
        Ok(Ok(manifest)) => {
            let result = RecipeCatalogScanExecutionResult {
                schema_version: RecipeCatalogScanJob::SCHEMA_VERSION,
                scan_id,
                manifest,
                completed_at: chrono::Utc::now(),
            };
            submit_success(
                client,
                base,
                credential,
                &claimed,
                serde_json::to_value(result).context("serializing catalog scan result")?,
            )
            .await
        }
        Ok(Err(error)) => {
            let (code, detail) = autopkg_failure(&error);
            submit_catalog_scan_failure(
                client,
                base,
                credential,
                &claimed,
                catalog_scan_failure(scan_id, Some(producer), code, detail),
            )
            .await
        }
        Err(_) => {
            submit_catalog_scan_failure(
                client,
                base,
                credential,
                &claimed,
                catalog_scan_failure(
                    scan_id,
                    Some(producer),
                    "catalog_scan_timeout",
                    "The catalog scan exceeded its server-issued execution deadline.",
                ),
            )
            .await
        }
    }
}

async fn execute_claimed_job(
    client: &reqwest::Client,
    base: &str,
    credential: &WorkerCredentialFile,
    data_dir: &Path,
    autopkg_program: Option<&Path>,
    claimed: ClaimedJob,
) -> Result<()> {
    if let JobSubject::RecipeCatalogScan { scan_id } = claimed.job.subject {
        return execute_recipe_catalog_scan(client, base, credential, data_dir, scan_id, claimed)
            .await;
    }
    let run_id = claimed
        .job
        .subject
        .build_run_id()
        .expect("job subject variants are exhaustive");
    let envelope = match serde_json::from_value::<BuilderJob>(claimed.job.payload.clone()) {
        Ok(envelope)
            if envelope.schema_version == BuilderJob::SCHEMA_VERSION
                && envelope.request.run_id == run_id =>
        {
            envelope
        }
        _ => {
            return submit_failure(
                client,
                base,
                credential,
                &claimed,
                execution_failure(
                    run_id,
                    None,
                    "invalid_job_envelope",
                    "The claimed job does not match the supported worker contract.",
                ),
            )
            .await;
        }
    };
    if !(60..=24 * 60 * 60).contains(&envelope.execution_timeout_seconds) {
        return submit_failure(
            client,
            base,
            credential,
            &claimed,
            execution_failure(
                run_id,
                Some(envelope.adapter),
                "invalid_execution_deadline",
                "The server-issued execution deadline is outside the supported range.",
            ),
        )
        .await;
    }
    if envelope.adapter == "fake" {
        return submit_success(client, base, credential, &claimed, fake_result(&claimed)).await;
    }
    if envelope.adapter != "autopkg" {
        return submit_failure(
            client,
            base,
            credential,
            &claimed,
            execution_failure(
                run_id,
                Some(envelope.adapter),
                "unsupported_builder",
                "This worker does not support the requested builder adapter.",
            ),
        )
        .await;
    }
    let adapter_name = envelope.adapter.clone();
    let mut recipe = match serde_json::from_value::<AutoPkgRecipe>(envelope.adapter_definition) {
        Ok(recipe) => recipe,
        Err(_) => {
            return submit_failure(
                client,
                base,
                credential,
                &claimed,
                execution_failure(
                    run_id,
                    Some(adapter_name),
                    "invalid_autopkg_definition",
                    "The immutable AutoPkg definition is invalid.",
                ),
            )
            .await;
        }
    };
    if let Err((code, detail)) = apply_autopkg_parameters(&mut recipe, envelope.request.parameters)
    {
        return submit_failure(
            client,
            base,
            credential,
            &claimed,
            execution_failure(run_id, Some(adapter_name), code, detail),
        )
        .await;
    }
    let availability = detect_autopkg(autopkg_program).await;
    let tools = detected_tools(&availability);
    let isolation_root = data_dir
        .join("attempts")
        .join(claimed.lease.attempt_id.to_string());
    let execution_recipe = recipe.clone();
    let execution_root = isolation_root.clone();
    let execution_program = autopkg_program.map(Path::to_path_buf);
    let mut lease = claimed.lease.clone();
    let mut log_batch = 0_u64;
    submit_log_chunks(
        client,
        base,
        credential,
        &lease,
        &mut log_batch,
        vec![AutoPkgLogChunk {
            stream: AutoPkgLogStream::Stdout,
            bytes: b"AutoPkg attempt started".to_vec(),
        }],
        true,
    )
    .await?;
    let (log_sender, mut log_receiver) = tokio::sync::mpsc::channel(64);
    let mut execution = tokio::spawn(async move {
        if let Some(program) = execution_program {
            AutoPkgAdapter::execute_with_program_and_logs(
                &execution_recipe,
                &execution_root,
                &program,
                log_sender,
            )
            .await
        } else {
            AutoPkgAdapter::execute_with_logs(&execution_recipe, &execution_root, log_sender).await
        }
    });
    let mut heartbeat = tokio::time::interval_at(
        tokio::time::Instant::now() + std::time::Duration::from_secs(20),
        std::time::Duration::from_secs(20),
    );
    let deadline = tokio::time::sleep(std::time::Duration::from_secs(u64::from(
        envelope.execution_timeout_seconds,
    )));
    tokio::pin!(deadline);
    let execution = loop {
        tokio::select! {
            result = &mut execution => {
                break result.context("joining AutoPkg execution task")?;
            }
            _ = heartbeat.tick() => {
                match send_heartbeat(client, base, credential, &lease).await {
                    Ok(updated) => lease = updated,
                    Err(error) => {
                        execution.abort();
                        return Err(error).context("maintaining AutoPkg job lease");
                    }
                }
            }
            () = &mut deadline => {
                execution.abort();
                return submit_failure(
                    client,
                    base,
                    credential,
                    &ClaimedJob { lease, ..claimed },
                    execution_failure(
                        envelope.request.run_id,
                        Some(adapter_name),
                        "execution_timeout",
                        "The builder exceeded the server-issued execution deadline.",
                    ),
                )
                .await;
            }
            chunk = log_receiver.recv(), if !log_receiver.is_closed() => {
                if let Some(chunk) = chunk {
                    let chunks = collect_log_batch(chunk, &mut log_receiver);
                    if let Err(error) = submit_log_chunks(
                        client,
                        base,
                        credential,
                        &lease,
                        &mut log_batch,
                        chunks,
                        false,
                    ).await {
                        execution.abort();
                        return Err(error).context("persisting AutoPkg logs");
                    }
                }
            }
        }
    };
    while let Some(chunk) = log_receiver.recv().await {
        let chunks = collect_log_batch(chunk, &mut log_receiver);
        let lease_snapshot = lease.clone();
        await_with_heartbeats(
            client,
            base,
            credential,
            &mut lease,
            submit_log_chunks(
                client,
                base,
                credential,
                &lease_snapshot,
                &mut log_batch,
                chunks,
                false,
            ),
        )
        .await?
        .context("flushing AutoPkg logs")?;
    }
    match execution {
        Ok(execution) => {
            info!(
                job_id = %claimed.job.id,
                stdout_bytes = execution.stdout_bytes,
                stderr_bytes = execution.stderr_bytes,
                "AutoPkg execution completed"
            );
            let selected = match await_with_heartbeats(
                client,
                base,
                credential,
                &mut lease,
                AutoPkgAdapter::select_outputs(&recipe, &execution.report, &isolation_root),
            )
            .await?
            {
                Ok(selected) => selected,
                Err(error) => {
                    let (code, detail) = autopkg_failure(&error);
                    return submit_failure(
                        client,
                        base,
                        credential,
                        &ClaimedJob { lease, ..claimed },
                        execution_failure(
                            envelope.request.run_id,
                            Some(adapter_name),
                            code,
                            detail,
                        ),
                    )
                    .await;
                }
            };
            let mut built_variants = Vec::with_capacity(selected.variants.len());
            let mut uploaded_artifacts = std::collections::BTreeSet::new();
            for selected_variant in selected.variants {
                let mut artifacts = Vec::with_capacity(selected_variant.artifacts.len());
                for selected_artifact in selected_variant.artifacts {
                    let (digest, size) = await_with_heartbeats(
                        client,
                        base,
                        credential,
                        &mut lease,
                        hash_artifact(&selected_artifact.path),
                    )
                    .await??;
                    let attempt_id = lease.attempt_id;
                    await_with_heartbeats(
                        client,
                        base,
                        credential,
                        &mut lease,
                        upload_attempt_artifact(
                            client,
                            base,
                            credential,
                            attempt_id,
                            &digest,
                            size,
                            &selected_artifact.media_type,
                            selected_artifact.role,
                            &selected_artifact.path,
                        ),
                    )
                    .await??;
                    uploaded_artifacts.insert(digest.clone());
                    artifacts.push(VariantArtifact {
                        digest,
                        size,
                        role: selected_artifact.role,
                    });
                }
                built_variants.push(BuiltVariant {
                    variant_id: None,
                    platform: selected_variant.platform,
                    architecture: selected_variant.architecture,
                    minimum_macos: selected_variant.minimum_macos,
                    maximum_macos: selected_variant.maximum_macos,
                    resolution_priority: selected_variant.resolution_priority,
                    artifacts,
                });
            }
            let captured_at = chrono::Utc::now();
            let raw_report = execution.report;
            let build_result = BuildResult {
                discovered_version: selected.discovered_version,
                variants: built_variants,
                uploaded_artifacts: uploaded_artifacts.into_iter().collect(),
                provenance: Provenance {
                    builder: tools.get("autopkg").map_or_else(
                        || "autopkg".to_owned(),
                        |version| format!("autopkg/{version}"),
                    ),
                    worker_version: env!("CARGO_PKG_VERSION").to_owned(),
                    operating_system: operating_system_version().await,
                    tools: tools.clone(),
                    sources: execution.sources.clone(),
                    recipe_trust_succeeded: selected.recipe_trust_succeeded,
                    raw_report: raw_report.clone(),
                    captured_at,
                },
                verification_results: selected.verification_results,
            };
            let result = BuilderExecutionResult {
                schema_version: BuilderJob::SCHEMA_VERSION,
                run_id,
                adapter: adapter_name,
                sources: execution.sources,
                tools,
                raw_report,
                build_result: Some(build_result),
                completed_at: captured_at,
            };
            submit_success(
                client,
                base,
                credential,
                &ClaimedJob { lease, ..claimed },
                serde_json::to_value(result).context("serializing AutoPkg result")?,
            )
            .await
        }
        Err(error) => {
            let (code, detail) = autopkg_failure(&error);
            submit_failure(
                client,
                base,
                credential,
                &ClaimedJob { lease, ..claimed },
                execution_failure(envelope.request.run_id, Some(adapter_name), code, detail),
            )
            .await
        }
    }
}

async fn await_with_heartbeats<F, T>(
    client: &reqwest::Client,
    base: &str,
    credential: &WorkerCredentialFile,
    lease: &mut stabbur_jobs_core::Lease,
    future: F,
) -> Result<T>
where
    F: Future<Output = T>,
{
    tokio::pin!(future);
    let mut heartbeat = tokio::time::interval_at(
        tokio::time::Instant::now() + std::time::Duration::from_secs(20),
        std::time::Duration::from_secs(20),
    );
    loop {
        tokio::select! {
            result = &mut future => return Ok(result),
            _ = heartbeat.tick() => {
                *lease = send_heartbeat(client, base, credential, lease)
                    .await
                    .context("maintaining job lease during post-processing")?;
            }
        }
    }
}

async fn hash_artifact(path: &std::path::Path) -> Result<(Sha256Digest, u64)> {
    let mut file = tokio::fs::File::open(path)
        .await
        .context("opening selected AutoPkg artifact")?;
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .context("hashing selected AutoPkg artifact")?;
        if read == 0 {
            break;
        }
        size = size
            .checked_add(read as u64)
            .context("artifact size overflow")?;
        hasher.update(&buffer[..read]);
    }
    let digest = Sha256Digest::new(hex::encode(hasher.finalize()))
        .expect("SHA-256 always produces a valid digest");
    Ok((digest, size))
}

const fn artifact_role_name(role: ArtifactRole) -> &'static str {
    match role {
        ArtifactRole::PrimaryInstaller => "primary_installer",
        ArtifactRole::Signature => "signature",
        ArtifactRole::Sbom => "sbom",
        ArtifactRole::DebugSymbols => "debug_symbols",
        ArtifactRole::Metadata => "metadata",
    }
}

#[allow(clippy::too_many_arguments)] // Mirrors the versioned streaming upload protocol fields.
async fn upload_attempt_artifact(
    client: &reqwest::Client,
    base: &str,
    credential: &WorkerCredentialFile,
    attempt_id: stabbur_domain::AttemptId,
    digest: &Sha256Digest,
    size: u64,
    media_type: &str,
    role: ArtifactRole,
    path: &std::path::Path,
) -> Result<()> {
    let file = tokio::fs::File::open(path)
        .await
        .context("opening selected artifact for upload")?;
    let body = reqwest::Body::wrap_stream(ReaderStream::new(file));
    let response = client
        .put(format!(
            "{base}/api/v1/internal/workers/{}/attempts/{attempt_id}/artifacts/{digest}",
            credential.worker_id
        ))
        .bearer_auth(&credential.token)
        .header(reqwest::header::CONTENT_TYPE, media_type)
        .header("x-stabbur-artifact-role", artifact_role_name(role))
        .header(reqwest::header::CONTENT_LENGTH, size)
        .body(body)
        .send()
        .await
        .context("uploading selected AutoPkg artifact")?;
    worker_response(response, "selected AutoPkg artifact upload").await?;
    Ok(())
}

async fn operating_system_version() -> String {
    if cfg!(target_os = "macos")
        && let Ok(output) = tokio::process::Command::new("sw_vers")
            .arg("-productVersion")
            .output()
            .await
        && output.status.success()
    {
        let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !version.is_empty() {
            return format!("macos/{version}");
        }
    }
    std::env::consts::OS.to_owned()
}

fn collect_log_batch(
    first: AutoPkgLogChunk,
    receiver: &mut tokio::sync::mpsc::Receiver<AutoPkgLogChunk>,
) -> Vec<AutoPkgLogChunk> {
    let mut chunks = vec![first];
    let mut bytes = chunks[0].bytes.len();
    while chunks.len() < 32 && bytes < 512 * 1024 {
        let Ok(chunk) = receiver.try_recv() else {
            break;
        };
        bytes += chunk.bytes.len();
        chunks.push(chunk);
    }
    chunks
}

async fn submit_log_chunks(
    client: &reqwest::Client,
    base: &str,
    credential: &WorkerCredentialFile,
    lease: &stabbur_jobs_core::Lease,
    batch: &mut u64,
    chunks: Vec<AutoPkgLogChunk>,
    system: bool,
) -> Result<()> {
    let entries = chunks
        .into_iter()
        .map(|chunk| WorkerLogEntryRequest {
            stream: if system {
                "system"
            } else {
                match chunk.stream {
                    AutoPkgLogStream::Stdout => "stdout",
                    AutoPkgLogStream::Stderr => "stderr",
                }
            }
            .to_owned(),
            message_base64: STANDARD.encode(chunk.bytes),
        })
        .collect();
    let idempotency_key = format!("{}-{batch}", lease.attempt_id);
    let response = client
        .post(format!(
            "{base}/api/v1/internal/workers/{}/logs",
            credential.worker_id
        ))
        .bearer_auth(&credential.token)
        .json(&AppendWorkerLogsRequest {
            lease: lease.clone(),
            idempotency_key,
            entries,
        })
        .send()
        .await
        .context("submitting worker log batch")?;
    worker_response(response, "worker log batch").await?;
    *batch = batch.checked_add(1).context("worker log batch overflow")?;
    Ok(())
}

fn apply_autopkg_parameters(
    recipe: &mut AutoPkgRecipe,
    parameters: BTreeMap<String, BuildParameter>,
) -> Result<(), (&'static str, &'static str)> {
    for (key, value) in parameters {
        let value = match value {
            BuildParameter::String(value) => value,
            BuildParameter::Boolean(value) => value.to_string(),
            BuildParameter::Integer(value) => value.to_string(),
            BuildParameter::List(_) => {
                return Err((
                    "unsupported_autopkg_parameter",
                    "AutoPkg run parameters must be strings, booleans, or integers.",
                ));
            }
        };
        recipe.inputs.insert(key, value);
    }
    recipe.validate().map_err(|_| {
        (
            "invalid_autopkg_definition",
            "The effective AutoPkg definition is invalid.",
        )
    })
}

fn detected_tools(
    availability: &stabbur_builder_autopkg::AutoPkgAvailability,
) -> BTreeMap<String, String> {
    let mut tools = BTreeMap::new();
    if let Some(version) = &availability.autopkg_version {
        tools.insert("autopkg".to_owned(), version.clone());
    }
    if let Some(version) = &availability.xcode_version {
        tools.insert("xcodebuild".to_owned(), version.clone());
    }
    tools
}

fn autopkg_failure(error: &AutoPkgError) -> (&'static str, &'static str) {
    match error {
        AutoPkgError::Unavailable => ("autopkg_unavailable", "AutoPkg is unavailable."),
        AutoPkgError::MaterializationFailed => (
            "autopkg_materialization_failed",
            "A pinned AutoPkg source could not be materialized.",
        ),
        AutoPkgError::ExecutionFailed => ("autopkg_execution_failed", "AutoPkg execution failed."),
        AutoPkgError::RecipeTrustFailed => (
            "autopkg_recipe_trust_failed",
            "AutoPkg recipe trust verification failed.",
        ),
        AutoPkgError::InvalidReport => (
            "autopkg_invalid_report",
            "AutoPkg returned an invalid report.",
        ),
        AutoPkgError::IsolationFailed => (
            "autopkg_isolation_failed",
            "AutoPkg isolation setup failed.",
        ),
        AutoPkgError::CatalogGenerationFailed => (
            "autopkg_catalog_generation_failed",
            "The pinned AutoPkg source could not be normalized into a catalog.",
        ),
        AutoPkgError::InvalidSource
        | AutoPkgError::InvalidCommit
        | AutoPkgError::InvalidRecipe
        | AutoPkgError::InvalidSelector
        | AutoPkgError::SensitiveInputRejected => (
            "invalid_autopkg_definition",
            "The immutable AutoPkg definition is invalid.",
        ),
        AutoPkgError::UnsafeArtifact => (
            "autopkg_unsafe_artifact",
            "AutoPkg selected an artifact outside the isolated attempt directory.",
        ),
        AutoPkgError::SelectorMismatch => (
            "autopkg_selector_mismatch",
            "AutoPkg output did not match the immutable selectors.",
        ),
    }
}

fn execution_failure(
    run_id: stabbur_domain::RunId,
    adapter: Option<String>,
    code: impl Into<String>,
    detail: impl Into<String>,
) -> BuilderExecutionFailure {
    BuilderExecutionFailure {
        schema_version: BuilderJob::SCHEMA_VERSION,
        run_id,
        adapter,
        code: code.into(),
        detail: detail.into(),
        failed_at: chrono::Utc::now(),
    }
}

fn catalog_scan_failure(
    scan_id: RecipeCatalogScanId,
    producer: Option<String>,
    code: impl Into<String>,
    detail: impl Into<String>,
) -> RecipeCatalogScanExecutionFailure {
    RecipeCatalogScanExecutionFailure {
        schema_version: RecipeCatalogScanJob::SCHEMA_VERSION,
        scan_id,
        producer,
        code: code.into(),
        detail: detail.into(),
        failed_at: chrono::Utc::now(),
    }
}

async fn send_heartbeat(
    client: &reqwest::Client,
    base: &str,
    credential: &WorkerCredentialFile,
    lease: &stabbur_jobs_core::Lease,
) -> Result<stabbur_jobs_core::Lease> {
    let response = client
        .post(format!(
            "{base}/api/v1/internal/workers/{}/heartbeat",
            credential.worker_id
        ))
        .bearer_auth(&credential.token)
        .json(&HeartbeatWorkerJobRequest {
            lease: lease.clone(),
            lease_seconds: 60,
        })
        .send()
        .await
        .context("sending worker heartbeat")?;
    worker_response(response, "worker heartbeat")
        .await?
        .json()
        .await
        .context("decoding renewed worker lease")
}

async fn submit_success(
    client: &reqwest::Client,
    base: &str,
    credential: &WorkerCredentialFile,
    claimed: &ClaimedJob,
    result: serde_json::Value,
) -> Result<()> {
    let response = client
        .post(format!(
            "{base}/api/v1/internal/workers/{}/complete",
            credential.worker_id
        ))
        .bearer_auth(&credential.token)
        .json(&CompleteWorkerJobRequest {
            lease: claimed.lease.clone(),
            idempotency_key: claimed.lease.attempt_id.to_string(),
            result,
        })
        .send()
        .await
        .context("submitting outbound worker result")?;
    worker_response(response, "worker result").await?;
    Ok(())
}

async fn submit_failure(
    client: &reqwest::Client,
    base: &str,
    credential: &WorkerCredentialFile,
    claimed: &ClaimedJob,
    failure: BuilderExecutionFailure,
) -> Result<()> {
    let response = client
        .post(format!(
            "{base}/api/v1/internal/workers/{}/fail",
            credential.worker_id
        ))
        .bearer_auth(&credential.token)
        .json(&FailWorkerJobRequest {
            lease: claimed.lease.clone(),
            idempotency_key: claimed.lease.attempt_id.to_string(),
            failure: serde_json::to_value(failure).context("serializing worker failure")?,
        })
        .send()
        .await
        .context("submitting outbound worker failure")?;
    worker_response(response, "worker failure").await?;
    Ok(())
}

async fn submit_catalog_scan_failure(
    client: &reqwest::Client,
    base: &str,
    credential: &WorkerCredentialFile,
    claimed: &ClaimedJob,
    failure: RecipeCatalogScanExecutionFailure,
) -> Result<()> {
    let response = client
        .post(format!(
            "{base}/api/v1/internal/workers/{}/fail",
            credential.worker_id
        ))
        .bearer_auth(&credential.token)
        .json(&FailWorkerJobRequest {
            lease: claimed.lease.clone(),
            idempotency_key: claimed.lease.attempt_id.to_string(),
            failure: serde_json::to_value(failure).context("serializing catalog scan failure")?,
        })
        .send()
        .await
        .context("submitting outbound catalog scan failure")?;
    worker_response(response, "catalog scan failure").await?;
    Ok(())
}

const MAX_WORKER_ERROR_BYTES: usize = 16 * 1024;

#[derive(Debug, Deserialize)]
struct WorkerProblem {
    code: Option<String>,
    detail: Option<String>,
}

async fn worker_response(
    mut response: reqwest::Response,
    operation: &str,
) -> Result<reqwest::Response> {
    if response.status().is_success() {
        return Ok(response);
    }

    let status = response.status();
    let request_id = response
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("reading rejected worker response")?
    {
        if body.len().saturating_add(chunk.len()) > MAX_WORKER_ERROR_BYTES {
            break;
        }
        body.extend_from_slice(&chunk);
    }
    let problem = serde_json::from_slice::<WorkerProblem>(&body).ok();
    let code = problem
        .as_ref()
        .and_then(|problem| problem.code.as_deref())
        .unwrap_or("unavailable");
    let detail = problem
        .as_ref()
        .and_then(|problem| problem.detail.as_deref())
        .unwrap_or("The control plane rejected the request without a problem detail.");
    let request_id = request_id.as_deref().unwrap_or("unavailable");
    bail!(
        "{operation} failed with HTTP {status}; code={code}; request_id={request_id}; detail={detail}"
    )
}

fn base_capabilities() -> Vec<Capability> {
    let mut capabilities = vec![
        Capability::new("runtime.portable").expect("static capability is valid"),
        Capability::new("builder.fake").expect("static capability is valid"),
    ];
    let operating_system = if cfg!(target_os = "macos") {
        "os.macos"
    } else if cfg!(target_os = "linux") {
        "os.linux"
    } else if cfg!(target_os = "windows") {
        "os.windows"
    } else {
        "os.unknown"
    };
    capabilities.push(Capability::new(operating_system).expect("static capability is valid"));
    capabilities
}

fn embedded_capabilities() -> CapabilitySet {
    CapabilitySet::new(base_capabilities())
}

async fn detected_worker_capabilities(autopkg_program: Option<&Path>) -> CapabilitySet {
    detected_worker_environment(autopkg_program).await.0
}

async fn detected_worker_environment(
    autopkg_program: Option<&Path>,
) -> (
    CapabilitySet,
    Option<stabbur_builder_autopkg::AutoPkgAvailability>,
) {
    let mut capabilities = base_capabilities();
    let availability = if cfg!(target_os = "macos") {
        let availability = detect_autopkg(autopkg_program).await;
        capabilities.extend(availability.capabilities().iter().cloned());
        Some(availability)
    } else {
        None
    };
    (CapabilitySet::new(capabilities), availability)
}

async fn detect_autopkg(
    autopkg_program: Option<&Path>,
) -> stabbur_builder_autopkg::AutoPkgAvailability {
    if let Some(program) = autopkg_program {
        AutoPkgAdapter::detect_with_program(program).await
    } else {
        AutoPkgAdapter::detect().await
    }
}

async fn resolve_autopkg_program(program: Option<&Path>) -> Result<Option<PathBuf>> {
    let Some(program) = program else {
        return Ok(None);
    };
    if !program.is_absolute() {
        bail!("configured AutoPkg program must be an absolute path");
    }
    let canonical = tokio::fs::canonicalize(program)
        .await
        .with_context(|| format!("resolving AutoPkg program {}", program.display()))?;
    let metadata = tokio::fs::metadata(&canonical)
        .await
        .with_context(|| format!("reading AutoPkg program metadata {}", canonical.display()))?;
    if !metadata.is_file() {
        bail!("configured AutoPkg program must be a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = metadata.permissions().mode();
        if mode & 0o111 == 0 {
            bail!("configured AutoPkg program is not executable");
        }
        if mode & 0o022 != 0 {
            bail!("configured AutoPkg program must not be group- or world-writable");
        }
    }
    Ok(Some(canonical))
}

/// Reports the capabilities this host would advertise without contacting a control plane.
pub async fn worker_capability_report(autopkg_program: Option<&Path>) -> Result<serde_json::Value> {
    let autopkg_program = resolve_autopkg_program(autopkg_program).await?;
    let (capabilities, availability) =
        detected_worker_environment(autopkg_program.as_deref()).await;
    let tools = availability
        .as_ref()
        .map(detected_tools)
        .unwrap_or_default();
    Ok(serde_json::json!({
        "capabilities": capabilities
            .iter()
            .map(|capability| capability.as_str())
            .collect::<Vec<_>>(),
        "tools": tools,
    }))
}

async fn worker_identity(data_dir: &std::path::Path) -> Result<WorkerId> {
    let path = data_dir.join("worker-id");
    match tokio::fs::read_to_string(&path).await {
        Ok(value) => value
            .trim()
            .parse()
            .context("worker identity file does not contain a UUIDv7"),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let identity = WorkerId::new();
            write_owner_only(&path, &identity.to_string()).await?;
            Ok(identity)
        }
        Err(error) => Err(error).context("reading worker identity file"),
    }
}

async fn read_owner_only(path: &std::path::Path) -> Result<String> {
    let metadata = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("reading metadata for {}", path.display()))?;
    if !metadata.is_file() {
        bail!("worker token input must be a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("worker token file must be owner-only (mode 0600 or stricter)");
        }
    }
    tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("reading worker token file {}", path.display()))
}

async fn prepare_bootstrap_file(storage: &dyn Storage, config: &ServiceConfig) -> Result<()> {
    let (secret, hash) = generate_token();
    match storage
        .prepare_bootstrap(&hash)
        .await
        .context("preparing first-administrator bootstrap")?
    {
        BootstrapPreparation::Created => {
            write_owner_only(&config.bootstrap_secret_path(), secret.expose_secret()).await?;
            // Deliberately log only the path, never the secret.
            info!(path = %config.bootstrap_secret_path().display(), "one-time bootstrap secret created");
        }
        BootstrapPreparation::Pending => {
            if tokio::fs::metadata(config.bootstrap_secret_path())
                .await
                .is_err()
            {
                bail!(
                    "bootstrap is pending but its secret file is missing; use a local break-glass bootstrap command"
                );
            }
        }
        BootstrapPreparation::Disabled => {
            if let Err(error) = tokio::fs::remove_file(config.bootstrap_secret_path()).await
                && error.kind() != ErrorKind::NotFound
            {
                return Err(error).context("removing stale bootstrap secret file");
            }
        }
    }
    Ok(())
}

async fn create_private_directory(path: &std::path::Path) -> Result<()> {
    tokio::fs::create_dir_all(path)
        .await
        .with_context(|| format!("creating data directory {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .await
            .with_context(|| format!("securing data directory {}", path.display()))?;
    }
    Ok(())
}

async fn write_owner_only(path: &std::path::Path, value: &str) -> Result<()> {
    let mut options = tokio::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .await
        .with_context(|| format!("creating owner-only file {}", path.display()))?;
    file.write_all(value.as_bytes())
        .await
        .context("writing owner-only credential file")?;
    file.write_all(b"\n")
        .await
        .context("terminating owner-only credential file")?;
    file.sync_all()
        .await
        .context("syncing owner-only credential file")
}

#[cfg(test)]
mod capability_tests {
    use super::*;

    #[test]
    fn embedded_fake_worker_never_claims_autopkg_jobs() {
        let capabilities = embedded_capabilities();
        assert!(
            capabilities
                .iter()
                .any(|capability| capability.as_str() == "builder.fake")
        );
        assert!(
            !capabilities
                .iter()
                .any(|capability| capability.as_str() == "builder.autopkg")
        );
    }

    #[tokio::test]
    async fn configured_autopkg_program_must_be_absolute() {
        let error = resolve_autopkg_program(Some(Path::new("autopkg")))
            .await
            .expect_err("relative worker-local programs must be rejected");
        assert!(error.to_string().contains("absolute path"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn configured_autopkg_program_must_be_executable_and_not_writable_by_peers() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let program = temporary.path().join("autopkg");
        tokio::fs::write(&program, b"#!/bin/sh\nexit 0\n")
            .await
            .unwrap();

        tokio::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o600))
            .await
            .unwrap();
        let error = resolve_autopkg_program(Some(&program))
            .await
            .expect_err("non-executable programs must be rejected");
        assert!(error.to_string().contains("not executable"));

        tokio::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o722))
            .await
            .unwrap();
        let error = resolve_autopkg_program(Some(&program))
            .await
            .expect_err("peer-writable programs must be rejected");
        assert!(
            error
                .to_string()
                .contains("must not be group- or world-writable")
        );

        tokio::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))
            .await
            .unwrap();
        let resolved = resolve_autopkg_program(Some(&program))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resolved, tokio::fs::canonicalize(program).await.unwrap());
    }
}
