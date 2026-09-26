use std::{
    net::{IpAddr, SocketAddr},
    path::Path,
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use reqwest::{Certificate, Client, Response, Url};
use tokio::net::lookup_host;

const MAX_REDIRECTS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundMode {
    Direct,
    TrustedProxy,
}

impl FromStr for OutboundMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "direct" => Ok(Self::Direct),
            "trusted-proxy" => Ok(Self::TrustedProxy),
            _ => Err("outbound mode must be direct or trusted-proxy".into()),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OutboundError {
    #[error("outbound URL policy violation: {0}")]
    Policy(String),
    #[error("outbound redirect is invalid: {0}")]
    Redirect(String),
    #[error("outbound request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("outbound DNS lookup failed: {0}")]
    Dns(#[from] std::io::Error),
    #[error("trusted-proxy mode requires HTTP_PROXY or HTTPS_PROXY")]
    ProxyRequired,
    #[error("invalid upstream CA bundle: {0}")]
    InvalidCa(String),
}

#[derive(Debug, Clone)]
pub struct OutboundClient {
    mode: OutboundMode,
    ca_bundle: Arc<Vec<Vec<u8>>>,
    allow_http: bool,
    allow_non_public_ips: bool,
}

impl OutboundClient {
    pub fn new(mode: OutboundMode, ca_file: Option<&Path>) -> Result<Self, OutboundError> {
        if mode == OutboundMode::TrustedProxy && !proxy_configured() {
            return Err(OutboundError::ProxyRequired);
        }
        let ca_bundle = match ca_file {
            Some(path) => {
                let pem = std::fs::read(path)
                    .map_err(|error| OutboundError::InvalidCa(error.to_string()))?;
                let certificates = Certificate::from_pem_bundle(&pem)
                    .map_err(|error| OutboundError::InvalidCa(error.to_string()))?;
                if certificates.is_empty() {
                    return Err(OutboundError::InvalidCa(
                        "CA bundle does not contain a PEM certificate".into(),
                    ));
                }
                for certificate in certificates {
                    Client::builder()
                        .add_root_certificate(certificate)
                        .build()
                        .map_err(|error| OutboundError::InvalidCa(error.to_string()))?;
                }
                vec![pem]
            }
            None => Vec::new(),
        };
        Ok(Self {
            mode,
            ca_bundle: Arc::new(ca_bundle),
            allow_http: false,
            allow_non_public_ips: false,
        })
    }

    pub fn for_tests() -> Self {
        Self {
            mode: OutboundMode::Direct,
            ca_bundle: Arc::default(),
            allow_http: true,
            allow_non_public_ips: true,
        }
    }

    pub fn mode(&self) -> OutboundMode {
        self.mode
    }

    pub fn allows_http_for_tests(&self) -> bool {
        self.allow_http
    }

    pub async fn get(&self, mut url: Url, timeout: Duration) -> Result<Response, OutboundError> {
        for redirect_count in 0..=MAX_REDIRECTS {
            let client = self.client_for_url(&url).await?;
            let response = client.get(url.clone()).timeout(timeout).send().await?;
            if !is_redirect(response.status()) {
                return Ok(response);
            }
            if redirect_count == MAX_REDIRECTS {
                return Err(OutboundError::Redirect(
                    "maximum redirect count exceeded".into(),
                ));
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .ok_or_else(|| OutboundError::Redirect("missing Location header".into()))?
                .to_str()
                .map_err(|_| OutboundError::Redirect("invalid Location header".into()))?;
            url = url
                .join(location)
                .map_err(|_| OutboundError::Redirect("invalid Location URL".into()))?;
            self.validate_url(&url)?;
        }
        unreachable!("bounded redirect loop returns")
    }

    fn validate_url(&self, url: &Url) -> Result<(), OutboundError> {
        if url.scheme() != "https" && !(self.allow_http && url.scheme() == "http") {
            return Err(OutboundError::Policy("HTTPS is required".into()));
        }
        if url.host_str().is_none() {
            return Err(OutboundError::Policy("URL host is required".into()));
        }
        Ok(())
    }

    async fn client_for_url(&self, url: &Url) -> Result<Client, OutboundError> {
        self.validate_url(url)?;
        let mut builder = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none());
        for pem in self.ca_bundle.iter() {
            let certificate = Certificate::from_pem(pem)
                .map_err(|error| OutboundError::InvalidCa(error.to_string()))?;
            builder = builder.add_root_certificate(certificate);
        }
        if self.mode == OutboundMode::Direct {
            builder = builder.no_proxy();
            let host = url
                .host_str()
                .ok_or_else(|| OutboundError::Policy("URL host is required".into()))?;
            let resolver_host = host
                .strip_prefix('[')
                .and_then(|value| value.strip_suffix(']'))
                .unwrap_or(host);
            let port = url
                .port_or_known_default()
                .ok_or_else(|| OutboundError::Policy("URL port is required".into()))?;
            let addresses = lookup_host((resolver_host, port))
                .await?
                .collect::<Vec<SocketAddr>>();
            if addresses.is_empty() {
                return Err(OutboundError::Policy("URL host did not resolve".into()));
            }
            if !self.allow_non_public_ips
                && addresses.iter().any(|address| !is_public_ip(address.ip()))
            {
                return Err(OutboundError::Policy(
                    "URL resolves to a non-public address".into(),
                ));
            }
            builder = builder.resolve_to_addrs(resolver_host, &addresses);
        }
        Ok(builder.build()?)
    }
}

fn proxy_configured() -> bool {
    ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"]
        .into_iter()
        .any(|name| {
            std::env::var(name)
                .ok()
                .is_some_and(|value| !value.trim().is_empty())
        })
}

fn is_redirect(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::MOVED_PERMANENTLY
            | reqwest::StatusCode::FOUND
            | reqwest::StatusCode::SEE_OTHER
            | reqwest::StatusCode::TEMPORARY_REDIRECT
            | reqwest::StatusCode::PERMANENT_REDIRECT
    )
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [first, second, third, _] = ip.octets();
            !ip.is_private()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_unspecified()
                && !ip.is_multicast()
                && first != 0
                && first < 224
                && !(first == 100 && (64..=127).contains(&second))
                && !(first == 192 && second == 0)
                && !(first == 192 && second == 88 && third == 99)
                && !(first == 192 && second == 0 && third == 2)
                && !(first == 198 && matches!(second, 18 | 19))
                && !(first == 198 && second == 51 && third == 100)
                && !(first == 203 && second == 0 && third == 113)
        }
        IpAddr::V6(ip) => {
            if let Some(ipv4) = ip.to_ipv4() {
                return is_public_ip(IpAddr::V4(ipv4));
            }
            let segments = ip.segments();
            !ip.is_loopback()
                && !ip.is_unspecified()
                && !ip.is_multicast()
                && !ip.is_unique_local()
                && !ip.is_unicast_link_local()
                && (segments[0] & 0xffc0) != 0xfec0
                && !(segments[0] == 0x2001 && segments[1] == 0x0db8)
        }
    }
}
