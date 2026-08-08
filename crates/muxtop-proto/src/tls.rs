// TLS client configuration for muxtop.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use rustls_pki_types::CertificateDer;
use rustls_pki_types::ServerName;
use rustls_pki_types::pem::PemObject;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

/// TLS client configuration errors.
#[derive(Debug, thiserror::Error)]
pub enum TlsClientError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("no certificates found in CA file")]
    NoCertificates,

    #[error("TLS configuration error: {0}")]
    Rustls(#[from] tokio_rustls::rustls::Error),
}

/// Errors from parsing the `--remote` host:port target.
#[derive(Debug, thiserror::Error)]
pub enum RemoteTargetError {
    #[error("missing ':port' in remote target: {0:?}")]
    MissingPort(String),

    #[error("invalid port in remote target {raw:?}: {source}")]
    InvalidPort {
        raw: String,
        #[source]
        source: std::num::ParseIntError,
    },

    #[error("DNS lookup failed for {host:?}: {source}")]
    DnsLookupFailed {
        host: String,
        #[source]
        source: std::io::Error,
    },

    #[error("DNS lookup for {host:?} returned no addresses")]
    DnsNoAddresses { host: String },

    #[error("invalid SNI hostname {host:?}: {message}")]
    InvalidSni { host: String, message: String },
}

/// Split `host:port` into its `(host, port)` text components.
///
/// Handles bracketed IPv6 literals (`[::1]:4242`) and rejects bare IPv6
/// literals that lack a port (which are ambiguous w.r.t. the colon).
fn split_host_port(input: &str) -> Result<(&str, u16), RemoteTargetError> {
    if let Some(rest) = input.strip_prefix('[') {
        // IPv6: `[host]:port`
        let close = rest
            .find(']')
            .ok_or_else(|| RemoteTargetError::MissingPort(input.to_string()))?;
        let host = &rest[..close];
        let after = &rest[close + 1..];
        let port_str = after
            .strip_prefix(':')
            .ok_or_else(|| RemoteTargetError::MissingPort(input.to_string()))?;
        let port = port_str
            .parse::<u16>()
            .map_err(|source| RemoteTargetError::InvalidPort {
                raw: input.to_string(),
                source,
            })?;
        Ok((host, port))
    } else {
        // IPv4 or DNS: split on the **last** ':' so that hostnames containing
        // letters work; an IPv4 literal `127.0.0.1:4242` only has one colon
        // anyway.
        let idx = input
            .rfind(':')
            .ok_or_else(|| RemoteTargetError::MissingPort(input.to_string()))?;
        let host = &input[..idx];
        let port_str = &input[idx + 1..];
        let port = port_str
            .parse::<u16>()
            .map_err(|source| RemoteTargetError::InvalidPort {
                raw: input.to_string(),
                source,
            })?;
        Ok((host, port))
    }
}

/// Parse a `--remote` target string into `(socket_addr, sni_server_name)`.
///
/// Per ADR-30-1: the `host` portion is preserved as-is for SNI so that
/// hostname-bound certificates work; the IP is resolved separately for the
/// TCP connect.
///
/// Behaviour:
/// - `host:port` where `host` is an IP literal → `(SocketAddr,
///   ServerName::IpAddress)`. No DNS round-trip.
/// - `host:port` where `host` is a DNS name → DNS-resolve `host:port` to an
///   IP, build `ServerName::DnsName(host.to_string())`. The hostname (NOT the
///   resolved IP) is what rustls validates against the cert SAN.
/// - `[ipv6]:port` → IPv6 literal handling (also IpAddress SNI).
pub fn parse_remote_target(
    input: &str,
) -> Result<(SocketAddr, ServerName<'static>), RemoteTargetError> {
    let (host, port) = split_host_port(input)?;

    // 1. Resolve a SocketAddr for `connect`. If the host is an IP literal we
    //    skip DNS entirely; otherwise we ask the OS resolver and pick the
    //    first answer (this matches std's `(host, port).to_socket_addrs()`
    //    semantics — the kernel honours `getaddrinfo` ordering, including
    //    IPv6/IPv4 preference).
    let socket_addr = if let Ok(ip) = IpAddr::from_str(host) {
        SocketAddr::new(ip, port)
    } else {
        let mut iter = (host, port).to_socket_addrs().map_err(|source| {
            RemoteTargetError::DnsLookupFailed {
                host: host.to_string(),
                source,
            }
        })?;
        iter.next()
            .ok_or_else(|| RemoteTargetError::DnsNoAddresses {
                host: host.to_string(),
            })?
    };

    // 2. Build the SNI ServerName from the **original host string**, not from
    //    `socket_addr.ip()`. This is the whole point of HIGH-S2: when the
    //    user types `--remote example.com:4242`, the rustls handshake
    //    validates the certificate's CN/SAN against `example.com`, not
    //    against whatever IP DNS happened to return.
    let server_name = if let Ok(ip) = IpAddr::from_str(host) {
        ServerName::IpAddress(ip.into())
    } else {
        ServerName::try_from(host.to_string()).map_err(|e| RemoteTargetError::InvalidSni {
            host: host.to_string(),
            message: e.to_string(),
        })?
    };

    Ok((socket_addr, server_name))
}

/// Build a `TlsConnector` that trusts certificates from a PEM-encoded CA file.
pub fn connector_from_ca(ca_path: &Path) -> Result<TlsConnector, TlsClientError> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(ca_path)
        .map_err(|_| TlsClientError::NoCertificates)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| TlsClientError::NoCertificates)?;

    if certs.is_empty() {
        return Err(TlsClientError::NoCertificates);
    }

    let mut root_store = RootCertStore::empty();
    for cert in certs {
        root_store.add(cert).map_err(TlsClientError::Rustls)?;
    }

    let config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    Ok(TlsConnector::from(Arc::new(config)))
}

/// Build a `TlsConnector` that skips certificate verification (INSECURE — for development only).
pub fn connector_insecure() -> TlsConnector {
    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerifier))
        .with_no_client_auth();

    TlsConnector::from(Arc::new(config))
}

/// A certificate verifier that accepts any certificate (INSECURE).
#[derive(Debug)]
struct NoVerifier;

impl tokio_rustls::rustls::client::danger::ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        server_name: &tokio_rustls::rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: tokio_rustls::rustls::pki_types::UnixTime,
    ) -> Result<tokio_rustls::rustls::client::danger::ServerCertVerified, tokio_rustls::rustls::Error>
    {
        // HIGH-S1 (partial): persistent log heartbeat. Each TLS handshake
        // performed in `--tls-skip-verify` mode emits a warning on the
        // dedicated `muxtop::insecure` target so operators can grep for it
        // and so that long-running sessions cannot silently forget that the
        // session is unauthenticated.
        tracing::warn!(
            target: "muxtop::insecure",
            server_name = ?server_name,
            "TLS certificate verification disabled — only safe in local dev"
        );
        Ok(tokio_rustls::rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<tokio_rustls::rustls::SignatureScheme> {
        tokio_rustls::rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn make_self_signed_cert() -> (String, String) {
        let san = vec!["localhost".to_string()];
        let ck = rcgen::generate_simple_self_signed(san).unwrap();
        (ck.cert.pem(), ck.signing_key.serialize_pem())
    }

    #[test]
    fn test_connector_from_ca_valid() {
        let (cert_pem, _) = make_self_signed_cert();
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(cert_pem.as_bytes()).unwrap();

        let connector = connector_from_ca(f.path());
        assert!(connector.is_ok());
    }

    #[test]
    fn test_connector_from_ca_missing_file() {
        let result = connector_from_ca(Path::new("/nonexistent/ca.pem"));
        assert!(result.is_err());
    }

    #[test]
    fn test_connector_insecure_builds() {
        let _connector = connector_insecure();
    }

    #[test]
    fn test_parse_remote_target_ipv4_literal() {
        let (addr, sni) = parse_remote_target("127.0.0.1:4242").unwrap();
        assert_eq!(addr, "127.0.0.1:4242".parse::<SocketAddr>().unwrap());
        match sni {
            ServerName::IpAddress(_) => {}
            other => panic!("expected ServerName::IpAddress, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_remote_target_ipv6_literal() {
        let (addr, sni) = parse_remote_target("[::1]:4242").unwrap();
        assert_eq!(addr, "[::1]:4242".parse::<SocketAddr>().unwrap());
        match sni {
            ServerName::IpAddress(_) => {}
            other => panic!("expected ServerName::IpAddress, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_remote_target_hostname_uses_dns_sni() {
        // `localhost` resolves on essentially every Unix and CI runner. We
        // only need to confirm the SNI name is `DnsName("localhost")` —
        // without a real network call to the open internet.
        let result = parse_remote_target("localhost:4242");
        let (addr, sni) = match result {
            Ok(v) => v,
            Err(e) => {
                // `localhost` resolution can theoretically fail on a
                // hardened/no-network sandbox; in that case there's nothing
                // useful to assert here, so skip rather than fail.
                eprintln!("skipping hostname SNI test: {e}");
                return;
            }
        };
        assert_eq!(addr.port(), 4242);
        match sni {
            ServerName::DnsName(name) => {
                assert_eq!(name.as_ref(), "localhost");
            }
            other => panic!("expected ServerName::DnsName, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_remote_target_missing_port() {
        let err = parse_remote_target("127.0.0.1").unwrap_err();
        assert!(matches!(err, RemoteTargetError::MissingPort(_)));
    }

    #[test]
    fn test_parse_remote_target_invalid_port() {
        let err = parse_remote_target("127.0.0.1:notaport").unwrap_err();
        assert!(matches!(err, RemoteTargetError::InvalidPort { .. }));
    }

    #[test]
    fn test_split_host_port_ipv6_no_port() {
        // `[::1]` with no `:port` after the bracket → MissingPort.
        let err = split_host_port("[::1]").unwrap_err();
        assert!(matches!(err, RemoteTargetError::MissingPort(_)));
    }
}
