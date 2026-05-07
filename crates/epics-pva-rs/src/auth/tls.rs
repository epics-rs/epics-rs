//! TLS configuration for `pvas://` (TLS-secured pvAccess).
//!
//! Wraps `rustls` with the conventions pvxs uses for cert distribution:
//!
//! - `EPICS_PVAS_TLS_KEYCHAIN`  — server cert + private key (PEM bundle)
//! - `EPICS_PVAS_TLS_KEYCHAIN_PASSWORD` — password (currently unused; PEM
//!   keys aren't password-protected in our pipeline)
//! - `EPICS_PVA_TLS_KEYCHAIN`   — client cert (mutual TLS)
//! - `EPICS_PVA_TLS_OPTIONS`    — option string; we recognise
//!   `client_cert=optional` / `client_cert=require`
//! - `EPICS_PVA_TLS_DISABLE`    — set to `YES` to disable TLS even when
//!   configured
//!
//! This module produces ready-to-use `rustls::ClientConfig` / `ServerConfig`
//! values; the client/server runtime layers wrap them in `TlsConnector`/
//! `TlsAcceptor` on demand. We deliberately *don't* spin up a TLS connection
//! here — that work belongs in `client_native::server_conn` / `server_native::tcp`,
//! which can decide per-target whether to upgrade the socket.

use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
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

    let (certs, key) = read_pem_bundle(&path)?;

    let options = std::env::var("EPICS_PVA_TLS_OPTIONS").unwrap_or_default();
    let require_client_cert = options.contains("client_cert=require");
    let optional_client_cert = require_client_cert || options.contains("client_cert=optional");

    let config = if optional_client_cert {
        // Build a client verifier whose root CA store is the same bundle.
        let mut roots = RootCertStore::empty();
        for cert in &certs {
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
            .with_single_cert(certs, key)?
    } else {
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)?
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
        let (certs, _) = read_pem_bundle(&path).or_else(|e| match e {
            TlsConfigError::NoKey(_) => read_pem_bundle_certs_only(&path).map(|c| (c, dummy_key())),
            other => Err(other),
        })?;
        for cert in certs {
            let _ = roots.add(cert);
        }
    }

    let builder = ClientConfig::builder().with_root_certificates(roots);

    let config = if let Ok(keychain) = std::env::var("EPICS_PVA_TLS_KEYCHAIN") {
        // PVA-466: expand $(VAR) / ${VAR} in path env.
        let keychain = crate::config::env::expand_dollar_vars(&keychain);
        let path = PathBuf::from(keychain);
        let (certs, key) = read_pem_bundle(&path)?;
        builder
            .with_client_auth_cert(certs, key)
            .map_err(TlsConfigError::Rustls)?
    } else {
        builder.with_no_client_auth()
    };

    Ok(Some(TlsClientConfig {
        config: Arc::new(config),
    }))
}

// ─── PEM I/O helpers ────────────────────────────────────────────────────

fn read_pem_bundle(
    path: &PathBuf,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), TlsConfigError> {
    let file = File::open(path).map_err(|source| TlsConfigError::Io {
        path: path.clone(),
        source,
    })?;
    let mut reader = BufReader::new(file);

    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| TlsConfigError::Pem {
            path: path.clone(),
            source,
        })?;
    if certs.is_empty() {
        return Err(TlsConfigError::NoCert(path.clone()));
    }

    // Re-open to scan keys (rustls_pemfile consumes the reader).
    let file = File::open(path).map_err(|source| TlsConfigError::Io {
        path: path.clone(),
        source,
    })?;
    let mut reader = BufReader::new(file);

    let key = rustls_pemfile::private_key(&mut reader)
        .map_err(|source| TlsConfigError::Pem {
            path: path.clone(),
            source,
        })?
        .ok_or_else(|| TlsConfigError::NoKey(path.clone()))?;

    Ok((certs, key))
}

fn read_pem_bundle_certs_only(
    path: &PathBuf,
) -> Result<Vec<CertificateDer<'static>>, TlsConfigError> {
    let file = File::open(path).map_err(|source| TlsConfigError::Io {
        path: path.clone(),
        source,
    })?;
    let mut reader = BufReader::new(file);
    rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| TlsConfigError::Pem {
            path: path.clone(),
            source,
        })
}

fn dummy_key() -> PrivateKeyDer<'static> {
    // Never used — only here to satisfy the tuple shape of `read_pem_bundle`'s
    // signature in the CA-only fallback path.
    PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(Vec::new()))
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
}
