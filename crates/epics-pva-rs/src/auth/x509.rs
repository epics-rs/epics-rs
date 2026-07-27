//! X.509 authorization identity — the credential type, free of any TLS
//! stack.
//!
//! This lives outside `crate::auth::tls` on purpose. The value an
//! authenticated peer contributes to authorization is two strings; it is
//! carried on the plain connection path too (as `None`), matched by ACF
//! rules, and reported by `PvaServer::report()`. Keeping the *type* in the
//! rustls-bearing module made "a build that can name a peer's credentials"
//! imply "a build that links rustls, ring and getrandom" — which is exactly
//! the coupling the RTEMS server build has to break (design doc §8.2).
//!
//! The construction side — turning a *verified rustls certificate chain*
//! into one of these — stays in `crate::auth::tls`, behind the `tls`
//! feature, because that genuinely needs the TLS types.

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
