use std::{
    collections::HashSet,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderValue, Method, Request, StatusCode, header, uri::Authority},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use clap::{ArgAction, Parser};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    service::TowerToHyperService,
};
use open_tf_mirror::{
    http_api::{AppState, RouterOptions, build_router_with_options},
    metadata::ProviderMetadataStore,
    module_mirror::ModuleCache,
    storage::ProviderStorage,
    tls_reload::ReloadingCertResolver,
};
use rustls::ServerConfig;
use tokio::{
    net::TcpListener,
    sync::{Semaphore, watch},
    task::JoinSet,
};
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

    #[arg(
        long,
        env = "SERVER_ALLOWED_REGISTRIES",
        value_delimiter = ',',
        default_value = "registry.terraform.io"
    )]
    allowed_registries: Vec<String>,

    #[arg(long, env = "SERVER_ENABLE_MODULE_MIRROR", default_value_t = false, action = ArgAction::Set)]
    enable_module_mirror: bool,

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
    install_crypto_provider()?;
    let args = Args::parse();
    init_tracing(args.log_debug, args.log_verbosity);
    prepare_data_dir(&args.data_source_dir).await?;

    let state = AppState {
        metadata: ProviderMetadataStore::new(
            &args.data_source_dir,
            args.allowed_registries
                .iter()
                .cloned()
                .collect::<HashSet<_>>(),
            Duration::from_secs(30 * 60),
        )?,
        provider_storage: ProviderStorage::new(&args.data_source_dir),
        module_cache: ModuleCache::new(&args.data_source_dir),
        module_registry_base: args.module_registry_base,
        data_dir: Arc::new(args.data_source_dir.clone()),
    };
    let app = build_router_with_options(
        state,
        RouterOptions {
            enable_module_mirror: args.enable_module_mirror,
            conn_qps: args.conn_qps,
            conn_burst: args.conn_burst,
        },
    )
    .layer(
        ServiceBuilder::new()
            .layer(TraceLayer::new_for_http())
            .into_inner(),
    );

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(wait_for_shutdown_signal(shutdown_tx));
    let http_addr = bind_addr(&args.bind_address, args.http_port, "HTTP")?;

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
        let http_app = app.clone().layer(middleware::from_fn_with_state(
            args.https_port,
            redirect_http_to_https,
        ));
        let http = serve_http(http_addr, http_app, shutdown_rx.clone());
        let https = serve_https(
            https_addr,
            cert.clone(),
            key.clone(),
            app,
            args.conn_burst as usize,
            shutdown_rx,
        );
        tokio::try_join!(http, https)?;
    } else {
        serve_http(http_addr, app, shutdown_rx).await?;
    }

    Ok(())
}

fn install_crypto_provider() -> Result<()> {
    if rustls::crypto::CryptoProvider::get_default().is_some() {
        return Ok(());
    }
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("a different rustls crypto provider was already installed"))
}

async fn prepare_data_dir(root: &Path) -> Result<()> {
    for directory in [
        root.to_path_buf(),
        root.join("metadata"),
        root.join("providers"),
    ] {
        tokio::fs::create_dir_all(&directory)
            .await
            .with_context(|| format!("create cache directory {}", directory.display()))?;
    }
    let probe = root.join(format!(".startup-write-test-{}", std::process::id()));
    tokio::fs::write(&probe, b"ok")
        .await
        .with_context(|| format!("write cache directory {}", root.display()))?;
    tokio::fs::remove_file(&probe)
        .await
        .with_context(|| format!("clean cache directory {}", root.display()))?;
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

async fn serve_http(
    addr: SocketAddr,
    app: Router,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind HTTP listener on {addr}"))?;
    tracing::info!(%addr, "serving HTTP");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            while !*shutdown.borrow() && shutdown.changed().await.is_ok() {}
        })
        .await
        .context("serve HTTP listener")
}

async fn serve_https(
    addr: SocketAddr,
    cert_path: PathBuf,
    key_path: PathBuf,
    app: Router,
    conn_burst: usize,
    mut shutdown: watch::Receiver<bool>,
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
    let mut connections = JoinSet::new();

    loop {
        let accepted = tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
                continue;
            }
            accepted = listener.accept() => accepted.context("accept TLS connection")?,
        };
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .context("connection limiter closed")?;
        let (stream, peer_addr) = accepted;
        let acceptor = acceptor.clone();
        let app = app.clone();
        connections.spawn(async move {
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
        while connections.try_join_next().is_some() {}
    }

    let drained = tokio::time::timeout(Duration::from_secs(15), async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        connections.abort_all();
    }
    Ok(())
}

async fn redirect_http_to_https(
    State(https_port): State<u16>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if matches!(request.uri().path(), "/readyz" | "/livez") {
        return next.run(request).await;
    }
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return (StatusCode::BAD_REQUEST, "Use HTTPS").into_response();
    }
    let Some(host) = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(location) = redirect_location(
        host,
        request
            .uri()
            .path_and_query()
            .map_or("/", |value| value.as_str()),
        https_port,
    ) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let mut response = StatusCode::FOUND.into_response();
    response.headers_mut().insert(header::LOCATION, location);
    response
}

fn redirect_location(host: &str, path_and_query: &str, https_port: u16) -> Option<HeaderValue> {
    let authority = host.parse::<Authority>().ok()?;
    let hostname = authority.host();
    let hostname = if hostname.contains(':') && !hostname.starts_with('[') {
        format!("[{hostname}]")
    } else {
        hostname.to_string()
    };
    let authority = if https_port == 443 {
        hostname
    } else {
        format!("{hostname}:{https_port}")
    };
    HeaderValue::from_str(&format!("https://{authority}{path_and_query}")).ok()
}

async fn wait_for_shutdown_signal(shutdown: watch::Sender<bool>) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            result = tokio::signal::ctrl_c() => { let _ = result; }
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    let _ = shutdown.send(true);
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Args, bind_addr, install_crypto_provider, redirect_location};

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
    fn cli_defaults_to_terraform_registry_and_disables_module_mirror() {
        let args = Args::parse_from(["open-tf-mirror", "--enable-tls=false"]);

        assert_eq!(args.allowed_registries, vec!["registry.terraform.io"]);
        assert!(!args.enable_module_mirror);
    }

    #[test]
    fn bind_address_uses_configured_port() {
        let addr = bind_addr("127.0.0.1", 18080, "HTTP").unwrap();

        assert_eq!(addr.port(), 18080);
    }

    #[test]
    fn http_redirect_targets_configured_https_port() {
        assert_eq!(
            redirect_location("mirror.example.test:8080", "/v1/providers/?x=1", 8443).unwrap(),
            "https://mirror.example.test:8443/v1/providers/?x=1"
        );
        assert_eq!(
            redirect_location("mirror.example.test:80", "/ready", 443).unwrap(),
            "https://mirror.example.test/ready"
        );
    }

    #[test]
    fn crypto_provider_installation_is_idempotent() {
        install_crypto_provider().unwrap();
        install_crypto_provider().unwrap();
    }
}
