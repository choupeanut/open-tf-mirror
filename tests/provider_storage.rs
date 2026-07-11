use std::{fs, path::Path, sync::Arc, time::Duration};

use open_tf_mirror::{
    metadata::PlatformMetadata,
    storage::{ProviderArchiveKey, ProviderStorage},
};
use sha2::{Digest, Sha256};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn key(filename: &str) -> ProviderArchiveKey {
    ProviderArchiveKey {
        hostname: "registry.terraform.io".into(),
        namespace: "hashicorp".into(),
        provider_type: "random".into(),
        filename: filename.into(),
    }
}

fn metadata(url: String, body: &[u8]) -> PlatformMetadata {
    PlatformMetadata {
        os: "linux".into(),
        arch: "amd64".into(),
        filename: "terraform-provider-random_3.6.2_linux_amd64.zip".into(),
        shasum: Some(hex::encode(Sha256::digest(body))),
        download_url: url,
    }
}

#[tokio::test]
async fn bundled_mirror_takes_precedence_over_persistent_cache() {
    let temp = tempfile::tempdir().unwrap();
    let bundled = tempfile::tempdir().unwrap();
    let filename = "terraform-provider-random_3.6.2_linux_amd64.zip";
    let archive = bundled
        .path()
        .join("registry.terraform.io/hashicorp/random")
        .join(filename);
    fs::create_dir_all(archive.parent().unwrap()).unwrap();
    fs::write(&archive, b"bundled").unwrap();
    let storage = ProviderStorage::with_bundled_mirror(temp.path(), Some(bundled.path())).unwrap();

    let resolved = storage
        .load_or_fetch(
            &key(filename),
            &metadata("http://unused.invalid".into(), b"bundled"),
        )
        .await
        .unwrap();

    assert_eq!(resolved, archive);
}

#[tokio::test]
async fn concurrent_cache_miss_downloads_once_and_publishes_verified_archive() {
    let temp = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let body = b"verified archive";
    Mock::given(method("GET"))
        .and(path("/provider.zip"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(&server)
        .await;
    let storage =
        Arc::new(ProviderStorage::new_for_tests(temp.path(), None::<&Path>, 1024 * 1024).unwrap());
    let filename = "terraform-provider-random_3.6.2_linux_amd64.zip";
    let key = key(filename);
    let metadata = metadata(format!("{}/provider.zip", server.uri()), body);

    let (left, right) = tokio::join!(
        storage.load_or_fetch(&key, &metadata),
        storage.load_or_fetch(&key, &metadata)
    );

    let path = left.unwrap();
    assert_eq!(right.unwrap(), path);
    assert_eq!(fs::read(path).unwrap(), body);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert_eq!(storage.active_lock_count(), 0);
}

#[tokio::test]
async fn checksum_mismatch_is_rejected_and_temporary_file_is_removed() {
    let temp = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/provider.zip"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes("corrupt"))
        .mount(&server)
        .await;
    let storage = ProviderStorage::new_for_tests(temp.path(), None::<&Path>, 1024).unwrap();
    let filename = "terraform-provider-random_3.6.2_linux_amd64.zip";
    let mut metadata = metadata(format!("{}/provider.zip", server.uri()), b"expected");
    metadata.filename = filename.into();

    let error = storage
        .load_or_fetch(&key(filename), &metadata)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("checksum"));
    let directory = temp
        .path()
        .join("providers/registry.terraform.io/hashicorp/random");
    assert!(!storage.archive_path(&key(filename)).exists());
    assert!(
        !directory.exists()
            || fs::read_dir(directory).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp"))
    );
    assert_eq!(storage.active_lock_count(), 0);
}

#[tokio::test]
async fn production_policy_rejects_http_and_private_ip_targets() {
    let temp = tempfile::tempdir().unwrap();
    let storage = ProviderStorage::new(temp.path());
    let filename = "terraform-provider-random_3.6.2_linux_amd64.zip";

    for url in [
        "http://example.com/provider.zip",
        "https://127.0.0.1/provider.zip",
        "https://0.0.0.1/provider.zip",
        "https://100.64.0.1/provider.zip",
        "https://192.0.2.1/provider.zip",
        "https://198.18.0.1/provider.zip",
        "https://198.51.100.1/provider.zip",
        "https://203.0.113.1/provider.zip",
        "https://240.0.0.1/provider.zip",
        "https://[2001:db8::1]/provider.zip",
    ] {
        let error = storage
            .load_or_fetch(&key(filename), &metadata(url.into(), b"archive"))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("policy"),
            "{url} returned unexpected error: {error}"
        );
    }
    assert_eq!(storage.active_lock_count(), 0);
}

#[tokio::test]
async fn http_error_removes_temporary_file_and_lock() {
    let temp = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/provider.zip"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let storage = ProviderStorage::new_for_tests(temp.path(), None::<&Path>, 1024).unwrap();
    let filename = "terraform-provider-random_3.6.2_linux_amd64.zip";

    assert!(
        storage
            .load_or_fetch(
                &key(filename),
                &metadata(format!("{}/provider.zip", server.uri()), b"body")
            )
            .await
            .is_err()
    );

    assert_no_temporary_files(temp.path());
    assert_eq!(storage.active_lock_count(), 0);
}

#[tokio::test]
async fn oversized_stream_removes_temporary_file_and_lock() {
    let temp = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/provider.zip"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes("too large"))
        .mount(&server)
        .await;
    let storage = ProviderStorage::new_for_tests(temp.path(), None::<&Path>, 4).unwrap();
    let filename = "terraform-provider-random_3.6.2_linux_amd64.zip";

    let error = storage
        .load_or_fetch(
            &key(filename),
            &metadata(format!("{}/provider.zip", server.uri()), b"too large"),
        )
        .await
        .unwrap_err();

    assert!(error.to_string().contains("maximum size"));
    assert_no_temporary_files(temp.path());
    assert_eq!(storage.active_lock_count(), 0);
}

#[tokio::test]
async fn global_download_limit_allows_at_most_eight_upstream_transfers() {
    let temp = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let storage =
        Arc::new(ProviderStorage::new_for_tests(temp.path(), None::<&Path>, 1024).unwrap());
    let mut tasks = Vec::new();
    for index in 0..9 {
        let version = format!("3.6.{index}");
        let filename = format!("terraform-provider-random_{version}_linux_amd64.zip");
        let route = format!("/provider-{index}.zip");
        Mock::given(method("GET"))
            .and(path(route.clone()))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(300))
                    .set_body_bytes("archive"),
            )
            .mount(&server)
            .await;
        let storage = Arc::clone(&storage);
        let server_uri = server.uri();
        tasks.push(tokio::spawn(async move {
            let key = key(&filename);
            let mut metadata = metadata(format!("{server_uri}{route}"), b"archive");
            metadata.filename = filename;
            storage.load_or_fetch(&key, &metadata).await
        }));
    }

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(storage.available_download_permits(), 0);
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert_eq!(storage.available_download_permits(), 8);
    assert_eq!(storage.active_lock_count(), 0);
}

fn assert_no_temporary_files(root: &Path) {
    let directory = root.join("providers/registry.terraform.io/hashicorp/random");
    assert!(
        !directory.exists()
            || fs::read_dir(directory).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp"))
    );
}

#[tokio::test]
async fn cache_hit_does_not_contact_upstream() {
    let temp = tempfile::tempdir().unwrap();
    let storage = ProviderStorage::new(temp.path());
    let filename = "terraform-provider-random_3.6.2_linux_amd64.zip";
    let archive = storage.archive_path(&key(filename));
    fs::create_dir_all(archive.parent().unwrap()).unwrap();
    fs::write(&archive, b"cached").unwrap();

    let resolved = storage
        .load_or_fetch(
            &key(filename),
            &metadata("http://127.0.0.1:1/unused".into(), b"cached"),
        )
        .await
        .unwrap();

    assert_eq!(resolved, archive);
}

#[tokio::test]
async fn traversal_components_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let storage = ProviderStorage::new(temp.path());
    let invalid = ProviderArchiveKey {
        hostname: "registry.terraform.io".into(),
        namespace: "..".into(),
        provider_type: "random".into(),
        filename: "terraform-provider-random_3.6.2_linux_amd64.zip".into(),
    };

    let error = storage
        .load_or_fetch(
            &invalid,
            &metadata("http://unused.invalid".into(), b"archive"),
        )
        .await
        .unwrap_err();

    assert!(error.to_string().contains("invalid"));
}
