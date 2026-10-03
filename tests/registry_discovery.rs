use open_tf_mirror::registry::RegistryClient;
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[tokio::test]
async fn discovery_preserves_relative_registry_prefix() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/terraform.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "providers.v1": "/registry/"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/registry/hashicorp/random/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "versions": []
        })))
        .mount(&server)
        .await;

    let registry = RegistryClient::with_discovery_origin("registry.example", server.uri()).unwrap();
    assert!(
        registry
            .versions("registry.example", "hashicorp", "random")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn discovery_accepts_absolute_service_url() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/terraform.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "providers.v1": format!("{}/absolute/", server.uri())
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/absolute/hashicorp/random/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "versions": []
        })))
        .mount(&server)
        .await;

    let registry = RegistryClient::with_discovery_origin("registry.example", server.uri()).unwrap();
    assert!(
        registry
            .versions("registry.example", "hashicorp", "random")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn discovery_requires_providers_v1() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/terraform.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "modules.v1": "/modules/"
        })))
        .mount(&server)
        .await;

    let registry = RegistryClient::with_discovery_origin("registry.example", server.uri()).unwrap();
    let error = registry
        .versions("registry.example", "hashicorp", "random")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("providers.v1"));
}

#[tokio::test]
async fn discovery_is_single_flight_for_parallel_first_requests() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/terraform.json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(50))
                .set_body_json(json!({ "providers.v1": "/registry/" })),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/registry/hashicorp/random/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "versions": [] })))
        .mount(&server)
        .await;

    let registry = RegistryClient::with_discovery_origin("registry.example", server.uri()).unwrap();
    let (left, right) = tokio::join!(
        registry.versions("registry.example", "hashicorp", "random"),
        registry.versions("registry.example", "hashicorp", "random")
    );
    assert!(left.unwrap().is_empty());
    assert!(right.unwrap().is_empty());
}

#[tokio::test]
async fn relative_service_uses_final_discovery_url() {
    let server = MockServer::start().await;
    Mock::given(path("/.well-known/terraform.json"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("Location", "/tenant/discovery.json"),
        )
        .mount(&server)
        .await;
    Mock::given(path("/tenant/discovery.json"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"providers.v1": "providers/"})),
        )
        .mount(&server)
        .await;
    Mock::given(path("/tenant/providers/hashicorp/random/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"versions": []})))
        .mount(&server)
        .await;
    let registry = RegistryClient::with_discovery_origin("registry.example", server.uri()).unwrap();
    assert!(
        registry
            .versions("registry.example", "hashicorp", "random")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn cold_discovery_failure_is_shared_across_providers() {
    let server = MockServer::start().await;
    Mock::given(path("/.well-known/terraform.json"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&server)
        .await;
    let registry = RegistryClient::with_discovery_origin("registry.example", server.uri()).unwrap();
    let (left, right) = tokio::join!(
        registry.versions("registry.example", "hashicorp", "random"),
        registry.versions("registry.example", "hashicorp", "null")
    );
    assert!(left.is_err());
    assert!(right.is_err());
}

#[tokio::test]
async fn package_uses_discovered_base_without_extra_prefix() {
    let server = MockServer::start().await;
    Mock::given(path("/.well-known/terraform.json"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"providers.v1": "/v1/providers/"})),
        )
        .mount(&server)
        .await;
    Mock::given(path(
        "/v1/providers/hashicorp/random/3.6.2/download/linux/amd64",
    ))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "os": "linux", "arch": "amd64",
        "filename": "terraform-provider-random_3.6.2_linux_amd64.zip",
        "download_url": "https://releases.example/random.zip", "shasum": "a".repeat(64)
    })))
    .expect(1)
    .mount(&server)
    .await;
    let registry = RegistryClient::with_discovery_origin("registry.example", server.uri()).unwrap();
    let platform = open_tf_mirror::registry::RegistryPlatform {
        os: "linux".into(),
        arch: "amd64".into(),
    };
    let package = registry
        .package(
            "registry.example",
            "hashicorp",
            "random",
            "3.6.2",
            &platform,
        )
        .await
        .unwrap();
    assert_eq!(package.os, "linux");
}
