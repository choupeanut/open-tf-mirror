use std::{collections::HashSet, time::Duration};

use open_tf_mirror::{
    metadata::{MetadataError, ProviderMetadataService},
    registry::RegistryClient,
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const VALID_SHA256: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn client(server: &MockServer) -> RegistryClient {
    RegistryClient::with_origin("registry.terraform.io", server.uri()).unwrap()
}

async fn mount_random(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/v1/providers/hashicorp/random/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "versions": [{
                "version": "3.6.2",
                "protocols": ["5.0"],
                "platforms": [{"os": "linux", "arch": "amd64"}]
            }]
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/v1/providers/hashicorp/random/3.6.2/download/linux/amd64",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "os": "linux",
            "arch": "amd64",
            "filename": "terraform-provider-random_3.6.2_linux_amd64.zip",
            "download_url": "https://releases.example/random.zip",
            "shasum": VALID_SHA256
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn package_response_must_match_requested_platform_and_archive() {
    let temp = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/providers/hashicorp/random/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "versions": [{"version": "3.6.2", "platforms": [{"os": "linux", "arch": "amd64"}]}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/v1/providers/hashicorp/random/3.6.2/download/linux/amd64",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "os": "darwin",
            "arch": "amd64",
            "filename": "terraform-provider-random_3.6.2_darwin_amd64.zip",
            "download_url": "https://releases.example/random.zip",
            "shasum": VALID_SHA256
        })))
        .mount(&server)
        .await;
    let service = ProviderMetadataService::with_registry_client(
        temp.path(),
        ["registry.terraform.io"],
        Duration::from_secs(1800),
        client(&server),
    )
    .unwrap();

    let error = service
        .get_version("registry.terraform.io", "hashicorp", "random", "3.6.2")
        .await
        .unwrap_err();

    assert!(matches!(error, MetadataError::InvalidAddress(_)));
    assert_eq!(service.refresh_lock_count(), 0);
    assert!(
        !temp
            .path()
            .join("metadata/registry.terraform.io/hashicorp/random/3.6.2.json")
            .exists()
    );
}

#[tokio::test]
async fn package_response_rejects_bad_checksum_and_non_https_url() {
    for (checksum, url) in [
        ("short", "https://releases.example/random.zip"),
        (VALID_SHA256, "http://releases.example/random.zip"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/providers/hashicorp/random/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "versions": [{"version": "3.6.2", "platforms": [{"os": "linux", "arch": "amd64"}]}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(
                "/v1/providers/hashicorp/random/3.6.2/download/linux/amd64",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "os": "linux", "arch": "amd64",
                "filename": "terraform-provider-random_3.6.2_linux_amd64.zip",
                "download_url": url, "shasum": checksum
            })))
            .mount(&server)
            .await;
        let service = ProviderMetadataService::with_registry_client(
            temp.path(),
            ["registry.terraform.io"],
            Duration::from_secs(1800),
            client(&server),
        )
        .unwrap();

        assert!(matches!(
            service
                .get_version("registry.terraform.io", "hashicorp", "random", "3.6.2")
                .await,
            Err(MetadataError::InvalidAddress(_))
        ));
    }
}

#[tokio::test]
async fn stale_version_packages_survive_restart_and_origin_failure() {
    let temp = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    mount_random(&server).await;
    let service = ProviderMetadataService::with_registry_client(
        temp.path(),
        ["registry.terraform.io"],
        Duration::ZERO,
        client(&server),
    )
    .unwrap();
    service
        .get_version("registry.terraform.io", "hashicorp", "random", "3.6.2")
        .await
        .unwrap();
    drop(service);
    drop(server);

    let unavailable = MockServer::start().await;
    let restarted = ProviderMetadataService::with_registry_client(
        temp.path(),
        ["registry.terraform.io"],
        Duration::ZERO,
        client(&unavailable),
    )
    .unwrap();
    let stale = restarted
        .get_version("registry.terraform.io", "hashicorp", "random", "3.6.2")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(stale.platforms[0].shasum.as_deref(), Some(VALID_SHA256));
}

#[tokio::test]
async fn sync_known_reports_refresh_failure_without_discarding_stale_index() {
    let temp = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    mount_random(&server).await;
    let service = ProviderMetadataService::with_registry_client(
        temp.path(),
        ["registry.terraform.io"],
        Duration::from_secs(1800),
        client(&server),
    )
    .unwrap();
    service
        .list_versions("registry.terraform.io", "hashicorp", "random")
        .await
        .unwrap();
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/v1/providers/hashicorp/random/versions"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    assert!(service.sync_known().await.is_err());
    assert_eq!(
        service
            .list_versions("registry.terraform.io", "hashicorp", "random")
            .await
            .unwrap(),
        vec!["3.6.2"]
    );
}

#[tokio::test]
async fn oversized_registry_json_is_rejected_without_persistence() {
    let temp = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/providers/hashicorp/random/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b' '; 2 * 1024 * 1024 + 1]))
        .mount(&server)
        .await;
    let service = ProviderMetadataService::with_registry_client(
        temp.path(),
        ["registry.terraform.io"],
        Duration::from_secs(1800),
        client(&server),
    )
    .unwrap();

    assert!(
        service
            .list_versions("registry.terraform.io", "hashicorp", "random")
            .await
            .is_err()
    );
    assert_eq!(service.refresh_lock_count(), 0);
}

#[tokio::test]
async fn registry_version_and_platform_counts_are_bounded() {
    let cases = [
        serde_json::json!({
            "versions": (0..4097).map(|index| serde_json::json!({
                "version": format!("1.0.{index}"), "platforms": []
            })).collect::<Vec<_>>()
        }),
        serde_json::json!({
            "versions": [{
                "version": "1.0.0",
                "platforms": (0..129).map(|index| serde_json::json!({
                    "os": "linux", "arch": format!("arch{index}")
                })).collect::<Vec<_>>()
            }]
        }),
    ];
    for response in cases {
        let temp = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/providers/hashicorp/random/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .mount(&server)
            .await;
        let service = ProviderMetadataService::with_registry_client(
            temp.path(),
            ["registry.terraform.io"],
            Duration::from_secs(1800),
            client(&server),
        )
        .unwrap();
        assert!(
            service
                .list_versions("registry.terraform.io", "hashicorp", "random")
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn oversized_package_json_is_rejected_and_lock_is_cleaned() {
    let temp = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/providers/hashicorp/random/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "versions": [{"version": "3.6.2", "platforms": [{"os": "linux", "arch": "amd64"}]}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/v1/providers/hashicorp/random/3.6.2/download/linux/amd64",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b' '; 256 * 1024 + 1]))
        .mount(&server)
        .await;
    let service = ProviderMetadataService::with_registry_client(
        temp.path(),
        ["registry.terraform.io"],
        Duration::from_secs(1800),
        client(&server),
    )
    .unwrap();

    assert!(
        service
            .get_version("registry.terraform.io", "hashicorp", "random", "3.6.2")
            .await
            .is_err()
    );
    assert_eq!(service.refresh_lock_count(), 0);
}

#[tokio::test]
async fn package_metadata_fetches_use_bounded_concurrency() {
    let temp = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let platforms = (0..9)
        .map(|index| serde_json::json!({"os": "linux", "arch": format!("arch{index}")}))
        .collect::<Vec<_>>();
    Mock::given(method("GET"))
        .and(path("/v1/providers/hashicorp/random/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "versions": [{"version": "3.6.2", "platforms": platforms}]
        })))
        .mount(&server)
        .await;
    for index in 0..9 {
        let arch = format!("arch{index}");
        Mock::given(method("GET"))
            .and(path(format!(
                "/v1/providers/hashicorp/random/3.6.2/download/linux/{arch}"
            )))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(250))
                    .set_body_json(serde_json::json!({
                        "os": "linux", "arch": arch,
                        "filename": format!("terraform-provider-random_3.6.2_linux_arch{index}.zip"),
                        "download_url": format!("https://releases.example/random-{index}.zip"),
                        "shasum": VALID_SHA256
                    })),
            )
            .mount(&server)
            .await;
    }
    let service = ProviderMetadataService::with_registry_client(
        temp.path(),
        ["registry.terraform.io"],
        Duration::from_secs(1800),
        client(&server),
    )
    .unwrap();

    let started = std::time::Instant::now();
    let metadata = service
        .get_version("registry.terraform.io", "hashicorp", "random", "3.6.2")
        .await
        .unwrap()
        .unwrap();
    let elapsed = started.elapsed();

    assert_eq!(metadata.platforms.len(), 9);
    assert!(
        elapsed >= Duration::from_millis(450),
        "ninth request was not bounded: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(1500),
        "package requests were sequential: {elapsed:?}"
    );
}

#[tokio::test]
async fn first_request_fetches_once_and_concurrent_request_shares_refresh() {
    let temp = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    mount_random(&server).await;
    let service = ProviderMetadataService::with_registry_client(
        temp.path(),
        HashSet::from(["registry.terraform.io".to_string()]),
        Duration::from_secs(1800),
        client(&server),
    )
    .unwrap();

    let (left, right) = tokio::join!(
        service.list_versions("registry.terraform.io", "hashicorp", "random"),
        service.list_versions("registry.terraform.io", "hashicorp", "random")
    );

    assert_eq!(left.unwrap(), vec!["3.6.2"]);
    assert_eq!(right.unwrap(), vec!["3.6.2"]);
    assert_eq!(service.refresh_lock_count(), 0);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path().ends_with("/versions"))
            .count(),
        1
    );
}

#[tokio::test]
async fn second_service_reads_persisted_metadata_without_origin() {
    let temp = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    mount_random(&server).await;
    let allowed = HashSet::from(["registry.terraform.io".to_string()]);
    let service = ProviderMetadataService::with_registry_client(
        temp.path(),
        allowed.clone(),
        Duration::from_secs(1800),
        client(&server),
    )
    .unwrap();
    assert_eq!(
        service
            .list_versions("registry.terraform.io", "hashicorp", "random")
            .await
            .unwrap(),
        vec!["3.6.2"]
    );
    drop(service);
    drop(server);

    let unavailable = MockServer::start().await;
    let restarted = ProviderMetadataService::with_registry_client(
        temp.path(),
        allowed,
        Duration::from_secs(1800),
        client(&unavailable),
    )
    .unwrap();
    assert_eq!(
        restarted
            .list_versions("registry.terraform.io", "hashicorp", "random")
            .await
            .unwrap(),
        vec!["3.6.2"]
    );
}

#[tokio::test]
async fn version_request_fetches_and_persists_package_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    mount_random(&server).await;
    let service = ProviderMetadataService::with_registry_client(
        temp.path(),
        HashSet::from(["registry.terraform.io".to_string()]),
        Duration::from_secs(1800),
        client(&server),
    )
    .unwrap();

    let version = service
        .get_version("registry.terraform.io", "hashicorp", "random", "3.6.2")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(version.platforms.len(), 1);
    assert_eq!(version.platforms[0].os, "linux");
    assert!(
        temp.path()
            .join("metadata/registry.terraform.io/hashicorp/random/3.6.2.json")
            .is_file()
    );
}

#[tokio::test]
async fn uncached_upstream_not_found_maps_to_not_found() {
    let temp = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/providers/hashicorp/missing/versions"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let service = ProviderMetadataService::with_registry_client(
        temp.path(),
        HashSet::from(["registry.terraform.io".to_string()]),
        Duration::from_secs(1800),
        client(&server),
    )
    .unwrap();

    let error = service
        .list_versions("registry.terraform.io", "hashicorp", "missing")
        .await
        .unwrap_err();
    assert!(matches!(error, MetadataError::NotFound));
}

#[tokio::test]
async fn stale_metadata_remains_available_when_origin_fails() {
    let temp = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    mount_random(&server).await;
    let allowed = HashSet::from(["registry.terraform.io".to_string()]);
    let service = ProviderMetadataService::with_registry_client(
        temp.path(),
        allowed.clone(),
        Duration::ZERO,
        client(&server),
    )
    .unwrap();
    assert_eq!(
        service
            .list_versions("registry.terraform.io", "hashicorp", "random")
            .await
            .unwrap(),
        vec!["3.6.2"]
    );
    drop(service);
    drop(server);

    let unavailable = MockServer::start().await;
    let restarted = ProviderMetadataService::with_registry_client(
        temp.path(),
        allowed,
        Duration::ZERO,
        client(&unavailable),
    )
    .unwrap();
    assert_eq!(
        restarted
            .list_versions("registry.terraform.io", "hashicorp", "random")
            .await
            .unwrap(),
        vec!["3.6.2"]
    );
}

#[tokio::test]
async fn sync_known_refreshes_provider_discovered_from_persistence() {
    let temp = tempfile::tempdir().unwrap();
    let first = MockServer::start().await;
    mount_random(&first).await;
    let allowed = HashSet::from(["registry.terraform.io".to_string()]);
    let service = ProviderMetadataService::with_registry_client(
        temp.path(),
        allowed.clone(),
        Duration::from_secs(1800),
        client(&first),
    )
    .unwrap();
    service
        .list_versions("registry.terraform.io", "hashicorp", "random")
        .await
        .unwrap();
    drop(service);

    let second = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/providers/hashicorp/random/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "versions": [{"version": "3.7.0", "platforms": []}]
        })))
        .mount(&second)
        .await;
    let restarted = ProviderMetadataService::with_registry_client(
        temp.path(),
        allowed,
        Duration::from_secs(1800),
        client(&second),
    )
    .unwrap();

    restarted.sync_known().await.unwrap();

    assert_eq!(
        restarted
            .list_versions("registry.terraform.io", "hashicorp", "random")
            .await
            .unwrap(),
        vec!["3.7.0"]
    );
}

#[tokio::test]
async fn invalid_upstream_platform_is_rejected_before_persistence() {
    let temp = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/providers/hashicorp/random/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "versions": [{
                "version": "3.6.2",
                "platforms": [{"os": "../linux", "arch": "amd64"}]
            }]
        })))
        .mount(&server)
        .await;
    let service = ProviderMetadataService::with_registry_client(
        temp.path(),
        HashSet::from(["registry.terraform.io".to_string()]),
        Duration::from_secs(1800),
        client(&server),
    )
    .unwrap();

    let error = service
        .list_versions("registry.terraform.io", "hashicorp", "random")
        .await
        .unwrap_err();

    assert!(matches!(error, MetadataError::InvalidAddress(_)));
    assert!(
        !temp
            .path()
            .join("metadata/registry.terraform.io/hashicorp/random/index.json")
            .exists()
    );
}
