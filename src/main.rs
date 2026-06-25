use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use axum::Router;
use clap::{ArgAction, Parser};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    service::TowerToHyperService,
};
use open_tf_mirror::{
    http_api::{AppState, build_router},
    metadata::ProviderMetadataStore,
    module_mirror::ModuleCache,
    storage::ProviderStorage,
    tls_reload::ReloadingCertResolver,
};
use rustls::ServerConfig;
use tokio::{net::TcpListener, sync::Semaphore};
use tokio_rustls::TlsAcceptor;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "open-tf-mirror", version)]
struct Args {
    #[arg(long, env = "SERVER_BIND_ADDRESS", default_value = "0.0.0.0")]
    bind_address: String,

    #[arg(long, env = "SERVER_HTTP_PORT", default_value_t = 8080)]
    http_port: u16,

    #[arg(long, env = "SERVER_HTTPS_PORT", default_value_t = 8443)]
    https_port: u16,

    #[arg(long, env = "SERVER_ENABLE_TLS", default_value_t = true, action = ArgAction::Set)]
    enable_tls: bool,

    #[arg(long, env = "SERVER_TLS_CERT_FILE")]
    tls_cert_file: Option<PathBuf>,

    #[arg(long, env = "SERVER_TLS_PRIVATE_KEY_FILE")]
    tls_private_key_file: Option<PathBuf>,

    #[arg(long, env = "SERVER_TLS_AUTO_CERT_DOMAINS", value_delimiter = ',')]
    tls_auto_cert_domains: Vec<String>,

    #[arg(
        long,
        env = "SERVER_DATA_SOURCE_DIR",
        default_value = "/var/run/open-tf-mirror"
    )]
    data_source_dir: PathBuf,

    #[arg(
        long,
        env = "SERVER_MODULE_REGISTRY_BASE",
        default_value = "https://registry.terraform.io"
    )]
    module_registry_base: String,

    #[arg(long, default_value_t = false)]
    log_debug: bool,

    #[arg(long, default_value_t = 0)]
    log_verbosity: u8,

    #[arg(long, default_value_t = 100)]
    conn_qps: u32,

    #[arg(long, default_value_t = 200)]
    conn_burst: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    init_tracing(args.log_debug, args.log_verbosity);

    let state = AppState {
        metadata: ProviderMetadataStore::default(),
        provider_storage: ProviderStorage::new(&args.data_source_dir),
        module_cache: ModuleCache::new(&args.data_source_dir),
        module_registry_base: args.module_registry_base,
    };
    let app = build_router(state).layer(
        ServiceBuilder::new()
            .layer(TraceLayer::new_for_http())
            .into_inner(),
    );

    let http_addr = bind_addr(&args.bind_address, args.http_port, "HTTP")?;
    let http = serve_http(http_addr, app.clone());

    if args.enable_tls {
        if !args.tls_auto_cert_domains.is_empty()
            && (args.tls_cert_file.is_none() || args.tls_private_key_file.is_none())
        {
            anyhow::bail!(
                "--tls-auto-cert-domains is accepted for chart compatibility, but ACME auto certificate issuance is not implemented yet; configure --tls-cert-file and --tls-private-key-file"
            );
        }
        let cert = args
            .tls_cert_file
            .as_ref()
            .context("--tls-cert-file is required when TLS is enabled")?;
        let key = args
            .tls_private_key_file
            .as_ref()
            .context("--tls-private-key-file is required when TLS is enabled")?;
        let https_addr = bind_addr(&args.bind_address, args.https_port, "HTTPS")?;
        let https = serve_https(
            https_addr,
            cert.clone(),
            key.clone(),
            app,
            args.conn_burst as usize,
        );
        tokio::try_join!(http, https)?;
    } else {
        http.await?;
    }

    Ok(())
}

fn bind_addr(bind_address: &str, port: u16, label: &str) -> Result<SocketAddr> {
    format!("{bind_address}:{port}")
        .parse()
        .with_context(|| format!("parse {label} bind address"))
}

fn init_tracing(debug: bool, verbosity: u8) {
    let default_level = if debug || verbosity > 0 {
        "debug"
    } else {
        "info"
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

async fn serve_http(addr: SocketAddr, app: Router) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind HTTP listener on {addr}"))?;
    tracing::info!(%addr, "serving HTTP");
    axum::serve(listener, app)
        .await
        .context("serve HTTP listener")
}

async fn serve_https(
    addr: SocketAddr,
    cert_path: PathBuf,
    key_path: PathBuf,
    app: Router,
    conn_burst: usize,
) -> Result<()> {
    let resolver = ReloadingCertResolver::new(cert_path, key_path)?;
    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(resolver));
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind HTTPS listener on {addr}"))?;
    tracing::info!(%addr, "serving HTTPS");
    let semaphore = Arc::new(Semaphore::new(conn_burst.max(1)));

    loop {
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .context("connection limiter closed")?;
        let (stream, peer_addr) = listener.accept().await.context("accept TLS connection")?;
        let acceptor = acceptor.clone();
        let app = app.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let Ok(stream) = acceptor.accept(stream).await else {
                tracing::debug!(%peer_addr, "TLS handshake failed");
                return;
            };
            let io = TokioIo::new(stream);
            let service = TowerToHyperService::new(app);
            if let Err(err) = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                .serve_connection(io, service)
                .await
            {
                tracing::debug!(%peer_addr, error = %err, "HTTPS connection failed");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Args, bind_addr};

    #[test]
    fn cli_defaults_to_unprivileged_container_ports() {
        let args = Args::parse_from(["open-tf-mirror", "--enable-tls=false"]);

        assert_eq!(args.http_port, 8080);
        assert_eq!(args.https_port, 8443);
    }

    #[test]
    fn cli_accepts_auto_cert_domain_argument_for_chart_compatibility() {
        let args = Args::parse_from([
            "open-tf-mirror",
            "--tls-auto-cert-domains=mirror.example.com",
            "--enable-tls=false",
        ]);

        assert_eq!(
            args.tls_auto_cert_domains,
            vec!["mirror.example.com".to_string()]
        );
    }

    #[test]
    fn bind_address_uses_configured_port() {
        let addr = bind_addr("127.0.0.1", 18080, "HTTP").unwrap();

        assert_eq!(addr.port(), 18080);
    }
}
