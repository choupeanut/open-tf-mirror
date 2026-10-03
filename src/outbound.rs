use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::Path,
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use reqwest::{
    Certificate, Client, Response, Url,
    dns::{Addrs, Name, Resolve, Resolving},
};
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
    client: Client,
    allow_http: bool,
    allow_non_public_ips: bool,
}

/// Raised by the resolver when a name resolves to a forbidden address.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct ResolvePolicyError(&'static str);

/// Resolver for direct mode: reqwest connects only to the addresses returned
/// here, so rejecting non-public results prevents DNS rebinding.
struct PublicOnlyResolver {
    allow_non_public_ips: bool,
}

impl Resolve for PublicOnlyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allow_non_public_ips = self.allow_non_public_ips;
        Box::pin(async move {
            let addresses = lookup_host((name.as_str(), 0))
                .await?
                .collect::<Vec<SocketAddr>>();
            if addresses.is_empty() {
                return Err(ResolvePolicyError("URL host did not resolve").into());
            }
            if !allow_non_public_ips && addresses.iter().any(|address| !is_public_ip(address.ip()))
            {
                return Err(ResolvePolicyError("URL resolves to a non-public address").into());
            }
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

impl OutboundClient {
    pub fn new(mode: OutboundMode, ca_file: Option<&Path>) -> Result<Self, OutboundError> {
        if mode == OutboundMode::TrustedProxy && !proxy_configured() {
            return Err(OutboundError::ProxyRequired);
        }
        let mut certificates = Vec::new();
        if let Some(path) = ca_file {
            let pem =
                std::fs::read(path).map_err(|error| OutboundError::InvalidCa(error.to_string()))?;
            certificates = Certificate::from_pem_bundle(&pem)
                .map_err(|error| OutboundError::InvalidCa(error.to_string()))?;
            if certificates.is_empty() {
                return Err(OutboundError::InvalidCa(
                    "CA bundle does not contain a PEM certificate".into(),
                ));
            }
        }
        Self::build(mode, certificates, false, false)
    }

    pub fn for_tests() -> Self {
        Self::build(OutboundMode::Direct, Vec::new(), true, true)
            .expect("test outbound client builds")
    }

    fn build(
        mode: OutboundMode,
        certificates: Vec<Certificate>,
        allow_http: bool,
        allow_non_public_ips: bool,
    ) -> Result<Self, OutboundError> {
        let mut builder = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none());
        for certificate in certificates {
            builder = builder.add_root_certificate(certificate);
        }
        if mode == OutboundMode::Direct {
            builder = builder
                .no_proxy()
                .dns_resolver(Arc::new(PublicOnlyResolver {
                    allow_non_public_ips,
                }));
        }
        let client = builder
            .build()
            .map_err(|error| OutboundError::InvalidCa(error.to_string()))?;
        Ok(Self {
            mode,
            client,
            allow_http,
            allow_non_public_ips,
        })
    }

    pub fn mode(&self) -> OutboundMode {
        self.mode
    }

    pub fn allows_http_for_tests(&self) -> bool {
        self.allow_http
    }

    pub async fn get(&self, mut url: Url, timeout: Duration) -> Result<Response, OutboundError> {
        for redirect_count in 0..=MAX_REDIRECTS {
            self.validate_url(&url)?;
            let response = self
                .client
                .get(url.clone())
                .timeout(timeout)
                .send()
                .await
                .map_err(map_request_error)?;
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
        let host = url
            .host_str()
            .ok_or_else(|| OutboundError::Policy("URL host is required".into()))?;
        // IP literals bypass the DNS resolver, so check them here.
        let literal_ip = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .ok();
        if self.mode == OutboundMode::Direct
            && !self.allow_non_public_ips
            && literal_ip.is_some_and(|ip| !is_public_ip(ip))
        {
            return Err(OutboundError::Policy(
                "URL targets a non-public address".into(),
            ));
        }
        Ok(())
    }
}

fn map_request_error(error: reqwest::Error) -> OutboundError {
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        if let Some(policy) = cause.downcast_ref::<ResolvePolicyError>() {
            return OutboundError::Policy(policy.0.into());
        }
        source = cause.source();
    }
    OutboundError::Request(error)
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
            let embedded_v4 = |high: u16, low: u16| {
                let [a, b] = high.to_be_bytes();
                let [c, d] = low.to_be_bytes();
                Ipv4Addr::new(a, b, c, d)
            };
            // NAT64 64:ff9b::/96 and 6to4 2002::/16 embed an IPv4 address.
            if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                return is_public_ip(IpAddr::V4(embedded_v4(segments[6], segments[7])));
            }
            if segments[0] == 0x2002 {
                return is_public_ip(IpAddr::V4(embedded_v4(segments[1], segments[2])));
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn public(value: &str) -> bool {
        is_public_ip(value.parse().unwrap())
    }

    #[test]
    fn embedded_ipv4_in_nat64_and_6to4_is_checked() {
        assert!(!public("64:ff9b::7f00:1"));
        assert!(!public("64:ff9b::a00:1"));
        assert!(public("64:ff9b::808:808"));
        assert!(!public("2002:7f00:1::1"));
        assert!(!public("2002:c0a8:101::"));
        assert!(public("2002:808:808::1"));
        assert!(!public("::ffff:127.0.0.1"));
        assert!(public("2606:4700:4700::1111"));
    }
}
