use std::fs;

use open_tf_mirror::outbound::{OutboundClient, OutboundMode};
use rcgen::generate_simple_self_signed;
use reqwest::Url;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[tokio::test]
async fn test_outbound_follows_redirect_and_returns_complete_response() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/start"))
        .respond_with(ResponseTemplate::new(302).insert_header("Location", "/archive"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/archive"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes("archive"))
        .mount(&server)
        .await;

    let client = OutboundClient::for_tests();
    let response = client
        .get(
            Url::parse(&format!("{}/start", server.uri())).unwrap(),
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(response.text().await.unwrap(), "archive");
}

#[tokio::test]
async fn direct_outbound_rejects_private_and_http_destinations() {
    let client = OutboundClient::new(OutboundMode::Direct, None).unwrap();
    for value in ["http://127.0.0.1/archive", "https://127.0.0.1/archive"] {
        let error = client
            .get(
                Url::parse(value).unwrap(),
                std::time::Duration::from_secs(1),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("policy"), "{value}: {error}");
    }
}

#[test]
fn upstream_ca_is_validated_at_startup() {
    let temp = tempfile::tempdir().unwrap();
    let malformed = temp.path().join("bad-ca.pem");
    fs::write(&malformed, b"not a certificate").unwrap();
    assert!(OutboundClient::new(OutboundMode::Direct, Some(&malformed)).is_err());

    let cert = generate_simple_self_signed(vec!["upstream.example".into()]).unwrap();
    let valid = temp.path().join("ca.pem");
    fs::write(&valid, cert.cert.pem()).unwrap();
    assert!(OutboundClient::new(OutboundMode::Direct, Some(&valid)).is_ok());
}
