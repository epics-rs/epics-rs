//! Authentication / transport-security helpers for pvAccess.
//!
//! - [`plain`] — username/host AuthZ ("ca" mode); used over plain TCP. This
//!   is what every connection negotiates today.
//! - [`tls`] — TLS-secured TCP via `rustls`. Reads cert/key paths from the
//!   standard `EPICS_PVA{,S}_TLS_*` environment variables and produces ready-
//!   to-use `TlsConnector` / `TlsAcceptor` handles.
//!
//! Use of TLS is opt-in — callers wire `auth::tls::client_connector()` /
//! `auth::tls::server_acceptor()` into their `Connection::connect_tls` /
//! `run_pva_server_tls` entry points.

pub mod plain;
pub mod tls;

pub use plain::{authnz_default_host, authnz_default_user, posix_groups};
pub use tls::{
    TlsClientConfig, TlsConfigError, TlsServerConfig, X509Credentials, x509_credentials_from_chain,
    x509_credentials_from_chain_with_roots,
};
