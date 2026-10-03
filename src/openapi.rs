//! Utoipa OpenAPI generation for the committed server contract.

use utoipa::{Modify, OpenApi};

use crate::{
    api,
    error::{Problem, ValidationError},
};

/// Released Stabbur HTTP contract.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Stabbur API",
        version = "0.0.1",
        description = "Software artifact control plane and outbound worker coordination API"
    ),
    paths(
        api::list_exports,
        api::create_export,
        api::get_export,
        api::update_export,
        api::plan_export,
        api::apply_export,
        api::get_export_snapshot,
        api::list_export_history,
        api::create_export_reader,
        api::revoke_export_readers,
        api::export_repository,
        api::healthz,
        api::readyz,
        api::openapi_document,
        api::bootstrap,
        api::login,
        api::me,
        api::list_principals,
        api::create_principal,
        api::get_principal,
        api::update_principal,
        api::change_password,
        api::reset_principal_password,
        api::revoke_principal_sessions,
        api::list_api_tokens,
        api::create_api_token,
        api::revoke_api_token,
        api::list_roles,
        api::create_role,
        api::provision_worker,
        api::list_workers,
        api::get_worker,
        api::update_worker,
        api::rotate_worker_token,
        api::create_recipe,
        api::list_recipes,
        api::get_recipe,
        api::create_recipe_revision,
        api::list_recipe_revisions,
        api::list_recipe_runs,
        api::list_recipe_catalog_snapshots,
        api::get_recipe_catalog_snapshot,
        api::resolve_recipe_catalog_entry,
        api::create_recipe_catalog_scan,
        api::list_recipe_catalog_scans,
        api::get_recipe_catalog_scan,
        api::cancel_recipe_catalog_scan,
        api::create_build_target,
        api::list_build_targets,
        api::get_build_target,
        api::update_build_target,
        api::list_build_target_runs,
        api::trigger_build_target,
        api::create_run,
        api::list_runs,
        api::get_run,
        api::cancel_run,
        api::list_run_logs,
        api::stream_run_events,
        api::list_jobs,
        api::get_job,
        api::list_software,
        api::software_library,
        api::create_software,
        api::get_software,
        api::update_software,
        api::list_releases,
        api::get_release,
        api::list_release_variants,
        api::list_channels,
        api::get_channel,
        api::promote_channel,
        api::reject_release,
        api::withdraw_release,
        api::drain_worker,
        api::software_status,
        api::operational_status,
        api::resolve_software,
        api::get_artifact,
        api::list_artifact_locations,
        api::list_stores,
        api::get_store,
        api::test_store,
        api::list_audit_events,
        api::upload_artifact_content,
        api::download_artifact_content,
        api::head_artifact_content
    ),
    components(schemas(
        Problem,
        ValidationError,
        api::DrainWorkerRequest,
        api::SoftwareStatusResponse,
        api::LibraryPageResponse,
        api::OperationalStatusResponse,
        api::ReleaseAvailabilityResponse,
        api::BootstrapRequest,
        api::PrincipalResponse,
        api::PrincipalAdminResponse,
        api::PrincipalPage,
        api::CreatePrincipalRequest,
        api::UpdatePrincipalRequest,
        api::ChangePasswordRequest,
        api::ResetPasswordRequest,
        api::ApiTokenResponse,
        api::ApiTokenList,
        api::CreateApiTokenRequest,
        api::CreatedApiTokenResponse,
        api::RoleResponse,
        api::RoleList,
        api::CreateRoleRequest,
        api::LoginRequest,
        api::LoginResponse,
        api::CreateSoftwareRequest,
        api::InstallationMetadata,
        api::UpdateSoftwareRequest,
        api::SoftwareResponse,
        api::SoftwarePage,
        api::ArtifactResponse,
        api::ArtifactLocationResponse,
        api::ArtifactLocationList,
        api::UploadArtifactResponse,
        api::StoreResponse,
        api::StoreList,
        api::StoreTestResponse,
        api::ReleaseResponse,
        api::ReleasePage,
        api::VariantArtifactResponse,
        api::VariantResponse,
        api::VariantList,
        api::ChannelResponse,
        api::ChannelList,
        api::PromoteChannelRequest,
        api::RejectReleaseRequest,
        api::ResolutionResponse,
        api::AuditEventResponse,
        api::AuditPage,
        api::ProvisionWorkerRequest,
        api::WorkerCredentialResponse,
        api::WorkerResponse,
        api::WorkerPage,
        api::UpdateWorkerRequest,
        api::RotatedWorkerCredentialResponse,
        api::CreateRecipeRequest,
        api::CreateRecipeRevisionRequest,
        api::RecipeResponse,
        api::RecipePage,
        api::RecipeRevisionResponse,
        api::RecipeRevisionList,
        api::RecipeCatalogSourceResponse,
        api::RecipeCatalogEntryResponse,
        api::RecipeCatalogGuidanceResponse,
        api::RecipePurposeResponse,
        api::RecipeCatalogDiagnosticSeverityResponse,
        api::RecipeCatalogDiagnosticResponse,
        api::RecipeCatalogManifestResponse,
        api::RecipeCatalogSnapshotSummaryResponse,
        api::RecipeCatalogSnapshotResponse,
        api::RecipeCatalogSnapshotPage,
        api::RecipeCatalogMatchResponse,
        api::RecipeCatalogLookupResponse,
        api::RecipeCatalogScanSourceRequest,
        api::CreateRecipeCatalogScanRequest,
        api::RecipeCatalogScanFailureResponse,
        api::RecipeCatalogScanResponse,
        api::RecipeCatalogScanPage,
        api::BuildTargetSchedulePayload,
        api::CreateBuildTargetRequest,
        api::UpdateBuildTargetRequest,
        api::BuildTargetResponse,
        api::BuildTargetPage,
        api::CreateRunRequest,
        api::RunResponse,
        api::RunSummaryResponse,
        api::RunPage,
        api::RunLogResponse,
        api::RunLogPage,
        api::JobResponse,
        api::JobSummaryResponse,
        api::JobPage
    )),
    modifiers(&Security),
    tags(
        (name = "auth", description = "Authentication and principals"),
        (name = "software", description = "Software catalog"),
        (name = "releases", description = "Release lifecycle and rejection"),
        (name = "variants", description = "Compatibility-specific release outputs"),
        (name = "channels", description = "Transactional promotion targets"),
        (name = "resolver", description = "Deterministic installer resolution"),
        (name = "artifacts", description = "Immutable artifact metadata and content"),
        (name = "stores", description = "Artifact store inspection and health"),
        (name = "recipes", description = "Immutable builder recipes and revisions"),
        (name = "recipe-catalogs", description = "Worker-observed pinned recipe catalogs"),
        (name = "build-targets", description = "Desired manual and recurring build policy"),
        (name = "runs", description = "Durable scheduled builder runs"),
        (name = "workers", description = "Worker provisioning and administration"),
        (name = "jobs", description = "Builder-neutral scheduling state"),
        (name = "audit", description = "Append-only audit evidence"),
        (name = "exports", description = "Saved selection, batch preview, atomic publication and Munki delivery"),
        (name = "system", description = "Service metadata")
    )
)]
pub struct ApiDoc;

struct Security;

impl Modify for Security {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        use utoipa::openapi::security::{Http, HttpAuthScheme, SecurityScheme};

        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "export_reader",
                SecurityScheme::Http(Http::new(HttpAuthScheme::Basic)),
            );
            components.add_security_scheme(
                "bearer_auth",
                SecurityScheme::Http(Http::new(HttpAuthScheme::Bearer)),
            );
        }
    }
}

/// Produces deterministic pretty JSON with a trailing newline.
#[must_use]
pub fn json() -> String {
    let mut output = ApiDoc::openapi()
        .to_pretty_json()
        .expect("the statically derived OpenAPI document is serializable");
    output.push('\n');
    output
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn released_operation_set_is_complete_and_excludes_worker_protocol() {
        let document: serde_json::Value = serde_json::from_str(&json()).unwrap();
        let actual = document["paths"]
            .as_object()
            .unwrap()
            .iter()
            .flat_map(|(path, operations)| {
                operations
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(move |method| format!("{method} {path}"))
            })
            .collect::<BTreeSet<_>>();
        let expected = [
            "get /api/v1/exports",
            "post /api/v1/exports",
            "get /api/v1/exports/{export}",
            "put /api/v1/exports/{export}",
            "post /api/v1/exports/{export}/plan",
            "post /api/v1/exports/{export}/apply",
            "get /api/v1/exports/{export}/snapshots/{generation}",
            "get /api/v1/exports/{export}/history",
            "post /api/v1/exports/{export}/readers",
            "post /api/v1/exports/{export}/readers/revoke",
            "get /api/v1/exports/{export}/repository/{kind}/{name}",
            "delete /api/v1/auth/tokens/{token}",
            "get /api/v1/artifacts/{digest}",
            "get /api/v1/artifacts/{digest}/content",
            "get /api/v1/artifacts/{digest}/locations",
            "get /api/v1/audit",
            "get /api/v1/auth/me",
            "get /api/v1/auth/principals",
            "get /api/v1/auth/principals/{principal}",
            "get /api/v1/auth/principals/{principal}/tokens",
            "get /api/v1/auth/roles",
            "get /api/v1/build-targets",
            "get /api/v1/build-targets/{target}",
            "get /api/v1/build-targets/{target}/runs",
            "get /api/v1/jobs",
            "get /api/v1/jobs/{job}",
            "get /api/v1/openapi.json",
            "get /api/v1/recipe-catalog-entries",
            "get /api/v1/recipe-catalog-scans",
            "get /api/v1/recipe-catalog-scans/{scan}",
            "get /api/v1/recipe-catalogs",
            "get /api/v1/recipe-catalogs/{snapshot}",
            "get /api/v1/recipes",
            "get /api/v1/recipes/{recipe}",
            "get /api/v1/recipes/{recipe}/revisions",
            "get /api/v1/recipes/{recipe}/runs",
            "get /api/v1/releases/{release}",
            "get /api/v1/releases/{release}/variants",
            "get /api/v1/runs",
            "get /api/v1/runs/{run}",
            "get /api/v1/runs/{run}/events",
            "get /api/v1/runs/{run}/logs",
            "get /api/v1/software",
            "get /api/v1/library/software",
            "get /api/v1/software/{software}",
            "get /api/v1/software/{software}/channels",
            "get /api/v1/software/{software}/channels/{channel}",
            "get /api/v1/software/{software}/releases",
            "get /api/v1/software/{software}/resolve",
            "get /api/v1/stores",
            "get /api/v1/stores/{store}",
            "get /api/v1/workers",
            "get /api/v1/workers/{worker}",
            "get /healthz",
            "get /api/v1/software/{software}/status",
            "get /api/v1/operations/status",
            "get /readyz",
            "head /api/v1/artifacts/{digest}/content",
            "patch /api/v1/auth/principals/{principal}",
            "patch /api/v1/build-targets/{target}",
            "patch /api/v1/software/{software}",
            "patch /api/v1/workers/{worker}",
            "post /api/v1/auth/bootstrap",
            "post /api/v1/auth/login",
            "post /api/v1/auth/password",
            "post /api/v1/auth/principals",
            "post /api/v1/auth/principals/{principal}/password",
            "post /api/v1/auth/principals/{principal}/revoke-sessions",
            "post /api/v1/auth/principals/{principal}/tokens",
            "post /api/v1/auth/roles",
            "post /api/v1/build-targets",
            "post /api/v1/build-targets/{target}/runs",
            "post /api/v1/recipe-catalog-scans",
            "post /api/v1/recipe-catalog-scans/{scan}/cancel",
            "post /api/v1/recipes",
            "post /api/v1/recipes/{recipe}/revisions",
            "post /api/v1/releases/{release}/reject",
            "post /api/v1/releases/{release}/withdraw",
            "post /api/v1/workers/{worker}/drain",
            "post /api/v1/runs",
            "post /api/v1/runs/{run}/cancel",
            "post /api/v1/software",
            "post /api/v1/stores/{store}/test",
            "post /api/v1/workers",
            "post /api/v1/workers/{worker}/rotate-token",
            "put /api/v1/artifacts/{digest}/content",
            "put /api/v1/software/{software}/channels/{channel}",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
        assert_eq!(actual, expected);
        assert!(
            document["paths"]
                .as_object()
                .unwrap()
                .keys()
                .all(|path| !path.contains("/internal/"))
        );
    }
}
