//! Authentication / transport-security helpers for pvAccess.
//!
//! - [`plain`] — username/host AuthZ ("ca" mode); used over plain TCP. This
//!   is what every connection negotiates today.
//! - [`x509`] — the X.509 credential type, free of any TLS stack.
//! - [`tls`] — TLS-secured TCP via `rustls`, behind the `tls` feature (ON by
//!   default). Reads cert/key paths from the standard `EPICS_PVA{,S}_TLS_*`
//!   environment variables and produces ready-to-use `TlsConnector` /
//!   `TlsAcceptor` handles.
//!
//! Use of TLS is opt-in — callers wire `auth::tls::client_connector()` /
//! `auth::tls::server_acceptor()` into their `Connection::connect_tls` /
//! `run_pva_server_tls` entry points.
//!
//! # Building without the `tls` feature
//!
//! An RTEMS server build drops rustls (and with it `ring` → `getrandom 0.2`,
//! which does not compile for `armv7-rtems-eabihf` — design doc §8.2). To
//! keep that from turning into `#[cfg]` noise across the server, the two
//! config handle types stay *nameable* in both configurations:
//! [`TlsServerConfig`] and [`TlsClientConfig`] are the real rustls-backed
//! structs with the feature on, and **uninhabited** placeholders with it off.
//!
//! That is the whole seam. `ServerConfig::tls: Option<Arc<TlsServerConfig>>`,
//! `PvaClientBuilder::with_tls`, `ChannelPool::set_tls` and every
//! `config.tls.is_some()` decision compile untouched either way — but with
//! the feature off no value of the config type can exist, so the option is
//! provably `None` and the accept/connect upgrade paths (the only code that
//! actually touches a rustls type) are the only things that need gating.

pub mod plain;
#[cfg(feature = "tls")]
pub mod tls;
pub mod x509;

pub use plain::{authnz_default_host, authnz_default_user, osd_get_roles, posix_groups};
pub use x509::X509Credentials;

#[cfg(feature = "tls")]
pub use tls::{
    TlsClientConfig, TlsConfigError, TlsServerConfig, x509_credentials_from_chain,
    x509_credentials_from_chain_with_roots,
};

/// Server-side TLS configuration — uninhabited without the `tls` feature.
///
/// Not a stub: it is deliberately impossible to construct, which is what
/// makes `ServerConfig::tls` provably `None` on a no-TLS build and lets the
/// non-TLS server path compile with no `#[cfg]` at all.
#[cfg(not(feature = "tls"))]
#[derive(Debug)]
pub enum TlsServerConfig {}

/// Client-side TLS configuration — uninhabited without the `tls` feature.
/// See [`TlsServerConfig`].
#[cfg(not(feature = "tls"))]
#[derive(Debug)]
pub enum TlsClientConfig {}
