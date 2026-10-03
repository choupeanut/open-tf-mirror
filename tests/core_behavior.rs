use open_tf_mirror::{
    provider::ArchiveName,
    storage::{ProviderArchiveKey, ProviderStorage},
};

#[test]
fn parses_upstream_compatible_provider_archive_names() {
    let parsed = ArchiveName::parse("random", "terraform-provider-random_3.6.2_linux_amd64.zip")
        .expect("valid archive should parse");

    assert_eq!(parsed.provider_type, "random");
    assert_eq!(parsed.version, "3.6.2");
    assert_eq!(parsed.os, "linux");
    assert_eq!(parsed.arch, "amd64");

    let prerelease = ArchiveName::parse(
        "random",
        "terraform-provider-random_1.0.0-beta.1_linux_amd64.zip",
    )
    .expect("semantic-version prerelease archives should parse");
    assert_eq!(prerelease.version, "1.0.0-beta.1");

    let simple_prerelease = ArchiveName::parse(
        "random",
        "terraform-provider-random_1.0.0-beta_linux_amd64.zip",
    )
    .expect("simple semantic-version prerelease archives should parse");
    assert_eq!(simple_prerelease.version, "1.0.0-beta");
    assert_eq!(simple_prerelease.os, "linux");
    assert_eq!(simple_prerelease.arch, "amd64");

    let build = ArchiveName::parse(
        "random",
        "terraform-provider-random_1.0.0+build.7_linux_amd64.zip",
    )
    .expect("semantic-version build metadata archives should parse");
    assert_eq!(build.version, "1.0.0+build.7");

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
