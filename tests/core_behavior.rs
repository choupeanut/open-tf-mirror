use std::fs;

use open_tf_mirror::{
    module_mirror::{ModuleCache, ModuleId},
    provider::ArchiveName,
    storage::{ProviderArchiveKey, ProviderStorage},
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[test]
fn parses_upstream_compatible_provider_archive_names() {
    let parsed = ArchiveName::parse("random", "terraform-provider-random_3.6.2_linux_amd64.zip")
        .expect("valid archive should parse");

    assert_eq!(parsed.provider_type, "random");
    assert_eq!(parsed.version, "3.6.2");
    assert_eq!(parsed.os, "linux");
    assert_eq!(parsed.arch, "amd64");

    let dashed = ArchiveName::parse(
        "teleport",
        "terraform-provider-teleport-v14.3.3-darwin-arm64-bin.zip",
    )
    .expect("upstream accepted dash-separated archives should parse");

    assert_eq!(dashed.version, "14.3.3");
    assert_eq!(dashed.os, "darwin");
    assert_eq!(dashed.arch, "arm64");
}

#[test]
fn rejects_archive_when_type_does_not_match_route() {
    let err = ArchiveName::parse("aws", "terraform-provider-random_3.6.2_linux_amd64.zip")
        .expect_err("route type must match archive type");

    assert!(err.to_string().contains("invalid type"));
}

#[tokio::test]
async fn provider_storage_uses_terraform_mirror_compatible_layout() {
    let tmp = tempfile::tempdir().unwrap();
    let storage = ProviderStorage::new(tmp.path());
    let key = ProviderArchiveKey {
        hostname: "registry.terraform.io".into(),
        namespace: "hashicorp".into(),
        provider_type: "random".into(),
        filename: "terraform-provider-random_3.6.2_linux_amd64.zip".into(),
    };

    let path = storage.archive_path(&key);

    assert_eq!(
        path,
        tmp.path()
            .join("providers/registry.terraform.io/hashicorp/random/terraform-provider-random_3.6.2_linux_amd64.zip")
    );
}

#[tokio::test]
async fn module_cache_uses_local_archive_when_present() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = ModuleCache::new(tmp.path());
    let id = ModuleId {
        hostname: "registry.terraform.io".into(),
        namespace: "terraform-aws-modules".into(),
        name: "vpc".into(),
        system: "aws".into(),
        version: "5.8.1".into(),
    };
    let archive = cache.archive_path(&id);
    fs::create_dir_all(archive.parent().unwrap()).unwrap();
    fs::write(&archive, b"local-module").unwrap();

    let resolved = cache
        .load_or_fetch(&id, "https://example.invalid/unused.tar.gz")
        .await
        .unwrap();

    assert_eq!(resolved.path, archive);
    assert!(!resolved.fetched);
    assert_eq!(fs::read(resolved.path).unwrap(), b"local-module");
}

#[tokio::test]
async fn module_cache_fetches_official_source_when_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/module.tar.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes("remote-module"))
        .mount(&upstream)
        .await;

    let cache = ModuleCache::new(tmp.path());
    let id = ModuleId {
        hostname: "registry.terraform.io".into(),
        namespace: "terraform-aws-modules".into(),
        name: "vpc".into(),
        system: "aws".into(),
        version: "5.8.1".into(),
    };
    let resolved = cache
        .load_or_fetch(&id, &format!("{}/module.tar.gz", upstream.uri()))
        .await
        .unwrap();

    assert!(resolved.fetched);
    assert_eq!(fs::read(resolved.path).unwrap(), b"remote-module");
}
