//! TLS configuration for Channel Access over TCP.
//!
//! `epics-ca-rs` extends CA with optional TLS-encrypted TCP virtual
//! circuits — UDP search remains plaintext (PV names are not secret).
//! Enable with the `experimental-rust-tls` cargo feature.
//!
//! Two modes:
//!
//! 1. **Server-auth** (TLS) — clients verify the server's certificate
//!    against a root CA. Equivalent to HTTPS without client certs.
//! 2. **mTLS** — both ends present certificates. The server's `ACF`
//!    rule matching uses the client cert's CN/SAN as the identity
//!    instead of the spoofable `CA_PROTO_HOST_NAME` message.
//!
//! Use cases:
//!
//! - Encrypt control traffic across an untrusted LAN segment
//! - Authenticate operators/services without trusting hostnames
//! - Comply with site policies (medical, nuclear, multi-tenant
//!   facilities) that mandate transport encryption

#[cfg(feature = "experimental-rust-tls")]
use std::io;
#[cfg(feature = "experimental-rust-tls")]
use std::path::Path;
#[cfg(feature = "experimental-rust-tls")]
use std::sync::Arc;

#[cfg(feature = "experimental-rust-tls")]
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
#[cfg(feature = "experimental-rust-tls")]
pub use tokio_rustls::rustls::ServerConfig;
#[cfg(feature = "experimental-rust-tls")]
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

#[cfg(feature = "experimental-rust-tls")]
pub mod ca_secure;

/// CA-over-TLS configuration. Wraps `rustls` ClientConfig/ServerConfig
/// with the conventions used by the CA TLS feature.
#[cfg(feature = "experimental-rust-tls")]
#[derive(Clone)]
pub enum TlsConfig {
    /// Server-side TLS configuration. Used by `CaServer::with_tls`.
    Server(Arc<ServerConfig>),
    /// Client-side TLS configuration. Used by `CaClient::with_tls`.
    Client(Arc<ClientConfig>),
}

#[cfg(feature = "experimental-rust-tls")]
impl TlsConfig {
    /// Build a server config for TLS-only (no client cert verification).
    /// `cert_chain_pem` should be the server certificate chain (leaf
    /// first), `key_pem` the corresponding private key.
    pub fn server_from_pem(
        cert_chain: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
    ) -> Result<Self, TlsError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let cfg = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert_chain, key)
            .map_err(|e| TlsError::Build(e.to_string()))?;
        Ok(TlsConfig::Server(Arc::new(cfg)))
    }

    /// Build a server config that **requires** a valid client cert
    /// (mTLS). The client's identity (CN or first SAN) becomes the
    /// effective hostname for ACF rule matching, replacing the
    /// `CA_PROTO_HOST_NAME` message.
    pub fn server_mtls_from_pem(
        cert_chain: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
        client_ca_roots: RootCertStore,
    ) -> Result<Self, TlsError> {
        use tokio_rustls::rustls::server::WebPkiClientVerifier;
        let _ = rustls::crypto::ring::default_provider().install_default();
        let verifier = WebPkiClientVerifier::builder(Arc::new(client_ca_roots))
            .build()
            .map_err(|e| TlsError::Build(e.to_string()))?;
        let cfg = ServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(cert_chain, key)
            .map_err(|e| TlsError::Build(e.to_string()))?;
        Ok(TlsConfig::Server(Arc::new(cfg)))
    }

    /// Build a client config that verifies the server cert against the
    /// supplied roots and presents no client cert (server-auth only).
    pub fn client_from_roots(roots: RootCertStore) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let cfg = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        TlsConfig::Client(Arc::new(cfg))
    }

    /// Build a client config that verifies the server cert AND
    /// presents the supplied client cert (mTLS).
    pub fn client_mtls(
        roots: RootCertStore,
        client_cert: Vec<CertificateDer<'static>>,
        client_key: PrivateKeyDer<'static>,
    ) -> Result<Self, TlsError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let cfg = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_client_auth_cert(client_cert, client_key)
            .map_err(|e| TlsError::Build(e.to_string()))?;
        Ok(TlsConfig::Client(Arc::new(cfg)))
    }
}

/// Helper: load a PEM-encoded certificate chain from a file.
#[cfg(feature = "experimental-rust-tls")]
pub fn load_certs(path: impl AsRef<Path>) -> io::Result<Vec<CertificateDer<'static>>> {
    let mut reader = std::io::BufReader::new(std::fs::File::open(path)?);
    rustls_pemfile::certs(&mut reader).collect::<io::Result<Vec<_>>>()
}

/// Helper: load a PEM-encoded private key from a file. Tries PKCS#8,
/// PKCS#1 (RSA), and SEC1 (EC) sequentially; returns the first match.
#[cfg(feature = "experimental-rust-tls")]
pub fn load_private_key(path: impl AsRef<Path>) -> io::Result<PrivateKeyDer<'static>> {
    let mut reader = std::io::BufReader::new(std::fs::File::open(&path)?);
    if let Some(key) = rustls_pemfile::pkcs8_private_keys(&mut reader)
        .next()
        .transpose()?
    {
        return Ok(PrivateKeyDer::Pkcs8(key));
    }
    let mut reader = std::io::BufReader::new(std::fs::File::open(&path)?);
    if let Some(key) = rustls_pemfile::rsa_private_keys(&mut reader)
        .next()
        .transpose()?
    {
        return Ok(PrivateKeyDer::Pkcs1(key));
    }
    let mut reader = std::io::BufReader::new(std::fs::File::open(&path)?);
    if let Some(key) = rustls_pemfile::ec_private_keys(&mut reader)
        .next()
        .transpose()?
    {
        return Ok(PrivateKeyDer::Sec1(key));
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "no PKCS8/PKCS1/EC private key found in file",
    ))
}

/// Helper: build a `RootCertStore` from a PEM file containing one or
/// more CA certificates.
#[cfg(feature = "experimental-rust-tls")]
pub fn load_root_store(path: impl AsRef<Path>) -> io::Result<RootCertStore> {
    let mut store = RootCertStore::empty();
    for cert in load_certs(path)? {
        store
            .add(cert)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    }
    Ok(store)
}

/// Build a server-side `TlsConfig` from environment variables.
///
/// Variables (all paths PEM-encoded):
/// - `EPICS_CAS_TLS_CERT_FILE`        — server certificate chain (required)
/// - `EPICS_CAS_TLS_KEY_FILE`         — server private key (required)
/// - `EPICS_CAS_TLS_CLIENT_CA_FILE`   — client CA bundle (optional → mTLS)
///
/// Returns `Ok(None)` when neither cert nor key is set (TLS disabled).
/// Returns `Err` when one is set without the other, or when files are
/// unreadable / invalid.
#[cfg(feature = "experimental-rust-tls")]
pub fn server_from_env() -> Result<Option<TlsConfig>, TlsError> {
    let cert_path = epics_base_rs::runtime::env::get("EPICS_CAS_TLS_CERT_FILE");
    let key_path = epics_base_rs::runtime::env::get("EPICS_CAS_TLS_KEY_FILE");
    let client_ca_path = epics_base_rs::runtime::env::get("EPICS_CAS_TLS_CLIENT_CA_FILE");

    match (cert_path, key_path) {
        (None, None) => Ok(None),
        (Some(cert), Some(key)) => {
            let chain = load_certs(&cert)?;
            let priv_key = load_private_key(&key)?;
            let cfg = if let Some(client_ca) = client_ca_path {
                let roots = load_root_store(&client_ca)?;
                TlsConfig::server_mtls_from_pem(chain, priv_key, roots)?
            } else {
                TlsConfig::server_from_pem(chain, priv_key)?
            };
            Ok(Some(cfg))
        }
        _ => Err(TlsError::Build(
            "EPICS_CAS_TLS_CERT_FILE and EPICS_CAS_TLS_KEY_FILE must both be set or both unset"
                .into(),
        )),
    }
}

/// Build a client-side `TlsConfig` from environment variables.
///
/// Variables (all paths PEM-encoded):
/// - `EPICS_CA_TLS_ROOTS_FILE`    — server cert authority bundle (required)
/// - `EPICS_CA_TLS_CLIENT_CERT`   — client certificate (optional → mTLS)
/// - `EPICS_CA_TLS_CLIENT_KEY`    — client private key (required when CERT is set)
///
/// Returns `Ok(None)` when `EPICS_CA_TLS_ROOTS_FILE` is unset (TLS disabled).
#[cfg(feature = "experimental-rust-tls")]
pub fn client_from_env() -> Result<Option<TlsConfig>, TlsError> {
    let Some(roots_path) = epics_base_rs::runtime::env::get("EPICS_CA_TLS_ROOTS_FILE") else {
        return Ok(None);
    };
    let roots = load_root_store(&roots_path)?;
    let client_cert = epics_base_rs::runtime::env::get("EPICS_CA_TLS_CLIENT_CERT");
    let client_key = epics_base_rs::runtime::env::get("EPICS_CA_TLS_CLIENT_KEY");

    match (client_cert, client_key) {
        (None, None) => Ok(Some(TlsConfig::client_from_roots(roots))),
        (Some(cert), Some(key)) => {
            let chain = load_certs(&cert)?;
            let priv_key = load_private_key(&key)?;
            Ok(Some(TlsConfig::client_mtls(roots, chain, priv_key)?))
        }
        _ => Err(TlsError::Build(
            "EPICS_CA_TLS_CLIENT_CERT and EPICS_CA_TLS_CLIENT_KEY must both be set or both unset"
                .into(),
        )),
    }
}

/// Errors returned by TLS configuration helpers.
#[derive(Debug)]
pub enum TlsError {
    Io(std::io::Error),
    Build(String),
}

impl std::fmt::Display for TlsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TlsError::Io(e) => write!(f, "TLS I/O: {e}"),
            TlsError::Build(s) => write!(f, "TLS build: {s}"),
        }
    }
}

impl std::error::Error for TlsError {}

impl From<std::io::Error> for TlsError {
    fn from(e: std::io::Error) -> Self {
        TlsError::Io(e)
    }
}

/// Extract a stable identity string from a verified peer certificate.
///
/// Used during mTLS to populate the per-client `hostname` field in
/// `ClientState` so ACF rules match against a cryptographically
/// verified principal rather than the spoofable
/// `CA_PROTO_HOST_NAME` message.
///
/// Lookup order, first match wins:
///
/// 1. The first `dNSName` from the SubjectAlternativeName extension.
/// 2. The first `uniformResourceIdentifier` from SAN.
/// 3. The Common Name (CN) from the Subject DN.
/// 4. Falls back to the cert's hex SHA-256 fingerprint when no usable
///    name field is present (rare — typically only with non-standard
///    issuance practices).
///
/// Identities are returned as plain ASCII strings suitable for use as
/// HAG host names in an ACF file.
#[cfg(feature = "experimental-rust-tls")]
pub fn identity_from_cert(cert: &CertificateDer<'_>) -> String {
    use std::sync::OnceLock;

    // x509-parser is heavy; lazy-init the parser only when the feature
    // is exercised. We keep the dependency surface small by using
    // rustls's bundled webpki types where possible.
    static FALLBACK_PREFIX: OnceLock<&'static str> = OnceLock::new();
    let _ = FALLBACK_PREFIX.get_or_init(|| "sha256:");

    if let Some(name) = parse_san_dns_or_cn(cert.as_ref()) {
        return name;
    }

    // Fallback: SHA-256 fingerprint as hex.
    use sha2::Digest;
    let digest = sha2::Sha256::digest(cert.as_ref());
    let mut s = String::with_capacity(7 + 64);
    s.push_str("sha256:");
    for b in digest.iter() {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Extract the certificate's issuer Distinguished Name (DN) in a
/// stable RFC 4514-ish string form. Mirrors epics-base PR #641's
/// "authority" concept — the cert was issued by *whom*, and that
/// identity becomes the `AUTHORITY()` clause an ACF rule can match
/// on. Returns `None` if the issuer field is malformed or missing
/// (rare — every conformant X.509 cert has a non-empty issuer).
///
/// Example output (Subject DN of the issuer CA):
/// `"CN=ops-ca, O=Lab, C=KR"`
#[cfg(feature = "experimental-rust-tls")]
pub fn issuer_from_cert(cert: &CertificateDer<'_>) -> Option<String> {
    let (_, parsed) = x509_parser::parse_x509_certificate(cert.as_ref()).ok()?;
    let dn = parsed.tbs_certificate.issuer.to_string();
    if dn.is_empty() {
        return None;
    }
    // x509-parser renders string-typed DN attributes via `from_utf8`
    // without escaping, so an embedded NUL survives into the authority
    // string used for ACF `AUTHORITY()` matching. Reject a NUL-bearing or
    // empty DN — same usability rule as the peer identity below.
    reject_unusable_identity(dn)
}

/// Reject a certificate-derived identity/authority string that is not a
/// usable ACF identity: empty (zero length) or containing an embedded
/// NUL. `x509-parser` returns string-typed attributes via `from_utf8`
/// (NUL is valid UTF-8), so an attacker-issued CN / SAN / issuer-DN such
/// as `admin\0.evil` would otherwise pass through into an ACF identity
/// and be matched/logged inconsistently — the NUL-prefix
/// identity-confusion class (CVE-2009-2408). Mirrors pvxs
/// `SSLContext::commonName()` (pvxs @b16b945), which rejects
/// BOTH an embedded NUL (the length must round-trip through `strlen()`)
/// AND a `len <= 0` (no usable CN). An unusable name is never a
/// legitimate identity, so the caller falls back to the safe SHA-256
/// fingerprint (or no authority).
#[cfg(feature = "experimental-rust-tls")]
fn reject_unusable_identity(s: String) -> Option<String> {
    if s.is_empty() || s.contains('\0') {
        None
    } else {
        Some(s)
    }
}

/// Best-effort parse of an X.509 cert to extract a name field. Returns
/// `None` when no usable name is present, leaving the caller to fall
/// back to a fingerprint identity.
#[cfg(feature = "experimental-rust-tls")]
fn parse_san_dns_or_cn(der: &[u8]) -> Option<String> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;

    // Prefer SAN dNSName / URI.
    if let Ok(Some(san_ext)) = cert.tbs_certificate.subject_alternative_name() {
        for name in &san_ext.value.general_names {
            match name {
                x509_parser::extensions::GeneralName::DNSName(s)
                | x509_parser::extensions::GeneralName::URI(s) => {
                    // A NUL-bearing or empty name is not a usable
                    // identity (CVE-2009-2408 vector / pvxs len<=0 rule);
                    // skip this entry and let a later SAN entry or the
                    // fingerprint fallback supply the identity.
                    if let Some(name) = reject_unusable_identity(s.to_string()) {
                        return Some(name);
                    }
                }
                _ => continue,
            }
        }
    }

    // Fall back to CN.
    cert.subject()
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok())
        .map(|s| s.to_string())
        .and_then(reject_unusable_identity)
}

// Re-exports needed by the public API when the feature is enabled.
#[cfg(feature = "experimental-rust-tls")]
pub use rustls_pki_types::CertificateDer as Cert;
#[cfg(feature = "experimental-rust-tls")]
pub use rustls_pki_types::PrivateKeyDer as Key;
#[cfg(feature = "experimental-rust-tls")]
pub use tokio_rustls::rustls::RootCertStore as Roots;

#[cfg(feature = "experimental-rust-tls")]
mod rustls {
    pub use tokio_rustls::rustls::*;
}

#[cfg(all(test, feature = "experimental-rust-tls"))]
mod nul_identity_tests {
    use super::*;

    /// Self-signed cert carrying `cn` as its only DN CommonName and no
    /// SAN, so `parse_san_dns_or_cn` exercises the CN fallback path.
    /// rcgen encodes the CN as a UTF-8 string, so an embedded NUL is
    /// serialized verbatim — the attacker-issued cert this models.
    fn self_signed_with_cn(cn: &str) -> CertificateDer<'static> {
        let mut params = rcgen::CertificateParams::new(Vec::new()).expect("params");
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, cn);
        let key = rcgen::KeyPair::generate().expect("key");
        let cert = params.self_signed(&key).expect("self-signed");
        CertificateDer::from(cert.der().to_vec())
    }

    /// CVE-2009-2408: a peer-cert CN with an embedded NUL
    /// must not become the ACF identity. Without the guard
    /// `parse_san_dns_or_cn` returns `"admin\0.evil"`; with it the name is
    /// rejected and `identity_from_cert` falls back to the SHA-256
    /// fingerprint — never the confused identity.
    #[test]
    fn identity_rejects_embedded_nul_cn_falls_back_to_fingerprint() {
        let cert = self_signed_with_cn("admin\0.evil");
        assert_eq!(parse_san_dns_or_cn(cert.as_ref()), None);
        let id = identity_from_cert(&cert);
        assert!(
            id.starts_with("sha256:"),
            "expected fingerprint, got {id:?}"
        );
        assert!(!id.contains('\0'));
    }

    /// pvxs `len <= 0` parity: an empty CN is not a usable identity, so
    /// `parse_san_dns_or_cn` rejects it and `identity_from_cert` falls
    /// back to the fingerprint rather than an empty identity string.
    #[test]
    fn identity_rejects_empty_cn_falls_back_to_fingerprint() {
        let cert = self_signed_with_cn("");
        assert_eq!(parse_san_dns_or_cn(cert.as_ref()), None);
        let id = identity_from_cert(&cert);
        assert!(
            id.starts_with("sha256:"),
            "expected fingerprint, got {id:?}"
        );
        assert!(!id.is_empty());
    }

    /// A clean CN still resolves to the CN identity (no false rejection).
    #[test]
    fn identity_extracts_clean_cn() {
        let cert = self_signed_with_cn("operator-bob");
        assert_eq!(
            parse_san_dns_or_cn(cert.as_ref()).as_deref(),
            Some("operator-bob")
        );
        assert_eq!(identity_from_cert(&cert), "operator-bob");
    }

    /// The issuer DN feeds the ACF `AUTHORITY()` clause; an embedded NUL
    /// in the (self-signed) issuer CN must drop the authority rather than
    /// surface a NUL-bearing string.
    #[test]
    fn issuer_rejects_embedded_nul() {
        let cert = self_signed_with_cn("ca\0.evil");
        assert_eq!(issuer_from_cert(&cert), None);
    }

    /// A clean issuer DN is still returned.
    #[test]
    fn issuer_extracts_clean_dn() {
        let cert = self_signed_with_cn("ops-ca");
        let dn = issuer_from_cert(&cert).expect("issuer dn");
        assert!(dn.contains("ops-ca"), "got {dn:?}");
        assert!(!dn.contains('\0'));
    }
}
