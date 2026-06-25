use axum::body::Body;
use http::{Request, StatusCode};
use open_tf_mirror::{
    http_api::{AppState, build_router},
    metadata::{PlatformMetadata, ProviderMetadataStore, VersionMetadata},
    storage::ProviderStorage,
};
use std::fs;
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[tokio::test]
async fn health_endpoints_are_compatible_with_existing_probes() {
    let app = build_router(AppState::for_tests(tempfile::tempdir().unwrap().path()));

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
        module_cache: open_tf_mirror::module_mirror::ModuleCache::new(tmp.path()),
        module_registry_base: "https://registry.terraform.io".into(),
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
            shasum: Some("abc123".into()),
            download_url: "https://releases.hashicorp.com/example.zip".into(),
        }],
    });
    let app = build_router(AppState {
        metadata,
        provider_storage: ProviderStorage::new(tmp.path()),
        module_cache: open_tf_mirror::module_mirror::ModuleCache::new(tmp.path()),
        module_registry_base: "https://registry.terraform.io".into(),
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
    assert_eq!(json["archives"]["linux_amd64"]["hashes"][0], "zh:abc123");
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
            shasum: None,
            download_url: "https://releases.hashicorp.com/example.zip".into(),
        }],
    });
    let storage = ProviderStorage::new(tmp.path());
    let archive = tmp
        .path()
        .join("data/providers/registry.terraform.io/hashicorp/random")
        .join(filename);
    fs::create_dir_all(archive.parent().unwrap()).unwrap();
    fs::write(&archive, b"zip-body").unwrap();
    let app = build_router(AppState {
        metadata,
        provider_storage: storage,
        module_cache: open_tf_mirror::module_mirror::ModuleCache::new(tmp.path()),
        module_registry_base: "https://registry.terraform.io".into(),
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

#[tokio::test]
async fn module_download_fetches_from_registry_when_local_cache_is_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(
            "/v1/modules/terraform-aws-modules/vpc/aws/5.8.1/download",
        ))
        .respond_with(ResponseTemplate::new(204).insert_header(
            "X-Terraform-Get",
            format!("{}/archives/vpc.tar.gz", registry.uri()),
        ))
        .mount(&registry)
        .await;
    Mock::given(method("GET"))
        .and(path("/archives/vpc.tar.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes("module-archive"))
        .mount(&registry)
        .await;

    let mut state = AppState::for_tests(tmp.path());
    state.module_registry_base = registry.uri();
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/modules/terraform-aws-modules/vpc/aws/5.8.1/download")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&body[..], b"module-archive");
    assert_eq!(
        fs::read(
            tmp.path().join(
                "data/modules/registry.terraform.io/terraform-aws-modules/vpc/aws/5.8.1.tar.gz"
            )
        )
        .unwrap(),
        b"module-archive"
    );
}
