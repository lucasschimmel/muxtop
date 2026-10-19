mod client;
mod error;
mod rate_limit;
mod server;
pub mod tls;

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use muxtop_core::collector::Collector;
use muxtop_core::container_engine::ContainerEngine;
use muxtop_core::docker_engine::maybe_connect_default_engine;
use muxtop_core::system::SystemSnapshot;

/// Newtype wrapper around the in-memory authentication token whose `Debug`
/// impl deliberately redacts the secret.
///
/// MED-S3: prevents accidental token disclosure via `tracing::debug!` /
/// `panic!` / `format!("{:?}", cli)` paths. We avoid pulling in the
/// `secrecy` crate (and its 2 transitive deps) because the only behaviour we
/// want is the redacting `Debug`.
#[derive(Clone)]
struct Token(String);

impl Token {
    /// Borrow the inner secret. Only used by unit tests; the production code
    /// path consumes the wrapper via [`Token::into_inner`] at the
    /// `ServerConfig` boundary.
    #[cfg(test)]
    fn as_str(&self) -> &str {
        &self.0
    }

    fn into_inner(self) -> String {
        self.0
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(\"[REDACTED]\")")
    }
}

/// Read an authentication token from a file: trim trailing whitespace,
/// validate the 16-character minimum, and wrap in [`Token`] so the secret is
/// debug-safe from this point forward.
fn read_token_file(path: &Path) -> Result<Token> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read --token-file {}", path.display()))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!(
            "--token-file {} is empty after trimming whitespace",
            path.display()
        );
    }
    if trimmed.len() < 16 {
        bail!(
            "--token-file {} contains a {}-char token; minimum is 16 characters",
            path.display(),
            trimmed.len()
        );
    }
    Ok(Token(trimmed.to_string()))
}

#[derive(Parser, Debug)]
#[command(
    name = "muxtop-server",
    about = "TCP server daemon for muxtop remote system monitoring",
    version,
    author
)]
struct Cli {
    /// Bind address (e.g., 127.0.0.1:4242)
    #[arg(long, default_value = "127.0.0.1:4242")]
    bind: SocketAddr,

    /// Refresh interval in seconds (1–3600)
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u64).range(1..=3600))]
    refresh: u64,

    /// Authentication token (required, ≥16 chars). Also reads from
    /// MUXTOP_TOKEN. Note: --token leaks via /proc/PID/cmdline and
    /// `ps eww` on shared hosts; prefer --token-file there.
    #[arg(long, env = "MUXTOP_TOKEN", conflicts_with = "token_file")]
    token: Option<String>,

    /// Read the authentication token from a file (preferred on shared hosts).
    /// File is read once at startup; trailing whitespace is trimmed; the
    /// 16-char minimum still applies.
    #[arg(long, value_name = "PATH")]
    token_file: Option<PathBuf>,

    /// Maximum concurrent client connections
    #[arg(long, default_value_t = 8)]
    max_clients: usize,

    /// Per-source-IP connection rate limit (token-bucket, connections/sec).
    /// Default 10/s with a burst of 10. Set to 0 to disable.
    #[arg(long, default_value_t = 10.0)]
    rate_limit_per_ip: f32,

    /// Path to PEM-encoded TLS certificate file
    #[arg(long, requires = "tls_key")]
    tls_cert: Option<PathBuf>,

    /// Path to PEM-encoded TLS private key file
    #[arg(long, requires = "tls_cert")]
    tls_key: Option<PathBuf>,

    /// Auto-generate a self-signed TLS certificate (for development)
    #[arg(long, conflicts_with_all = ["tls_cert", "tls_key"])]
    tls_generate: bool,

    /// Override Docker/Podman socket path. If omitted, the server auto-detects
    /// via $DOCKER_HOST and the standard socket locations.
    ///
    /// Note: docker socket access is root-equivalent; prefer rootless Podman.
    #[arg(long, value_name = "PATH")]
    docker_socket: Option<PathBuf>,

    /// Disable container engine autodetection entirely. Remote clients see an
    /// empty Containers tab.
    #[arg(long)]
    no_containers: bool,

    /// Override the kubeconfig context the server uses to populate the Kube
    /// tab for remote clients (default = current-context).
    #[arg(long, value_name = "NAME")]
    kube_context: Option<String>,

    /// Scope the served Kube snapshots to a single namespace. Pods and
    /// Deployments are listed from this namespace only, which works with a
    /// Role bound to it — no cluster-wide permissions required. Without this
    /// flag the server lists every namespace. Nodes are cluster-scoped in
    /// Kubernetes and always need cluster-wide access.
    #[arg(long, value_name = "NS", value_parser = muxtop_core::cluster_engine::parse_namespace)]
    kube_namespace: Option<String>,

    /// Disable cluster engine autodetection entirely. Remote clients see an
    /// empty Kube tab. The kubeconfig content never crosses the wire — this
    /// flag is the only way for an operator to opt-out at the server.
    #[arg(long, conflicts_with = "kube_context")]
    no_kube: bool,

    /// Disable GPU detection entirely; remote clients see an empty GPU tab.
    ///
    /// The server probes NVIDIA (NVML) and AMD (amdgpu sysfs) at startup and
    /// serves the digested snapshot. Only aggregate telemetry crosses the
    /// wire — device names, utilisation, memory and the PIDs already visible
    /// in the Processes tab.
    #[arg(long)]
    no_gpu: bool,
}

/// Create the data directory and harden it (Unix: chmod 0700) so other users
/// on the host can't read the private key once it's written. Idempotent —
/// safe to call multiple times.
///
/// Per MED-S7: the parent directory is rechmod'd to 0700 *after* the
/// `create_dir_all`, closing the TOCTOU window where another user could
/// pre-create the directory at 0777 and then symlink things in.
fn ensure_data_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).context("failed to create muxtop data directory")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(path, perms)
            .with_context(|| format!("failed to chmod 0700 {}", path.display()))?;
    }

    Ok(())
}

/// Open a file for writing with mode 0600 and `O_NOFOLLOW` (Unix) or fall
/// back to a regular `OpenOptions::write` (non-Unix). On Linux/macOS the
/// `O_NOFOLLOW` flag refuses to follow symlinks at the leaf — so even if an
/// attacker pre-creates `server.key` as a symlink to `/etc/passwd` we'll
/// fail loudly rather than overwrite it.
///
/// Per MED-S7.
fn open_secret_file_for_write(path: &Path) -> Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW);
        opts.open(path)
            .with_context(|| format!("failed to open {} (O_NOFOLLOW)", path.display()))
    }
    #[cfg(not(unix))]
    {
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .with_context(|| format!("failed to open {}", path.display()))
    }
}

/// Persist the SHA-256 cert fingerprint as a sibling of the cert file (mode
/// 0644). Per INFO-S1: the eprintln on first generation can be lost in
/// systemd journals, so a stable on-disk record helps the operator pin the
/// cert on the client.
fn write_fingerprint_file(path: &Path, fingerprint: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o644)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .with_context(|| format!("failed to open {} (fingerprint)", path.display()))?;
        f.write_all(fingerprint.as_bytes())?;
        f.write_all(b"\n")?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, format!("{fingerprint}\n"))
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(())
}

fn init_tracing() -> Result<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};

    let data_dir = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("muxtop");

    std::fs::create_dir_all(&data_dir).context("failed to create muxtop data directory")?;

    let log_path = data_dir.join("muxtop-server.log");
    let log_file = muxtop_proto::logging::BoundedLog::open(&log_path)
        .context("failed to open bounded log file")?;
    let (log_writer, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
        .buffered_lines_limit(muxtop_proto::logging::LOG_QUEUE_LINES_LIMIT)
        .lossy(true)
        .finish(log_file);

    let env_filter =
        EnvFilter::try_from_env("MUXTOP_LOG").unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_writer(log_writer)
                .with_ansi(false)
                .with_target(true)
                .with_thread_ids(true),
        )
        .with(env_filter)
        .init();

    Ok(guard)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let _log_guard = init_tracing()?;

    // Mandatory authentication: token is required (minimum 16 characters).
    // --token and --token-file are mutually exclusive (enforced via clap's
    // `conflicts_with`). --token-file is preferred on shared hosts because
    // --token leaks via /proc/PID/cmdline and `ps eww`.
    let auth_token: Token = if let Some(path) = cli.token_file.as_deref() {
        read_token_file(path)?
    } else {
        match cli.token {
            Some(t) if t.len() >= 16 => Token(t),
            Some(t) if !t.is_empty() => {
                bail!(
                    "Authentication token is too short ({} chars). \
                     Use at least 16 characters for security.",
                    t.len()
                );
            }
            _ => {
                bail!(
                    "Authentication token is required. Set --token <secret>, --token-file \
                     <path>, or the MUXTOP_TOKEN env var (minimum 16 characters)."
                );
            }
        }
    };

    // Build TLS acceptor.
    let tls_acceptor = if let (Some(cert_path), Some(key_path)) = (&cli.tls_cert, &cli.tls_key) {
        tls::acceptor_from_pem(cert_path, key_path).context("failed to load TLS certificate/key")?
    } else if cli.tls_generate {
        let hostname = cli.bind.ip().to_string();
        let (cert_pem, key_pem) =
            tls::generate_self_signed(&hostname).context("failed to generate self-signed cert")?;

        // Parse cert DER for fingerprint.
        use rustls_pki_types::pem::PemObject;
        let certs: Vec<rustls_pki_types::CertificateDer<'static>> =
            rustls_pki_types::CertificateDer::pem_slice_iter(cert_pem.as_bytes())
                .collect::<Result<Vec<_>, _>>()
                .context("failed to parse generated certificate")?;
        let fingerprint = tls::cert_fingerprint(certs[0].as_ref());
        eprintln!("TLS: using auto-generated self-signed certificate");
        eprintln!("TLS: SHA-256 fingerprint: {fingerprint}");

        // Hardened data dir — chmod 0700 (MED-S7).
        let data_dir = dirs::data_local_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("muxtop");
        ensure_data_dir(&data_dir)?;

        let cert_path = data_dir.join("server.crt");
        let key_path = data_dir.join("server.key");
        let fp_path = data_dir.join("server.fingerprint");

        // Write cert with O_NOFOLLOW too — even though it's not secret, we
        // don't want a pre-planted symlink turning the write into an attack.
        {
            let mut f = open_secret_file_for_write(&cert_path)?;
            f.write_all(cert_pem.as_bytes())?;
        }

        // Key: 0600 + O_NOFOLLOW (MED-S7).
        {
            let mut f = open_secret_file_for_write(&key_path)?;
            f.write_all(key_pem.as_bytes())?;
        }

        // INFO-S1: persist the fingerprint as a stable on-disk record.
        write_fingerprint_file(&fp_path, &fingerprint)?;

        eprintln!(
            "TLS: cert saved to {}, key saved to {}",
            cert_path.display(),
            key_path.display()
        );
        eprintln!("TLS: fingerprint saved to {}", fp_path.display());

        tls::acceptor_from_pem(&cert_path, &key_path)
            .context("failed to load generated TLS certificate")?
    } else {
        bail!(
            "TLS is required. Use --tls-cert/--tls-key to provide certificates, \
             or --tls-generate for a self-signed certificate."
        );
    };

    if cli.rate_limit_per_ip < 0.0 {
        bail!(
            "--rate-limit-per-ip must be ≥ 0 (got {})",
            cli.rate_limit_per_ip
        );
    }

    tracing::info!(
        bind = %cli.bind,
        refresh = cli.refresh,
        max_clients = cli.max_clients,
        rate_limit_per_ip = cli.rate_limit_per_ip,
        "muxtop-server starting (TLS enabled)"
    );

    // Auto-detect a container engine for the Containers tab on remote
    // clients. `None` keeps the previous behaviour (Containers tab shows the
    // "no engine configured" fallback).
    let container_engine: Option<std::sync::Arc<dyn ContainerEngine + Send + Sync>> =
        if cli.no_containers {
            None
        } else {
            maybe_connect_default_engine(cli.docker_socket.as_deref()).await
        };

    if let Some(engine) = &container_engine {
        tracing::info!(engine = ?engine.kind(), "Containers tab enabled for remote clients");
    } else if !cli.no_containers {
        tracing::info!("Containers tab unavailable (no engine detected)");
    }

    // Auto-detect a cluster engine for the Kube tab. Same `None` semantics
    // as the container engine. The kubeconfig content NEVER crosses the
    // wire — only the digested KubeSnapshot does (see anti-leak guard
    // tests in muxtop-proto/tests/integration.rs and crate kube_engine).
    let cluster_engine: Option<
        std::sync::Arc<dyn muxtop_core::cluster_engine::ClusterEngine + Send + Sync>,
    > = if cli.no_kube {
        None
    } else {
        let source = muxtop_core::cluster_engine::detect_kubeconfig();
        match muxtop_core::kube_engine::KubeEngine::connect(
            source,
            cli.kube_context.as_deref(),
            cli.kube_namespace.as_deref(),
        )
        .await
        {
            Ok(engine) => Some(std::sync::Arc::new(engine) as _),
            Err(e) => {
                tracing::warn!(target: "muxtop::kube", error = %e, "Kube tab unavailable for remote clients");
                None
            }
        }
    };

    if cluster_engine.is_some() {
        tracing::info!("Kube tab enabled for remote clients");
    } else if !cli.no_kube {
        tracing::info!("Kube tab unavailable (no kubeconfig / unreachable)");
    }

    // Detect GPUs for the GPU tab. Detection never fails — it returns an
    // engine carrying the reason when no backend initialises — so `--no-gpu`
    // is the only way this ends up `None`.
    let gpu_engine: Option<std::sync::Arc<dyn muxtop_core::gpu_engine::GpuEngine + Send + Sync>> =
        if cli.no_gpu {
            tracing::info!("GPU tab disabled by --no-gpu");
            None
        } else {
            Some(muxtop_core::gpu_engine::detect_gpu_engines())
        };

    let token = CancellationToken::new();

    // Spawn the system collector with optional container + cluster + GPU
    // engines.
    let (collector_tx, collector_rx) = mpsc::channel::<SystemSnapshot>(4);
    let collector = Collector::with_all_engines(
        Duration::from_secs(cli.refresh),
        container_engine,
        cluster_engine,
        gpu_engine,
    );
    let collector_handle = collector.spawn(collector_tx, token.clone());

    // Run the TCP+TLS server. The `Token` newtype is unwrapped here at the
    // boundary because `ServerConfig::auth_token` is a `String` owned by the
    // server crate (Slice A territory).
    let server_config = server::ServerConfig {
        bind: cli.bind,
        max_clients: cli.max_clients,
        auth_token: auth_token.into_inner(),
        refresh_hz: cli.refresh as u32,
        tls_acceptor,
        rate_limit_per_ip: cli.rate_limit_per_ip,
    };

    // Install signal handler for graceful shutdown.
    let shutdown_token = token.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install CTRL+C handler");
        tracing::info!("received SIGINT, shutting down");
        shutdown_token.cancel();
    });

    server::run(server_config, collector_rx, token.clone()).await?;

    // Shut down collector.
    token.cancel();
    if let Err(e) = collector_handle.await {
        tracing::error!("collector task panicked: {e:?}");
    }

    tracing::info!("muxtop-server stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn test_cli_defaults() {
        let cli = Cli::parse_from(["muxtop-server"]);
        assert_eq!(cli.bind, "127.0.0.1:4242".parse().unwrap());
        assert_eq!(cli.refresh, 1);
        assert_eq!(cli.max_clients, 8);
        assert_eq!(cli.rate_limit_per_ip, 10.0);
        assert!(cli.token.is_none());
        assert!(!cli.tls_generate);
        assert!(cli.tls_cert.is_none());
        assert!(cli.tls_key.is_none());
    }

    #[test]
    fn test_cli_custom_args() {
        let cli = Cli::parse_from([
            "muxtop-server",
            "--bind",
            "127.0.0.1:9999",
            "--refresh",
            "5",
            "--token",
            "my-super-secret-token-1234",
            "--max-clients",
            "4",
            "--rate-limit-per-ip",
            "25",
            "--tls-generate",
        ]);
        assert_eq!(cli.bind, "127.0.0.1:9999".parse().unwrap());
        assert_eq!(cli.refresh, 5);
        assert_eq!(cli.max_clients, 4);
        assert_eq!(cli.rate_limit_per_ip, 25.0);
        assert_eq!(cli.token.as_deref(), Some("my-super-secret-token-1234"));
        assert!(cli.tls_generate);
    }

    #[test]
    fn test_cli_rate_limit_zero_disables() {
        let cli = Cli::parse_from(["muxtop-server", "--rate-limit-per-ip", "0"]);
        assert_eq!(cli.rate_limit_per_ip, 0.0);
    }

    #[test]
    fn test_cli_refresh_range_validation() {
        // 0 is out of range
        let result = Cli::try_parse_from(["muxtop-server", "--refresh", "0"]);
        assert!(result.is_err());

        // 3601 is out of range
        let result = Cli::try_parse_from(["muxtop-server", "--refresh", "3601"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_cli_tls_cert_requires_key() {
        let result = Cli::try_parse_from(["muxtop-server", "--tls-cert", "cert.pem"]);
        assert!(result.is_err(), "--tls-cert requires --tls-key");
    }

    #[test]
    fn test_cli_tls_generate_conflicts_with_cert() {
        let result = Cli::try_parse_from([
            "muxtop-server",
            "--tls-generate",
            "--tls-cert",
            "cert.pem",
            "--tls-key",
            "key.pem",
        ]);
        assert!(
            result.is_err(),
            "--tls-generate conflicts with --tls-cert/--tls-key"
        );
    }

    #[test]
    fn test_cli_default_no_container_flags() {
        let cli = Cli::parse_from(["muxtop-server"]);
        assert!(cli.docker_socket.is_none());
        assert!(!cli.no_containers);
    }

    #[test]
    fn test_cli_docker_socket_override() {
        let cli = Cli::parse_from(["muxtop-server", "--docker-socket", "/var/run/docker.sock"]);
        assert_eq!(
            cli.docker_socket.as_deref(),
            Some(std::path::Path::new("/var/run/docker.sock"))
        );
    }

    #[test]
    fn test_cli_no_containers_flag() {
        let cli = Cli::parse_from(["muxtop-server", "--no-containers"]);
        assert!(cli.no_containers);
    }

    #[test]
    fn test_token_debug_is_redacted() {
        let t = Token("super-secret-token-1234567890".to_string());
        let dbg = format!("{t:?}");
        assert!(dbg.contains("[REDACTED]"), "Debug must redact: {dbg}");
        assert!(
            !dbg.contains("super-secret"),
            "Debug must NOT contain raw secret: {dbg}"
        );
    }

    #[test]
    fn test_token_as_str_returns_inner() {
        let t = Token("super-secret-token-1234567890".to_string());
        assert_eq!(t.as_str(), "super-secret-token-1234567890");
    }

    #[test]
    fn test_read_token_file_happy_path() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut f, b"super-secret-token-abcdef\n").unwrap();
        let token = read_token_file(f.path()).unwrap();
        // Whitespace trimmed.
        assert_eq!(token.as_str(), "super-secret-token-abcdef");
    }

    #[test]
    fn test_read_token_file_too_short() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut f, b"tiny\n").unwrap();
        let err = read_token_file(f.path()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("minimum is 16 characters"), "{msg}");
    }

    #[test]
    fn test_read_token_file_empty() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut f, b"   \n\t  \n").unwrap();
        let err = read_token_file(f.path()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("empty after trimming"), "{msg}");
    }

    #[test]
    fn test_read_token_file_missing() {
        let err = read_token_file(std::path::Path::new("/nonexistent/token.file")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("failed to read --token-file"), "{msg}");
    }

    #[test]
    fn test_cli_token_and_token_file_conflict() {
        let result = Cli::try_parse_from([
            "muxtop-server",
            "--token",
            "some-very-long-secret-token-1234",
            "--token-file",
            "/etc/muxtop/token",
        ]);
        assert!(
            result.is_err(),
            "--token and --token-file must be mutually exclusive"
        );
    }

    #[test]
    fn test_cli_token_file_parses() {
        let cli = Cli::parse_from(["muxtop-server", "--token-file", "/tmp/token.secret"]);
        assert_eq!(
            cli.token_file.as_deref(),
            Some(std::path::Path::new("/tmp/token.secret"))
        );
        assert!(cli.token.is_none());
    }

    /// MED-S7: parent dir of generated key is chmod'd 0700.
    #[cfg(unix)]
    #[test]
    fn test_ensure_data_dir_chmods_0700() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("nested/muxtop");
        ensure_data_dir(&nested).unwrap();
        let mode = std::fs::metadata(&nested).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "data dir must be 0700, got {mode:o}");
    }

    /// INFO-S1: fingerprint is persisted next to the cert (mode 0644).
    #[cfg(unix)]
    #[test]
    fn test_fingerprint_file_persisted() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let fp_path = tmp.path().join("server.fingerprint");
        write_fingerprint_file(&fp_path, "AB:CD:EF").unwrap();

        let contents = std::fs::read_to_string(&fp_path).unwrap();
        assert_eq!(contents.trim(), "AB:CD:EF");

        let mode = std::fs::metadata(&fp_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644, "fingerprint file must be 0644, got {mode:o}");
    }

    /// MED-S7: open_secret_file_for_write writes mode 0600.
    #[cfg(unix)]
    #[test]
    fn test_open_secret_file_writes_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("server.key");
        {
            let mut f = open_secret_file_for_write(&path).unwrap();
            f.write_all(b"keymaterial").unwrap();
        }
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "key file must be 0600, got {mode:o}");
    }

    /// MED-S7: O_NOFOLLOW refuses to overwrite a symlink leaf.
    #[cfg(unix)]
    #[test]
    fn test_open_secret_file_refuses_symlink_leaf() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real.txt");
        std::fs::write(&real, b"real data").unwrap();

        let link = tmp.path().join("server.key");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let res = open_secret_file_for_write(&link);
        assert!(
            res.is_err(),
            "open_secret_file_for_write must refuse symlinked leaf (got Ok)"
        );
        // The real file must NOT have been clobbered.
        let after = std::fs::read(&real).unwrap();
        assert_eq!(after, b"real data");
    }
}
