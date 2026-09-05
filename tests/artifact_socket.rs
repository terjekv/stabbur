use std::{sync::Arc, time::Duration};

use actix_web::{App, HttpServer, web};
use bytes::Bytes;
use futures_util::stream;
use sha2::{Digest, Sha256};
use stabbur_auth_core::{PasswordPolicy, generate_token};
use stabbur_domain::StoreId;
use stabbur_server::api::{AppState, configure};
use stabbur_storage_core::BootstrapPreparation;
use stabbur_storage_runtime::open_test_storage;
use stabbur_store_core::StoreRole;
use stabbur_store_fs::FsArtifactStore;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_artifact_streams_over_a_real_socket_with_ranges() {
    const CHUNK_SIZE: usize = 64 * 1024;
    const CHUNK_COUNT: usize = 1024;

    let temp = tempfile::tempdir().unwrap();
    let storage = open_test_storage().await.unwrap().storage();
    let (bootstrap_secret, bootstrap_hash) = generate_token();
    assert_eq!(
        storage.prepare_bootstrap(&bootstrap_hash).await.unwrap(),
        BootstrapPreparation::Created
    );
    let password = PasswordPolicy::default()
        .hash("socket test password phrase")
        .unwrap();
    let admin = storage
        .bootstrap_admin(
            bootstrap_secret.expose_secret(),
            "socket-admin",
            &password,
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    let (token, token_hash) = generate_token();
    storage
        .create_credential(
            admin.id,
            Some("socket-test"),
            "session",
            &token_hash,
            Some(chrono::Utc::now() + chrono::Duration::minutes(5)),
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    let store_record = storage
        .ensure_local_primary_store(StoreId::new(), "socket-primary")
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
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(state.clone()))
            .configure(configure)
    })
    .listen(listener)
    .unwrap()
    .run();
    let handle = server.handle();
    let task = tokio::spawn(server);

    let chunk = Bytes::from(vec![0x5a; CHUNK_SIZE]);
    let mut hasher = Sha256::new();
    for _ in 0..CHUNK_COUNT {
        hasher.update(&chunk);
    }
    let digest = hex::encode(hasher.finalize());
    let size = CHUNK_SIZE * CHUNK_COUNT;
    let source_chunk = chunk.clone();
    let source =
        stream::iter((0..CHUNK_COUNT).map(move |_| Ok::<_, std::io::Error>(source_chunk.clone())));
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let content_url = format!("http://{address}/api/v1/artifacts/{digest}/content");
    let upload = client
        .put(&content_url)
        .bearer_auth(token.expose_secret())
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .header(reqwest::header::CONTENT_LENGTH, size)
        .body(reqwest::Body::wrap_stream(source))
        .send()
        .await
        .unwrap();
    assert_eq!(upload.status(), reqwest::StatusCode::CREATED);

    let head = client
        .head(&content_url)
        .bearer_auth(token.expose_secret())
        .send()
        .await
        .unwrap();
    assert_eq!(head.status(), reqwest::StatusCode::OK);
    assert_eq!(
        head.headers().get(reqwest::header::CONTENT_LENGTH).unwrap(),
        size.to_string().as_str()
    );
    assert_eq!(
        head.headers().get(reqwest::header::ACCEPT_RANGES).unwrap(),
        "bytes"
    );
    assert!(head.bytes().await.unwrap().is_empty());

    let suffix = client
        .get(&content_url)
        .bearer_auth(token.expose_secret())
        .header(reqwest::header::RANGE, "bytes=-16")
        .send()
        .await
        .unwrap();
    assert_eq!(suffix.status(), reqwest::StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        suffix
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .unwrap()
            .to_str()
            .unwrap(),
        format!("bytes {}-{}/{size}", size - 16, size - 1)
    );
    assert_eq!(suffix.bytes().await.unwrap(), Bytes::from(vec![0x5a; 16]));

    handle.stop(true).await;
    task.await.unwrap().unwrap();
}
