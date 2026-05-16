//! TLS configuration for `pvas://` (TLS-secured pvAccess).
//!
//! Wraps `rustls` with the conventions pvxs uses for cert distribution:
//!
//! - `EPICS_PVAS_TLS_KEYCHAIN`  — server cert + private key. Accepts a
//!   PEM bundle *or* a PKCS#12 (`.p12`/`.pfx`) keychain — the format is
//!   auto-detected from the file content, not the extension.
//! - `EPICS_PVAS_TLS_KEYCHAIN_PASSWORD` — password used to decrypt a
//!   PKCS#12 keychain (also accepted on the client side as
//!   `EPICS_PVA_TLS_KEYCHAIN_PASSWORD`). PEM keys in our pipeline are
//!   unencrypted, so the password is only consulted for PKCS#12 input.
//! - `EPICS_PVA_TLS_KEYCHAIN`   — client cert (mutual TLS); PEM or PKCS#12.
//! - `EPICS_PVA_TLS_OPTIONS`    — option string; we recognise
//!   `client_cert=optional` / `client_cert=require`
//! - `EPICS_PVA_TLS_DISABLE`    — set to `YES` to disable TLS even when
//!   configured
//!
//! ## PKCS#12 parity with pvxs
//!
//! pvxs (`src/ossl.cpp`) calls OpenSSL `PKCS12_parse`, which splits a
//! keychain into a leaf certificate, its private key, and a stack of CA
//! certificates. We reproduce that split with the pure-Rust `p12` crate
//! (gated behind the `pkcs12` feature, on by default):
//!
//! - the certificate whose PKCS#12 `localKeyID` attribute matches the
//!   private key bag's is the leaf — it is installed as the cert
//!   presented on the wire (this is how `PKCS12_parse` pairs them too);
//! - every other certificate in the bag is treated as a CA. Mirroring
//!   the comment in `ossl_setup_common`, any CA shipped inside the
//!   PKCS#12 is *trusted* (added to the verifier root store) — a CA
//!   would never appear in a valid chain otherwise.
//!
//! Note: `p12` 0.6 implements the classic PKCS#12 PBE algorithms
//! (`pbeWithSHA1And3-KeyTripleDES-CBC` for keys,
//! `pbeWithSHA1And40BitRC2-CBC` for certs). PKCS#12 files written with
//! OpenSSL 3.x defaults use PBES2 (PBKDF2 + AES) and will fail to
//! decrypt — generate keychains with `openssl pkcs12 -legacy` (or the
//! explicit `-keypbe`/`-certpbe` classic algorithms) for compatibility.
//! This is a documented limitation; see the crate-level UNFIXED notes.
//!
//! This module produces ready-to-use `rustls::ClientConfig` / `ServerConfig`
//! values; the client/server runtime layers wrap them in `TlsConnector`/
//! `TlsAcceptor` on demand. We deliberately *don't* spin up a TLS connection
//! here — that work belongs in `client_native::server_conn` / `server_native::tcp`,
//! which can decide per-target whether to upgrade the socket.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{ClientConfig, RootCertStore, ServerConfig};

#[derive(Debug, thiserror::Error)]
pub enum TlsConfigError {
    #[error("env var {0} not set")]
    MissingEnv(&'static str),
    #[error("I/O error reading {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("PEM parse error in {path:?}: {source}")]
    Pem {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("no certificate found in {0:?}")]
    NoCert(PathBuf),
    #[error("no private key found in {0:?}")]
    NoKey(PathBuf),
    #[error("rustls error: {0}")]
    Rustls(#[from] rustls::Error),
    #[error("verifier error: {0}")]
    Verifier(String),
    /// PKCS#12 keychain failed to parse or decrypt. Most commonly a
    /// wrong/missing `EPICS_PVA*_TLS_KEYCHAIN_PASSWORD`, or a keychain
    /// encrypted with a PBE algorithm `p12` does not implement (PBES2 /
    /// AES — use `openssl pkcs12 -legacy` to re-encode).
    #[error("PKCS#12 parse error in {path:?}: {reason}")]
    Pkcs12 { path: PathBuf, reason: String },
    /// The crate was built without the `pkcs12` feature but the keychain
    /// content is not PEM (no `-----BEGIN` marker).
    #[error("{0:?} is not a PEM bundle and PKCS#12 support is disabled (enable the `pkcs12` feature)")]
    Pkcs12Disabled(PathBuf),
}

/// Server-side TLS configuration.
pub struct TlsServerConfig {
    pub config: Arc<ServerConfig>,
    pub require_client_cert: bool,
}

/// Client-side TLS configuration.
pub struct TlsClientConfig {
    pub config: Arc<ClientConfig>,
}

/// True iff `EPICS_PVA_TLS_DISABLE` is set to a truthy value.
pub fn tls_disabled() -> bool {
    matches!(
        std::env::var("EPICS_PVA_TLS_DISABLE")
            .as_deref()
            .map(|s| s.trim().to_ascii_uppercase()),
        Ok(s) if s == "YES" || s == "1" || s == "TRUE"
    )
}

/// Load a server-side TLS configuration from environment variables.
///
/// Returns `Ok(None)` when TLS is not configured (no `EPICS_PVAS_TLS_KEYCHAIN`
/// set) or explicitly disabled.
pub fn load_server_config() -> Result<Option<TlsServerConfig>, TlsConfigError> {
    if tls_disabled() {
        return Ok(None);
    }
    let Ok(keychain) = std::env::var("EPICS_PVAS_TLS_KEYCHAIN") else {
        return Ok(None);
    };
    // PVA-466: expand $(VAR) / ${VAR} in path env so operators can
    // template `EPICS_PVAS_TLS_KEYCHAIN="$(IOC_HOME)/server.pem"`.
    let keychain = crate::config::env::expand_dollar_vars(&keychain);
    let path = PathBuf::from(keychain);

    // PKCS#12 keychains carry the password from the env; PEM keys in
    // our pipeline are unencrypted so the password is harmless there.
    let password = crate::config::env::server_tls_keychain_password();
    let Keychain {
        certs,
        key,
        ca_certs,
    } = load_keychain(&path, password.as_deref())?;
    let key = key.ok_or_else(|| TlsConfigError::NoKey(path.to_path_buf()))?;

    let options = std::env::var("EPICS_PVA_TLS_OPTIONS").unwrap_or_default();
    let require_client_cert = options.contains("client_cert=require");
    let optional_client_cert = require_client_cert || options.contains("client_cert=optional");

    // Presented chain = leaf + any CA certs the keychain carried, so a
    // peer that lacks the intermediates can still build the path.
    // (pvxs does the same via `SSL_CTX_build_cert_chain`.) For PEM
    // input `ca_certs` is empty and this is exactly the old `certs`.
    let chain: Vec<CertificateDer<'static>> =
        certs.iter().chain(ca_certs.iter()).cloned().collect();

    let config = if optional_client_cert {
        // Build a client verifier whose root CA store is the same bundle.
        // For PKCS#12, `ca_certs` holds the CA stack `PKCS12_parse`
        // split out; for PEM the whole bundle (including the leaf) is
        // offered and the verifier rejects non-CA entries on its own.
        let mut roots = RootCertStore::empty();
        for cert in certs.iter().chain(ca_certs.iter()) {
            // Best-effort: skip non-CA leaf certs — verifier will reject any.
            let _ = roots.add(cert.clone());
        }
        let verifier = if require_client_cert {
            WebPkiClientVerifier::builder(Arc::new(roots))
                .build()
                .map_err(|e| TlsConfigError::Verifier(e.to_string()))?
        } else {
            WebPkiClientVerifier::builder(Arc::new(roots))
                .allow_unauthenticated()
                .build()
                .map_err(|e| TlsConfigError::Verifier(e.to_string()))?
        };
        ServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(chain, key)?
    } else {
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(chain, key)?
    };

    Ok(Some(TlsServerConfig {
        config: Arc::new(config),
        require_client_cert,
    }))
}

/// Load a client-side TLS configuration from environment variables.
///
/// Returns `Ok(None)` when no client cert / CA is configured (in which case
/// callers can still build a `webpki_roots`-rooted client config) or when
/// TLS is explicitly disabled.
pub fn load_client_config() -> Result<Option<TlsClientConfig>, TlsConfigError> {
    if tls_disabled() {
        return Ok(None);
    }
    let mut roots = RootCertStore::empty();
    if let Ok(ca_path) = std::env::var("EPICS_PVA_TLS_CA_KEYCHAIN") {
        // PVA-466: expand $(VAR) / ${VAR} in path env.
        let ca_path = crate::config::env::expand_dollar_vars(&ca_path);
        let path = PathBuf::from(&ca_path);
        let password = crate::config::env::client_tls_keychain_password();
        // A CA keychain may have no private key (PEM cert-only or a
        // PKCS#12 trust store) — that is fine, we only want the certs.
        let kc = load_keychain(&path, password.as_deref())?;
        for cert in kc.certs.into_iter().chain(kc.ca_certs) {
            let _ = roots.add(cert);
        }
    }

    let builder = ClientConfig::builder().with_root_certificates(roots);

    let config = if let Ok(keychain) = std::env::var("EPICS_PVA_TLS_KEYCHAIN") {
        // PVA-466: expand $(VAR) / ${VAR} in path env.
        let keychain = crate::config::env::expand_dollar_vars(&keychain);
        let path = PathBuf::from(keychain);
        let password = crate::config::env::client_tls_keychain_password();
        let Keychain {
            certs,
            key,
            ca_certs,
        } = load_keychain(&path, password.as_deref())?;
        let key = key.ok_or_else(|| TlsConfigError::NoKey(path.to_path_buf()))?;
        // Present leaf + carried CA chain (see server-side rationale).
        let chain: Vec<CertificateDer<'static>> =
            certs.into_iter().chain(ca_certs).collect();
        builder
            .with_client_auth_cert(chain, key)
            .map_err(TlsConfigError::Rustls)?
    } else {
        builder.with_no_client_auth()
    };

    Ok(Some(TlsClientConfig {
        config: Arc::new(config),
    }))
}

// ─── Keychain loading (PEM or PKCS#12) ──────────────────────────────────

/// A loaded keychain, split the way pvxs's `PKCS12_parse` splits one:
/// the leaf certificate(s) + private key, plus a separate CA stack.
#[derive(Debug)]
struct Keychain {
    /// Leaf certificate(s). For a PEM bundle this is the whole cert
    /// list (leaf first, intermediates after); for PKCS#12 it is just
    /// the cert that matches the private key.
    certs: Vec<CertificateDer<'static>>,
    /// Private key, when the keychain carries one. `None` for a
    /// cert-only PEM file or a PKCS#12 trust store.
    key: Option<PrivateKeyDer<'static>>,
    /// CA certificates the keychain carried (PKCS#12 only; empty for
    /// PEM, where intermediates stay inside `certs`).
    ca_certs: Vec<CertificateDer<'static>>,
}

/// Read `path` and parse it as either a PEM bundle or a PKCS#12
/// keychain. The format is auto-detected from the content: a file
/// containing a `-----BEGIN` marker is treated as PEM, anything else
/// is handed to the PKCS#12 parser. `password` is only consulted for
/// PKCS#12 input.
fn load_keychain(
    path: &Path,
    password: Option<&str>,
) -> Result<Keychain, TlsConfigError> {
    let mut file = File::open(path).map_err(|source| TlsConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|source| TlsConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;

    if is_pem(&bytes) {
        load_pem_keychain(path, &bytes)
    } else {
        load_pkcs12_keychain(path, &bytes, password.unwrap_or(""))
    }
}

/// True iff `bytes` looks like PEM — i.e. contains a `-----BEGIN`
/// armour header. PKCS#12 / DER input is binary and never does.
fn is_pem(bytes: &[u8]) -> bool {
    // Scan for the literal marker; a PEM file may be prefixed with
    // comments or blank lines before the first block.
    bytes
        .windows(11)
        .any(|w| w == b"-----BEGIN ")
}

fn load_pem_keychain(
    path: &Path,
    bytes: &[u8],
) -> Result<Keychain, TlsConfigError> {
    let mut reader = BufReader::new(bytes);
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| TlsConfigError::Pem {
            path: path.to_path_buf(),
            source,
        })?;
    if certs.is_empty() {
        return Err(TlsConfigError::NoCert(path.to_path_buf()));
    }

    let mut reader = BufReader::new(bytes);
    let key = rustls_pemfile::private_key(&mut reader).map_err(|source| {
        TlsConfigError::Pem {
            path: path.to_path_buf(),
            source,
        }
    })?;

    Ok(Keychain {
        certs,
        key,
        ca_certs: Vec::new(),
    })
}

/// Parse a PKCS#12 keychain, splitting leaf cert / key / CA stack the
/// way pvxs's `PKCS12_parse` does. Requires the `pkcs12` feature.
///
/// Leaf-vs-CA pairing uses the PKCS#12 `localKeyID` attribute — the
/// same mechanism OpenSSL's `PKCS12_parse` uses to associate a cert
/// with its key. The cert bag whose `localKeyID` matches the key
/// bag's is the leaf; every other cert is a CA.
///
/// F7: when the *cert* bags carry no `localKeyID` of their own, we
/// still pair reliably by reconstructing the OpenSSL `localKeyID`:
/// OpenSSL sets a key bag's `localKeyID` to `SHA-1(leaf_cert_DER)`
/// (see `p12` crate `PFX::new`), so the leaf is the cert whose DER
/// hashes to the key bag's id. If the key bag has a `localKeyID` but
/// no cert matches it, that is a hard error — the keychain references
/// a leaf cert that is not present, and silently presenting some
/// other cert (possibly a CA) would be wrong. If the key bag carries
/// no `localKeyID` at all, the keychain is unambiguous only when it
/// holds exactly one cert; more than one is a hard error rather than
/// a guess.
#[cfg(feature = "pkcs12")]
fn load_pkcs12_keychain(
    path: &Path,
    bytes: &[u8],
    password: &str,
) -> Result<Keychain, TlsConfigError> {
    use p12::SafeBagKind;

    let pfx = p12::PFX::parse(bytes).map_err(|e| TlsConfigError::Pkcs12 {
        path: path.to_path_buf(),
        reason: format!("not a valid PKCS#12 structure: {e}"),
    })?;

    // Reject a wrong password up front with a clear message rather
    // than letting the later bag decryption fail opaquely. An empty
    // (password-less) keychain still verifies here.
    if !pfx.verify_mac(password) {
        return Err(TlsConfigError::Pkcs12 {
            path: path.to_path_buf(),
            reason: "MAC verification failed (wrong EPICS_PVA*_TLS_KEYCHAIN_PASSWORD?)"
                .to_string(),
        });
    }

    let bags = pfx.bags(password).map_err(|e| TlsConfigError::Pkcs12 {
        path: path.to_path_buf(),
        reason: format!(
            "could not decrypt PKCS#12 bags (PBES2/AES keychains are \
             unsupported — re-encode with `openssl pkcs12 -legacy`): {e}"
        ),
    })?;

    // First key bag wins (EPICS keychains hold one end-entity key).
    // `get_key` yields a decrypted PKCS#8 `PrivateKeyInfo` DER blob.
    let key_bmp = bmp_password(password);
    let mut key: Option<PrivateKeyDer<'static>> = None;
    let mut key_local_id: Option<Vec<u8>> = None;
    for bag in &bags {
        if let SafeBagKind::Pkcs8ShroudedKeyBag(_) = bag.bag {
            if let Some(der) = bag.bag.get_key(&key_bmp) {
                key = Some(PrivateKeyDer::Pkcs8(der.into()));
                key_local_id = bag.local_key_id();
                break;
            }
        }
    }

    // Collect every cert bag's DER alongside its own `localKeyID`
    // (most cert bags carry none — OpenSSL only stamps the leaf).
    let mut cert_bags: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
    for bag in &bags {
        if let Some(der) = bag.bag.get_x509_cert() {
            cert_bags.push((der, bag.local_key_id()));
        }
    }
    if cert_bags.is_empty() {
        return Err(TlsConfigError::NoCert(path.to_path_buf()));
    }

    // Identify the leaf index — pure logic, see `select_leaf_index`.
    let leaf_idx = select_leaf_index(&cert_bags, key_local_id.as_deref())
        .map_err(|reason| TlsConfigError::Pkcs12 {
            path: path.to_path_buf(),
            reason,
        })?;

    // Split: the identified leaf first, every other cert is a CA.
    let mut certs: Vec<CertificateDer<'static>> = Vec::new();
    let mut ca_certs: Vec<CertificateDer<'static>> = Vec::new();
    for (idx, (der, _)) in cert_bags.into_iter().enumerate() {
        let cert = CertificateDer::from(der);
        if idx == leaf_idx {
            certs.push(cert);
        } else {
            ca_certs.push(cert);
        }
    }

    Ok(Keychain {
        certs,
        key,
        ca_certs,
    })
}

/// Pick the leaf certificate's index among a PKCS#12 keychain's cert
/// bags (F7). `cert_bags` is `(cert_DER, that bag's own localKeyID)`
/// in bag order; `key_local_id` is the private-key bag's `localKeyID`.
///
/// Decision order:
/// 1. Key bag has a `localKeyID` → the leaf is the cert whose bag
///    `localKeyID` equals it, or failing that the cert whose
///    `SHA-1(DER)` equals it (OpenSSL derives the id that way). On no
///    match, `Err` — the keychain's leaf cert is missing; falling
///    back to bag order could present a CA cert as the leaf.
/// 2. Key bag has no `localKeyID` → unambiguous only with exactly one
///    cert. More than one cert is `Err`: there is no signal to tell
///    leaf from CA, and a guess can pick a CA cert.
#[cfg(feature = "pkcs12")]
fn select_leaf_index(
    cert_bags: &[(Vec<u8>, Option<Vec<u8>>)],
    key_local_id: Option<&[u8]>,
) -> Result<usize, String> {
    match key_local_id {
        Some(key_id) => {
            // (a) match a cert bag's own `localKeyID` attribute.
            if let Some(idx) = cert_bags
                .iter()
                .position(|(_, id)| id.as_deref() == Some(key_id))
            {
                return Ok(idx);
            }
            // (b) match `SHA-1(cert_DER)` — the value OpenSSL hashes
            // to build the key bag's `localKeyID`. This pairs the key
            // to its leaf even when the cert bags carry no attribute.
            if let Some(idx) = cert_bags
                .iter()
                .position(|(der, _)| sha1_digest(der) == key_id)
            {
                return Ok(idx);
            }
            Err("PKCS#12 key bag's localKeyID matches no certificate in \
                 the keychain — the leaf certificate paired with the \
                 private key is missing"
                .to_string())
        }
        None => {
            if cert_bags.len() == 1 {
                Ok(0)
            } else {
                Err(format!(
                    "PKCS#12 keychain has {} certificates but no \
                     localKeyID to identify which is the leaf paired \
                     with the private key — re-encode the keychain with \
                     `openssl pkcs12 -export` so the key carries a \
                     localKeyID",
                    cert_bags.len()
                ))
            }
        }
    }
}

/// SHA-1 digest of `bytes`. Used to reconstruct a PKCS#12
/// `localKeyID`: OpenSSL stamps a key bag's `localKeyID` with the
/// SHA-1 of its leaf certificate's DER, so `sha1_digest(cert_der)`
/// equals the key bag's id for the leaf and nothing else (F7). SHA-1
/// is used here purely as an identifier, not for any security
/// property — it mirrors OpenSSL's keychain convention exactly.
#[cfg(feature = "pkcs12")]
fn sha1_digest(bytes: &[u8]) -> Vec<u8> {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    hasher.finalize().to_vec()
}

/// Encode a password as a PKCS#12 BMPString (UTF-16BE + NUL terminator)
/// — the form `p12`'s `SafeBagKind::get_key` expects. Mirrors the
/// crate-internal `bmp_string` helper, which is not public.
#[cfg(feature = "pkcs12")]
fn bmp_password(password: &str) -> Vec<u8> {
    let mut bytes: Vec<u8> = password
        .encode_utf16()
        .flat_map(|c| c.to_be_bytes())
        .collect();
    bytes.push(0);
    bytes.push(0);
    bytes
}

#[cfg(not(feature = "pkcs12"))]
fn load_pkcs12_keychain(
    path: &Path,
    _bytes: &[u8],
    _password: &str,
) -> Result<Keychain, TlsConfigError> {
    Err(TlsConfigError::Pkcs12Disabled(path.to_path_buf()))
}

// ─── X.509 identity → authorization credentials ─────────────────────────

/// Authorization identity derived from a verified TLS peer certificate
/// chain. Mirrors pvxs `PeerCredentials` for the `x509` auth method
/// (`SSLContext::fill_credentials`, `src/ossl.cpp`):
///
/// - `account` = the **leaf** (peer) certificate's subject CommonName.
/// - `authority` = the **root CA**'s subject CommonName, but only when
///   that last cert in the chain is a self-signed CA (pvxs checks
///   `X509_check_ca(root) && EXFLAG_SS`).
///
/// The `method` is always `"x509"`. Server-side ACF rules of the form
/// `RULE(1, WRITE) { ... METHOD("x509") AUTHORITY("Root CA") }` match
/// against these fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct X509Credentials {
    /// Peer leaf certificate subject CommonName → ACF account.
    pub account: String,
    /// Root CA subject CommonName → ACF authority. Empty when the
    /// chain has no self-signed CA at its end.
    pub authority: String,
}

/// Extract the subject CommonName from a DER-encoded X.509 certificate.
/// Returns `None` when the cert fails to parse or carries no CN RDN.
fn subject_common_name(der: &[u8]) -> Option<String> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;
    cert.subject()
        .iter_common_name()
        .next()
        .and_then(|attr| attr.as_str().ok())
        .map(|s| s.to_string())
}

/// True iff the DER-encoded cert is a self-signed CA (root). Mirrors
/// pvxs `X509_check_ca(root) && (X509_get_extension_flags(root) & EXFLAG_SS)`:
/// a root CA must (a) have `basicConstraints` CA:TRUE and (b) be
/// self-signed (subject == issuer).
fn is_self_signed_ca(der: &[u8]) -> bool {
    let Ok((_, cert)) = x509_parser::parse_x509_certificate(der) else {
        return false;
    };
    let is_ca = cert
        .basic_constraints()
        .ok()
        .flatten()
        .map(|bc| bc.value.ca)
        .unwrap_or(false);
    is_ca && cert.subject() == cert.issuer()
}

/// Map a verified TLS peer certificate chain to authorization
/// credentials, mirroring pvxs `SSLContext::fill_credentials`.
///
/// `chain` is the peer's certificate chain in leaf-first order — the
/// exact ordering rustls exposes via `CommonState::peer_certificates()`
/// and `tokio_rustls`'s `TlsStream` connection info. The leaf
/// (`chain[0]`) supplies the `account`; the last entry, when it is a
/// self-signed CA, supplies the `authority`.
///
/// Returns `None` when the chain is empty or the leaf has no subject
/// CommonName — matching pvxs, which only sets `method="x509"` once a
/// peer CN is in hand.
pub fn x509_credentials_from_chain(chain: &[CertificateDer<'_>]) -> Option<X509Credentials> {
    let leaf = chain.first()?;
    let account = subject_common_name(leaf)?;

    // Root CA = last cert in the chain, but only honoured as the
    // `authority` when it is a self-signed CA (pvxs ossl.cpp:410).
    let authority = chain
        .last()
        .filter(|root| is_self_signed_ca(root))
        .and_then(|root| subject_common_name(root))
        .unwrap_or_default();

    Some(X509Credentials { account, authority })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // Both tests in this module mutate process-wide env vars, so
    // they must NOT run in parallel — cargo's default test
    // parallelism would race the set_var/remove_var calls. Use the
    // same `epics_env` group key as `epics-base-rs::runtime::net`
    // tests so cross-crate env-mutating tests serialize together
    // when the workspace runs them in one harness.
    #[test]
    #[serial(epics_env)]
    fn tls_disabled_respects_env() {
        let prev = std::env::var("EPICS_PVA_TLS_DISABLE").ok();
        unsafe {
            std::env::set_var("EPICS_PVA_TLS_DISABLE", "YES");
        }
        assert!(tls_disabled());
        unsafe {
            std::env::set_var("EPICS_PVA_TLS_DISABLE", "NO");
        }
        assert!(!tls_disabled());
        match prev {
            Some(v) => unsafe { std::env::set_var("EPICS_PVA_TLS_DISABLE", v) },
            None => unsafe { std::env::remove_var("EPICS_PVA_TLS_DISABLE") },
        }
    }

    #[test]
    #[serial(epics_env)]
    fn unset_env_yields_none() {
        let prev_keychain = std::env::var("EPICS_PVAS_TLS_KEYCHAIN").ok();
        let prev_disable = std::env::var("EPICS_PVA_TLS_DISABLE").ok();
        unsafe {
            std::env::remove_var("EPICS_PVAS_TLS_KEYCHAIN");
            std::env::remove_var("EPICS_PVA_TLS_DISABLE");
        }
        assert!(load_server_config().unwrap().is_none());
        if let Some(v) = prev_keychain {
            unsafe { std::env::set_var("EPICS_PVAS_TLS_KEYCHAIN", v) }
        }
        if let Some(v) = prev_disable {
            unsafe { std::env::set_var("EPICS_PVA_TLS_DISABLE", v) }
        }
    }

    // ─── format auto-detection ──────────────────────────────────────

    #[test]
    fn is_pem_detects_armour() {
        assert!(is_pem(b"-----BEGIN CERTIFICATE-----\nMII...\n"));
        assert!(is_pem(b"# leading comment\n-----BEGIN PRIVATE KEY-----\n"));
        // Binary DER (PKCS#12 starts with a SEQUENCE tag 0x30) is not PEM.
        assert!(!is_pem(&[0x30, 0x82, 0x0a, 0x00, 0x02, 0x01, 0x03]));
        assert!(!is_pem(b""));
    }

    #[test]
    fn bmp_password_is_utf16be_nul_terminated() {
        // "ab" -> 0x00 0x61 0x00 0x62 + 0x00 0x00
        #[cfg(feature = "pkcs12")]
        {
            assert_eq!(bmp_password("ab"), vec![0x00, 0x61, 0x00, 0x62, 0, 0]);
            assert_eq!(bmp_password(""), vec![0, 0]);
        }
        let _ = "ab"; // keep test non-empty when feature is off
    }

    // ─── PKCS#12 keychain loading ───────────────────────────────────
    //
    // These tests generate fixtures with the `openssl` CLI using the
    // *legacy* PBE algorithms (`-keypbe`/`-certpbe`), because the `p12`
    // crate (0.6) does not implement PBES2/AES. If `openssl` is not on
    // PATH the test is skipped — fixture generation, not the loader,
    // is what's unavailable.

    #[cfg(feature = "pkcs12")]
    struct Pem {
        cert: String,
        key: String,
    }

    /// Generate a self-signed leaf cert+key as PEM via `rcgen`.
    #[cfg(feature = "pkcs12")]
    fn gen_self_signed(cn: &str) -> Pem {
        let mut params = rcgen::CertificateParams::new(vec![cn.to_string()]).unwrap();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, cn);
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        Pem {
            cert: cert.pem(),
            key: key.serialize_pem(),
        }
    }

    /// Build a legacy-PBE PKCS#12 from PEM cert+key using `openssl`.
    /// Returns the `.p12` bytes, or `None` if `openssl` is unavailable.
    #[cfg(feature = "pkcs12")]
    fn make_p12(pem: &Pem, ca_pem: Option<&str>, password: &str) -> Option<Vec<u8>> {
        use std::io::Write;
        let dir = tempfile::tempdir().ok()?;
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        let p12_path = dir.path().join("out.p12");
        std::fs::File::create(&cert_path)
            .ok()?
            .write_all(pem.cert.as_bytes())
            .ok()?;
        std::fs::File::create(&key_path)
            .ok()?
            .write_all(pem.key.as_bytes())
            .ok()?;

        let mut cmd = std::process::Command::new("openssl");
        cmd.arg("pkcs12")
            .arg("-export")
            // `-legacy` selects the classic PKCS#12 PBE algorithms
            // (3DES for keys, RC2-40 for certs) *and* loads OpenSSL's
            // legacy provider so those algorithms are available — this
            // is exactly the subset the pure-Rust `p12` crate decrypts.
            .arg("-legacy")
            .arg("-macalg")
            .arg("sha1")
            .arg("-in")
            .arg(&cert_path)
            .arg("-inkey")
            .arg(&key_path)
            .arg("-out")
            .arg(&p12_path)
            .arg("-passout")
            .arg(format!("pass:{password}"));
        if let Some(ca) = ca_pem {
            let ca_path = dir.path().join("ca.pem");
            std::fs::File::create(&ca_path)
                .ok()?
                .write_all(ca.as_bytes())
                .ok()?;
            cmd.arg("-certfile").arg(&ca_path);
        }
        let status = cmd.status().ok()?;
        if !status.success() {
            return None;
        }
        std::fs::read(&p12_path).ok()
    }

    #[cfg(feature = "pkcs12")]
    fn write_temp(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keychain");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
        (dir, path)
    }

    #[test]
    #[cfg(feature = "pkcs12")]
    fn pkcs12_loads_leaf_and_key_with_password() {
        let pem = gen_self_signed("epics-pkcs12-leaf");
        let Some(p12) = make_p12(&pem, None, "secret123") else {
            eprintln!("skipping: `openssl` not available for fixture generation");
            return;
        };
        let (_dir, path) = write_temp(&p12);

        // Correct password -> leaf cert + key extracted.
        let kc = load_keychain(&path, Some("secret123")).expect("load p12");
        assert_eq!(kc.certs.len(), 1, "one leaf cert expected");
        assert!(kc.key.is_some(), "private key must be extracted");
        assert!(matches!(kc.key, Some(PrivateKeyDer::Pkcs8(_))));
        // Self-signed leaf, no separate CA chain.
        assert!(kc.ca_certs.is_empty());
    }

    #[test]
    #[cfg(feature = "pkcs12")]
    fn pkcs12_wrong_password_is_rejected() {
        let pem = gen_self_signed("epics-pkcs12-leaf");
        let Some(p12) = make_p12(&pem, None, "secret123") else {
            eprintln!("skipping: `openssl` not available");
            return;
        };
        let (_dir, path) = write_temp(&p12);

        let err = load_keychain(&path, Some("wrong-password")).unwrap_err();
        match err {
            TlsConfigError::Pkcs12 { reason, .. } => {
                assert!(reason.contains("MAC"), "got: {reason}");
            }
            other => panic!("expected Pkcs12 MAC error, got {other:?}"),
        }
    }

    #[test]
    #[cfg(feature = "pkcs12")]
    fn pkcs12_with_ca_chain_splits_leaf_from_ca() {
        // A CA + a leaf signed by it; pack both into one PKCS#12 and
        // confirm the loader puts the leaf in `certs` and the CA in
        // `ca_certs`, matching pvxs `PKCS12_parse` semantics.
        let mut ca_params = rcgen::CertificateParams::new(vec![]).unwrap();
        ca_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "epics-test-ca");
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();

        let mut leaf_params =
            rcgen::CertificateParams::new(vec!["epics-leaf".to_string()]).unwrap();
        leaf_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "epics-leaf");
        let leaf_key = rcgen::KeyPair::generate().unwrap();
        let leaf_cert = leaf_params
            .signed_by(&leaf_key, &ca_cert, &ca_key)
            .unwrap();

        let pem = Pem {
            cert: leaf_cert.pem(),
            key: leaf_key.serialize_pem(),
        };
        let Some(p12) = make_p12(&pem, Some(&ca_cert.pem()), "pw") else {
            eprintln!("skipping: `openssl` not available");
            return;
        };
        let (_dir, path) = write_temp(&p12);

        let kc = load_keychain(&path, Some("pw")).expect("load p12 w/ CA");
        assert_eq!(kc.certs.len(), 1, "exactly one leaf");
        assert_eq!(kc.ca_certs.len(), 1, "exactly one CA cert");
        assert!(kc.key.is_some());
        // Leaf and CA must be distinct certs.
        assert_ne!(kc.certs[0].as_ref(), kc.ca_certs[0].as_ref());
    }

    /// F7: `select_leaf_index` must NOT fall back to bag order when no
    /// `localKeyID` pairs the key to a cert — that can pick a CA cert
    /// as the leaf. Covers all three decision paths.
    #[test]
    #[cfg(feature = "pkcs12")]
    fn select_leaf_index_pairs_key_and_cert_or_hard_errors() {
        // Two distinct cert DERs (content is opaque to the picker).
        let ca: Vec<u8> = b"CA-CERT-DER-BYTES".to_vec();
        let leaf: Vec<u8> = b"LEAF-CERT-DER-BYTES".to_vec();

        // (1) Key bag localKeyID matches a cert bag's own attribute.
        // CA is first in bag order — the OLD code would have picked it.
        let attr_id = vec![0xAAu8; 20];
        let bags = vec![
            (ca.clone(), None),
            (leaf.clone(), Some(attr_id.clone())),
        ];
        assert_eq!(
            select_leaf_index(&bags, Some(&attr_id)).unwrap(),
            1,
            "must pick the cert whose localKeyID matches the key, not bag[0]"
        );

        // (2) Cert bags carry NO localKeyID; the key bag's localKeyID
        // is SHA-1(leaf_DER). Must pair via the SHA-1 reconstruction.
        let leaf_sha1 = sha1_digest(&leaf);
        let bags_no_attr = vec![(ca.clone(), None), (leaf.clone(), None)];
        assert_eq!(
            select_leaf_index(&bags_no_attr, Some(&leaf_sha1)).unwrap(),
            1,
            "must pair key to leaf via SHA-1(cert_DER)"
        );

        // (3) Key bag localKeyID matches NO cert — hard error, never
        // a silent bag-order fallback.
        let err = select_leaf_index(&bags_no_attr, Some(&[0x99u8; 20]))
            .expect_err("must hard-error when no cert matches the key");
        assert!(err.contains("matches no certificate"), "got: {err}");

        // (4) No key localKeyID + a single cert → unambiguous.
        assert_eq!(
            select_leaf_index(&[(leaf.clone(), None)], None).unwrap(),
            0,
        );

        // (5) No key localKeyID + multiple certs → ambiguous, hard
        // error rather than guessing (the F7 silent-CA-as-leaf bug).
        let err = select_leaf_index(&bags_no_attr, None)
            .expect_err("must hard-error on ambiguous multi-cert keychain");
        assert!(err.contains("no localKeyID"), "got: {err}");
    }

    #[test]
    #[cfg(feature = "pkcs12")]
    #[serial(epics_env)]
    fn server_config_loads_from_pkcs12_env() {
        let pem = gen_self_signed("epics-pkcs12-server");
        let Some(p12) = make_p12(&pem, None, "kc-pw") else {
            eprintln!("skipping: `openssl` not available");
            return;
        };
        let (_dir, path) = write_temp(&p12);

        // Drive the full env-var path: keychain + password env vars.
        let _g = EnvGuard::set(&[
            ("EPICS_PVAS_TLS_KEYCHAIN", Some(path.to_str().unwrap())),
            ("EPICS_PVAS_TLS_KEYCHAIN_PASSWORD", Some("kc-pw")),
            ("EPICS_PVA_TLS_DISABLE", None),
            ("EPICS_PVA_TLS_OPTIONS", None),
        ]);
        let cfg = load_server_config()
            .expect("load_server_config")
            .expect("Some(config)");
        assert!(!cfg.require_client_cert);
    }

    #[test]
    #[cfg(feature = "pkcs12")]
    #[serial(epics_env)]
    fn server_config_pkcs12_wrong_password_env_errors() {
        let pem = gen_self_signed("epics-pkcs12-server");
        let Some(p12) = make_p12(&pem, None, "kc-pw") else {
            eprintln!("skipping: `openssl` not available");
            return;
        };
        let (_dir, path) = write_temp(&p12);

        let _g = EnvGuard::set(&[
            ("EPICS_PVAS_TLS_KEYCHAIN", Some(path.to_str().unwrap())),
            ("EPICS_PVAS_TLS_KEYCHAIN_PASSWORD", Some("not-the-password")),
            ("EPICS_PVA_TLS_DISABLE", None),
            ("EPICS_PVA_TLS_OPTIONS", None),
        ]);
        match load_server_config() {
            Err(TlsConfigError::Pkcs12 { .. }) => {}
            Err(other) => panic!("expected Pkcs12 error, got {other:?}"),
            Ok(_) => panic!("expected error for wrong PKCS#12 password"),
        }
    }

    /// RAII env-var guard: sets the given vars on construction (or
    /// removes them when the value is `None`) and restores the prior
    /// state on drop. Keeps env-mutating tests hermetic.
    #[cfg(feature = "pkcs12")]
    struct EnvGuard {
        prev: Vec<(&'static str, Option<String>)>,
    }

    #[cfg(feature = "pkcs12")]
    impl EnvGuard {
        fn set(vars: &[(&'static str, Option<&str>)]) -> Self {
            let prev = vars
                .iter()
                .map(|(k, _)| (*k, std::env::var(k).ok()))
                .collect();
            for (k, v) in vars {
                match v {
                    Some(val) => unsafe { std::env::set_var(k, val) },
                    None => unsafe { std::env::remove_var(k) },
                }
            }
            Self { prev }
        }
    }

    #[cfg(feature = "pkcs12")]
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.prev {
                match v {
                    Some(val) => unsafe { std::env::set_var(k, val) },
                    None => unsafe { std::env::remove_var(k) },
                }
            }
        }
    }

    // ─── X.509 credential extraction ────────────────────────────────

    /// Build a self-signed root CA cert with the given CommonName.
    fn make_ca(cn: &str) -> (rcgen::Certificate, rcgen::KeyPair) {
        let mut params = rcgen::CertificateParams::new(Vec::new()).expect("ca params");
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, cn);
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let key = rcgen::KeyPair::generate().expect("ca key");
        let cert = params.self_signed(&key).expect("ca self-signed");
        (cert, key)
    }

    /// Build a leaf (end-entity) cert with the given CN, signed by `ca`.
    fn make_leaf(
        cn: &str,
        ca: &rcgen::Certificate,
        ca_key: &rcgen::KeyPair,
    ) -> CertificateDer<'static> {
        let mut params = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()])
            .expect("leaf params");
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, cn);
        params.is_ca = rcgen::IsCa::ExplicitNoCa;
        let key = rcgen::KeyPair::generate().expect("leaf key");
        let cert = params.signed_by(&key, ca, ca_key).expect("leaf signed");
        CertificateDer::from(cert.der().to_vec())
    }

    #[test]
    fn x509_credentials_account_is_leaf_cn_and_authority_is_root_cn() {
        // Chain: leaf "operator-alice" signed by root CA "EPICS Root CA".
        let (ca_cert, ca_key) = make_ca("EPICS Root CA");
        let leaf = make_leaf("operator-alice", &ca_cert, &ca_key);
        let root = CertificateDer::from(ca_cert.der().to_vec());

        let chain = vec![leaf, root];
        let creds = x509_credentials_from_chain(&chain).expect("credentials");
        assert_eq!(creds.account, "operator-alice");
        assert_eq!(creds.authority, "EPICS Root CA");
    }

    #[test]
    fn x509_credentials_leaf_only_chain_has_no_authority() {
        // A leaf-only chain (no root appended) still yields the
        // account, but the authority stays empty because the last
        // entry is the leaf itself — not a self-signed CA.
        let (ca_cert, ca_key) = make_ca("Some CA");
        let leaf = make_leaf("svc-readonly", &ca_cert, &ca_key);

        let chain = vec![leaf];
        let creds = x509_credentials_from_chain(&chain).expect("credentials");
        assert_eq!(creds.account, "svc-readonly");
        assert_eq!(creds.authority, "");
    }

    #[test]
    fn x509_credentials_empty_chain_is_none() {
        assert!(x509_credentials_from_chain(&[]).is_none());
    }

    #[test]
    fn is_self_signed_ca_distinguishes_root_from_leaf() {
        let (ca_cert, ca_key) = make_ca("Root");
        let root = CertificateDer::from(ca_cert.der().to_vec());
        let leaf = make_leaf("leaf", &ca_cert, &ca_key);
        assert!(is_self_signed_ca(&root), "root CA must be detected");
        assert!(
            !is_self_signed_ca(&leaf),
            "CA-signed leaf must not be treated as root"
        );
    }

    #[test]
    fn subject_common_name_extracts_cn() {
        let (ca_cert, ca_key) = make_ca("CA");
        let leaf = make_leaf("my-account", &ca_cert, &ca_key);
        assert_eq!(subject_common_name(&leaf).as_deref(), Some("my-account"));
    }
}
