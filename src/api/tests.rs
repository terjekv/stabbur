use actix_web::{App, body::to_bytes, test};
use bytes::Bytes;
use sha2::{Digest, Sha256};
use stabbur_auth_core::PasswordPolicy;
use stabbur_domain::StoreId;
use stabbur_storage_core::BootstrapPreparation;
use stabbur_storage_runtime::open_test_storage;
use stabbur_store_core::StoreRole;
use stabbur_store_fs::FsArtifactStore;

use super::*;

async fn authenticated_state() -> (AppState, String, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let storage = open_test_storage().await.unwrap().storage();
    let (bootstrap_secret, bootstrap_hash) = generate_token();
    assert_eq!(
        storage.prepare_bootstrap(&bootstrap_hash).await.unwrap(),
        BootstrapPreparation::Created
    );
    let password = PasswordPolicy::default()
        .hash("correct horse battery staple")
        .unwrap();
    let admin = storage
        .bootstrap_admin(
            bootstrap_secret.expose_secret(),
            "admin",
            &password,
            Utc::now(),
        )
        .await
        .unwrap();
    let (token, token_hash) = generate_token();
    storage
        .create_credential(
            admin.id,
            Some("test"),
            "session",
            &token_hash,
            Some(Utc::now() + Duration::hours(1)),
            Utc::now(),
        )
        .await
        .unwrap();
    let store_record = storage
        .ensure_local_primary_store(StoreId::new(), "local")
        .await
        .unwrap();
    let store = Arc::new(
        FsArtifactStore::open(temp.path().join("objects"), StoreRole::Primary)
            .await
            .unwrap(),
    );
    let state = AppState::new(
        storage,
        store,
        store_record.id,
        temp.path().join("bootstrap.secret"),
    );
    (state, token.expose_secret().into(), temp)
}

#[actix_web::test]
async fn liveness_and_readiness_have_distinct_dependency_semantics() {
    let (state, _token, temp) = authenticated_state().await;
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(state))
            .configure(configure),
    )
    .await;

    let response =
        test::call_service(&app, test::TestRequest::get().uri("/healthz").to_request()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-store"
    );
    let body: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(body["status"], "ok");

    let response =
        test::call_service(&app, test::TestRequest::get().uri("/readyz").to_request()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(body["status"], "ready");

    tokio::fs::remove_dir(temp.path().join("objects/uploads"))
        .await
        .unwrap();
    let response =
        test::call_service(&app, test::TestRequest::get().uri("/readyz").to_request()).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(body["status"], "not_ready");

    let liveness =
        test::call_service(&app, test::TestRequest::get().uri("/healthz").to_request()).await;
    assert_eq!(liveness.status(), StatusCode::OK);
    let old_health =
        test::call_service(&app, test::TestRequest::get().uri("/health").to_request()).await;
    assert_eq!(old_health.status(), StatusCode::NOT_FOUND);
}

#[actix_web::test]
async fn software_workflow_is_authenticated_audited_and_cursor_shaped() {
    let (state, token, _temp) = authenticated_state().await;
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(state))
            .configure(configure),
    )
    .await;
    let create = test::TestRequest::post()
        .uri("/api/v1/software")
        .insert_header((header::AUTHORIZATION, format!("Bearer {token}")))
        .set_json(serde_json::json!({"slug": "firefox", "name": "Firefox"}))
        .to_request();
    let response = test::call_service(&app, create).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers().get(header::ETAG).unwrap(), "\"rev-1\"");

    let installation = test::TestRequest::patch()
        .uri("/api/v1/software/firefox")
        .insert_header((header::AUTHORIZATION, format!("Bearer {token}")))
        .insert_header((header::IF_MATCH, "\"rev-1\""))
        .set_json(serde_json::json!({
            "installation": {
                "install": {"kind": "pkg"},
                "detection": {"bundle_id": "org.mozilla.firefox"}
            }
        }))
        .to_request();
    let response = test::call_service(&app, installation).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get(header::ETAG).unwrap(), "\"rev-2\"");
    let software: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(software["installation"]["install"]["kind"], "pkg");

    let secret_metadata = test::TestRequest::patch()
        .uri("/api/v1/software/firefox")
        .insert_header((header::AUTHORIZATION, format!("Bearer {token}")))
        .insert_header((header::IF_MATCH, "\"rev-2\""))
        .set_json(serde_json::json!({
            "installation": {
                "install": {"api_token": "must-not-persist"},
                "detection": {}
            }
        }))
        .to_request();
    assert_eq!(
        test::call_service(&app, secret_metadata).await.status(),
        StatusCode::BAD_REQUEST
    );

    let list = test::TestRequest::get()
        .uri("/api/v1/software?limit=50")
        .insert_header((header::AUTHORIZATION, format!("Bearer {token}")))
        .to_request();
    let response = test::call_service(&app, list).await;
    assert_eq!(response.status(), StatusCode::OK);
    let page: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(page["items"][0]["slug"], "firefox");
    assert!(page["next_cursor"].is_null());
}

#[actix_web::test]
async fn identity_administration_enforces_roles_revocation_and_password_recovery() {
    let (state, admin_token, _temp) = authenticated_state().await;
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(state))
            .configure(configure),
    )
    .await;
    let admin_auth = (header::AUTHORIZATION, format!("Bearer {admin_token}"));

    let role = test::TestRequest::post()
        .uri("/api/v1/auth/roles")
        .insert_header(admin_auth.clone())
        .set_json(serde_json::json!({
            "name": "catalog-reader",
            "permissions": ["software:read"]
        }))
        .to_request();
    assert_eq!(
        test::call_service(&app, role).await.status(),
        StatusCode::CREATED
    );

    let service = test::TestRequest::post()
        .uri("/api/v1/auth/principals")
        .insert_header(admin_auth.clone())
        .set_json(serde_json::json!({
            "name": "inventory-reader",
            "kind": "service",
            "roles": ["catalog-reader"]
        }))
        .to_request();
    let response = test::call_service(&app, service).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers().get(header::ETAG).unwrap(), "\"rev-1\"");
    let service: serde_json::Value = test::read_body_json(response).await;
    let service_id = service["id"].as_str().unwrap();

    let token = test::TestRequest::post()
        .uri(&format!("/api/v1/auth/principals/{service_id}/tokens"))
        .insert_header(admin_auth.clone())
        .set_json(serde_json::json!({"name": "inventory-prod"}))
        .to_request();
    let response = test::call_service(&app, token).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created_token: serde_json::Value = test::read_body_json(response).await;
    let service_secret = created_token["secret"].as_str().unwrap();

    let authorized = test::TestRequest::get()
        .uri("/api/v1/software")
        .insert_header((header::AUTHORIZATION, format!("Bearer {service_secret}")))
        .to_request();
    assert_eq!(
        test::call_service(&app, authorized).await.status(),
        StatusCode::OK
    );
    let forbidden = test::TestRequest::get()
        .uri("/api/v1/audit")
        .insert_header((header::AUTHORIZATION, format!("Bearer {service_secret}")))
        .to_request();
    assert_eq!(
        test::call_service(&app, forbidden).await.status(),
        StatusCode::FORBIDDEN
    );

    let tokens = test::TestRequest::get()
        .uri(&format!("/api/v1/auth/principals/{service_id}/tokens"))
        .insert_header(admin_auth.clone())
        .to_request();
    let response = test::call_service(&app, tokens).await;
    let body = to_bytes(response.into_body()).await.unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(body.contains("inventory-prod"));
    assert!(!body.contains(service_secret));
    assert!(!body.contains("secret"));

    let disable = test::TestRequest::patch()
        .uri(&format!("/api/v1/auth/principals/{service_id}"))
        .insert_header(admin_auth.clone())
        .insert_header((header::IF_MATCH, "\"rev-1\""))
        .set_json(serde_json::json!({"enabled": false}))
        .to_request();
    assert_eq!(
        test::call_service(&app, disable).await.status(),
        StatusCode::OK
    );
    let revoked = test::TestRequest::get()
        .uri("/api/v1/software")
        .insert_header((header::AUTHORIZATION, format!("Bearer {service_secret}")))
        .to_request();
    assert_eq!(
        test::call_service(&app, revoked).await.status(),
        StatusCode::UNAUTHORIZED
    );

    let human = test::TestRequest::post()
        .uri("/api/v1/auth/principals")
        .insert_header(admin_auth)
        .set_json(serde_json::json!({
            "name": "operator-one",
            "kind": "human",
            "password": "initial password phrase",
            "roles": ["operator"]
        }))
        .to_request();
    assert_eq!(
        test::call_service(&app, human).await.status(),
        StatusCode::CREATED
    );
    let login_request = test::TestRequest::post()
        .uri("/api/v1/auth/login")
        .set_json(serde_json::json!({
            "username": "operator-one",
            "password": "initial password phrase"
        }))
        .to_request();
    let response = test::call_service(&app, login_request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let session: serde_json::Value = test::read_body_json(response).await;
    let session = session["token"].as_str().unwrap();

    let change = test::TestRequest::post()
        .uri("/api/v1/auth/password")
        .insert_header((header::AUTHORIZATION, format!("Bearer {session}")))
        .set_json(serde_json::json!({
            "current_password": "initial password phrase",
            "new_password": "replacement password phrase"
        }))
        .to_request();
    assert_eq!(
        test::call_service(&app, change).await.status(),
        StatusCode::NO_CONTENT
    );
    let old_session = test::TestRequest::get()
        .uri("/api/v1/auth/me")
        .insert_header((header::AUTHORIZATION, format!("Bearer {session}")))
        .to_request();
    assert_eq!(
        test::call_service(&app, old_session).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let new_login = test::TestRequest::post()
        .uri("/api/v1/auth/login")
        .set_json(serde_json::json!({
            "username": "operator-one",
            "password": "replacement password phrase"
        }))
        .to_request();
    assert_eq!(
        test::call_service(&app, new_login).await.status(),
        StatusCode::OK
    );
}

#[actix_web::test]
async fn artifact_http_supports_ranges_head_etags_and_mismatch_rejection() {
    let (state, token, _temp) = authenticated_state().await;
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(state))
            .configure(configure),
    )
    .await;
    let content = Bytes::from_static(b"immutable artifact bytes");
    let digest = hex::encode(Sha256::digest(&content));
    let upload = test::TestRequest::put()
        .uri(&format!("/api/v1/artifacts/{digest}/content"))
        .insert_header((header::AUTHORIZATION, format!("Bearer {token}")))
        .insert_header((header::CONTENT_TYPE, "application/octet-stream"))
        .set_payload(content.clone())
        .to_request();
    let response = test::call_service(&app, upload).await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let head = test::TestRequest::default()
        .method(actix_web::http::Method::HEAD)
        .uri(&format!("/api/v1/artifacts/{digest}/content"))
        .insert_header((header::AUTHORIZATION, format!("Bearer {token}")))
        .insert_header((header::RANGE, "bytes=-5"))
        .to_request();
    let response = test::call_service(&app, head).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.headers().get(header::CONTENT_RANGE).unwrap(),
        "bytes 19-23/24"
    );
    assert_eq!(response.headers().get(header::CONTENT_LENGTH).unwrap(), "5");
    assert!(to_bytes(response.into_body()).await.unwrap().is_empty());

    let range = test::TestRequest::get()
        .uri(&format!("/api/v1/artifacts/{digest}/content"))
        .insert_header((header::AUTHORIZATION, format!("Bearer {token}")))
        .insert_header((header::RANGE, "bytes=10-17"))
        .to_request();
    let response = test::call_service(&app, range).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.headers().get(header::CONTENT_RANGE).unwrap(),
        "bytes 10-17/24"
    );
    assert_eq!(
        to_bytes(response.into_body()).await.unwrap(),
        b"artifact".as_slice()
    );

    let not_modified = test::TestRequest::get()
        .uri(&format!("/api/v1/artifacts/{digest}/content"))
        .insert_header((header::AUTHORIZATION, format!("Bearer {token}")))
        .insert_header((header::IF_NONE_MATCH, format!("\"{digest}\"")))
        .to_request();
    let response = test::call_service(&app, not_modified).await;
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);

    let multiple = test::TestRequest::get()
        .uri(&format!("/api/v1/artifacts/{digest}/content"))
        .insert_header((header::AUTHORIZATION, format!("Bearer {token}")))
        .insert_header((header::RANGE, "bytes=0-1,4-5"))
        .to_request();
    let response = test::call_service(&app, multiple).await;
    assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/problem+json"
    );

    let wrong = "0".repeat(64);
    let upload = test::TestRequest::put()
        .uri(&format!("/api/v1/artifacts/{wrong}/content"))
        .insert_header((header::AUTHORIZATION, format!("Bearer {token}")))
        .set_payload(content)
        .to_request();
    let response = test::call_service(&app, upload).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[actix_web::test]
async fn provisioned_worker_claims_and_completes_a_typed_autopkg_run() {
    let (state, admin_token, _temp) = authenticated_state().await;
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(state))
            .configure(configure),
    )
    .await;
    let admin_auth = (header::AUTHORIZATION, format!("Bearer {admin_token}"));

    let create_software_request = test::TestRequest::post()
        .uri("/api/v1/software")
        .insert_header(admin_auth.clone())
        .set_json(serde_json::json!({"slug": "firefox", "name": "Firefox"}))
        .to_request();
    assert_eq!(
        test::call_service(&app, create_software_request)
            .await
            .status(),
        StatusCode::CREATED
    );

    let provision = test::TestRequest::post()
        .uri("/api/v1/workers")
        .insert_header(admin_auth.clone())
        .set_json(serde_json::json!({
            "name": "mac-builder-01",
            "allowed_capabilities": ["os.macos", "builder.autopkg"]
        }))
        .to_request();
    let response = test::call_service(&app, provision).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let credential: serde_json::Value = test::read_body_json(response).await;
    let worker_id = credential["worker_id"].as_str().unwrap();
    let worker_token = credential["token"].as_str().unwrap();

    let forbidden_admin_registration = test::TestRequest::post()
        .uri("/api/v1/internal/workers/register")
        .insert_header(admin_auth.clone())
        .set_json(serde_json::json!({
            "worker_id": worker_id,
            "capabilities": ["os.macos", "builder.autopkg"]
        }))
        .to_request();
    assert_eq!(
        test::call_service(&app, forbidden_admin_registration)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );

    let escalation = test::TestRequest::post()
        .uri("/api/v1/internal/workers/register")
        .insert_header((header::AUTHORIZATION, format!("Bearer {worker_token}")))
        .set_json(serde_json::json!({
            "worker_id": worker_id,
            "capabilities": ["os.macos", "builder.autopkg", "storage.manage"]
        }))
        .to_request();
    assert_eq!(
        test::call_service(&app, escalation).await.status(),
        StatusCode::FORBIDDEN
    );

    let registration = test::TestRequest::post()
        .uri("/api/v1/internal/workers/register")
        .insert_header((header::AUTHORIZATION, format!("Bearer {worker_token}")))
        .set_json(serde_json::json!({
            "worker_id": worker_id,
            "capabilities": ["os.macos", "builder.autopkg"]
        }))
        .to_request();
    assert_eq!(
        test::call_service(&app, registration).await.status(),
        StatusCode::NO_CONTENT
    );

    let catalog_manifest = serde_json::json!({
        "schema_version": 1,
        "producer": "autopkg",
        "source": {
            "locator": "https://example.test/recipes.git",
            "revision": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        },
        "recipes": [{
            "identifier": "com.example.firefox",
            "builder": "autopkg",
            "parents": [],
            "required_capabilities": ["builder.autopkg", "os.macos"]
        }],
        "diagnostics": []
    });
    let publish = test::TestRequest::post()
        .uri(&format!(
            "/api/v1/internal/workers/{worker_id}/recipe-catalogs"
        ))
        .insert_header((header::AUTHORIZATION, format!("Bearer {worker_token}")))
        .set_json(&catalog_manifest)
        .to_request();
    let response = test::call_service(&app, publish).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let published: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(published["outcome"], "published");
    let catalog_snapshot_id = published["snapshot"]["summary"]["id"].as_str().unwrap();

    let replay = test::TestRequest::post()
        .uri(&format!(
            "/api/v1/internal/workers/{worker_id}/recipe-catalogs"
        ))
        .insert_header((header::AUTHORIZATION, format!("Bearer {worker_token}")))
        .set_json(&catalog_manifest)
        .to_request();
    let response = test::call_service(&app, replay).await;
    assert_eq!(response.status(), StatusCode::OK);
    let replayed: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(replayed["outcome"], "replayed");
    assert_eq!(replayed["snapshot"]["summary"]["id"], catalog_snapshot_id);

    let lookup = test::TestRequest::get()
        .uri("/api/v1/recipe-catalog-entries?identifier=com.example.firefox")
        .insert_header(admin_auth.clone())
        .to_request();
    let response = test::call_service(&app, lookup).await;
    assert_eq!(response.status(), StatusCode::OK);
    let lookup: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(lookup["exists"], true);
    assert_eq!(lookup["matches"][0]["snapshot_id"], catalog_snapshot_id);

    let catalog = test::TestRequest::get()
        .uri(&format!("/api/v1/recipe-catalogs/{catalog_snapshot_id}"))
        .insert_header(admin_auth.clone())
        .to_request();
    let response = test::call_service(&app, catalog).await;
    assert_eq!(response.status(), StatusCode::OK);
    let catalog: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(
        catalog["manifest"]["recipes"][0]["identifier"],
        "com.example.firefox"
    );

    let create_scan = test::TestRequest::post()
        .uri("/api/v1/recipe-catalog-scans")
        .insert_header(admin_auth.clone())
        .insert_header(("idempotency-key", "scan-recipes-b"))
        .set_json(serde_json::json!({
            "producer": "autopkg",
            "source": {
                "locator": "https://example.test/recipes.git",
                "revision": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            }
        }))
        .to_request();
    let response = test::call_service(&app, create_scan).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let scan: serde_json::Value = test::read_body_json(response).await;
    let scan_id = scan["id"].as_str().unwrap();
    assert_eq!(scan["state"], "queued");

    let claim_scan = test::TestRequest::post()
        .uri(&format!("/api/v1/internal/workers/{worker_id}/claim"))
        .insert_header((header::AUTHORIZATION, format!("Bearer {worker_token}")))
        .set_json(serde_json::json!({"lease_seconds": 60}))
        .to_request();
    let response = test::call_service(&app, claim_scan).await;
    assert_eq!(response.status(), StatusCode::OK);
    let claimed_scan: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(
        claimed_scan["job"]["subject"]["kind"],
        "recipe_catalog_scan"
    );
    assert_eq!(claimed_scan["job"]["subject"]["scan_id"], scan_id);
    assert_eq!(
        claimed_scan["job"]["payload"]["request"]["scan_id"],
        scan_id
    );

    let complete_scan = test::TestRequest::post()
        .uri(&format!("/api/v1/internal/workers/{worker_id}/complete"))
        .insert_header((header::AUTHORIZATION, format!("Bearer {worker_token}")))
        .set_json(serde_json::json!({
            "lease": claimed_scan["lease"],
            "idempotency_key": "scan-success",
            "result": {
                "schema_version": 1,
                "scan_id": scan_id,
                "manifest": {
                    "schema_version": 1,
                    "producer": "autopkg",
                    "source": {
                        "locator": "https://example.test/recipes.git",
                        "revision": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                    },
                    "recipes": [{
                        "identifier": "com.example.firefox.beta",
                        "builder": "autopkg",
                        "parents": ["com.example.firefox"],
                        "required_capabilities": ["builder.autopkg", "os.macos"]
                    }],
                    "diagnostics": []
                },
                "completed_at": Utc::now()
            }
        }))
        .to_request();
    let response = test::call_service(&app, complete_scan).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let completion: serde_json::Value = test::read_body_json(response).await;
    let scanned_snapshot_id = completion["publication"]["snapshot"]["id"]
        .as_str()
        .unwrap();

    let get_scan = test::TestRequest::get()
        .uri(&format!("/api/v1/recipe-catalog-scans/{scan_id}"))
        .insert_header(admin_auth.clone())
        .to_request();
    let response = test::call_service(&app, get_scan).await;
    assert_eq!(response.status(), StatusCode::OK);
    let scan: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(scan["state"], "succeeded");
    assert_eq!(scan["snapshot_id"], scanned_snapshot_id);

    let create_recipe_request = test::TestRequest::post()
        .uri("/api/v1/recipes")
        .insert_header(admin_auth.clone())
        .set_json(serde_json::json!({"name": "firefox-autopkg"}))
        .to_request();
    let response = test::call_service(&app, create_recipe_request).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let recipe: serde_json::Value = test::read_body_json(response).await;

    let create_revision = test::TestRequest::post()
        .uri(&format!(
            "/api/v1/recipes/{}/revisions",
            recipe["id"].as_str().unwrap()
        ))
        .insert_header(admin_auth.clone())
        .set_json(serde_json::json!({
            "builder": "autopkg",
            "definition": {
                "sources": [{
                    "url": "https://example.test/recipes.git",
                    "commit": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                }],
                "entrypoint": "Firefox.pkg.recipe",
                "output": {
                    "version_pointer": "/version",
                    "recipe_trust_pointer": "/stabbur/recipe_trust_succeeded",
                    "variants": [{
                        "platform": "mac_os",
                        "architecture": "universal",
                        "minimum_macos": "13.0",
                        "maximum_macos": null,
                        "resolution_priority": 0,
                        "artifacts": [{
                            "path_pointer": "/artifact_path",
                            "media_type": "application/vnd.apple.installer+xml",
                            "role": "primary_installer"
                        }]
                    }],
                    "verification": [{
                        "name": "signature",
                        "pointer": "/signature_valid",
                        "required": true
                    }]
                }
            },
            "required_capabilities": []
        }))
        .to_request();
    let response = test::call_service(&app, create_revision).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let revision: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(revision["definition"]["inputs"], serde_json::json!({}));

    let create_fake_recipe = test::TestRequest::post()
        .uri("/api/v1/recipes")
        .insert_header(admin_auth.clone())
        .set_json(serde_json::json!({"name": "portable-fake"}))
        .to_request();
    let response = test::call_service(&app, create_fake_recipe).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let fake_recipe: serde_json::Value = test::read_body_json(response).await;
    let fake_revision_uri = format!(
        "/api/v1/recipes/{}/revisions",
        fake_recipe["id"].as_str().unwrap()
    );
    let invalid_fake_revision = test::TestRequest::post()
        .uri(&fake_revision_uri)
        .insert_header(admin_auth.clone())
        .set_json(serde_json::json!({
            "builder": "fake",
            "definition": {"must_be": "empty"}
        }))
        .to_request();
    assert_eq!(
        test::call_service(&app, invalid_fake_revision)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    let unsupported_revision = test::TestRequest::post()
        .uri(&fake_revision_uri)
        .insert_header(admin_auth.clone())
        .set_json(serde_json::json!({
            "builder": "unreviewed",
            "definition": {}
        }))
        .to_request();
    let response = test::call_service(&app, unsupported_revision).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let problem: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(
        problem["validation_errors"][0]["code"],
        "unsupported_builder"
    );
    let create_fake_revision = test::TestRequest::post()
        .uri(&fake_revision_uri)
        .insert_header(admin_auth.clone())
        .set_json(serde_json::json!({
            "builder": "fake",
            "definition": {}
        }))
        .to_request();
    let response = test::call_service(&app, create_fake_revision).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let fake_revision: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(fake_revision["builder"], "fake");
    assert_eq!(fake_revision["definition"], serde_json::json!({}));
    assert_eq!(
        fake_revision["required_capabilities"],
        serde_json::json!(["builder.fake", "runtime.portable"])
    );

    let create_run_request = test::TestRequest::post()
        .uri("/api/v1/runs")
        .insert_header(admin_auth.clone())
        .insert_header(("idempotency-key", "firefox-run-1"))
        .set_json(serde_json::json!({
            "software": "firefox",
            "recipe_revision": revision["id"],
            "parameters": {"NAME": "Firefox"}
        }))
        .to_request();
    let response = test::call_service(&app, create_run_request).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let run: serde_json::Value = test::read_body_json(response).await;
    let run_id = run["id"].as_str().unwrap();

    let replay_run = test::TestRequest::post()
        .uri("/api/v1/runs")
        .insert_header(admin_auth.clone())
        .insert_header(("idempotency-key", "firefox-run-1"))
        .set_json(serde_json::json!({
            "software": "firefox",
            "recipe_revision": revision["id"],
            "parameters": {"NAME": "Firefox"}
        }))
        .to_request();
    let response = test::call_service(&app, replay_run).await;
    assert_eq!(response.status(), StatusCode::OK);
    let replayed: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(replayed["id"], run_id);

    let conflicting_run = test::TestRequest::post()
        .uri("/api/v1/runs")
        .insert_header(admin_auth.clone())
        .insert_header(("idempotency-key", "firefox-run-1"))
        .set_json(serde_json::json!({
            "software": "firefox",
            "recipe_revision": revision["id"],
            "parameters": {"NAME": "Different"}
        }))
        .to_request();
    assert_eq!(
        test::call_service(&app, conflicting_run).await.status(),
        StatusCode::CONFLICT
    );

    let claim = test::TestRequest::post()
        .uri(&format!("/api/v1/internal/workers/{worker_id}/claim"))
        .insert_header((header::AUTHORIZATION, format!("Bearer {worker_token}")))
        .set_json(serde_json::json!({"lease_seconds": 60}))
        .to_request();
    let response = test::call_service(&app, claim).await;
    assert_eq!(response.status(), StatusCode::OK);
    let claimed: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(claimed["job"]["payload"]["adapter"], "autopkg");
    assert_eq!(claimed["job"]["payload"]["request"]["run_id"], run_id);

    let append_logs = test::TestRequest::post()
        .uri(&format!("/api/v1/internal/workers/{worker_id}/logs"))
        .insert_header((header::AUTHORIZATION, format!("Bearer {worker_token}")))
        .set_json(serde_json::json!({
            "lease": claimed["lease"],
            "idempotency_key": "log-batch-0",
            "entries": [
                {
                    "stream": "system",
                    "message_base64": STANDARD.encode(b"attempt started")
                },
                {
                    "stream": "stdout",
                    "message_base64": STANDARD.encode(b"building Firefox")
                }
            ]
        }))
        .to_request();
    let response = test::call_service(&app, append_logs).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let receipt: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(receipt["first_sequence"], 0);
    assert_eq!(receipt["last_sequence"], 1);

    let first_log_page = test::TestRequest::get()
        .uri(&format!("/api/v1/runs/{run_id}/logs?limit=1"))
        .insert_header(admin_auth.clone())
        .to_request();
    let response = test::call_service(&app, first_log_page).await;
    assert_eq!(response.status(), StatusCode::OK);
    let page: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(page["items"][0]["stream"], "system");
    assert_eq!(
        page["items"][0]["message_base64"],
        STANDARD.encode(b"attempt started")
    );
    let cursor = page["next_cursor"].as_str().unwrap();
    let second_log_page = test::TestRequest::get()
        .uri(&format!(
            "/api/v1/runs/{run_id}/logs?limit=50&cursor={cursor}"
        ))
        .insert_header(admin_auth.clone())
        .to_request();
    let response = test::call_service(&app, second_log_page).await;
    let page: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(page["items"][0]["sequence"], 1);
    assert_eq!(page["items"][0]["stream"], "stdout");

    let wrong_run = RunId::new();
    let wrong_completion = test::TestRequest::post()
        .uri(&format!("/api/v1/internal/workers/{worker_id}/complete"))
        .insert_header((header::AUTHORIZATION, format!("Bearer {worker_token}")))
        .set_json(serde_json::json!({
            "lease": claimed["lease"],
            "idempotency_key": "wrong-run",
            "result": {
                "schema_version": 1,
                "run_id": wrong_run,
                "adapter": "autopkg",
                "sources": [],
                "tools": {},
                "raw_report": {},
                "build_result": null,
                "completed_at": Utc::now()
            }
        }))
        .to_request();
    assert_eq!(
        test::call_service(&app, wrong_completion).await.status(),
        StatusCode::BAD_REQUEST
    );

    let artifact_bytes = Bytes::from_static(b"verified AutoPkg package bytes");
    let artifact_digest = Sha256Digest::new(hex::encode(Sha256::digest(&artifact_bytes))).unwrap();
    let attempt_id = claimed["lease"]["attempt_id"].as_str().unwrap();
    let upload = test::TestRequest::put()
        .uri(&format!(
            "/api/v1/internal/workers/{worker_id}/attempts/{attempt_id}/artifacts/{artifact_digest}"
        ))
        .insert_header((header::AUTHORIZATION, format!("Bearer {worker_token}")))
        .insert_header((header::CONTENT_TYPE, "application/vnd.apple.installer+xml"))
        .insert_header(("x-stabbur-artifact-role", "primary_installer"))
        .set_payload(artifact_bytes.clone())
        .to_request();
    let response = test::call_service(&app, upload).await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let completion = test::TestRequest::post()
        .uri(&format!("/api/v1/internal/workers/{worker_id}/complete"))
        .insert_header((header::AUTHORIZATION, format!("Bearer {worker_token}")))
        .set_json(serde_json::json!({
            "lease": claimed["lease"],
            "idempotency_key": "typed-success",
            "result": {
                "schema_version": 1,
                "run_id": run_id,
                "adapter": "autopkg",
                "sources": [],
                "tools": {"autopkg": "2.7.6"},
                "raw_report": {
                    "version": "128.0",
                    "stabbur": {"recipe_trust_succeeded": true},
                    "signature_valid": true,
                    "artifact_path": "cache/Firefox.pkg"
                },
                "build_result": {
                    "discovered_version": "128.0",
                    "variants": [{
                        "variant_id": null,
                        "platform": "mac_os",
                        "architecture": "universal",
                        "minimum_macos": "13",
                        "maximum_macos": null,
                        "resolution_priority": 0,
                        "artifacts": [{
                            "digest": artifact_digest,
                            "size": artifact_bytes.len(),
                            "role": "primary_installer"
                        }]
                    }],
                    "uploaded_artifacts": [artifact_digest],
                    "provenance": {
                        "builder": "autopkg/2.7.6",
                        "worker_version": "0.1.0",
                        "operating_system": "macos/15.0",
                        "tools": {"autopkg": "2.7.6"},
                        "sources": [],
                        "recipe_trust_succeeded": true,
                        "raw_report": {"summary": "ok"},
                        "captured_at": Utc::now()
                    },
                    "verification_results": [{
                        "check": "signature",
                        "required": true,
                        "succeeded": true,
                        "detail": null
                    }]
                },
                "completed_at": Utc::now()
            }
        }))
        .to_request();
    let response = test::call_service(&app, completion).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let published: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(published["outcome"], "completed");
    assert_eq!(published["disposition"], "release_created");
    assert_eq!(published["release"]["version"], "128.0");
    assert_eq!(published["release"]["state"], "candidate");
    let release_id = published["release"]["id"].as_str().unwrap();

    let channels = test::TestRequest::get()
        .uri("/api/v1/software/firefox/channels")
        .insert_header((header::AUTHORIZATION, format!("Bearer {admin_token}")))
        .to_request();
    let response = test::call_service(&app, channels).await;
    assert_eq!(response.status(), StatusCode::OK);
    let channels: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(channels["items"][0]["name"], "candidate");
    assert_eq!(channels["items"][0]["release_id"], release_id);

    let promote = test::TestRequest::put()
        .uri("/api/v1/software/firefox/channels/stable")
        .insert_header((header::AUTHORIZATION, format!("Bearer {admin_token}")))
        .insert_header((header::IF_MATCH, "\"rev-0\""))
        .set_json(serde_json::json!({
            "release_id": release_id,
            "reason": "operator acceptance"
        }))
        .to_request();
    let response = test::call_service(&app, promote).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers().get(header::ETAG).unwrap(), "\"rev-1\"");

    let stale_promotion = test::TestRequest::put()
        .uri("/api/v1/software/firefox/channels/stable")
        .insert_header((header::AUTHORIZATION, format!("Bearer {admin_token}")))
        .insert_header((header::IF_MATCH, "\"rev-0\""))
        .set_json(serde_json::json!({"release_id": release_id}))
        .to_request();
    assert_eq!(
        test::call_service(&app, stale_promotion).await.status(),
        StatusCode::PRECONDITION_FAILED
    );

    let resolution = test::TestRequest::get()
            .uri("/api/v1/software/firefox/resolve?channel=stable&platform=mac_os&architecture=aarch64&macos=14.5")
            .insert_header((header::AUTHORIZATION, format!("Bearer {admin_token}")))
            .to_request();
    let response = test::call_service(&app, resolution).await;
    assert_eq!(response.status(), StatusCode::OK);
    let resolution: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(resolution["release"]["state"], "stable");
    assert_eq!(resolution["artifact_digest"], artifact_digest.as_str());
    assert_eq!(resolution["variant"]["architecture"], "universal");

    for path in [
        "/api/v1/workers",
        "/api/v1/recipes",
        "/api/v1/runs",
        "/api/v1/jobs",
        "/api/v1/stores",
    ] {
        let request = test::TestRequest::get()
            .uri(path)
            .insert_header((header::AUTHORIZATION, format!("Bearer {admin_token}")))
            .to_request();
        let response = test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::OK, "inspection path {path}");
        let page: serde_json::Value = test::read_body_json(response).await;
        assert!(!page["items"].as_array().unwrap().is_empty(), "{path}");
        if path == "/api/v1/runs" {
            assert!(page["items"][0].get("parameters").is_none());
            assert!(page["items"][0].get("result").is_none());
        }
        if path == "/api/v1/jobs" {
            assert!(page["items"][0].get("payload").is_none());
        }
    }

    let revisions = test::TestRequest::get()
        .uri(&format!(
            "/api/v1/recipes/{}/revisions",
            recipe["id"].as_str().unwrap()
        ))
        .insert_header((header::AUTHORIZATION, format!("Bearer {admin_token}")))
        .to_request();
    let revisions: serde_json::Value =
        test::read_body_json(test::call_service(&app, revisions).await).await;
    assert_eq!(revisions["items"].as_array().unwrap().len(), 1);

    let locations = test::TestRequest::get()
        .uri(&format!("/api/v1/artifacts/{artifact_digest}/locations"))
        .insert_header((header::AUTHORIZATION, format!("Bearer {admin_token}")))
        .to_request();
    let response = test::call_service(&app, locations).await;
    assert_eq!(response.status(), StatusCode::OK);
    let locations: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(locations["items"][0]["state"], "present");

    let stores = test::TestRequest::get()
        .uri("/api/v1/stores")
        .insert_header((header::AUTHORIZATION, format!("Bearer {admin_token}")))
        .to_request();
    let stores: serde_json::Value =
        test::read_body_json(test::call_service(&app, stores).await).await;
    let store_id = stores["items"][0]["id"].as_str().unwrap();
    let store_test = test::TestRequest::post()
        .uri(&format!("/api/v1/stores/{store_id}/test"))
        .insert_header((header::AUTHORIZATION, format!("Bearer {admin_token}")))
        .to_request();
    assert_eq!(
        test::call_service(&app, store_test).await.status(),
        StatusCode::OK
    );

    let get_run_request = test::TestRequest::get()
        .uri(&format!("/api/v1/runs/{run_id}"))
        .insert_header(admin_auth.clone())
        .to_request();
    let response = test::call_service(&app, get_run_request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let completed: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(completed["state"], "succeeded");
    assert_eq!(completed["result"]["adapter"], "autopkg");
    assert_eq!(
        completed["result"]["publication"]["disposition"],
        "release_created"
    );
    assert_eq!(completed["result"]["publication"]["release_id"], release_id);

    let events = test::TestRequest::get()
        .uri(&format!("/api/v1/runs/{run_id}/events"))
        .insert_header((header::AUTHORIZATION, format!("Bearer {admin_token}")))
        .to_request();
    let response = test::call_service(&app, events).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/event-stream"
    );
    let events = to_bytes(response.into_body()).await.unwrap();
    let events = String::from_utf8(events.to_vec()).unwrap();
    assert!(events.contains("id: 0\nevent: log"));
    assert!(events.contains("id: 1\nevent: log"));
    assert!(events.contains("event: complete"));

    let rotate = test::TestRequest::post()
        .uri(&format!("/api/v1/workers/{worker_id}/rotate-token"))
        .insert_header(admin_auth.clone())
        .insert_header((header::IF_MATCH, "\"rev-1\""))
        .to_request();
    let response = test::call_service(&app, rotate).await;
    assert_eq!(response.status(), StatusCode::OK);
    let rotated: serde_json::Value = test::read_body_json(response).await;
    let rotated_token = rotated["token"].as_str().unwrap();

    let old_registration = test::TestRequest::post()
        .uri("/api/v1/internal/workers/register")
        .insert_header((header::AUTHORIZATION, format!("Bearer {worker_token}")))
        .set_json(serde_json::json!({
            "worker_id": worker_id,
            "capabilities": ["os.macos", "builder.autopkg"]
        }))
        .to_request();
    assert_eq!(
        test::call_service(&app, old_registration).await.status(),
        StatusCode::UNAUTHORIZED
    );

    let disable = test::TestRequest::patch()
        .uri(&format!("/api/v1/workers/{worker_id}"))
        .insert_header(admin_auth)
        .insert_header((header::IF_MATCH, "\"rev-2\""))
        .set_json(serde_json::json!({"enabled": false}))
        .to_request();
    let response = test::call_service(&app, disable).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get(header::ETAG).unwrap(), "\"rev-3\"");

    let disabled_registration = test::TestRequest::post()
        .uri("/api/v1/internal/workers/register")
        .insert_header((header::AUTHORIZATION, format!("Bearer {rotated_token}")))
        .set_json(serde_json::json!({
            "worker_id": worker_id,
            "capabilities": ["os.macos", "builder.autopkg"]
        }))
        .to_request();
    assert_eq!(
        test::call_service(&app, disabled_registration)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[actix_web::test]
async fn build_targets_support_etags_manual_triggers_and_due_scheduling() {
    let (state, token, _temp) = authenticated_state().await;
    let storage = state.storage();
    let authorization = (header::AUTHORIZATION, format!("Bearer {token}"));
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(state))
            .configure(configure),
    )
    .await;

    let create_software_request = test::TestRequest::post()
        .uri("/api/v1/software")
        .insert_header(authorization.clone())
        .set_json(serde_json::json!({"slug": "scheduled-app", "name": "Scheduled App"}))
        .to_request();
    assert_eq!(
        test::call_service(&app, create_software_request)
            .await
            .status(),
        StatusCode::CREATED
    );
    let create_recipe_request = test::TestRequest::post()
        .uri("/api/v1/recipes")
        .insert_header(authorization.clone())
        .set_json(serde_json::json!({"name": "scheduled-fake"}))
        .to_request();
    let response = test::call_service(&app, create_recipe_request).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let recipe: serde_json::Value = test::read_body_json(response).await;
    let create_revision = test::TestRequest::post()
        .uri(&format!(
            "/api/v1/recipes/{}/revisions",
            recipe["id"].as_str().unwrap()
        ))
        .insert_header(authorization.clone())
        .set_json(serde_json::json!({"builder": "fake", "definition": {}}))
        .to_request();
    let response = test::call_service(&app, create_revision).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let revision: serde_json::Value = test::read_body_json(response).await;

    let initial_cursor = Utc::now() - Duration::minutes(1);
    let create_target = test::TestRequest::post()
        .uri("/api/v1/build-targets")
        .insert_header(authorization.clone())
        .set_json(serde_json::json!({
            "name": "scheduled-app-hourly",
            "software": "scheduled-app",
            "recipe_revision": revision["id"],
            "parameters": {"CHANNEL": "test"},
            "schedule": {"kind": "interval", "every_seconds": 60},
            "next_run_at": initial_cursor
        }))
        .to_request();
    let response = test::call_service(&app, create_target).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers().get(header::ETAG).unwrap(), "\"rev-1\"");
    let target: serde_json::Value = test::read_body_json(response).await;
    let target_id = target["id"].as_str().unwrap();
    assert_eq!(target["schedule"]["kind"], "interval");
    assert_eq!(target["schedule"]["every_seconds"], 60);

    let disable = test::TestRequest::patch()
        .uri("/api/v1/build-targets/scheduled-app-hourly")
        .insert_header(authorization.clone())
        .insert_header((header::IF_MATCH, "\"rev-1\""))
        .set_json(serde_json::json!({"enabled": false}))
        .to_request();
    let response = test::call_service(&app, disable).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get(header::ETAG).unwrap(), "\"rev-2\"");

    let disabled_trigger = test::TestRequest::post()
        .uri(&format!("/api/v1/build-targets/{target_id}/runs"))
        .insert_header(authorization.clone())
        .insert_header(("idempotency-key", "disabled-trigger"))
        .to_request();
    assert_eq!(
        test::call_service(&app, disabled_trigger).await.status(),
        StatusCode::CONFLICT
    );
    let stale = test::TestRequest::patch()
        .uri(&format!("/api/v1/build-targets/{target_id}"))
        .insert_header(authorization.clone())
        .insert_header((header::IF_MATCH, "\"rev-1\""))
        .set_json(serde_json::json!({"enabled": true}))
        .to_request();
    assert_eq!(
        test::call_service(&app, stale).await.status(),
        StatusCode::PRECONDITION_FAILED
    );
    let enable = test::TestRequest::patch()
        .uri(&format!("/api/v1/build-targets/{target_id}"))
        .insert_header(authorization.clone())
        .insert_header((header::IF_MATCH, "\"rev-2\""))
        .set_json(serde_json::json!({"enabled": true}))
        .to_request();
    let response = test::call_service(&app, enable).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get(header::ETAG).unwrap(), "\"rev-3\"");

    let manual_trigger = test::TestRequest::post()
        .uri(&format!("/api/v1/build-targets/{target_id}/runs"))
        .insert_header(authorization.clone())
        .insert_header(("idempotency-key", "manual-target-run"))
        .to_request();
    let response = test::call_service(&app, manual_trigger).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let manual_run: serde_json::Value = test::read_body_json(response).await;
    let manual_replay = test::TestRequest::post()
        .uri(&format!("/api/v1/build-targets/{target_id}/runs"))
        .insert_header(authorization.clone())
        .insert_header(("idempotency-key", "manual-target-run"))
        .to_request();
    let response = test::call_service(&app, manual_replay).await;
    assert_eq!(response.status(), StatusCode::OK);
    let replayed: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(replayed["id"], manual_run["id"]);

    assert_eq!(
        crate::scheduler::schedule_due_once(&storage, Utc::now())
            .await
            .unwrap(),
        0,
        "an outstanding manual run suppresses recurring work for the same target"
    );
    let cancel = test::TestRequest::post()
        .uri(&format!(
            "/api/v1/runs/{}/cancel",
            manual_run["id"].as_str().unwrap()
        ))
        .insert_header(authorization.clone())
        .insert_header(("idempotency-key", "cancel-manual-target-run"))
        .to_request();
    assert_eq!(
        test::call_service(&app, cancel).await.status(),
        StatusCode::OK
    );

    let scheduler_now = Utc::now();
    let (first_scheduler, second_scheduler) = tokio::join!(
        crate::scheduler::schedule_due_once(&storage, scheduler_now),
        crate::scheduler::schedule_due_once(&storage, scheduler_now),
    );
    assert_eq!(first_scheduler.unwrap() + second_scheduler.unwrap(), 1);
    assert_eq!(
        crate::scheduler::schedule_due_once(&storage, Utc::now())
            .await
            .unwrap(),
        0
    );
    let get_target = test::TestRequest::get()
        .uri(&format!("/api/v1/build-targets/{target_id}"))
        .insert_header(authorization.clone())
        .to_request();
    let response = test::call_service(&app, get_target).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get(header::ETAG).unwrap(), "\"rev-4\"");
    let current: serde_json::Value = test::read_body_json(response).await;
    assert!(
        current["next_run_at"]
            .as_str()
            .unwrap()
            .parse::<chrono::DateTime<Utc>>()
            .unwrap()
            > Utc::now()
    );
    let target_runs = test::TestRequest::get()
        .uri(&format!("/api/v1/build-targets/{target_id}/runs"))
        .insert_header(authorization.clone())
        .to_request();
    let response = test::call_service(&app, target_runs).await;
    assert_eq!(response.status(), StatusCode::OK);
    let target_runs: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(target_runs["items"].as_array().unwrap().len(), 2);
    let audit = test::TestRequest::get()
        .uri("/api/v1/audit?limit=200")
        .insert_header(authorization)
        .to_request();
    let audit: serde_json::Value =
        test::read_body_json(test::call_service(&app, audit).await).await;
    assert!(audit["items"].as_array().unwrap().iter().any(|event| {
        event["action"] == "build_target.schedule" && event["actor"]["kind"] == "system"
    }));
}

#[actix_web::test]
async fn unauthenticated_requests_use_stable_problem_documents() {
    let (state, _token, _temp) = authenticated_state().await;
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(state))
            .configure(configure),
    )
    .await;
    let request = test::TestRequest::get()
        .uri("/api/v1/software")
        .to_request();
    let response = test::call_service(&app, request).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let problem: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(problem["code"], "unauthorized");
    assert!(problem["request_id"].as_str().is_some());
}
