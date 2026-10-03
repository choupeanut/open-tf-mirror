use axum::body::Body;
use http::{Request, StatusCode};
use open_tf_mirror::{
    http_api::{AppState, RouterOptions, build_router, build_router_with_options},
    metadata::{PlatformMetadata, ProviderMetadataStore, VersionMetadata},
    storage::ProviderStorage,
};
use std::fs;
use tower::ServiceExt;

#[tokio::test]
async fn health_endpoints_are_compatible_with_existing_probes() {
    let tmp = tempfile::tempdir().unwrap();
    let app = build_router(AppState::for_tests(tmp.path()));

    for path in ["/readyz", "/livez"] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn readiness_fails_when_data_directory_disappears() {
    let tmp = tempfile::tempdir().unwrap();
    let app = build_router(AppState::for_tests(tmp.path()));
    fs::remove_dir_all(tmp.path()).unwrap();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn module_routes_are_not_served() {
    let app = build_router(AppState::for_tests(tempfile::tempdir().unwrap().path()));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/modules/hashicorp/consul/aws/0.0.1/download")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn provider_api_enforces_configured_burst_limit() {
    let app = build_router_with_options(
        AppState::for_tests(tempfile::tempdir().unwrap().path()),
        RouterOptions {
            conn_qps: 1,
            conn_burst: 1,
        },
    );
    let request = || {
        Request::builder()
            .uri("/v1/providers/registry.terraform.io/hashicorp/random/index.txt")
            .body(Body::empty())
            .unwrap()
    };

    assert_eq!(
        app.clone().oneshot(request()).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.oneshot(request()).await.unwrap().status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn provider_index_json_returns_versions_object() {
    let tmp = tempfile::tempdir().unwrap();
    let metadata = ProviderMetadataStore::default();
    metadata.insert_version(VersionMetadata {
        hostname: "registry.terraform.io".into(),
        namespace: "hashicorp".into(),
        provider_type: "random".into(),
        version: "3.6.2".into(),
        platforms: vec![],
    });
    let app = build_router(AppState {
        metadata,
        provider_storage: ProviderStorage::new(tmp.path()),
        data_dir: std::sync::Arc::new(tmp.path().to_path_buf()),
    });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/providers/registry.terraform.io/hashicorp/random/index.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["versions"]["3.6.2"], serde_json::json!({}));
}

#[tokio::test]
async fn provider_metadata_rejects_actions_without_json_suffix() {
    let app = build_router(AppState::for_tests(tempfile::tempdir().unwrap().path()));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/providers/registry.terraform.io/hashicorp/random/index.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn provider_version_json_returns_relative_download_archives() {
    let tmp = tempfile::tempdir().unwrap();
    let metadata = ProviderMetadataStore::default();
    metadata.insert_version(VersionMetadata {
        hostname: "registry.terraform.io".into(),
        namespace: "hashicorp".into(),
        provider_type: "random".into(),
        version: "3.6.2".into(),
        platforms: vec![PlatformMetadata {
            os: "linux".into(),
            arch: "amd64".into(),
            filename: "terraform-provider-random_3.6.2_linux_amd64.zip".into(),
            shasum: Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into()),
            download_url: "https://releases.hashicorp.com/example.zip".into(),
        }],
    });
    let app = build_router(AppState {
        metadata,
        provider_storage: ProviderStorage::new(tmp.path()),
        data_dir: std::sync::Arc::new(tmp.path().to_path_buf()),
    });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/providers/registry.terraform.io/hashicorp/random/3.6.2.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["archives"]["linux_amd64"]["url"],
        "download/terraform-provider-random_3.6.2_linux_amd64.zip"
    );
    assert_eq!(
        json["archives"]["linux_amd64"]["hashes"][0],
        "zh:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    );
}

#[tokio::test]
async fn provider_download_streams_cached_archive_with_zip_headers() {
    let tmp = tempfile::tempdir().unwrap();
    let metadata = ProviderMetadataStore::default();
    let filename = "terraform-provider-random_3.6.2_linux_amd64.zip";
    metadata.insert_version(VersionMetadata {
        hostname: "registry.terraform.io".into(),
        namespace: "hashicorp".into(),
        provider_type: "random".into(),
        version: "3.6.2".into(),
        platforms: vec![PlatformMetadata {
            os: "linux".into(),
            arch: "amd64".into(),
            filename: filename.into(),
            shasum: Some("4880130a58b9b6c31a056e79db0dc17e8bbfb1e0ac4da3ede76788cc27d74014".into()),
            download_url: "https://releases.hashicorp.com/example.zip".into(),
        }],
    });
    let storage = ProviderStorage::new(tmp.path());
    let archive = tmp
        .path()
        .join("providers/registry.terraform.io/hashicorp/random")
        .join(filename);
    fs::create_dir_all(archive.parent().unwrap()).unwrap();
    fs::write(&archive, b"zip-body").unwrap();
    let app = build_router(AppState {
        metadata,
        provider_storage: storage,
        data_dir: std::sync::Arc::new(tmp.path().to_path_buf()),
    });

    let response = app
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/v1/providers/registry.terraform.io/hashicorp/random/download/{filename}"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "application/zip");
    assert_eq!(
        response.headers()["content-disposition"],
        format!("attachment; filename=\"{filename}\"")
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&body[..], b"zip-body");
}
