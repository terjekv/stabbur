//! Saved, reviewed batch exports; repository materialization is derived from immutable snapshots.
use super::*;
use sha2::{Digest, Sha256};
use stabbur_domain::{ExportId, ReleaseAvailability, ReleaseState, exports::*};
use stabbur_storage_core::{ExportRecord, ExportSnapshot};

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExportDestinationInput {
    Hosted,
    Download,
}
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExportSourceInput {
    Channel { channel: String },
    Release { release: Uuid },
}
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExportDetectionInput {
    Application { name: String, bundle_id: String },
    Receipt { package_id: String },
}
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormatInput {
    Pkg,
    DmgApp,
}
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ExportSettingsInput {
    pub format: ExportFormatInput,
    pub detection: ExportDetectionInput,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub category: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ExportSelectionInput {
    pub software: Uuid,
    pub source: ExportSourceInput,
    /// Empty means both Mac hardware architectures.
    #[serde(default)]
    pub architectures: Vec<String>,
    pub settings: Option<ExportSettingsInput>,
}
/// Complete saved definition; settings may be omitted in a draft, but block publication.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ExportDefinitionInput {
    pub slug: String,
    pub name: String,
    pub destination: ExportDestinationInput,
    /// Munki catalog name, independently chosen from Stabbur selection channels.
    pub catalog: String,
    pub selections: Vec<ExportSelectionInput>,
}
#[derive(ToSchema)]
pub struct ExportResponse {
    pub id: Uuid,
    pub definition: ExportDefinitionInput,
    pub revision: u64,
    pub generation: u64,
    pub updated_at: chrono::DateTime<Utc>,
}
#[derive(ToSchema)]
pub struct ExportPage {
    pub items: Vec<ExportResponse>,
    pub next_cursor: Option<String>,
}
#[derive(ToSchema)]
pub struct ExportItemResponse {
    pub software: Uuid,
    pub slug: String,
    pub name: String,
    pub release: Uuid,
    pub version: String,
    pub variant: Uuid,
    pub architecture: String,
    pub architectures: Vec<String>,
    pub minimum_macos: Option<String>,
    pub maximum_macos: Option<String>,
    pub digest: String,
    pub size: u64,
    pub settings: ExportSettingsInput,
}
#[derive(ToSchema)]
pub struct ExportSnapshotResponse {
    pub export: Uuid,
    pub generation: u64,
    pub definition_revision: u64,
    pub definition: ExportDefinitionInput,
    pub items: Vec<ExportItemResponse>,
    pub created_at: chrono::DateTime<Utc>,
}
#[derive(ToSchema)]
pub struct ExportSnapshotView {
    pub snapshot: ExportSnapshotResponse,
    /// Withdrawn or rejected releases; clients must not materialize them.
    pub unavailable_releases: Vec<Uuid>,
    /// Destination pkginfo, rendered from the snapshot by the server.
    pub pkginfo: Vec<serde_json::Value>,
}
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ExportChange {
    pub software: Uuid,
    pub name: String,
    /// add, update, unchanged, remove, or blocked.
    pub action: String,
    pub before: Vec<String>,
    pub after: Vec<String>,
    pub detail: Option<String>,
}
#[derive(Serialize, ToSchema)]
pub struct ExportPlanResponse {
    pub export: Uuid,
    pub definition_revision: u64,
    pub published_generation: u64,
    /// Exact comparison token; apply recomputes it and rejects changed facts.
    pub fingerprint: String,
    pub ready: bool,
    pub changes: Vec<ExportChange>,
    #[schema(value_type = Vec<ExportItemResponse>)]
    pub items: Vec<ExportItem>,
}
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyExportRequest {
    pub fingerprint: String,
    pub reviewed: bool,
}
#[derive(ToSchema)]
pub struct ExportHistoryResponse {
    pub items: Vec<ExportSnapshotResponse>,
    pub next_cursor: Option<String>,
}
#[derive(Deserialize, IntoParams)]
pub struct ExportHistoryQuery {
    pub after: Option<u64>,
    pub limit: Option<u32>,
}
#[derive(Serialize, ToSchema)]
pub struct ExportReaderResponse {
    /// One-time repository-only credential. Download, never render or log.
    pub token: String,
}
struct Planned {
    response: ExportPlanResponse,
    publication: Option<PreparedExport>,
}
fn failure(detail: &str, rid: &str) -> ApiError {
    ApiError::validation(detail, vec![], rid)
}
fn missing(rid: &str) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "export_not_found",
        "Export or snapshot does not exist.",
        rid,
    )
}
fn audit_export(principal: &Principal, id: ExportId, action: &str, rid: &str) -> AuditEvent {
    AuditEvent {
        id: AuditEventId::new(),
        actor: AuditActor::Principal(principal.id),
        action: action.into(),
        resource_kind: "export".into(),
        resource_id: Some(id.to_string()),
        details: serde_json::json!({}),
        request_id: Some(rid.into()),
        occurred_at: Utc::now(),
    }
}
fn definition(input: &ExportDefinitionInput, rid: &str) -> Result<ExportDefinition, ApiError> {
    serde_json::from_value(
        serde_json::to_value(input).map_err(|_| failure("Invalid export definition.", rid))?,
    )
    .map_err(|error| failure(&error.to_string(), rid))
}
async fn load(state: &AppState, identity: &str, rid: &str) -> Result<ExportRecord, ApiError> {
    state
        .storage
        .export(identity)
        .await
        .map_err(|e| ApiError::storage(e, rid))?
        .ok_or_else(|| missing(rid))
}
async fn snapshot(
    state: &AppState,
    record: &ExportRecord,
    generation: u64,
    rid: &str,
) -> Result<ExportSnapshot, ApiError> {
    state
        .storage
        .export_snapshot(record.id, generation)
        .await
        .map_err(|e| ApiError::storage(e, rid))?
        .ok_or_else(|| missing(rid))
}
fn available(release: &Release) -> bool {
    release.availability == ReleaseAvailability::Available
        && matches!(release.state, ReleaseState::Testing | ReleaseState::Stable)
}
fn hardware(architecture: Architecture) -> Vec<Architecture> {
    match architecture {
        Architecture::Universal => vec![Architecture::Aarch64, Architecture::X86_64],
        Architecture::Aarch64 | Architecture::X86_64 => vec![architecture],
    }
}
fn overlaps(a: &ExportItem, b: &ExportItem) -> bool {
    a.architectures
        .iter()
        .any(|arch| b.architectures.contains(arch))
        && !matches!((&a.maximum_macos, &b.minimum_macos), (Some(max), Some(min)) if max < min)
        && !matches!((&b.maximum_macos, &a.minimum_macos), (Some(max), Some(min)) if max < min)
}
async fn selected(
    state: &AppState,
    selection: &ExportSelection,
    rid: &str,
) -> Result<(Vec<ExportItem>, ExportBinding), ApiError> {
    let software = state
        .storage
        .software(&selection.software.to_string())
        .await
        .map_err(|e| ApiError::storage(e, rid))?
        .ok_or_else(|| failure("The selected application no longer exists.", rid))?;
    let settings = selection
        .settings
        .as_ref()
        .ok_or_else(|| failure("Review and save installation detection settings.", rid))?;
    let (release_id, channel, pinned_variant) = match &selection.source {
        ExportSource::Channel { channel } => {
            let current = state
                .storage
                .channel(software.id, channel.as_str())
                .await
                .map_err(|e| ApiError::storage(e, rid))?
                .ok_or_else(|| {
                    failure(
                        "Approve a release to the selected channel, or choose another selection.",
                        rid,
                    )
                })?;
            (
                current.release_id,
                Some((channel.clone(), current.revision)),
                current.pinned_variant_id,
            )
        }
        ExportSource::Release { release } => (*release, None, None),
    };
    let release = state
        .storage
        .release(release_id)
        .await
        .map_err(|e| ApiError::storage(e, rid))?
        .ok_or_else(|| failure("The pinned release does not exist.", rid))?;
    if release.software_id != software.id || !available(&release) {
        return Err(failure(
            "Select a release approved for testing or stable that has not been withdrawn.",
            rid,
        ));
    }
    let mut items = Vec::new();
    for variant in state
        .storage
        .release_variants(release.id)
        .await
        .map_err(|e| ApiError::storage(e, rid))?
    {
        if variant.compatibility.platform != Platform::MacOs
            || pinned_variant.is_some_and(|id| id != variant.id)
        {
            continue;
        }
        let architectures: Vec<_> = hardware(variant.compatibility.architecture)
            .into_iter()
            .filter(|a| selection.architectures.is_empty() || selection.architectures.contains(a))
            .collect();
        if architectures.is_empty() {
            continue;
        }
        let artifact = state
            .storage
            .variant_artifacts(variant.id)
            .await
            .map_err(|e| ApiError::storage(e, rid))?
            .into_iter()
            .find(|a| a.role == ArtifactRole::PrimaryInstaller)
            .ok_or_else(|| failure("A selected variant has no primary installer.", rid))?;
        let locations = state
            .storage
            .artifact_locations(&artifact.artifact.digest)
            .await
            .map_err(|e| ApiError::storage(e, rid))?;
        if !locations.iter().any(|l| {
            l.store_id == state.store_id
                && l.state == LocationState::Present
                && l.verified_at.is_some()
        }) {
            return Err(failure(
                "A selected installer is not available in verified storage.",
                rid,
            ));
        }
        if artifact.artifact.size == 0 || artifact.artifact.size > 4 * 1024 * 1024 * 1024 {
            return Err(failure(
                "An installer must contain between 1 byte and 4 GiB.",
                rid,
            ));
        }
        items.push(ExportItem {
            software: software.id,
            slug: software.slug.clone(),
            name: software.name.clone(),
            release: release.id,
            version: release.version.clone(),
            variant: variant.id,
            architecture: variant.compatibility.architecture,
            architectures,
            minimum_macos: variant.compatibility.minimum_macos,
            maximum_macos: variant.compatibility.maximum_macos,
            digest: artifact.artifact.digest,
            size: artifact.artifact.size,
            settings: settings.clone(),
        });
    }
    if items.is_empty() {
        return Err(failure(
            "No installer matches the selected Mac architectures.",
            rid,
        ));
    }
    for (index, item) in items.iter().enumerate() {
        if items[index + 1..].iter().any(|other| overlaps(item, other)) {
            return Err(failure(
                "Variants overlap for the same Mac. Pin an unambiguous channel variant before exporting.",
                rid,
            ));
        }
    }
    items.sort_by_key(|i| i.variant);
    Ok((
        items,
        ExportBinding {
            software: software.id,
            software_revision: software.revision,
            release: release.id,
            release_revision: release.revision,
            channel,
        },
    ))
}
fn versions(items: &[ExportItem]) -> Vec<String> {
    let mut result: Vec<_> = items
        .iter()
        .map(|i| i.version.as_str().to_owned())
        .collect();
    result.sort();
    result.dedup();
    result
}
async fn plan(state: &AppState, record: &ExportRecord, rid: &str) -> Result<Planned, ApiError> {
    let previous = if record.generation == 0 {
        None
    } else {
        Some(snapshot(state, record, record.generation, rid).await?)
    };
    let mut items = Vec::new();
    let mut bindings = Vec::new();
    let mut changes = Vec::new();
    let mut ready = true;
    for selection in &record.definition.data().selections {
        let before: Vec<_> = previous
            .iter()
            .flat_map(|s| &s.items)
            .filter(|i| i.software == selection.software)
            .cloned()
            .collect();
        match selected(state, selection, rid).await {
            Ok((after, binding)) => {
                let action = if before.is_empty() {
                    "add"
                } else if before == after
                    && previous.as_ref().is_some_and(|p| {
                        p.definition.data().catalog == record.definition.data().catalog
                            && p.definition.data().destination
                                == record.definition.data().destination
                    })
                {
                    "unchanged"
                } else {
                    "update"
                };
                changes.push(ExportChange {
                    software: selection.software.as_uuid(),
                    name: after[0].name.clone(),
                    action: action.into(),
                    before: versions(&before),
                    after: versions(&after),
                    detail: None,
                });
                items.extend(after);
                bindings.push(binding);
            }
            Err(error) => {
                if actix_web::ResponseError::status_code(&error).is_server_error() {
                    return Err(error);
                }
                // Validation failures are actionable blockers in a read-only preview.
                let problem = serde_json::to_value(error.problem()).unwrap_or_default();
                let name = state
                    .storage
                    .software(&selection.software.to_string())
                    .await
                    .map_err(|e| ApiError::storage(e, rid))?
                    .map_or_else(|| selection.software.to_string(), |s| s.name);
                changes.push(ExportChange {
                    software: selection.software.as_uuid(),
                    name,
                    action: "blocked".into(),
                    before: versions(&before),
                    after: vec![],
                    detail: Some(
                        problem["detail"]
                            .as_str()
                            .unwrap_or("This application is not ready to export.")
                            .to_owned(),
                    ),
                });
                ready = false;
            }
        }
    }
    let mut removed = std::collections::BTreeSet::new();
    for item in previous.iter().flat_map(|s| &s.items) {
        if !record
            .definition
            .data()
            .selections
            .iter()
            .any(|s| s.software == item.software)
            && removed.insert(item.software)
        {
            let before: Vec<_> = previous
                .iter()
                .flat_map(|s| &s.items)
                .filter(|i| i.software == item.software)
                .cloned()
                .collect();
            changes.push(ExportChange {
                software: item.software.as_uuid(),
                name: item.name.clone(),
                action: "remove".into(),
                before: versions(&before),
                after: vec![],
                detail: None,
            });
        }
    }
    let fingerprint = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(record, &items, &bindings, &changes))
                .map_err(|_| failure("Cannot encode export preview.", rid))?
        )
    );
    let publication = if ready {
        Some(
            PreparedExport::new(
                record.id,
                record.revision,
                record.generation,
                items.clone(),
                bindings,
            )
            .map_err(|e| failure(e, rid))?,
        )
    } else {
        None
    };
    Ok(Planned {
        response: ExportPlanResponse {
            export: record.id.as_uuid(),
            definition_revision: record.revision,
            published_generation: record.generation,
            fingerprint,
            ready,
            changes,
            items,
        },
        publication,
    })
}
/// Standard Munki metadata is rendered once by the server for both UI and CLI exports.
pub(crate) fn export_pkginfo(item: &ExportItem, catalog: &str) -> serde_json::Value {
    let settings = item.settings.data();
    let mut info = serde_json::json!({"name":item.slug,"display_name":if settings.display_name.is_empty() { &item.name } else { &settings.display_name },"description":settings.description,"category":settings.category,"version":item.version,"catalogs":[catalog],"installer_item_location":format!("{}.{}",item.digest,settings.format.extension()),"installer_item_hash":item.digest,"installer_item_size":item.size.div_ceil(1024),"unattended_install":false,"supported_architectures":item.architectures.iter().map(|a| if *a == Architecture::Aarch64 {"arm64"} else {"x86_64"}).collect::<Vec<_>>()});
    if let Some(v) = &item.minimum_macos {
        info["minimum_os_version"] = serde_json::json!(v);
    }
    if let Some(v) = &item.maximum_macos {
        info["maximum_os_version"] = serde_json::json!(v);
    }
    match &settings.detection {
        InstalledState::Application { name, bundle_id } => {
            info["installs"] = serde_json::json!([{"type":"application","path":format!("/Applications/{name}"),"CFBundleIdentifier":bundle_id,"CFBundleShortVersionString":item.version,"version_comparison_key":"CFBundleShortVersionString"}]);
            if settings.format == InstallerFormat::DmgApp {
                info["installer_type"] = serde_json::json!("copy_from_dmg");
                info["items_to_copy"] =
                    serde_json::json!([{"source_item":name,"destination_path":"/Applications"}]);
            }
        }
        InstalledState::Receipt { package_id } => {
            info["receipts"] = serde_json::json!([{"packageid":package_id,"version":item.version}]);
        }
    }
    info
}
fn xml(value: &serde_json::Value, rid: &str) -> Result<Vec<u8>, ApiError> {
    let value: plist::Value = serde_json::from_value(value.clone())
        .map_err(|_| failure("Invalid repository metadata.", rid))?;
    let mut bytes = Vec::new();
    value
        .to_writer_xml(&mut bytes)
        .map_err(|_| failure("Cannot encode repository metadata.", rid))?;
    Ok(bytes)
}
async fn unavailable(
    state: &AppState,
    snapshot: &ExportSnapshot,
    rid: &str,
) -> Result<Vec<ReleaseId>, ApiError> {
    let mut denied = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for item in &snapshot.items {
        if seen.insert(item.release)
            && !state
                .storage
                .release(item.release)
                .await
                .map_err(|e| ApiError::storage(e, rid))?
                .as_ref()
                .is_some_and(available)
        {
            denied.push(item.release);
        }
    }
    Ok(denied)
}
async fn view(
    state: &AppState,
    snapshot: ExportSnapshot,
    rid: &str,
) -> Result<serde_json::Value, ApiError> {
    let denied = unavailable(state, &snapshot, rid).await?;
    Ok(
        serde_json::json!({"pkginfo":snapshot.items.iter().map(|i| export_pkginfo(i, snapshot.definition.data().catalog.as_str())).collect::<Vec<_>>(),"snapshot":snapshot,"unavailable_releases":denied}),
    )
}

#[utoipa::path(get, path="/api/v1/exports", tag="exports", params(PageQuery), security(("bearer_auth"=[])), responses((status=200, description="Saved export page", body=ExportPage)))]
#[actix_web::get("/exports")]
pub(crate) async fn list_exports(
    request: HttpRequest,
    state: web::Data<AppState>,
    query: web::Query<PageQuery>,
) -> Result<HttpResponse, ApiError> {
    authenticate(&request, &state, Permission::SoftwareRead).await?;
    let rid = request_id(&request);
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let items = state
        .storage
        .list_exports(
            decode_cursor(query.cursor.as_deref(), &rid)?.as_deref(),
            limit,
        )
        .await
        .map_err(|e| ApiError::storage(e, &rid))?;
    let next_cursor = if items.len() == limit as usize {
        items
            .last()
            .map(|e| URL_SAFE_NO_PAD.encode(e.id.to_string()))
    } else {
        None
    };
    Ok(HttpResponse::Ok().json(serde_json::json!({"items":items,"next_cursor":next_cursor})))
}
#[utoipa::path(post, path="/api/v1/exports", tag="exports", request_body=ExportDefinitionInput, security(("bearer_auth"=[])), responses((status=201, description="Saved draft export", body=ExportResponse)))]
#[actix_web::post("/exports")]
pub(crate) async fn create_export(
    request: HttpRequest,
    state: web::Data<AppState>,
    body: web::Json<ExportDefinitionInput>,
) -> Result<HttpResponse, ApiError> {
    let principal = authenticate(&request, &state, Permission::ReleasePromote).await?;
    let rid = request_id(&request);
    let id = ExportId::new();
    let definition = definition(&body, &rid)?;
    let record = state
        .storage
        .create_export(
            id,
            &definition,
            &audit_export(&principal, id, "export.created", &rid),
        )
        .await
        .map_err(|e| ApiError::storage(e, &rid))?;
    Ok(HttpResponse::Created()
        .insert_header((header::ETAG, revision_etag(record.revision)))
        .json(record))
}
#[utoipa::path(get, path="/api/v1/exports/{export}", tag="exports", params(("export"=String, Path)), security(("bearer_auth"=[])), responses((status=200, description="Saved definition and published generation", body=ExportResponse)))]
#[actix_web::get("/exports/{export}")]
pub(crate) async fn get_export(
    request: HttpRequest,
    state: web::Data<AppState>,
    identity: web::Path<String>,
) -> Result<HttpResponse, ApiError> {
    authenticate(&request, &state, Permission::SoftwareRead).await?;
    let record = load(&state, &identity, &request_id(&request)).await?;
    Ok(HttpResponse::Ok()
        .insert_header((header::ETAG, revision_etag(record.revision)))
        .json(record))
}
#[utoipa::path(put, path="/api/v1/exports/{export}", tag="exports", params(("export"=String, Path),("If-Match"=String, Header)), request_body=ExportDefinitionInput, security(("bearer_auth"=[])), responses((status=200, description="New draft definition revision; publication unchanged", body=ExportResponse),(status=412, description="Stale definition", body=Problem)))]
#[actix_web::put("/exports/{export}")]
pub(crate) async fn update_export(
    request: HttpRequest,
    state: web::Data<AppState>,
    identity: web::Path<String>,
    body: web::Json<ExportDefinitionInput>,
) -> Result<HttpResponse, ApiError> {
    let principal = authenticate(&request, &state, Permission::ReleasePromote).await?;
    let rid = request_id(&request);
    let current = load(&state, &identity, &rid).await?;
    let record = state
        .storage
        .update_export(
            current.id,
            &definition(&body, &rid)?,
            expected_revision(&request, &rid)?,
            &audit_export(&principal, current.id, "export.updated", &rid),
        )
        .await
        .map_err(|e| ApiError::storage(e, &rid))?;
    Ok(HttpResponse::Ok()
        .insert_header((header::ETAG, revision_etag(record.revision)))
        .json(record))
}
#[utoipa::path(post, path="/api/v1/exports/{export}/plan", tag="exports", params(("export"=String, Path)), security(("bearer_auth"=[])), responses((status=200, description="Read-only batch preview with blockers and exact fingerprint", body=ExportPlanResponse)))]
#[actix_web::post("/exports/{export}/plan")]
pub(crate) async fn plan_export(
    request: HttpRequest,
    state: web::Data<AppState>,
    identity: web::Path<String>,
) -> Result<HttpResponse, ApiError> {
    authenticate(&request, &state, Permission::SoftwareRead).await?;
    let rid = request_id(&request);
    let record = load(&state, &identity, &rid).await?;
    Ok(HttpResponse::Ok().json(plan(&state, &record, &rid).await?.response))
}
/// All installer bytes are bounded and verified before the publication transaction.
async fn verify_installers(
    state: &AppState,
    publication: &PreparedExport,
    rid: &str,
) -> Result<(), ApiError> {
    let mut seen = std::collections::BTreeSet::new();
    for item in publication.items() {
        let format = item.settings.data().format;
        if !seen.insert((item.digest.clone(), format.extension())) {
            continue;
        }
        let mut read = state
            .store
            .read(&item.digest, None)
            .await
            .map_err(|e| ApiError::store(e, rid))?;
        let mut hash = Sha256::new();
        let mut size = 0u64;
        let mut prefix: Vec<u8> = Vec::new();
        let mut tail = Vec::new();
        while let Some(chunk) = read.stream.next().await {
            let chunk = chunk.map_err(|e| ApiError::store(e, rid))?;
            size = size
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| failure("Installer exceeds its expected size.", rid))?;
            if size > item.size {
                return Err(failure("Installer exceeds its expected size.", rid));
            }
            hash.update(&chunk);
            if prefix.len() < 4 {
                prefix.extend(chunk.iter().take(4 - prefix.len()));
            }
            if chunk.len() >= 512 {
                tail.clear();
                tail.extend_from_slice(&chunk[chunk.len() - 512..]);
            } else {
                tail.extend_from_slice(&chunk);
                if tail.len() > 512 {
                    tail.drain(..tail.len() - 512);
                }
            }
        }
        let marker = match format {
            InstallerFormat::Pkg => prefix == b"xar!",
            InstallerFormat::DmgApp => tail.len() == 512 && tail.starts_with(b"koly"),
        };
        if size != item.size || format!("{:x}", hash.finalize()) != item.digest.as_str() || !marker
        {
            return Err(failure(
                "An installer failed checksum, size, or format verification. The published export was not changed.",
                rid,
            ));
        }
    }
    Ok(())
}
#[utoipa::path(post, path="/api/v1/exports/{export}/apply", tag="exports", params(("export"=String, Path)), request_body=ApplyExportRequest, security(("bearer_auth"=[])), responses((status=200, description="Entire reviewed snapshot published atomically", body=ExportSnapshotView),(status=409, description="Preview changed or contains blockers", body=Problem)))]
#[actix_web::post("/exports/{export}/apply")]
pub(crate) async fn apply_export(
    request: HttpRequest,
    state: web::Data<AppState>,
    identity: web::Path<String>,
    body: web::Json<ApplyExportRequest>,
) -> Result<HttpResponse, ApiError> {
    let principal = authenticate(&request, &state, Permission::ReleasePromote).await?;
    let rid = request_id(&request);
    if !body.reviewed {
        return Err(failure(
            "Review the entire export before applying it.",
            &rid,
        ));
    }
    let record = load(&state, &identity, &rid).await?;
    let planned = plan(&state, &record, &rid).await?;
    if body.fingerprint != planned.response.fingerprint || !planned.response.ready {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "export_plan_changed",
            "The export changed or is blocked. Review a fresh preview.",
            &rid,
        ));
    }
    let publication = planned
        .publication
        .ok_or_else(|| failure("Export is not ready.", &rid))?;
    verify_installers(&state, &publication, &rid).await?;
    // Reauthorize after potentially long verification; storage fences every selected mutable fact.
    authenticate(&request, &state, Permission::ReleasePromote).await?;
    let published = state
        .storage
        .apply_export(
            &publication,
            &audit_export(&principal, record.id, "export.published", &rid),
        )
        .await
        .map_err(|e| ApiError::storage(e, &rid))?;
    Ok(HttpResponse::Ok().json(view(&state, published, &rid).await?))
}
#[utoipa::path(get, path="/api/v1/exports/{export}/snapshots/{generation}", tag="exports", params(("export"=String, Path),("generation"=u64, Path)), security(("bearer_auth"=[])), responses((status=200, description="Immutable snapshot with current withdrawal eligibility", body=ExportSnapshotView)))]
#[actix_web::get("/exports/{export}/snapshots/{generation}")]
pub(crate) async fn get_export_snapshot(
    request: HttpRequest,
    state: web::Data<AppState>,
    path: web::Path<(String, u64)>,
) -> Result<HttpResponse, ApiError> {
    authenticate(&request, &state, Permission::SoftwareRead).await?;
    let rid = request_id(&request);
    let record = load(&state, &path.0, &rid).await?;
    Ok(HttpResponse::Ok()
        .json(view(&state, snapshot(&state, &record, path.1, &rid).await?, &rid).await?))
}
#[utoipa::path(get, path="/api/v1/exports/{export}/history", tag="exports", params(("export"=String, Path),ExportHistoryQuery), security(("bearer_auth"=[])), responses((status=200, description="Append-only publication history", body=ExportHistoryResponse)))]
#[actix_web::get("/exports/{export}/history")]
pub(crate) async fn list_export_history(
    request: HttpRequest,
    state: web::Data<AppState>,
    identity: web::Path<String>,
    query: web::Query<ExportHistoryQuery>,
) -> Result<HttpResponse, ApiError> {
    authenticate(&request, &state, Permission::SoftwareRead).await?;
    let rid = request_id(&request);
    let record = load(&state, &identity, &rid).await?;
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let items = state
        .storage
        .export_history(record.id, query.after.unwrap_or(0), limit)
        .await
        .map_err(|e| ApiError::storage(e, &rid))?;
    let cursor = if items.len() == limit as usize {
        items.last().map(|s| s.generation.to_string())
    } else {
        None
    };
    Ok(HttpResponse::Ok().json(serde_json::json!({"items":items,"next_cursor":cursor})))
}
#[utoipa::path(post, path="/api/v1/exports/{export}/readers", tag="exports", params(("export"=String, Path)), security(("bearer_auth"=[])), responses((status=201, description="One-time export-only device credential", body=ExportReaderResponse)))]
#[actix_web::post("/exports/{export}/readers")]
pub(crate) async fn create_export_reader(
    request: HttpRequest,
    state: web::Data<AppState>,
    identity: web::Path<String>,
) -> Result<HttpResponse, ApiError> {
    let principal = authenticate(&request, &state, Permission::AuthManage).await?;
    let rid = request_id(&request);
    let record = load(&state, &identity, &rid).await?;
    let published = snapshot(&state, &record, record.generation, &rid).await?;
    if published.definition.data().destination != ExportDestination::Hosted {
        return Err(failure(
            "This export produces files for an existing repository.",
            &rid,
        ));
    }
    let (secret, hash) = generate_token();
    state
        .storage
        .create_export_reader(
            record.id,
            &hash,
            &audit_export(&principal, record.id, "export.reader_created", &rid),
        )
        .await
        .map_err(|e| ApiError::storage(e, &rid))?;
    Ok(HttpResponse::Created()
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .json(ExportReaderResponse {
            token: secret.expose_secret().into(),
        }))
}
#[utoipa::path(post, path="/api/v1/exports/{export}/readers/revoke", tag="exports", params(("export"=String, Path)), security(("bearer_auth"=[])), responses((status=204, description="All earlier device profiles revoked")))]
#[actix_web::post("/exports/{export}/readers/revoke")]
pub(crate) async fn revoke_export_readers(
    request: HttpRequest,
    state: web::Data<AppState>,
    identity: web::Path<String>,
) -> Result<HttpResponse, ApiError> {
    let principal = authenticate(&request, &state, Permission::AuthManage).await?;
    let rid = request_id(&request);
    let record = load(&state, &identity, &rid).await?;
    state
        .storage
        .revoke_export_readers(
            record.id,
            &audit_export(&principal, record.id, "export.readers_revoked", &rid),
        )
        .await
        .map_err(|e| ApiError::storage(e, &rid))?;
    Ok(HttpResponse::NoContent().finish())
}
#[utoipa::path(get, path="/api/v1/exports/{export}/repository/{kind}/{name}", tag="exports", params(("export"=String, Path),("kind"=String, Path),("name"=String, Path)), security(("export_reader"=[])), responses((status=200, description="Published catalog, manifest, pkginfo, or installer bytes", content_type="application/octet-stream"),(status=401, description="Export-only Basic credential required", body=Problem)))]
#[actix_web::get("/exports/{export}/repository/{kind}/{name}")]
pub(crate) async fn export_repository(
    request: HttpRequest,
    state: web::Data<AppState>,
    path: web::Path<(String, String, String)>,
) -> Result<HttpResponse, ApiError> {
    let rid = request_id(&request);
    let record = load(&state, &path.0, &rid).await?;
    let secret = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Basic "))
        .and_then(|s| STANDARD.decode(s).ok())
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|s| s.strip_prefix("stabbur:").map(str::to_owned))
        .filter(|s| s.len() <= 200)
        .ok_or_else(|| ApiError::unauthorized(&rid))?;
    if !state
        .storage
        .export_reader_valid(record.id, &TokenHash::from_secret(&secret))
        .await
        .map_err(|e| ApiError::storage(e, &rid))?
    {
        return Err(ApiError::unauthorized(&rid));
    }
    let published = snapshot(&state, &record, record.generation, &rid).await?;
    if published.definition.data().destination != ExportDestination::Hosted {
        return Err(missing(&rid));
    }
    let denied = unavailable(&state, &published, &rid).await?;
    let entries: Vec<_> = published
        .items
        .iter()
        .filter(|i| !denied.contains(&i.release))
        .collect();
    let catalog = published.definition.data().catalog.as_str();
    let mut response = HttpResponse::Ok();
    response.insert_header((header::CACHE_CONTROL, "no-store"));
    match (path.1.as_str(), path.2.as_str()) {
        ("catalogs", name) if name == catalog || name == "all" => {
            Ok(response.content_type("application/x-plist").body(xml(
                &serde_json::json!(
                    entries
                        .iter()
                        .map(|i| export_pkginfo(i, catalog))
                        .collect::<Vec<_>>()
                ),
                &rid,
            )?))
        }
        ("manifests", name) => {
            let mut software: Vec<_> = entries
                .iter()
                .filter(|i| name == "site_default" || name == "test-all" || i.slug.as_str() == name)
                .map(|i| i.slug.as_str())
                .collect();
            software.sort_unstable();
            software.dedup();
            if !matches!(name, "site_default" | "test-all") && software.is_empty() {
                return Err(missing(&rid));
            }
            let key = if name == "site_default" {
                "optional_installs"
            } else {
                "managed_installs"
            };
            Ok(response.content_type("application/x-plist").body(xml(
                &serde_json::json!({"catalogs":[catalog],key:software}),
                &rid,
            )?))
        }
        ("pkgsinfo", name) => {
            let item = entries
                .iter()
                .find(|i| format!("{}-{}.plist", i.slug, i.variant) == name)
                .ok_or_else(|| missing(&rid))?;
            Ok(response
                .content_type("application/x-plist")
                .body(xml(&export_pkginfo(item, catalog), &rid)?))
        }
        ("pkgs", name) => {
            let item = entries
                .iter()
                .find(|i| format!("{}.{}", i.digest, i.settings.data().format.extension()) == name)
                .ok_or_else(|| missing(&rid))?;
            let read = state
                .store
                .read(&item.digest, None)
                .await
                .map_err(|e| ApiError::store(e, &rid))?;
            if read.object.size != item.size {
                return Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "export_content_unavailable",
                    "Published installer is not readable.",
                    &rid,
                ));
            }
            Ok(response
                .insert_header((header::CONTENT_LENGTH, item.size.to_string()))
                .content_type("application/octet-stream")
                .streaming(read.stream))
        }
        _ => Err(missing(&rid)),
    }
}
