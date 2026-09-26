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
        .and(path("/registry/v1/providers/hashicorp/random/versions"))
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
        .and(path("/absolute/v1/providers/hashicorp/random/versions"))
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
        .and(path("/registry/v1/providers/hashicorp/random/versions"))
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
