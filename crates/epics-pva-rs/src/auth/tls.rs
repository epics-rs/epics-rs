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
//! Two PKCS#12 encryption families are supported, auto-detected from
//! the bag's algorithm OID:
//!
//! - **Classic PBE** — `pbeWithSHA1And3-KeyTripleDES-CBC` (keys),
//!   `pbeWithSHA1And40BitRC2-CBC` (certs). These are what
//!   `openssl pkcs12 -export -legacy` and OpenSSL 1.x produce. Handled
//!   by the pure-Rust `p12` crate.
//! - **PBES2** — PBKDF2 + AES-CBC, the OpenSSL 3.x default (no
//!   `-legacy` flag). The `p12` crate cannot decrypt these, so the
//!   loader parses the PKCS#12 PFX / `AuthenticatedSafe` /
//!   `ContentInfo` / `EncryptedData` / `SafeBag` ASN.1 directly with
//!   the stable `der` crate and decrypts the PBES2-protected
//!   `SafeContents` and `pkcs8ShroudedKeyBag`s with the stable `pkcs5`
//!   crate. The OpenSSL-3 keychain MAC (PKCS#12 KDF with SHA-256,
//!   HMAC-SHA256) is verified before any bag is decrypted.
//!
//! The loader tries the classic `p12` path first; when `p12` reports a
//! bag-decryption failure (the symptom of PBES2 content) it retries
//! through the RustCrypto PBES2 path. Either OpenSSL major version's
//! `.p12` output therefore loads with no `-legacy` requirement.
//!
//! This module produces ready-to-use `rustls::ClientConfig` / `ServerConfig`
//! values; the client/server runtime layers wrap them in `TlsConnector`/
//! `TlsAcceptor` on demand. We deliberately *don't* spin up a TLS connection
//! here — that work belongs in `client_native::server_conn` / `server_native::tcp`,
//! which can decide per-target whether to upgrade the socket.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use x509_parser::prelude::FromDer as _;

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
    #[error(
        "{0:?} is not a PEM bundle and PKCS#12 support is disabled (enable the `pkcs12` feature)"
    )]
    Pkcs12Disabled(PathBuf),
    /// The loaded keychain leaf is a CA certificate, but an end-entity
    /// certificate is required for the local TLS identity. Mirrors pvxs
    /// `ossl.cpp:269` (`flags & EXFLAG_CA` → "Found CA Certificate when
    /// End Entity expected").
    #[error("keychain {0:?} leaf is a CA certificate; an end-entity certificate is required")]
    LeafIsCa(PathBuf),
    /// The loaded keychain leaf's `extendedKeyUsage` does not permit the
    /// required SSL role. Mirrors pvxs `ossl.cpp:272-275`
    /// ("extendedKeyUsage does not permit usage by SSL Client/Server").
    #[error("keychain {path:?} leaf extendedKeyUsage does not permit usage by {role}")]
    LeafEkuForbidsRole { path: PathBuf, role: &'static str },
    /// The loaded keychain leaf failed to parse as X.509 for the
    /// config-time role/CA sanity check.
    #[error("keychain {path:?} leaf is not a valid X.509 certificate: {reason}")]
    LeafInvalid { path: PathBuf, reason: String },
}

/// Server-side TLS configuration.
pub struct TlsServerConfig {
    pub config: Arc<ServerConfig>,
    pub require_client_cert: bool,
    /// trust anchors used to resolve the root CA's CN when the peer
    /// sends a partial chain (leaf-only or leaf+intermediate). Mirrors
    /// pvxs `SSL_get0_verified_chain` which includes trust-store roots
    /// even when the peer omits the root from its chain.
    pub trust_roots: Arc<RootCertStore>,
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

/// Resolve the shared TLS key-logger from `$SSLKEYLOGFILE`.
///
/// pvxs honours `$SSLKEYLOGFILE` (`ossl.cpp:148-160`, `:221-223`): when the env
/// var names a writable file, it installs an OpenSSL keylog callback on the
/// shared `SSL_CTX` so the per-session pre-master secrets are appended there for
/// Wireshark / `tshark` decryption. rustls exposes the identical hook via
/// `Config::key_log`, and `rustls::KeyLogFile` reads the same `SSLKEYLOGFILE`
/// env var. Resolved once so the security-sensitive NOTICE is emitted a single
/// time, mirroring pvxs's one-shot `commonSetup` message; `None` when unset so
/// the rustls default (`NoKeyLog`) stays in place.
fn ssl_key_log() -> Option<Arc<dyn rustls::KeyLog>> {
    static KEYLOG: OnceLock<Option<Arc<dyn rustls::KeyLog>>> = OnceLock::new();
    KEYLOG
        .get_or_init(|| match std::env::var_os("SSLKEYLOGFILE") {
            Some(path) if !path.is_empty() => {
                eprintln!(
                    "NOTICE: debug logging TLS SECRETS to SSLKEYLOGFILE={}",
                    path.to_string_lossy()
                );
                Some(Arc::new(rustls::KeyLogFile::new()) as Arc<dyn rustls::KeyLog>)
            }
            _ => None,
        })
        .clone()
}

/// Load a server-side TLS configuration from environment variables.
///
/// Returns `Ok(None)` when TLS is not configured (no `EPICS_PVAS_TLS_KEYCHAIN`
/// set) or explicitly disabled.
pub fn load_server_config() -> Result<Option<TlsServerConfig>, TlsConfigError> {
    if tls_disabled() {
        return Ok(None);
    }
    // PVAS-priority fallback (pvxs `config.cpp:497`): server reads
    // `EPICS_PVAS_TLS_KEYCHAIN` first, then shared `EPICS_PVA_TLS_KEYCHAIN`.
    // Reading only the server-specific form left a server configured with
    // the shared keychain with TLS silently disabled. `$(VAR)` is expanded
    // inside the helper (PVA-466 parity).
    let Some(keychain) = crate::config::env::server_tls_keychain() else {
        return Ok(None);
    };
    // pvxs (`ossl.cpp:232-238`) splits the keychain spec at the first
    // `;`: the path precedes it, the PKCS#12 password follows. The
    // inline suffix is the pvxs source of truth and takes precedence;
    // the non-pvxs EPICS_PVAS/PVA_TLS_KEYCHAIN_PASSWORD env var is a
    // Rust-only fallback consulted only when the spec carries no `;`.
    // (PEM keys in our pipeline are unencrypted, so the password is
    // harmless there.)
    let (keychain_path, inline_password) = crate::config::env::split_keychain_spec(&keychain);
    let path = PathBuf::from(keychain_path);
    let password = inline_password.or_else(crate::config::env::server_tls_keychain_password);
    let Keychain {
        certs,
        key,
        ca_certs,
    } = load_keychain(&path, password.as_deref())?;
    let key = key.ok_or_else(|| TlsConfigError::NoKey(path.to_path_buf()))?;

    // pvxs ossl.cpp:264-275 — config-time sanity check on the loaded
    // server identity leaf: reject a CA cert (EXFLAG_CA) or a cert whose
    // extendedKeyUsage forbids the SSL Server role.
    if let Some(leaf) = certs.first() {
        validate_leaf_cert(leaf, &path, false)?;
    }

    // PVAS-priority fallback (pvxs `config.cpp:501`): server reads
    // `EPICS_PVAS_TLS_OPTIONS` first, then shared `EPICS_PVA_TLS_OPTIONS`.
    // Reading only the shared form silently dropped a server-only
    // `client_cert=require`, accepting certless clients (fail-open).
    let options = crate::config::env::server_tls_options();
    let require_client_cert = options.contains("client_cert=require");

    // Presented chain = leaf + any CA certs the keychain carried, so a
    // peer that lacks the intermediates can still build the path.
    // (pvxs does the same via `SSL_CTX_build_cert_chain`.) For PEM
    // input `ca_certs` is empty and this is exactly the old `certs`.
    let chain: Vec<CertificateDer<'static>> =
        certs.iter().chain(ca_certs.iter()).cloned().collect();

    // build the root CA store up front so we can (a) give it to the
    // WebPkiClientVerifier and (b) keep it in TlsServerConfig for
    // authority resolution when the peer sends a partial chain.
    let trust_roots = {
        let mut roots = RootCertStore::empty();
        for cert in certs.iter().chain(ca_certs.iter()) {
            let _ = roots.add(cert.clone());
        }
        Arc::new(roots)
    };

    // pvxs `ossl.cpp:355` sets `SSL_VERIFY_PEER | SSL_VERIFY_CLIENT_ONCE`
    // on the server unconditionally — for `Default`, `Optional`, AND
    // `Require` alike — so the server always sends a CertificateRequest
    // and verifies a presented client cert; only `Require` additionally
    // sets `SSL_VERIFY_FAIL_IF_NO_PEER_CERT` to reject certless clients.
    // There is no pvxs mode that skips the client-cert request.
    //
    // The pre-fix port took `with_no_client_auth()` whenever neither
    // `client_cert=optional` nor `=require` was set (the default), so the
    // default TLS server never requested a client cert — every client
    // connected anonymously and the x509 ACF identity was never
    // established. Match pvxs: always install the verifier, requesting +
    // verifying-if-present, and only require a cert under `=require`.
    let verifier = if require_client_cert {
        WebPkiClientVerifier::builder(trust_roots.clone())
            .build()
            .map_err(|e| TlsConfigError::Verifier(e.to_string()))?
    } else {
        WebPkiClientVerifier::builder(trust_roots.clone())
            .allow_unauthenticated()
            .build()
            .map_err(|e| TlsConfigError::Verifier(e.to_string()))?
    };
    let mut config = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(chain, key)?;
    if let Some(key_log) = ssl_key_log() {
        config.key_log = key_log;
    }

    Ok(Some(TlsServerConfig {
        config: Arc::new(config),
        require_client_cert,
        trust_roots,
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
        // pvxs splits the keychain spec at the first `;` (`ossl.cpp:232-238`):
        // inline password wins; the env var is the non-pvxs fallback.
        let (ca_keychain_path, inline_password) = crate::config::env::split_keychain_spec(&ca_path);
        let path = PathBuf::from(&ca_keychain_path);
        let password = inline_password.or_else(crate::config::env::client_tls_keychain_password);
        // A CA keychain may have no private key (PEM cert-only or a
        // PKCS#12 trust store) — that is fine, we only want the certs.
        let kc = load_keychain(&path, password.as_deref())?;
        for cert in kc.certs.into_iter().chain(kc.ca_certs) {
            let _ = roots.add(cert);
        }
    }

    let builder = ClientConfig::builder().with_root_certificates(roots);

    let mut config = if let Ok(keychain) = std::env::var("EPICS_PVA_TLS_KEYCHAIN") {
        // PVA-466: expand $(VAR) / ${VAR} in path env.
        let keychain = crate::config::env::expand_dollar_vars(&keychain);
        // pvxs splits the keychain spec at the first `;` (`ossl.cpp:232-238`):
        // inline password wins; the env var is the non-pvxs fallback.
        let (keychain_path, inline_password) = crate::config::env::split_keychain_spec(&keychain);
        let path = PathBuf::from(keychain_path);
        let password = inline_password.or_else(crate::config::env::client_tls_keychain_password);
        let Keychain {
            certs,
            key,
            ca_certs,
        } = load_keychain(&path, password.as_deref())?;
        let key = key.ok_or_else(|| TlsConfigError::NoKey(path.to_path_buf()))?;
        // pvxs ossl.cpp:264-275 — config-time sanity check on the loaded
        // client identity leaf: reject a CA cert (EXFLAG_CA) or a cert
        // whose extendedKeyUsage forbids the SSL Client role.
        if let Some(leaf) = certs.first() {
            validate_leaf_cert(leaf, &path, true)?;
        }
        // Present leaf + carried CA chain (see server-side rationale).
        let chain: Vec<CertificateDer<'static>> = certs.into_iter().chain(ca_certs).collect();
        builder
            .with_client_auth_cert(chain, key)
            .map_err(TlsConfigError::Rustls)?
    } else {
        builder.with_no_client_auth()
    };
    if let Some(key_log) = ssl_key_log() {
        config.key_log = key_log;
    }

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
fn load_keychain(path: &Path, password: Option<&str>) -> Result<Keychain, TlsConfigError> {
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
    bytes.windows(11).any(|w| w == b"-----BEGIN ")
}

fn load_pem_keychain(path: &Path, bytes: &[u8]) -> Result<Keychain, TlsConfigError> {
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
    let key = rustls_pemfile::private_key(&mut reader).map_err(|source| TlsConfigError::Pem {
        path: path.to_path_buf(),
        source,
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
/// when the *cert* bags carry no `localKeyID` of their own, we
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

    // Route by the keychain MAC digest algorithm. The `p12` crate
    // hard-`assert_eq!`s the MAC digest against SHA-1 and *panics* on
    // anything else (`p12` 0.6 src/lib.rs:381). OpenSSL 3.x stamps an
    // HMAC-SHA256 MAC, so we MUST decide *before* calling any `p12`
    // entry point: SHA-1 MAC → classic `p12` path; anything else →
    // RustCrypto PBES2 path (which also handles a MAC-less keychain).
    if !load_pkcs12_pbes2::has_classic_sha1_mac(bytes) {
        return load_pkcs12_pbes2::load(path, bytes, password);
    }

    let pfx = p12::PFX::parse(bytes).map_err(|e| TlsConfigError::Pkcs12 {
        path: path.to_path_buf(),
        reason: format!("not a valid PKCS#12 structure: {e}"),
    })?;

    // Classic SHA-1 MAC: a failed `verify_mac` is a real wrong
    // password (no other MAC family reaches this branch).
    if !pfx.verify_mac(password) {
        return Err(TlsConfigError::Pkcs12 {
            path: path.to_path_buf(),
            reason: "MAC verification failed (wrong EPICS_PVA*_TLS_KEYCHAIN_PASSWORD?)".to_string(),
        });
    }

    let bags = match pfx.bags(password) {
        Ok(bags) => bags,
        // Classic MAC passed but a bag would not decrypt with the
        // classic PBE primitives — a SHA-1-MAC keychain whose bags are
        // nonetheless PBES2-encrypted (OpenSSL `-macalg sha1` without
        // `-legacy`). Retry through the RustCrypto path.
        Err(_) => return load_pkcs12_pbes2::load(path, bytes, password),
    };

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

    // Mixed-encryption keychain: the outer SafeContents decrypted with
    // the classic primitives (so `bags()` succeeded) but the
    // `pkcs8ShroudedKeyBag` itself is PBES2-shrouded, so `get_key`
    // could not decrypt it. The RustCrypto loader handles a PBES2
    // shrouded key, so retry there rather than surfacing a misleading
    // `NoKey`. (A genuine cert-only trust store also has no key — the
    // PBES2 loader returns `key: None` for it too, so the retry stays
    // correct: it never invents a key that is not there.)
    if key.is_none() {
        if let Ok(kc) = load_pkcs12_pbes2::load(path, bytes, password) {
            if kc.key.is_some() {
                return Ok(kc);
            }
        }
    }

    // Identify the leaf index — pure logic, see `select_leaf_index`.
    let leaf_idx = select_leaf_index(&cert_bags, key_local_id.as_deref()).map_err(|reason| {
        TlsConfigError::Pkcs12 {
            path: path.to_path_buf(),
            reason,
        }
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
/// bags. `cert_bags` is `(cert_DER, that bag's own localKeyID)`
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
/// equals the key bag's id for the leaf and nothing else. SHA-1
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

/// PBES2 (PBKDF2 + AES-CBC) PKCS#12 keychain loader — the OpenSSL 3.x
/// default encoding the classic `p12` crate cannot decrypt.
///
/// `load_pkcs12_keychain` falls through to [`load_pkcs12_pbes2::load`]
/// whenever the classic path fails (classic MAC mismatch, or a bag the
/// classic PBE primitives reject). The split mirrors OpenSSL's own
/// `PKCS12_parse`, which transparently handles both PBE families.
///
/// Pipeline (RFC 7292):
/// 1. Parse the `PFX` SEQUENCE with `der`.
/// 2. Verify the keychain MAC. OpenSSL 3 stamps a SHA-256 MAC keyed by
///    the PKCS#12 KDF; a mismatch here is a *real* wrong-password and
///    is reported as one.
/// 3. Decode `authSafe` → `AuthenticatedSafe` (`SEQUENCE OF
///    ContentInfo`). Each element is either plain `id-data`
///    (`SafeContents` in the clear) or `id-encryptedData` (a PBES2
///    `EncryptedData` whose decrypted content is `SafeContents`).
/// 4. Walk every `SafeBag`: `certBag` → X.509 cert DER;
///    `pkcs8ShroudedKeyBag` → PBES2-encrypted `EncryptedPrivateKeyInfo`,
///    decrypted to a PKCS#8 `PrivateKeyInfo`; `keyBag` → plain PKCS#8.
/// 5. Pair leaf vs. CA with the shared [`select_leaf_index`] logic.
#[cfg(feature = "pkcs12")]
mod load_pkcs12_pbes2 {
    use super::{Keychain, TlsConfigError, select_leaf_index};
    use std::path::Path;

    use der::asn1::{Any, ObjectIdentifier, OctetString, SetOfVec};
    use der::{Decode, Encode, Sequence};
    // `digest` traits, re-exported by `sha2` — the PKCS#12 MAC KDF
    // (RFC 7292 Appendix B) is generic over the hash via these.
    use rc_sha2::digest;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};

    /// `id-data` — an unencrypted content blob (RFC 5652).
    const ID_DATA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.1");
    /// `id-encryptedData` — password-encrypted content (RFC 5652).
    const ID_ENCRYPTED_DATA: ObjectIdentifier =
        ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.6");
    /// `pkcs-9-at-localKeyID` — the bag attribute pairing a cert to its key.
    const OID_LOCAL_KEY_ID: ObjectIdentifier =
        ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.21");

    /// `id-sha1` digest algorithm OID (OIW).
    const OID_SHA1: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.14.3.2.26");
    /// `id-sha256` digest algorithm OID (NIST).
    const OID_SHA256: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.2.1");

    /// `keyBag` — an unencrypted PKCS#8 `PrivateKeyInfo` (RFC 7292 §4.2.1).
    const OID_KEY_BAG: ObjectIdentifier =
        ObjectIdentifier::new_unwrap("1.2.840.113549.1.12.10.1.1");
    /// `pkcs8ShroudedKeyBag` — a PBES2/PBE-encrypted private key (RFC 7292 §4.2.2).
    const OID_PKCS8_SHROUDED_KEY_BAG: ObjectIdentifier =
        ObjectIdentifier::new_unwrap("1.2.840.113549.1.12.10.1.2");
    /// `certBag` — an X.509 (or SDSI) certificate (RFC 7292 §4.2.3).
    const OID_CERT_BAG: ObjectIdentifier =
        ObjectIdentifier::new_unwrap("1.2.840.113549.1.12.10.1.3");

    // ─── PKCS#12 / RFC 5652 ASN.1 structures ────────────────────────
    //
    // RFC 7292 (PKCS#12) and RFC 5652 (CMS) define these as plain DER
    // `SEQUENCE`s. We decode them with the stable `der` crate's
    // `Sequence` derive — no pre-release `cms` / `pkcs12` crate needed.

    /// `ContentInfo` (RFC 5652 §3): a content type OID plus an optional
    /// `[0] EXPLICIT` content body. Used both as the PFX `authSafe` and
    /// as each `AuthenticatedSafe` element.
    #[derive(Sequence)]
    struct ContentInfo {
        content_type: ObjectIdentifier,
        #[asn1(context_specific = "0", tag_mode = "EXPLICIT", optional = "true")]
        content: Option<Any>,
    }

    /// `DigestInfo` (RFC 7292 §4): the MAC's digest algorithm and value.
    #[derive(Sequence)]
    struct DigestInfo {
        digest_algorithm: spki::AlgorithmIdentifierOwned,
        digest: OctetString,
    }

    /// `MacData` (RFC 7292 §4): the keychain integrity MAC. `iterations`
    /// carries an ASN.1 `DEFAULT 1`, so DER omits it when it equals 1 —
    /// modelled `Option` and defaulted on read.
    #[derive(Sequence)]
    struct MacData {
        mac: DigestInfo,
        mac_salt: OctetString,
        #[asn1(optional = "true")]
        iterations: Option<u32>,
    }

    /// `PFX` (RFC 7292 §4): the PKCS#12 top-level structure.
    #[derive(Sequence)]
    struct Pfx {
        version: u8,
        auth_safe: ContentInfo,
        #[asn1(optional = "true")]
        mac_data: Option<MacData>,
    }

    /// `EncryptedContentInfo` (RFC 5652 §6.1): the encrypted payload of
    /// an `EncryptedData`. `encrypted_content` is `[0] IMPLICIT OCTET
    /// STRING OPTIONAL`.
    #[derive(Sequence)]
    struct EncryptedContentInfo {
        content_type: ObjectIdentifier,
        content_enc_alg: spki::AlgorithmIdentifierOwned,
        #[asn1(context_specific = "0", tag_mode = "IMPLICIT", optional = "true")]
        encrypted_content: Option<OctetString>,
    }

    /// `EncryptedData` (RFC 5652 §8): a password-encrypted `SafeContents`
    /// carried as an `id-encryptedData` `AuthenticatedSafe` element.
    #[derive(Sequence)]
    struct EncryptedData {
        version: u8,
        enc_content_info: EncryptedContentInfo,
    }

    /// `Attribute` (RFC 5652 §5.3): a `SafeBag`'s attribute — used to
    /// read `localKeyID`. `ValueOrd` is derived so a `SetOfVec` of
    /// these can be DER-decoded (a SET OF must be value-orderable).
    #[derive(Sequence, der::ValueOrd)]
    struct Attribute {
        oid: ObjectIdentifier,
        values: SetOfVec<Any>,
    }

    /// `SafeBag` (RFC 7292 §4.2): one bag inside a `SafeContents`. The
    /// `bag_value` is `[0] EXPLICIT ANY` — its concrete type depends on
    /// `bag_id` (CertBag / EncryptedPrivateKeyInfo / PrivateKeyInfo).
    #[derive(Sequence)]
    struct SafeBag {
        bag_id: ObjectIdentifier,
        #[asn1(context_specific = "0", tag_mode = "EXPLICIT")]
        bag_value: Any,
        #[asn1(optional = "true")]
        bag_attributes: Option<SetOfVec<Attribute>>,
    }

    /// `CertBag` (RFC 7292 §4.2.3): a certificate bag. For X.509 the
    /// `cert_value` is `[0] EXPLICIT OCTET STRING` holding the cert DER.
    #[derive(Sequence)]
    struct CertBag {
        cert_id: ObjectIdentifier,
        #[asn1(context_specific = "0", tag_mode = "EXPLICIT")]
        cert_value: OctetString,
    }

    fn err(path: &Path, reason: impl Into<String>) -> TlsConfigError {
        TlsConfigError::Pkcs12 {
            path: path.to_path_buf(),
            reason: reason.into(),
        }
    }

    /// True iff the keychain carries a classic SHA-1 PKCS#12 MAC — the
    /// only MAC the `p12` crate can handle (it panics on any other).
    /// A MAC-less keychain or any non-SHA-1 MAC returns `false`, which
    /// routes the load through the stable-`der` PBES2 path. A
    /// structurally invalid keychain also returns `false`; the PBES2
    /// loader then surfaces the precise parse error.
    pub(super) fn has_classic_sha1_mac(bytes: &[u8]) -> bool {
        match Pfx::from_der(bytes) {
            Ok(pfx) => pfx
                .mac_data
                .map(|m| m.mac.digest_algorithm.oid == OID_SHA1)
                .unwrap_or(false),
            Err(_) => false,
        }
    }

    /// Decrypt a PBES2 ciphertext: decode `alg` as a `pkcs5`
    /// `EncryptionScheme` and run it over `ciphertext`. `pkcs5`'s
    /// `pbes2` feature covers PBKDF2 + AES-128/192/256-CBC, which is
    /// the full set OpenSSL 3.x emits for `.p12` bags.
    fn pbes2_decrypt(
        path: &Path,
        alg: &spki::AlgorithmIdentifierOwned,
        ciphertext: &[u8],
        password: &str,
    ) -> Result<Vec<u8>, TlsConfigError> {
        // `EncryptionScheme` is a DER SEQUENCE with the exact shape of
        // an `AlgorithmIdentifier` (oid + parameters), so re-encode the
        // algorithm id and decode it back as the scheme.
        let alg_der = alg
            .to_der()
            .map_err(|e| err(path, format!("re-encoding PBES2 algorithm id failed: {e}")))?;
        let scheme = pkcs5::EncryptionScheme::from_der(&alg_der).map_err(|e| {
            err(
                path,
                format!(
                    "unsupported PKCS#12 bag encryption (not PBES2/AES — \
                     re-encode with `openssl pkcs12 -legacy` if this is a \
                     classic-PBE keychain): {e}"
                ),
            )
        })?;
        scheme.decrypt(password, ciphertext).map_err(|e| {
            err(
                path,
                format!("PBES2 bag decryption failed (wrong password?): {e}"),
            )
        })
    }

    /// PKCS#12 password-based key derivation (RFC 7292 Appendix B.2),
    /// generic over the digest. `id` is the RFC's purpose byte: 1 =
    /// key material, 2 = IV, 3 = MAC key. `password` is the BMPString
    /// form (UTF-16BE + a trailing `00 00`). `block_len` is the hash's
    /// block size in bytes (64 for both SHA-1 and SHA-256). Returns
    /// `n` derived bytes.
    ///
    /// This is the algorithm OpenSSL's `PKCS12_key_gen` implements; we
    /// reproduce it directly so the keychain MAC can be checked with
    /// only the stable `digest`-based hash crates — no `pkcs12` crate.
    fn pkcs12_kdf<D>(
        id: u8,
        password_bmp: &[u8],
        salt: &[u8],
        iterations: u32,
        block_len: usize,
        n: usize,
    ) -> Vec<u8>
    where
        D: digest::Digest + digest::FixedOutputReset,
    {
        let u = <D as digest::OutputSizeUser>::output_size(); // digest length
        let v = block_len; // hash block length (bytes)

        // D = v bytes all equal to `id`.
        let diversifier = vec![id; v];

        // S = salt expanded to a multiple of v; P = password likewise.
        let expand = |src: &[u8]| -> Vec<u8> {
            if src.is_empty() {
                return Vec::new();
            }
            let len = v * src.len().div_ceil(v);
            let mut out = Vec::with_capacity(len);
            while out.len() < len {
                out.push(src[out.len() % src.len()]);
            }
            out
        };
        let s = expand(salt);
        let p = expand(password_bmp);

        // I = S || P.
        let mut i_buf = Vec::with_capacity(s.len() + p.len());
        i_buf.extend_from_slice(&s);
        i_buf.extend_from_slice(&p);

        let mut hasher = D::new();
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            // A = H^iterations( D || I ).
            digest::Digest::update(&mut hasher, &diversifier);
            digest::Digest::update(&mut hasher, &i_buf);
            let mut a = hasher.finalize_reset();
            for _ in 1..iterations {
                digest::Digest::update(&mut hasher, &a);
                a = hasher.finalize_reset();
            }
            out.extend_from_slice(&a[..u.min(n - out.len())]);
            if out.len() >= n {
                break;
            }

            // B = v-byte block formed by repeating A.
            let mut b = vec![0u8; v];
            for (k, byte) in b.iter_mut().enumerate() {
                *byte = a[k % u];
            }
            // I_j = (I_j + B + 1) mod 2^v, for each v-byte block of I.
            for chunk in i_buf.chunks_mut(v) {
                let mut carry = 1u16;
                for k in (0..chunk.len()).rev() {
                    let sum = chunk[k] as u16 + b[k] as u16 + carry;
                    chunk[k] = sum as u8;
                    carry = sum >> 8;
                }
            }
        }
        out.truncate(n);
        out
    }

    /// Verify the PKCS#12 keychain MAC. Reproduces OpenSSL's
    /// `PKCS12_verify_mac`: derive an HMAC key with the PKCS#12 KDF
    /// (RFC 7292 Appendix B) over the MAC salt, then HMAC the
    /// `authSafe` `Data` content. Supports the SHA-1 MAC (OpenSSL 1.x)
    /// and the SHA-256 MAC (OpenSSL 3.x default). A mismatch is a
    /// definitive wrong-password verdict.
    fn verify_mac(
        path: &Path,
        pfx: &Pfx,
        auth_safe_content: &[u8],
        password: &str,
    ) -> Result<(), TlsConfigError> {
        use rc_hmac::{KeyInit, Mac};

        let Some(mac_data) = &pfx.mac_data else {
            // A MAC-less PKCS#12 is legal (RFC 7292) — nothing to
            // verify. OpenSSL accepts these too.
            return Ok(());
        };
        let digest_oid = mac_data.mac.digest_algorithm.oid;
        let salt = mac_data.mac_salt.as_bytes();
        // ASN.1 `iterations` is `DEFAULT 1`; an absent field means 1.
        let rounds = mac_data.iterations.unwrap_or(1);
        let expected = mac_data.mac.digest.as_bytes();
        let password_bmp = super::bmp_password(password);

        // SHA-1 and SHA-256 cover every MAC OpenSSL emits for a `.p12`.
        // RFC 7292 MAC purpose byte is 3 (MAC key material).
        let ok = if digest_oid == OID_SHA256 {
            // SHA-256: 32-byte output, 64-byte block.
            let key = pkcs12_kdf::<rc_sha2::Sha256>(3, &password_bmp, salt, rounds, 64, 32);
            let mut mac = <rc_hmac::Hmac<rc_sha2::Sha256>>::new_from_slice(&key)
                .map_err(|e| err(path, format!("HMAC init failed: {e}")))?;
            mac.update(auth_safe_content);
            mac.verify_slice(expected).is_ok()
        } else if digest_oid == OID_SHA1 {
            // SHA-1: 20-byte output, 64-byte block.
            let key = pkcs12_kdf::<rc_sha1::Sha1>(3, &password_bmp, salt, rounds, 64, 20);
            let mut mac = <rc_hmac::Hmac<rc_sha1::Sha1>>::new_from_slice(&key)
                .map_err(|e| err(path, format!("HMAC init failed: {e}")))?;
            mac.update(auth_safe_content);
            mac.verify_slice(expected).is_ok()
        } else {
            return Err(err(
                path,
                format!("unsupported PKCS#12 MAC digest OID {digest_oid}"),
            ));
        };

        if ok {
            Ok(())
        } else {
            Err(err(
                path,
                "MAC verification failed (wrong EPICS_PVA*_TLS_KEYCHAIN_PASSWORD?)",
            ))
        }
    }

    /// Decode a `SafeContents` (`SEQUENCE OF SafeBag`) from DER.
    fn decode_safe_contents(path: &Path, der: &[u8]) -> Result<Vec<SafeBag>, TlsConfigError> {
        Vec::<SafeBag>::from_der(der)
            .map_err(|e| err(path, format!("SafeContents decode failed: {e}")))
    }

    /// Extract a `SafeBag`'s `localKeyID` attribute value, if present.
    /// The attribute value is an `OCTET STRING`; on any decode mishap
    /// we treat the id as absent rather than failing the whole load —
    /// `select_leaf_index` already handles a missing `localKeyID`.
    fn bag_local_key_id(bag: &SafeBag) -> Option<Vec<u8>> {
        let attrs = bag.bag_attributes.as_ref()?;
        for attr in attrs.iter() {
            if attr.oid == OID_LOCAL_KEY_ID {
                if let Some(v) = attr.values.iter().next() {
                    if let Ok(os) = v.decode_as::<OctetString>() {
                        return Some(os.as_bytes().to_vec());
                    }
                }
            }
        }
        None
    }

    /// Process one `SafeBag`, appending any X.509 cert / private key it
    /// carries to the running keychain accumulators. The `der`-derived
    /// `SafeBag` already strips the `[0] EXPLICIT` wrapper RFC 7292 puts
    /// around `bagValue`, so `bag.bag_value` is the inner CertBag /
    /// EncryptedPrivateKeyInfo / PrivateKeyInfo element directly.
    fn process_bag(
        path: &Path,
        bag: &SafeBag,
        password: &str,
        cert_bags: &mut Vec<(Vec<u8>, Option<Vec<u8>>)>,
        key: &mut Option<PrivateKeyDer<'static>>,
        key_local_id: &mut Option<Vec<u8>>,
    ) -> Result<(), TlsConfigError> {
        if bag.bag_id == OID_CERT_BAG {
            let cert_bag = bag
                .bag_value
                .decode_as::<CertBag>()
                .map_err(|e| err(path, format!("certBag decode failed: {e}")))?;
            cert_bags.push((
                cert_bag.cert_value.as_bytes().to_vec(),
                bag_local_key_id(bag),
            ));
        } else if bag.bag_id == OID_PKCS8_SHROUDED_KEY_BAG {
            // pkcs8ShroudedKeyBag = EncryptedPrivateKeyInfo:
            // SEQUENCE { encryptionAlgorithm AlgorithmIdentifier,
            //            encryptedData       OCTET STRING }.
            let epki = bag
                .bag_value
                .decode_as::<EncryptedPrivateKeyInfo>()
                .map_err(|e| err(path, format!("EncryptedPrivateKeyInfo decode failed: {e}")))?;
            let pkcs8 = pbes2_decrypt(
                path,
                &epki.encryption_algorithm,
                epki.encrypted_data.as_bytes(),
                password,
            )?;
            if key.is_none() {
                *key = Some(PrivateKeyDer::Pkcs8(pkcs8.into()));
                *key_local_id = bag_local_key_id(bag);
            }
        } else if bag.bag_id == OID_KEY_BAG {
            // keyBag = an unencrypted PKCS#8 PrivateKeyInfo.
            if key.is_none() {
                let inner = bag
                    .bag_value
                    .to_der()
                    .map_err(|e| err(path, format!("keyBag re-encode failed: {e}")))?;
                *key = Some(PrivateKeyDer::Pkcs8(inner.into()));
                *key_local_id = bag_local_key_id(bag);
            }
        }
        // CRL / secret / nested safeContents bags carry no TLS material
        // for our purposes — skip them.
        Ok(())
    }

    /// EncryptedPrivateKeyInfo (RFC 5958 §3) as carried inside a
    /// `pkcs8ShroudedKeyBag`. Decoded with `der` directly to avoid a
    /// `pkcs8`-crate dependency: the algorithm id feeds straight into
    /// `pkcs5::EncryptionScheme`.
    #[derive(Sequence)]
    struct EncryptedPrivateKeyInfo {
        encryption_algorithm: spki::AlgorithmIdentifierOwned,
        encrypted_data: OctetString,
    }

    /// Load a PBES2 (or mixed PBE) PKCS#12 keychain. See module doc.
    pub(super) fn load(
        path: &Path,
        bytes: &[u8],
        password: &str,
    ) -> Result<Keychain, TlsConfigError> {
        let pfx = Pfx::from_der(bytes)
            .map_err(|e| err(path, format!("not a valid PKCS#12 structure: {e}")))?;

        // `authSafe` must be an `id-data` ContentInfo wrapping the
        // DER of `AuthenticatedSafe`. The content `Any`'s value bytes
        // (after the OCTET STRING tag/len) are what the MAC covers.
        if pfx.auth_safe.content_type != ID_DATA {
            return Err(err(
                path,
                format!(
                    "PKCS#12 authSafe is not id-data ({}) — public-key \
                     protected keychains are not supported",
                    pfx.auth_safe.content_type
                ),
            ));
        }
        let auth_safe_octets = pfx
            .auth_safe
            .content
            .as_ref()
            .ok_or_else(|| err(path, "PKCS#12 authSafe has no content"))?
            .decode_as::<OctetString>()
            .map_err(|e| err(path, format!("authSafe content decode failed: {e}")))?;
        let auth_safe_der = auth_safe_octets.as_bytes();

        verify_mac(path, &pfx, auth_safe_der, password)?;

        // AuthenticatedSafe ::= SEQUENCE OF ContentInfo.
        let safes = Vec::<ContentInfo>::from_der(auth_safe_der)
            .map_err(|e| err(path, format!("AuthenticatedSafe decode failed: {e}")))?;

        let mut cert_bags: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
        let mut key: Option<PrivateKeyDer<'static>> = None;
        let mut key_local_id: Option<Vec<u8>> = None;

        for ci in &safes {
            let Some(content) = ci.content.as_ref() else {
                // A `ContentInfo` with no content carries no bags.
                continue;
            };
            let safe_contents_der: Vec<u8> = if ci.content_type == ID_DATA {
                // Plain SafeContents — content is an OCTET STRING.
                content
                    .decode_as::<OctetString>()
                    .map_err(|e| err(path, format!("data SafeContents decode failed: {e}")))?
                    .as_bytes()
                    .to_vec()
            } else if ci.content_type == ID_ENCRYPTED_DATA {
                // PBES2-encrypted SafeContents.
                let enc = content
                    .decode_as::<EncryptedData>()
                    .map_err(|e| err(path, format!("EncryptedData decode failed: {e}")))?;
                let eci = &enc.enc_content_info;
                let ciphertext = eci
                    .encrypted_content
                    .as_ref()
                    .ok_or_else(|| err(path, "EncryptedData has no encryptedContent"))?;
                pbes2_decrypt(path, &eci.content_enc_alg, ciphertext.as_bytes(), password)?
            } else {
                // Enveloped (public-key) SafeContents — not supported.
                continue;
            };

            for bag in decode_safe_contents(path, &safe_contents_der)? {
                process_bag(
                    path,
                    &bag,
                    password,
                    &mut cert_bags,
                    &mut key,
                    &mut key_local_id,
                )?;
            }
        }

        if cert_bags.is_empty() {
            return Err(TlsConfigError::NoCert(path.to_path_buf()));
        }

        // Reuse the classic path's leaf/CA pairing logic verbatim so
        // both encodings split the keychain identically.
        let leaf_idx = select_leaf_index(&cert_bags, key_local_id.as_deref())
            .map_err(|reason| err(path, reason))?;

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

/// The credential type itself lives in [`crate::auth::x509`] so that a build
/// without the `tls` feature can still name a peer's authorization identity
/// without linking rustls. Only the *chain → credentials* mapping below
/// needs the TLS types, and so only that stays here.
pub use crate::auth::x509::X509Credentials;

/// Extract the subject CommonName from a DER-encoded X.509 certificate.
/// Returns `None` when the cert fails to parse, carries no CN RDN, or the
/// CN is not a usable account name (empty or containing an embedded NUL).
///
/// The usability rejection mirrors pvxs `SSLContext::commonName()`
/// (pvxs @b16b945), which rejects BOTH an embedded NUL and a
/// `len <= 0` CN. `attr.as_str()` returns the CN's raw `ASN1_STRING`
/// bytes and a NUL is valid UTF-8, so a CN such as `admin\0.evil` would
/// otherwise become a Rust `String` that downstream ACF matching/logging
/// treats inconsistently — the NUL-prefix identity-confusion class
/// (CVE-2009-2408); an empty CN would map to an empty account. Neither is
/// a legitimate account, so we drop it (leaving the credential unset)
/// rather than map it to an account. Both the leaf `account` and the
/// root-CA `authority` route through this one helper, so the rejection
/// holds for both by construction.
fn subject_common_name(der: &[u8]) -> Option<String> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;
    cert.subject()
        .iter_common_name()
        .next()
        .and_then(|attr| attr.as_str().ok())
        .filter(|s| !s.is_empty() && !s.contains('\0'))
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

/// Validate a freshly-loaded keychain leaf certificate the way pvxs
/// `ossl.cpp:264-275` does before installing it as the local TLS
/// identity, a config-time fail-fast sanity check:
///
/// - **Reject a CA certificate** presented as the end-entity (pvxs
///   `flags & EXFLAG_CA` → "Found CA Certificate when End Entity
///   expected"). x509-parser surfaces this as `basicConstraints.ca`.
/// - **Require `extendedKeyUsage` to permit the SSL role** (pvxs reads
///   `X509_get_extended_key_usage` and rejects when the role bit is
///   absent: `XKU_SSL_SERVER` for a server identity, `XKU_SSL_CLIENT`
///   for a client identity). Two OpenSSL semantics matched exactly:
///   - An **absent** EKU extension is ACCEPTED. OpenSSL's
///     `X509_get_extended_key_usage` returns `UINT32_MAX` (all usages)
///     when the extension is missing, so `kusage & XKU_SSL_*` is
///     non-zero. Here `extended_key_usage()` returns `Ok(None)` for that
///     case, which we treat as "no restriction → accept".
///   - `anyExtendedKeyUsage` does NOT satisfy the role. OpenSSL maps it
///     to `XKU_ANYEKU` only, never to the SSL_CLIENT/SSL_SERVER bits, so
///     pvxs rejects an any-only EKU. We check `server_auth`/`client_auth`
///     alone, not `any`.
///
/// `is_client` selects the required role (`true` = client identity).
/// rustls/webpki validates the PEER's certificate at handshake but never
/// the locally-loaded own leaf, so without this check a misissued CA or
/// wrong-EKU cert installed as the local identity is silently accepted at
/// config time and only fails (if at all) as a peer-side handshake error.
fn validate_leaf_cert(
    leaf: &CertificateDer<'_>,
    path: &Path,
    is_client: bool,
) -> Result<(), TlsConfigError> {
    let (_, cert) = x509_parser::parse_x509_certificate(leaf.as_ref()).map_err(|e| {
        TlsConfigError::LeafInvalid {
            path: path.to_path_buf(),
            reason: e.to_string(),
        }
    })?;

    // pvxs ossl.cpp:269 — EXFLAG_CA: a CA cert is not a valid end-entity
    // identity. Absent basicConstraints ⇒ not a CA.
    let is_ca = cert
        .basic_constraints()
        .ok()
        .flatten()
        .map(|bc| bc.value.ca)
        .unwrap_or(false);
    if is_ca {
        return Err(TlsConfigError::LeafIsCa(path.to_path_buf()));
    }

    // pvxs ossl.cpp:272-275 — extendedKeyUsage must permit the SSL role.
    // Absent EKU (`Ok(None)`) ⇒ accept (OpenSSL UINT32_MAX). A present
    // EKU must carry the specific role bit; anyExtendedKeyUsage alone
    // does not count (matches pvxs's literal XKU_SSL_* check).
    if let Some(eku) = cert.extended_key_usage().ok().flatten() {
        let permitted = if is_client {
            eku.value.client_auth
        } else {
            eku.value.server_auth
        };
        if !permitted {
            return Err(TlsConfigError::LeafEkuForbidsRole {
                path: path.to_path_buf(),
                role: if is_client {
                    "SSL Client"
                } else {
                    "SSL Server"
                },
            });
        }
    }
    Ok(())
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
///
/// **Note:** this function resolves `authority` only when the peer
/// explicitly includes the root CA in its chain. For the common case
/// where the peer omits the root (RFC 5246 §7.4.2 allows this), call
/// [`x509_credentials_from_chain_with_roots`] which walks the server
/// trust store to find the root CA CN.
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

/// Like [`x509_credentials_from_chain`] but also resolves the `authority`
/// from the server trust store when the peer sends a partial chain.
///
/// pvxs uses `SSL_get0_verified_chain` (`ossl.cpp:423`) which appends the
/// root CA from the trust store even when the peer omits it; this function
/// reproduces that by walking `trust_roots` to find the anchor whose
/// subject DN matches the issuer of the last peer-provided cert.
pub fn x509_credentials_from_chain_with_roots(
    chain: &[CertificateDer<'_>],
    trust_roots: &RootCertStore,
) -> Option<X509Credentials> {
    let mut creds = x509_credentials_from_chain(chain)?;
    if creds.authority.is_empty() {
        creds.authority = authority_from_trust_roots(chain, trust_roots);
    }
    Some(creds)
}

/// Resolve the root CA CN for a peer chain by walking the server trust
/// store. Used when the peer omits the root from its chain.
///
/// Finds the trust anchor whose subject CN matches the issuer CN of the
/// last peer-provided cert, then returns that CN as the authority.
///
/// **Note on encoding:** rustls `TrustAnchor::subject` stores the CONTENT
/// bytes of the Subject SEQUENCE (no outer TLV tag+length) as returned by
/// webpki's `der::expect_tag`. `x509_parser::X509Name::from_der` expects
/// the full SEQUENCE TLV, so we reconstruct the header before parsing.
fn authority_from_trust_roots(chain: &[CertificateDer<'_>], roots: &RootCertStore) -> String {
    let Some(last) = chain.last() else {
        return String::new();
    };
    let Ok((_, last_cert)) = x509_parser::parse_x509_certificate(last) else {
        return String::new();
    };
    // Extract the issuer CN from the last peer cert.
    let issuer_cn = last_cert
        .issuer()
        .iter_common_name()
        .next()
        .and_then(|attr| attr.as_str().ok())
        .filter(|s| !s.is_empty() && !s.contains('\0'))
        .map(|s| s.to_string());
    let Some(issuer_cn) = issuer_cn else {
        return String::new();
    };

    // Confirm a trust anchor with that issuer CN exists.
    // anchor.subject is the raw CONTENT bytes (no outer SEQUENCE TLV) —
    // prepend the SEQUENCE tag+length before x509_parser can parse it.
    for anchor in &roots.roots {
        let content = anchor.subject.as_ref();
        let full = der_sequence_wrap(content);
        let Ok((_, name)) = x509_parser::x509::X509Name::from_der(&full) else {
            continue;
        };
        if let Some(cn) = name
            .iter_common_name()
            .next()
            .and_then(|attr| attr.as_str().ok())
            .filter(|s| !s.is_empty() && !s.contains('\0'))
        {
            if cn == issuer_cn.as_str() {
                return issuer_cn;
            }
        }
    }
    String::new()
}

/// Wrap raw DER content bytes in a SEQUENCE TLV (tag 0x30 + length encoding).
/// Used to reconstruct a parsable Name from a `TrustAnchor::subject` content
/// slice, which rustls/webpki stores without the outer tag+length header.
fn der_sequence_wrap(content: &[u8]) -> Vec<u8> {
    let n = content.len();
    let mut out = Vec::with_capacity(n + 4);
    out.push(0x30); // SEQUENCE tag
    if n < 0x80 {
        out.push(n as u8);
    } else if n < 0x100 {
        out.push(0x81);
        out.push(n as u8);
    } else {
        out.push(0x82);
        out.push((n >> 8) as u8);
        out.push(n as u8);
    }
    out.extend_from_slice(content);
    out
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
        // The server keychain now falls back to the shared
        // EPICS_PVA_TLS_KEYCHAIN (pvxs config.cpp:497), so this test must
        // also clear it — otherwise a shared client keychain in the
        // ambient env would enable server TLS and defeat the assertion.
        let prev_pva_keychain = std::env::var("EPICS_PVA_TLS_KEYCHAIN").ok();
        let prev_disable = std::env::var("EPICS_PVA_TLS_DISABLE").ok();
        unsafe {
            std::env::remove_var("EPICS_PVAS_TLS_KEYCHAIN");
            std::env::remove_var("EPICS_PVA_TLS_KEYCHAIN");
            std::env::remove_var("EPICS_PVA_TLS_DISABLE");
        }
        assert!(load_server_config().unwrap().is_none());
        if let Some(v) = prev_keychain {
            unsafe { std::env::set_var("EPICS_PVAS_TLS_KEYCHAIN", v) }
        }
        if let Some(v) = prev_pva_keychain {
            unsafe { std::env::set_var("EPICS_PVA_TLS_KEYCHAIN", v) }
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

    /// Build a PBES2 PKCS#12 from PEM cert+key using `openssl` with NO
    /// `-legacy` flag — exactly what OpenSSL 3.x emits by default
    /// (PBKDF2 + AES-256-CBC bags, HMAC-SHA256 MAC). The classic `p12`
    /// crate cannot decrypt this; the loader must fall through to the
    /// RustCrypto PBES2 path. Returns `None` if `openssl` is absent or
    /// is an OpenSSL 1.x build (which does not default to PBES2).
    #[cfg(feature = "pkcs12")]
    fn make_p12_pbes2(pem: &Pem, ca_pem: Option<&str>, password: &str) -> Option<Vec<u8>> {
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
        // No `-legacy`: OpenSSL 3.x then uses PBES2 (PBKDF2 + AES) for
        // both key and cert bags and an HMAC-SHA256 keychain MAC.
        cmd.arg("pkcs12")
            .arg("-export")
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
        let bytes = std::fs::read(&p12_path).ok()?;
        // Guard: an OpenSSL 1.x build writes a classic SHA-1-MAC
        // keychain here, which the `p12` path already covers and does
        // not exercise the PBES2 loader. Skip the fixture in that case.
        // (`has_classic_sha1_mac` is a pure `der` parse — it never
        // touches the panic-prone `p12` MAC check.)
        if load_pkcs12_pbes2::has_classic_sha1_mac(&bytes) {
            return None;
        }
        Some(bytes)
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
        let leaf_cert = leaf_params.signed_by(&leaf_key, &ca_cert, &ca_key).unwrap();

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

    /// PVA item #3: a PKCS#12 written with OpenSSL 3.x defaults (PBES2
    /// = PBKDF2 + AES-256-CBC bags, HMAC-SHA256 MAC, no `-legacy`)
    /// must load — the classic `p12` crate cannot decrypt these, so
    /// the loader has to fall through to the RustCrypto PBES2 path.
    #[test]
    #[cfg(feature = "pkcs12")]
    fn pkcs12_pbes2_openssl3_default_loads_leaf_and_key() {
        let pem = gen_self_signed("epics-pbes2-leaf");
        let Some(p12) = make_p12_pbes2(&pem, None, "pbes2-pw") else {
            eprintln!(
                "skipping: `openssl` unavailable or not an OpenSSL-3 \
                 (PBES2-default) build"
            );
            return;
        };
        let (_dir, path) = write_temp(&p12);

        let kc = load_keychain(&path, Some("pbes2-pw")).expect("load PBES2 p12");
        assert_eq!(kc.certs.len(), 1, "one leaf cert from PBES2 keychain");
        assert!(kc.key.is_some(), "PBES2-shrouded private key must decrypt");
        assert!(matches!(kc.key, Some(PrivateKeyDer::Pkcs8(_))));
        assert!(kc.ca_certs.is_empty(), "self-signed leaf — no CA stack");
    }

    /// PBES2 keychain with a CA: the leaf goes to `certs`, the CA to
    /// `ca_certs`, same split as the classic path.
    #[test]
    #[cfg(feature = "pkcs12")]
    fn pkcs12_pbes2_with_ca_chain_splits_leaf_from_ca() {
        let mut ca_params = rcgen::CertificateParams::new(vec![]).unwrap();
        ca_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "epics-pbes2-ca");
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();

        let mut leaf_params =
            rcgen::CertificateParams::new(vec!["epics-pbes2-svc".to_string()]).unwrap();
        leaf_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "epics-pbes2-svc");
        let leaf_key = rcgen::KeyPair::generate().unwrap();
        let leaf_cert = leaf_params.signed_by(&leaf_key, &ca_cert, &ca_key).unwrap();

        let pem = Pem {
            cert: leaf_cert.pem(),
            key: leaf_key.serialize_pem(),
        };
        let Some(p12) = make_p12_pbes2(&pem, Some(&ca_cert.pem()), "pw") else {
            eprintln!("skipping: `openssl` unavailable or not OpenSSL-3");
            return;
        };
        let (_dir, path) = write_temp(&p12);

        let kc = load_keychain(&path, Some("pw")).expect("load PBES2 p12 w/ CA");
        assert_eq!(kc.certs.len(), 1, "exactly one leaf");
        assert_eq!(kc.ca_certs.len(), 1, "exactly one CA cert");
        assert!(kc.key.is_some());
        assert_ne!(kc.certs[0].as_ref(), kc.ca_certs[0].as_ref());
    }

    /// A wrong password on a PBES2 keychain must be rejected with a
    /// clear MAC error — the SHA-256 keychain MAC catches it before
    /// any bag is decrypted.
    #[test]
    #[cfg(feature = "pkcs12")]
    fn pkcs12_pbes2_wrong_password_is_rejected() {
        let pem = gen_self_signed("epics-pbes2-leaf");
        let Some(p12) = make_p12_pbes2(&pem, None, "right-pw") else {
            eprintln!("skipping: `openssl` unavailable or not OpenSSL-3");
            return;
        };
        let (_dir, path) = write_temp(&p12);

        let err = load_keychain(&path, Some("wrong-pw")).unwrap_err();
        match err {
            TlsConfigError::Pkcs12 { reason, .. } => {
                assert!(reason.contains("MAC"), "got: {reason}");
            }
            other => panic!("expected Pkcs12 MAC error, got {other:?}"),
        }
    }

    /// `select_leaf_index` must NOT fall back to bag order when no
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
        let bags = vec![(ca.clone(), None), (leaf.clone(), Some(attr_id.clone()))];
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
        assert_eq!(select_leaf_index(&[(leaf.clone(), None)], None).unwrap(), 0,);

        // (5) No key localKeyID + multiple certs → ambiguous, hard
        // error rather than guessing (the silent-CA-as-leaf bug).
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
        // Clear both TLS_OPTIONS forms (server now reads PVAS-first).
        let _g = EnvGuard::set(&[
            ("EPICS_PVAS_TLS_KEYCHAIN", Some(path.to_str().unwrap())),
            ("EPICS_PVAS_TLS_KEYCHAIN_PASSWORD", Some("kc-pw")),
            ("EPICS_PVA_TLS_DISABLE", None),
            ("EPICS_PVAS_TLS_OPTIONS", None),
            ("EPICS_PVA_TLS_OPTIONS", None),
        ]);
        let cfg = load_server_config()
            .expect("load_server_config")
            .expect("Some(config)");
        assert!(!cfg.require_client_cert);
    }

    /// pvxs `ossl.cpp:355` sets `SSL_VERIFY_PEER` on the server for
    /// `Default`/`Optional`/`Require` alike, so even the default TLS
    /// server requests a client cert and verifies it if presented. The
    /// pre-fix port took `with_no_client_auth()` for the default (no
    /// `client_cert=` option), never sending a CertificateRequest — every
    /// client stayed anonymous and the x509 ACF identity was never
    /// established. Observable boundary: the default server now builds a
    /// non-empty trust store and installs the verifier (the old
    /// no-client-auth path left `trust_roots` empty); `require` stays off.
    #[test]
    #[cfg(feature = "pkcs12")]
    #[serial(epics_env)]
    fn server_default_requests_client_cert() {
        let pem = gen_self_signed("epics-pkcs12-server");
        let Some(p12) = make_p12(&pem, None, "kc-pw") else {
            eprintln!("skipping: `openssl` not available");
            return;
        };
        let (_dir, path) = write_temp(&p12);

        // No client_cert option set → the pvxs `Default` mode.
        let _g = EnvGuard::set(&[
            ("EPICS_PVAS_TLS_KEYCHAIN", Some(path.to_str().unwrap())),
            ("EPICS_PVAS_TLS_KEYCHAIN_PASSWORD", Some("kc-pw")),
            ("EPICS_PVA_TLS_DISABLE", None),
            ("EPICS_PVAS_TLS_OPTIONS", None),
            ("EPICS_PVA_TLS_OPTIONS", None),
        ]);
        let cfg = load_server_config()
            .expect("load_server_config")
            .expect("Some(config)");
        assert!(
            !cfg.require_client_cert,
            "default (no client_cert option) must not require a client cert"
        );
        assert!(
            !cfg.trust_roots.is_empty(),
            "default server must build a trust store and install the client-cert \
             verifier (request + verify-if-present); the pre-fix no_client_auth \
             path left trust_roots empty and never requested a client cert"
        );
    }

    /// Regression: pvxs (`ossl.cpp:232-238`) sources the PKCS#12 password
    /// from the `;password` suffix of the keychain spec. A pvxs-style
    /// `EPICS_PVAS_TLS_KEYCHAIN=<path>;<password>` with NO separate
    /// `_PASSWORD` env var must open. Pre-fix the whole `<path>;<password>`
    /// string was fed to `PathBuf::from`, so the file never opened.
    #[test]
    #[cfg(feature = "pkcs12")]
    #[serial(epics_env)]
    fn server_config_inline_keychain_password_splits_at_semicolon() {
        let pem = gen_self_signed("epics-pkcs12-server");
        let Some(p12) = make_p12(&pem, None, "kc-pw") else {
            eprintln!("skipping: `openssl` not available");
            return;
        };
        let (_dir, path) = write_temp(&p12);
        let spec = format!("{};kc-pw", path.to_str().unwrap());

        // Inline `;password`, NO separate _PASSWORD env var.
        let _g = EnvGuard::set(&[
            ("EPICS_PVAS_TLS_KEYCHAIN", Some(spec.as_str())),
            ("EPICS_PVAS_TLS_KEYCHAIN_PASSWORD", None),
            ("EPICS_PVA_TLS_KEYCHAIN_PASSWORD", None),
            ("EPICS_PVA_TLS_DISABLE", None),
            ("EPICS_PVAS_TLS_OPTIONS", None),
            ("EPICS_PVA_TLS_OPTIONS", None),
        ]);
        let cfg = load_server_config()
            .expect("inline `;password` keychain spec must open (pvxs ossl.cpp:232-238)")
            .expect("Some(config)");
        assert!(!cfg.require_client_cert);
    }

    /// The inline `;password` suffix is the pvxs source of truth and takes
    /// precedence over the non-pvxs `_PASSWORD` env fallback: a correct
    /// inline password opens even when a WRONG `_PASSWORD` env var is set.
    #[test]
    #[cfg(feature = "pkcs12")]
    #[serial(epics_env)]
    fn server_config_inline_keychain_password_beats_env_fallback() {
        let pem = gen_self_signed("epics-pkcs12-server");
        let Some(p12) = make_p12(&pem, None, "kc-pw") else {
            eprintln!("skipping: `openssl` not available");
            return;
        };
        let (_dir, path) = write_temp(&p12);
        let spec = format!("{};kc-pw", path.to_str().unwrap());

        let _g = EnvGuard::set(&[
            ("EPICS_PVAS_TLS_KEYCHAIN", Some(spec.as_str())),
            // Wrong env-var password: inline must win, so this is ignored.
            ("EPICS_PVAS_TLS_KEYCHAIN_PASSWORD", Some("not-the-password")),
            ("EPICS_PVA_TLS_KEYCHAIN_PASSWORD", None),
            ("EPICS_PVA_TLS_DISABLE", None),
            ("EPICS_PVAS_TLS_OPTIONS", None),
            ("EPICS_PVA_TLS_OPTIONS", None),
        ]);
        let cfg = load_server_config()
            .expect("inline password must take precedence over the env fallback")
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
            ("EPICS_PVAS_TLS_OPTIONS", None),
            ("EPICS_PVA_TLS_OPTIONS", None),
        ]);
        match load_server_config() {
            Err(TlsConfigError::Pkcs12 { .. }) => {}
            Err(other) => panic!("expected Pkcs12 error, got {other:?}"),
            Ok(_) => panic!("expected error for wrong PKCS#12 password"),
        }
    }

    /// Fail-open regression: a server-only
    /// `EPICS_PVAS_TLS_OPTIONS=client_cert=require` must be honoured even
    /// when the shared `EPICS_PVA_TLS_OPTIONS` is unset (pvxs PVAS-first
    /// `pickone`, config.cpp:501). Reading only the shared form dropped the
    /// requirement and let the server accept certless clients.
    #[test]
    #[cfg(feature = "pkcs12")]
    #[serial(epics_env)]
    fn server_honors_pvas_only_tls_options_require() {
        let pem = gen_self_signed("epics-pkcs12-server");
        let Some(p12) = make_p12(&pem, None, "kc-pw") else {
            eprintln!("skipping: `openssl` not available");
            return;
        };
        let (_dir, path) = write_temp(&p12);

        let _g = EnvGuard::set(&[
            ("EPICS_PVAS_TLS_KEYCHAIN", Some(path.to_str().unwrap())),
            ("EPICS_PVAS_TLS_KEYCHAIN_PASSWORD", Some("kc-pw")),
            ("EPICS_PVAS_TLS_OPTIONS", Some("client_cert=require")),
            ("EPICS_PVA_TLS_OPTIONS", None),
            ("EPICS_PVA_TLS_DISABLE", None),
        ]);
        let cfg = load_server_config()
            .expect("load_server_config")
            .expect("Some(config)");
        assert!(
            cfg.require_client_cert,
            "server-only EPICS_PVAS_TLS_OPTIONS=client_cert=require must be honoured"
        );
    }

    /// pvxs `Config::server()` resolves the keychain via
    /// `pickone({PVAS, PVA})` (config.cpp:497), so a server configured with
    /// only the shared `EPICS_PVA_TLS_KEYCHAIN` must still enable TLS.
    /// Reading the server-specific form alone left it silently disabled.
    #[test]
    #[cfg(feature = "pkcs12")]
    #[serial(epics_env)]
    fn server_falls_back_to_shared_pva_keychain() {
        let pem = gen_self_signed("epics-pkcs12-server");
        let Some(p12) = make_p12(&pem, None, "kc-pw") else {
            eprintln!("skipping: `openssl` not available");
            return;
        };
        let (_dir, path) = write_temp(&p12);

        let _g = EnvGuard::set(&[
            ("EPICS_PVAS_TLS_KEYCHAIN", None),
            ("EPICS_PVA_TLS_KEYCHAIN", Some(path.to_str().unwrap())),
            ("EPICS_PVAS_TLS_KEYCHAIN_PASSWORD", None),
            ("EPICS_PVA_TLS_KEYCHAIN_PASSWORD", Some("kc-pw")),
            ("EPICS_PVAS_TLS_OPTIONS", None),
            ("EPICS_PVA_TLS_OPTIONS", None),
            ("EPICS_PVA_TLS_DISABLE", None),
        ]);
        load_server_config()
            .expect("load_server_config")
            .expect("Some(config) — server must fall back to shared EPICS_PVA_TLS_KEYCHAIN");
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
        let mut params =
            rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()]).expect("leaf params");
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

    /// Build a self-signed DER cert with explicit CA flag and EKU set,
    /// for the `validate_leaf_cert` boundary tests.
    fn leaf_with(
        cn: &str,
        is_ca: bool,
        ekus: &[rcgen::ExtendedKeyUsagePurpose],
    ) -> CertificateDer<'static> {
        let mut params =
            rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()]).expect("leaf params");
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, cn);
        params.is_ca = if is_ca {
            rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained)
        } else {
            rcgen::IsCa::ExplicitNoCa
        };
        params.extended_key_usages = ekus.to_vec();
        let key = rcgen::KeyPair::generate().expect("leaf key");
        let cert = params.self_signed(&key).expect("self-signed leaf");
        CertificateDer::from(cert.der().to_vec())
    }

    /// pvxs ossl.cpp:269 — a CA certificate is rejected as the local
    /// end-entity identity (EXFLAG_CA), regardless of role.
    #[test]
    fn validate_leaf_rejects_ca_certificate() {
        let leaf = leaf_with("ca-as-identity", true, &[]);
        let p = Path::new("ca.p12");
        assert!(
            matches!(
                validate_leaf_cert(&leaf, p, false),
                Err(TlsConfigError::LeafIsCa(_))
            ),
            "CA cert must be rejected as a server identity"
        );
        assert!(
            matches!(
                validate_leaf_cert(&leaf, p, true),
                Err(TlsConfigError::LeafIsCa(_))
            ),
            "CA cert must be rejected as a client identity"
        );
    }

    /// pvxs/OpenSSL: an ABSENT extendedKeyUsage is accepted
    /// (`X509_get_extended_key_usage` returns UINT32_MAX = all usages).
    #[test]
    fn validate_leaf_absent_eku_is_accepted() {
        let leaf = leaf_with("no-eku", false, &[]);
        let p = Path::new("noeku.p12");
        assert!(validate_leaf_cert(&leaf, p, false).is_ok(), "server");
        assert!(validate_leaf_cert(&leaf, p, true).is_ok(), "client");
    }

    /// pvxs ossl.cpp:272-275 — a present EKU must carry the role bit:
    /// serverAuth-only is valid as a server identity, rejected as client.
    #[test]
    fn validate_leaf_server_auth_eku_role_gated() {
        let leaf = leaf_with(
            "server-only",
            false,
            &[rcgen::ExtendedKeyUsagePurpose::ServerAuth],
        );
        let p = Path::new("srv.p12");
        assert!(
            validate_leaf_cert(&leaf, p, false).is_ok(),
            "server accepts"
        );
        assert!(
            matches!(
                validate_leaf_cert(&leaf, p, true),
                Err(TlsConfigError::LeafEkuForbidsRole {
                    role: "SSL Client",
                    ..
                })
            ),
            "serverAuth-only must be rejected as a client identity"
        );
    }

    /// Symmetric: clientAuth-only is valid as a client identity, rejected
    /// as a server identity.
    #[test]
    fn validate_leaf_client_auth_eku_role_gated() {
        let leaf = leaf_with(
            "client-only",
            false,
            &[rcgen::ExtendedKeyUsagePurpose::ClientAuth],
        );
        let p = Path::new("cli.p12");
        assert!(validate_leaf_cert(&leaf, p, true).is_ok(), "client accepts");
        assert!(
            matches!(
                validate_leaf_cert(&leaf, p, false),
                Err(TlsConfigError::LeafEkuForbidsRole {
                    role: "SSL Server",
                    ..
                })
            ),
            "clientAuth-only must be rejected as a server identity"
        );
    }

    /// A cert carrying both serverAuth and clientAuth is valid for both
    /// roles.
    #[test]
    fn validate_leaf_both_eku_accepted_for_both_roles() {
        let leaf = leaf_with(
            "dual",
            false,
            &[
                rcgen::ExtendedKeyUsagePurpose::ServerAuth,
                rcgen::ExtendedKeyUsagePurpose::ClientAuth,
            ],
        );
        let p = Path::new("dual.p12");
        assert!(validate_leaf_cert(&leaf, p, false).is_ok(), "server");
        assert!(validate_leaf_cert(&leaf, p, true).is_ok(), "client");
    }

    /// pvxs/OpenSSL: `anyExtendedKeyUsage` maps to `XKU_ANYEKU` only, never
    /// the SSL_CLIENT/SSL_SERVER bits, so an any-only EKU is REJECTED for
    /// both roles (we match the literal pvxs `XKU_SSL_*` check, not "any").
    #[test]
    fn validate_leaf_any_eku_only_is_rejected() {
        let leaf = leaf_with("any-only", false, &[rcgen::ExtendedKeyUsagePurpose::Any]);
        let p = Path::new("any.p12");
        assert!(
            matches!(
                validate_leaf_cert(&leaf, p, false),
                Err(TlsConfigError::LeafEkuForbidsRole {
                    role: "SSL Server",
                    ..
                })
            ),
            "anyExtendedKeyUsage alone must not satisfy the server role"
        );
        assert!(
            matches!(
                validate_leaf_cert(&leaf, p, true),
                Err(TlsConfigError::LeafEkuForbidsRole {
                    role: "SSL Client",
                    ..
                })
            ),
            "anyExtendedKeyUsage alone must not satisfy the client role"
        );
    }

    #[test]
    fn subject_common_name_extracts_cn() {
        let (ca_cert, ca_key) = make_ca("CA");
        let leaf = make_leaf("my-account", &ca_cert, &ca_key);
        assert_eq!(subject_common_name(&leaf).as_deref(), Some("my-account"));
    }

    /// a peer-certificate CN with an embedded NUL must not be
    /// mapped to an ACF account. Without the rejection `subject_common_name`
    /// returns the full `"admin\0.evil"` string and the credential is built
    /// as that confused identity; the guard drops it so no `x509` credential
    /// is produced (matching pvxs leaving `method`/`account` at default).
    #[test]
    fn subject_common_name_rejects_embedded_nul_cn() {
        let (ca_cert, ca_key) = make_ca("CA");
        let leaf = make_leaf("admin\0.evil", &ca_cert, &ca_key);
        // Sanity: the CN really is the NUL-bearing one (the cert encodes it).
        assert_eq!(subject_common_name(&leaf), None);
        // And no credential is built from a NUL-bearing leaf CN.
        let chain = vec![leaf];
        assert!(x509_credentials_from_chain(&chain).is_none());
    }

    /// The same single helper guards the root-CA `authority`: a self-signed
    /// CA whose CN embeds a NUL yields a clean account from the leaf but an
    /// empty authority, never the truncated/confused authority string.
    #[test]
    fn nul_in_root_ca_cn_leaves_authority_empty() {
        let (ca_cert, ca_key) = make_ca("evil\0.ca");
        let leaf = make_leaf("svc-ok", &ca_cert, &ca_key);
        let root = CertificateDer::from(ca_cert.der().to_vec());
        let chain = vec![leaf, root];
        let creds = x509_credentials_from_chain(&chain).expect("credentials");
        assert_eq!(creds.account, "svc-ok");
        assert_eq!(creds.authority, "");
    }

    /// The `len <= 0` arm: an empty CN is not a usable account
    /// either — pvxs `commonName()` rejects `len <= 0`, so the Rust port
    /// must drop it rather than build an empty-account credential.
    #[test]
    fn subject_common_name_rejects_empty_cn() {
        let (ca_cert, ca_key) = make_ca("CA");
        let leaf = make_leaf("", &ca_cert, &ca_key);
        assert_eq!(subject_common_name(&leaf), None);
        assert!(x509_credentials_from_chain(&[leaf]).is_none());
    }
}
