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
    rt::{TokioExecutor, TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use open_tf_mirror::{
    http_api::{AppState, RouterOptions, build_router_with_options},
    metadata::ProviderMetadataStore,
    outbound::{OutboundClient, OutboundMode},
    registry::RegistryClient,
    storage::{ProviderStorage, remove_stale_temp_files},
    tls_reload::ReloadingCertResolver,
};
use rustls::ServerConfig;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Semaphore, watch},
    task::JoinSet,
};
use tokio_rustls::{TlsAcceptor, server::TlsStream};
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const HTTP2_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(30);
const HTTPS_SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy)]
struct HttpsServeOptions {
    handshake_timeout: Duration,
    shutdown_grace_period: Duration,
}

#[derive(Debug, Parser)]
#[command(name = "open-tf-mirror", version)]
struct Args {
    #[arg(long, env = "SERVER_BIND_ADDRESS", default_value = "0.0.0.0")]
    bind_address: String,

    #[arg(long, env = "SERVER_HTTP_PORT", default_value_t = 8080)]
    http_port: u16,

    #[arg(long, env = "SERVER_HTTPS_PORT", default_value_t = 8443)]
    https_port: u16,

    #[arg(long, env = "SERVER_HTTPS_REDIRECT_PORT")]
    https_redirect_port: Option<u16>,

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
        env = "SERVER_ALLOWED_REGISTRIES",
        value_delimiter = ',',
        default_value = "registry.terraform.io"
    )]
    allowed_registries: Vec<String>,

    #[arg(
        long,
        env = "SERVER_METADATA_TTL_SECONDS",
        default_value_t = 30 * 60
    )]
    metadata_ttl_seconds: u64,

    #[arg(long, env = "SERVER_OUTBOUND_MODE", default_value = "direct")]
    outbound_mode: OutboundMode,

    #[arg(long, env = "SERVER_UPSTREAM_CA_FILE")]
    upstream_ca_file: Option<PathBuf>,

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
    if args.metadata_ttl_seconds < 30 {
        anyhow::bail!("--metadata-ttl-seconds must be at least 30");
    }
    let outbound = OutboundClient::new(args.outbound_mode, args.upstream_ca_file.as_deref())
        .context("configure upstream outbound client")?;
    prepare_data_dir(&args.data_source_dir).await?;
    let bundled_mirror = std::env::var_os("TF_PLUGIN_MIRROR_DIR").map(PathBuf::from);

    let state = AppState {
        metadata: ProviderMetadataStore::with_registry_client(
            &args.data_source_dir,
            args.allowed_registries
                .iter()
                .cloned()
                .collect::<HashSet<_>>(),
            Duration::from_secs(args.metadata_ttl_seconds),
            RegistryClient::with_outbound(outbound.clone()),
        )?,
        provider_storage: ProviderStorage::with_bundled_mirror_and_outbound(
            &args.data_source_dir,
            bundled_mirror.as_deref(),
            outbound,
        )?,
        data_dir: Arc::new(args.data_source_dir.clone()),
    };
    let app = build_router_with_options(
        state,
        RouterOptions {
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
            args.https_redirect_port.unwrap_or(args.https_port),
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
    let root_for_cleanup = root.to_path_buf();
    let removed = tokio::task::spawn_blocking(move || remove_stale_temp_files(&root_for_cleanup))
        .await
        .context("join stale temp file cleanup")?
        .with_context(|| format!("remove stale temp files under {}", root.display()))?;
    if removed > 0 {
        tracing::info!(removed, "removed stale temp files from an interrupted run");
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
    let mut shutdown_signal = shutdown.clone();
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        while !*shutdown.borrow() && shutdown.changed().await.is_ok() {}
    });
    let mut server = Box::pin(server.into_future());
    tokio::select! {
        result = &mut server => result.context("serve HTTP listener"),
        _ = shutdown_requested(&mut shutdown_signal) => {
            match tokio::time::timeout(HTTPS_SHUTDOWN_GRACE_PERIOD, &mut server).await {
                Ok(result) => result.context("serve HTTP listener"),
                Err(_) => {
                    tracing::warn!("HTTP shutdown grace period exceeded; cancelling remaining connections");
                    Ok(())
                }
            }
        }
    }
}

async fn serve_https(
    addr: SocketAddr,
    cert_path: PathBuf,
    key_path: PathBuf,
    app: Router,
    conn_burst: usize,
    shutdown: watch::Receiver<bool>,
) -> Result<()> {
    serve_https_with_timeouts(
        addr,
        cert_path,
        key_path,
        app,
        conn_burst,
        shutdown,
        HttpsServeOptions {
            handshake_timeout: TLS_HANDSHAKE_TIMEOUT,
            shutdown_grace_period: HTTPS_SHUTDOWN_GRACE_PERIOD,
        },
    )
    .await
}

async fn serve_https_with_timeouts(
    addr: SocketAddr,
    cert_path: PathBuf,
    key_path: PathBuf,
    app: Router,
    conn_burst: usize,
    shutdown: watch::Receiver<bool>,
    options: HttpsServeOptions,
) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind HTTPS listener on {addr}"))?;
    serve_https_with_listener(
        listener, cert_path, key_path, app, conn_burst, shutdown, options,
    )
    .await
}

async fn serve_https_with_listener(
    listener: TcpListener,
    cert_path: PathBuf,
    key_path: PathBuf,
    app: Router,
    conn_burst: usize,
    mut shutdown: watch::Receiver<bool>,
    options: HttpsServeOptions,
) -> Result<()> {
    let resolver = ReloadingCertResolver::new(cert_path, key_path)?;
    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(resolver));
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(config));
    tracing::info!(addr = ?listener.local_addr()?, "serving HTTPS");
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
        let permit = tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
                continue;
            }
            permit = semaphore.clone().acquire_owned() => {
                permit.context("connection limiter closed")?
            }
        };
        let (stream, peer_addr) = accepted;
        let acceptor = acceptor.clone();
        let app = app.clone();
        let connection_shutdown = shutdown.clone();
        connections.spawn(async move {
            let _permit = permit;
            let Some(stream) = accept_tls_with_timeout(
                &acceptor,
                stream,
                connection_shutdown.clone(),
                options.handshake_timeout,
            )
            .await
            else {
                tracing::debug!(%peer_addr, "TLS handshake failed, timed out, or was cancelled");
                return;
            };
            let io = TokioIo::new(stream);
            let service = TowerToHyperService::new(app);
            let mut builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
            builder
                .http1()
                .header_read_timeout(HTTP_IDLE_TIMEOUT)
                .timer(TokioTimer::new());
            builder
                .http2()
                .keep_alive_interval(HTTP2_KEEP_ALIVE_INTERVAL)
                .keep_alive_timeout(HTTP_IDLE_TIMEOUT)
                .timer(TokioTimer::new());
            let mut connection = Box::pin(builder.serve_connection(io, service));
            let mut connection_shutdown = connection_shutdown;
            tokio::select! {
                result = &mut connection => {
                    if let Err(err) = result {
                        tracing::debug!(%peer_addr, error = %err, "HTTPS connection failed");
                    }
                }
                _ = shutdown_requested(&mut connection_shutdown) => {
                    connection.as_mut().graceful_shutdown();
                    if let Err(err) = connection.await {
                        tracing::debug!(%peer_addr, error = %err, "HTTPS connection failed during graceful shutdown");
                    }
                }
            }
        });
        while connections.try_join_next().is_some() {}
    }

    let drained = tokio::time::timeout(options.shutdown_grace_period, async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }
    Ok(())
}

async fn accept_tls_with_timeout(
    acceptor: &TlsAcceptor,
    stream: TcpStream,
    mut shutdown: watch::Receiver<bool>,
    handshake_timeout: Duration,
) -> Option<TlsStream<TcpStream>> {
    tokio::select! {
        _ = shutdown_requested(&mut shutdown) => None,
        result = tokio::time::timeout(handshake_timeout, acceptor.accept(stream)) => {
            result.ok().and_then(Result::ok)
        }
    }
}

async fn shutdown_requested(shutdown: &mut watch::Receiver<bool>) {
    while !*shutdown.borrow() && shutdown.changed().await.is_ok() {}
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
    use std::{fs, path::Path, sync::Arc};

    use axum::{Router, routing::get};
    use clap::Parser;
    use rcgen::{CertifiedKey, generate_simple_self_signed};
    use rustls::{ClientConfig, RootCertStore, ServerConfig, pki_types::ServerName};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        sync::watch,
        time::{sleep, timeout},
    };
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    use super::{
        Args, HttpsServeOptions, OutboundMode, ReloadingCertResolver, accept_tls_with_timeout,
        bind_addr, install_crypto_provider, redirect_location, serve_https_with_listener,
    };

    fn test_tls_acceptor(dir: &Path) -> TlsAcceptor {
        let resolver =
            ReloadingCertResolver::new(dir.join("tls.crt"), dir.join("tls.key")).unwrap();
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(resolver));
        TlsAcceptor::from(Arc::new(config))
    }

    fn write_test_tls_pair() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        fs::write(dir.path().join("tls.crt"), cert.pem()).unwrap();
        fs::write(dir.path().join("tls.key"), signing_key.serialize_pem()).unwrap();
        dir
    }

    fn write_test_tls_pair_with_certificate() -> (
        tempfile::TempDir,
        rustls::pki_types::CertificateDer<'static>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        fs::write(dir.path().join("tls.crt"), cert.pem()).unwrap();
        fs::write(dir.path().join("tls.key"), signing_key.serialize_pem()).unwrap();
        (
            dir,
            rustls::pki_types::CertificateDer::from(cert.der().to_vec()),
        )
    }

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
    fn cli_defaults_to_terraform_registry() {
        let args = Args::parse_from(["open-tf-mirror", "--enable-tls=false"]);

        assert_eq!(args.allowed_registries, vec!["registry.terraform.io"]);
        assert_eq!(args.metadata_ttl_seconds, 1800);
        assert_eq!(args.outbound_mode, OutboundMode::Direct);
        assert_eq!(args.https_redirect_port, None);
    }

    #[test]
    fn cli_accepts_outbound_and_redirect_overrides() {
        let args = Args::parse_from([
            "open-tf-mirror",
            "--enable-tls=false",
            "--metadata-ttl-seconds=60",
            "--outbound-mode=trusted-proxy",
            "--https-redirect-port=443",
        ]);

        assert_eq!(args.metadata_ttl_seconds, 60);
        assert_eq!(args.outbound_mode, OutboundMode::TrustedProxy);
        assert_eq!(args.https_redirect_port, Some(443));
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

    #[tokio::test]
    async fn tls_handshake_times_out_without_client_input() {
        install_crypto_provider().unwrap();
        let dir = write_test_tls_pair();
        let acceptor = test_tls_acceptor(dir.path());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            accept_tls_with_timeout(
                &acceptor,
                stream,
                shutdown_rx,
                std::time::Duration::from_millis(25),
            )
            .await
        });
        let _client = tokio::net::TcpStream::connect(addr).await.unwrap();

        let result = timeout(std::time::Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn tls_handshake_is_cancelled_when_shutdown_is_requested() {
        install_crypto_provider().unwrap();
        let dir = write_test_tls_pair();
        let acceptor = test_tls_acceptor(dir.path());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            accept_tls_with_timeout(
                &acceptor,
                stream,
                shutdown_rx,
                std::time::Duration::from_secs(60),
            )
            .await
        });
        let _client = tokio::net::TcpStream::connect(addr).await.unwrap();
        shutdown_tx.send(true).unwrap();

        let result = timeout(std::time::Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn https_shutdown_drains_and_closes_keepalive_connections() {
        install_crypto_provider().unwrap();
        let (dir, certificate) = write_test_tls_pair_with_certificate();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route("/", get(|| async { "ok" }));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let server = tokio::spawn(serve_https_with_listener(
            listener,
            dir.path().join("tls.crt"),
            dir.path().join("tls.key"),
            app,
            8,
            shutdown_rx,
            HttpsServeOptions {
                handshake_timeout: std::time::Duration::from_secs(1),
                shutdown_grace_period: std::time::Duration::from_secs(1),
            },
        ));

        let mut roots = RootCertStore::empty();
        roots.add(certificate).unwrap();
        let mut client_config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        client_config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let connector = TlsConnector::from(Arc::new(client_config));
        let server_name = ServerName::try_from("localhost".to_string()).unwrap();
        let tcp = TcpStream::connect(addr).await.unwrap();
        let mut stream = connector.connect(server_name, tcp).await.unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        let mut buffer = [0_u8; 512];
        while !response.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut buffer).await.unwrap();
            assert!(read > 0);
            response.extend_from_slice(&buffer[..read]);
        }
        assert!(response.starts_with(b"HTTP/1.1 200"));

        shutdown_tx.send(true).unwrap();
        sleep(std::time::Duration::from_millis(50)).await;
        let second_request = stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await;
        if second_request.is_ok() {
            let read = timeout(std::time::Duration::from_secs(1), stream.read(&mut buffer))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(read, 0, "shutdown accepted a new keep-alive request");
        }
        timeout(std::time::Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
