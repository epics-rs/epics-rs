//! Ed25519-signed capability tokens for CA identity.
//!
//! ACF was designed around UNIX hostnames and usernames — the model
//! breaks for clients behind NAT, in Kubernetes, or otherwise unable
//! to present a stable host identity. This module provides an
//! orthogonal identity layer: a small signed JSON token the client
//! ships in CLIENT_NAME, the server verifies, and the resolved
//! `subject` becomes the username for ACF matching.
//!
//! Token shape (the payload):
//!
//! ```json
//! {"sub":"alice","groups":["BEAM"],"exp":1714200000,"iss":"ops-1",
//!  "aud":"server-a","cnf":"<base64url(sha256(peer-cert-der))>"}
//! ```
//!
//! Encoded form: `cap:<base64url(payload)>.<base64url(signature)>`
//!
//! - signature is Ed25519 over the base64url-encoded payload bytes
//! - issuer key is identified by `iss` and looked up in the
//!   verifier's keyring
//! - `exp` is unix seconds; tokens past expiry are rejected
//! - `groups` are surfaced through ACF UAG matching as
//!   `cap-token-group:<NAME>` virtual entries
//! - `aud` is the server identity the token is minted for; a
//!   verifier configured with a non-empty audience rejects any
//!   token whose `aud` does not match (defeats cross-server replay)
//! - `cnf` is the channel-binding confirmation: base64url of the
//!   SHA-256 of the peer's leaf TLS certificate DER. It binds the
//!   token to the exact TLS channel it was minted for
//!
//! ## Replay protection / mTLS-only requirement
//!
//! Cap-tokens are bearer credentials shipped in the *plaintext*
//! `CA_PROTO_CLIENT_NAME` message. Without binding, any on-path
//! observer could capture and replay a token verbatim until `exp`.
//! To defeat this:
//!
//! - Every cap-token MUST be channel-bound: [`TokenIssuer::issue`]
//!   is called with a [`ChannelBinding`] derived from the TLS peer
//!   certificate, and [`TokenVerifier::verify`] rejects any token
//!   with no `cnf` claim ([`TokenError::Unbound`]).
//! - Because a [`ChannelBinding`] can only be derived from a TLS
//!   peer certificate, a bound token is useless without the matching
//!   mTLS channel. Cap-tokens therefore MUST NOT be used over a
//!   plaintext (non-TLS) circuit — `verify()` will reject them.
//! - A verifier SHOULD be configured with an audience
//!   ([`TokenVerifier::with_audience`]) so a token minted for one
//!   server cannot be presented to another.
//!
//! The format is intentionally not JWT — JWT carries a lot of
//! historical baggage we don't need. Custom but compact suits the
//! single-protocol scope here.

#![cfg(feature = "cap-tokens")]

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Transport channel binding: SHA-256 of the peer's leaf TLS
/// certificate DER. A cap-token is cryptographically bound to the
/// peer-certificate identity it was minted for, defeating replay over
/// any circuit presenting a different certificate.
///
/// This is a peer-*certificate* binding (RFC 5929 `tls-server-end-point`
/// style), not a per-TLS-session binding: two sessions that present the
/// same client certificate produce the same binding. That is sufficient
/// here — possession of the cert+key already grants mTLS access — but it
/// does not distinguish two connections from the same cert holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelBinding([u8; 32]);

impl ChannelBinding {
    /// Derive from the peer's leaf certificate DER bytes (SHA-256).
    pub fn from_peer_cert_der(der: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(der);
        let digest = hasher.finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        Self(out)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenClaims {
    /// Subject — the identifier ACF matches as the username.
    pub sub: String,
    /// Optional group memberships; surfaced through ACF UAG matching.
    #[serde(default)]
    pub groups: Vec<String>,
    /// Expiration in unix seconds.
    pub exp: u64,
    /// Issuer key id — looked up in the verifier's keyring.
    pub iss: String,
    /// Audience: the server identity the token is minted for.
    pub aud: String,
    /// Channel-binding confirmation: base64url (url-safe, no-pad) of
    /// the 32-byte SHA-256 of the peer's leaf TLS certificate DER.
    /// `None` = an unbound token (rejected by [`TokenVerifier::verify`]).
    #[serde(default)]
    pub cnf: Option<String>,
}

impl TokenClaims {
    pub fn is_expired(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.exp <= now
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("missing 'cap:' prefix")]
    MissingPrefix,
    #[error("malformed token (no '.' separator)")]
    Malformed,
    #[error("base64 decode failed: {0}")]
    Base64(String),
    #[error("json decode failed: {0}")]
    Json(String),
    #[error("unknown issuer key id: {0}")]
    UnknownIssuer(String),
    #[error("invalid signature")]
    BadSignature,
    #[error("token expired")]
    Expired,
    #[error("token revoked")]
    Revoked,
    #[error("token audience does not match verifier audience")]
    AudienceMismatch,
    #[error("token channel binding does not match the transport")]
    BindingMismatch,
    #[error("token is not channel-bound; cap-tokens require an mTLS circuit")]
    Unbound,
}

/// Issues tokens. One signing key per server / role.
pub struct TokenIssuer {
    iss_id: String,
    key: SigningKey,
}

impl TokenIssuer {
    pub fn new(iss_id: impl Into<String>, key: SigningKey) -> Self {
        Self {
            iss_id: iss_id.into(),
            key,
        }
    }

    /// Generate a fresh signing keypair. Caller is responsible for
    /// persistence.
    pub fn generate(iss_id: impl Into<String>) -> Self {
        use rand_core::OsRng;
        let mut csprng = OsRng;
        Self::new(iss_id, SigningKey::generate(&mut csprng))
    }

    /// Mint a token for `sub` with `groups`, valid for `ttl_secs`.
    ///
    /// `aud` is the server identity the token is minted for and is
    /// written into the payload. `binding`, when `Some`, writes the
    /// `cnf` channel-binding claim — callers MUST supply a binding
    /// derived from the TLS peer certificate, since the verifier
    /// rejects unbound tokens.
    pub fn issue(
        &self,
        sub: &str,
        groups: &[String],
        ttl_secs: u64,
        aud: &str,
        binding: Option<&ChannelBinding>,
    ) -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let cnf =
            binding.map(|b| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b.as_bytes()));
        let claims = TokenClaims {
            sub: sub.to_string(),
            groups: groups.to_vec(),
            // saturating: a near-u64::MAX ttl must not wrap to a past
            // exp (which would still fail-closed, but confusingly).
            exp: now.saturating_add(ttl_secs),
            iss: self.iss_id.clone(),
            aud: aud.to_string(),
            cnf,
        };
        let payload = serde_json::to_vec(&claims).expect("TokenClaims serializes infallibly");
        let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&payload);
        let sig: Signature = self.key.sign(payload_b64.as_bytes());
        let sig_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig.to_bytes());
        format!("cap:{payload_b64}.{sig_b64}")
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.key.verifying_key()
    }
}

/// Verifies tokens against a keyring of trusted issuers. Tracks a
/// revocation list of `(iss, sub)` pairs so an operator can blacklist
/// a stolen / leaked subject without rotating the issuer's key.
#[derive(Default)]
pub struct TokenVerifier {
    keys: HashMap<String, VerifyingKey>,
    /// Revocation entries keyed by `(iss, sub)`. Stateless tokens
    /// can't be "expired early" any other way; we accept the storage
    /// cost (small — typically a handful of entries).
    revoked: std::collections::HashSet<(String, String)>,
    /// Expected audience. When non-empty, [`TokenVerifier::verify`]
    /// rejects any token whose `aud` claim does not match. Empty =
    /// audience checking disabled.
    audience: String,
}

impl TokenVerifier {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the expected audience — the identity of this server. A
    /// token whose `aud` claim does not match is rejected with
    /// [`TokenError::AudienceMismatch`].
    pub fn with_audience(mut self, aud: impl Into<String>) -> Self {
        self.audience = aud.into();
        self
    }

    pub fn trust(&mut self, iss_id: impl Into<String>, key: VerifyingKey) {
        self.keys.insert(iss_id.into(), key);
    }

    /// Block a subject issued by a specific issuer. Future verifies
    /// of any token with the same `(iss, sub)` pair return
    /// [`TokenError::Revoked`]. Idempotent.
    pub fn revoke(&mut self, iss_id: impl Into<String>, sub: impl Into<String>) {
        self.revoked.insert((iss_id.into(), sub.into()));
    }

    /// Lift a previous revoke. Idempotent.
    pub fn unrevoke(&mut self, iss_id: &str, sub: &str) {
        self.revoked.remove(&(iss_id.to_string(), sub.to_string()));
    }

    /// Snapshot of the trusted keys for export / publishing
    /// (e.g. via a `/keys` introspection endpoint). Returned as
    /// `(issuer_id, public_key_bytes)` pairs.
    pub fn export_keys(&self) -> Vec<(String, [u8; 32])> {
        self.keys
            .iter()
            .map(|(iss, vk)| (iss.clone(), vk.to_bytes()))
            .collect()
    }

    /// Verify a token, returning its claims on success.
    ///
    /// `binding` is the channel binding derived from the current TLS
    /// peer certificate, or `None` if the circuit is plaintext.
    /// Beyond signature / expiry / revocation, verification enforces:
    ///
    /// - **Audience**: if the verifier has a non-empty audience, the
    ///   token's `aud` must match it, else [`TokenError::AudienceMismatch`].
    /// - **Channel binding**: a token with a `cnf` claim requires a
    ///   matching `binding`, else [`TokenError::BindingMismatch`]. A
    ///   token with no `cnf` claim is rejected with
    ///   [`TokenError::Unbound`] — cap-tokens MUST be used over mTLS.
    pub fn verify(
        &self,
        token: &str,
        binding: Option<&ChannelBinding>,
    ) -> Result<TokenClaims, TokenError> {
        let body = token
            .strip_prefix("cap:")
            .ok_or(TokenError::MissingPrefix)?;
        let (payload_b64, sig_b64) = body.split_once('.').ok_or(TokenError::Malformed)?;
        let sig_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(sig_b64)
            .map_err(|e| TokenError::Base64(e.to_string()))?;
        if sig_bytes.len() != 64 {
            return Err(TokenError::BadSignature);
        }
        let mut sig_arr = [0u8; 64];
        sig_arr.copy_from_slice(&sig_bytes);
        let signature = Signature::from_bytes(&sig_arr);
        let payload_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload_b64)
            .map_err(|e| TokenError::Base64(e.to_string()))?;
        // G5: bound the JSON parse before verification. JSON parse
        // runs against attacker-controlled bytes; while we still
        // need claims.iss to look up the verification key (so we
        // can't fully verify-first), capping the payload length
        // prevents pathological deeply-nested JSON from burning CPU.
        // Real CA cap-tokens are <1 KiB; 4 KiB gives generous headroom.
        const MAX_PAYLOAD_BYTES: usize = 4096;
        if payload_bytes.len() > MAX_PAYLOAD_BYTES {
            return Err(TokenError::Malformed);
        }
        let claims: TokenClaims =
            serde_json::from_slice(&payload_bytes).map_err(|e| TokenError::Json(e.to_string()))?;
        let key = self
            .keys
            .get(&claims.iss)
            .ok_or_else(|| TokenError::UnknownIssuer(claims.iss.clone()))?;
        // verify_strict (not verify): additionally rejects low-order /
        // non-canonical public keys, closing the signature-malleability
        // hole for a malicious issuer keyring entry. Behavior for honest
        // keys and signatures is unchanged.
        key.verify_strict(payload_b64.as_bytes(), &signature)
            .map_err(|_| TokenError::BadSignature)?;
        if claims.is_expired() {
            return Err(TokenError::Expired);
        }
        if self
            .revoked
            .contains(&(claims.iss.clone(), claims.sub.clone()))
        {
            return Err(TokenError::Revoked);
        }
        // Audience: a token minted for another server must not be
        // accepted here.
        if !self.audience.is_empty() && claims.aud != self.audience {
            return Err(TokenError::AudienceMismatch);
        }
        // Channel binding / mTLS gating: every cap-token must be
        // bound to the TLS channel it was minted for. An unbound
        // token (no `cnf`) is rejected outright — since a binding is
        // only derivable from a TLS peer cert, this enforces that
        // cap-tokens only work over an mTLS circuit.
        match &claims.cnf {
            Some(cnf_b64) => {
                let cnf_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(cnf_b64)
                    .map_err(|e| TokenError::Base64(e.to_string()))?;
                let bound = binding.ok_or(TokenError::BindingMismatch)?;
                if cnf_bytes.as_slice() != bound.as_bytes() {
                    return Err(TokenError::BindingMismatch);
                }
            }
            None => return Err(TokenError::Unbound),
        }
        Ok(claims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fabricate a deterministic [`ChannelBinding`] for tests by
    /// hashing fake certificate DER bytes.
    fn fake_binding(der: &[u8]) -> ChannelBinding {
        ChannelBinding::from_peer_cert_der(der)
    }

    #[test]
    fn round_trip_valid() {
        let issuer = TokenIssuer::generate("ops-1");
        let mut verifier = TokenVerifier::new();
        verifier.trust("ops-1", issuer.verifying_key());
        let binding = fake_binding(b"fake-cert-der");
        let tok = issuer.issue(
            "alice",
            &["BEAM".into(), "DIAG".into()],
            3600,
            "server-a",
            Some(&binding),
        );
        let claims = verifier.verify(&tok, Some(&binding)).expect("valid token");
        assert_eq!(claims.sub, "alice");
        assert_eq!(claims.groups, vec!["BEAM", "DIAG"]);
        assert_eq!(claims.iss, "ops-1");
        assert_eq!(claims.aud, "server-a");
    }

    #[test]
    fn rejects_unknown_issuer() {
        let issuer = TokenIssuer::generate("ops-1");
        let verifier = TokenVerifier::new();
        let binding = fake_binding(b"fake-cert-der");
        let tok = issuer.issue("alice", &[], 3600, "server-a", Some(&binding));
        let err = verifier.verify(&tok, Some(&binding)).unwrap_err();
        matches!(err, TokenError::UnknownIssuer(_))
            .then_some(())
            .expect("expected UnknownIssuer");
    }

    #[test]
    fn rejects_tampered_payload() {
        let issuer = TokenIssuer::generate("ops-1");
        let mut verifier = TokenVerifier::new();
        verifier.trust("ops-1", issuer.verifying_key());
        let binding = fake_binding(b"fake-cert-der");
        let tok = issuer.issue("alice", &[], 3600, "server-a", Some(&binding));
        // Flip a byte in the payload portion.
        let body = tok.strip_prefix("cap:").unwrap();
        let (p, s) = body.split_once('.').unwrap();
        let mut p_bytes = p.as_bytes().to_vec();
        p_bytes[0] ^= 0xFF;
        let tampered = format!("cap:{}.{s}", String::from_utf8_lossy(&p_bytes));
        assert!(verifier.verify(&tampered, Some(&binding)).is_err());
    }

    #[test]
    fn rejects_revoked_then_unrevoke_works() {
        let issuer = TokenIssuer::generate("ops-1");
        let mut verifier = TokenVerifier::new();
        verifier.trust("ops-1", issuer.verifying_key());
        let binding = fake_binding(b"fake-cert-der");
        let tok = issuer.issue("alice", &[], 3600, "server-a", Some(&binding));
        assert!(verifier.verify(&tok, Some(&binding)).is_ok());
        verifier.revoke("ops-1", "alice");
        let err = verifier.verify(&tok, Some(&binding)).unwrap_err();
        matches!(err, TokenError::Revoked)
            .then_some(())
            .expect("expected Revoked");
        // Lifting the revocation re-allows the token.
        verifier.unrevoke("ops-1", "alice");
        assert!(verifier.verify(&tok, Some(&binding)).is_ok());
    }

    #[test]
    fn export_keys_reflects_keyring() {
        let issuer = TokenIssuer::generate("ops-1");
        let mut verifier = TokenVerifier::new();
        verifier.trust("ops-1", issuer.verifying_key());
        let exported = verifier.export_keys();
        assert_eq!(exported.len(), 1);
        assert_eq!(exported[0].0, "ops-1");
        assert_eq!(exported[0].1, issuer.verifying_key().to_bytes());
    }

    #[test]
    fn rejects_expired() {
        let issuer = TokenIssuer::generate("ops-1");
        let mut verifier = TokenVerifier::new();
        verifier.trust("ops-1", issuer.verifying_key());
        let binding = fake_binding(b"fake-cert-der");
        let tok = issuer.issue("alice", &[], 0, "server-a", Some(&binding)); // exp = now
        std::thread::sleep(std::time::Duration::from_secs(1));
        let err = verifier.verify(&tok, Some(&binding)).unwrap_err();
        matches!(err, TokenError::Expired)
            .then_some(())
            .expect("expected Expired");
    }

    #[test]
    fn bound_token_verifies_with_matching_binding() {
        let issuer = TokenIssuer::generate("ops-1");
        let mut verifier = TokenVerifier::new();
        verifier.trust("ops-1", issuer.verifying_key());
        let binding = fake_binding(b"peer-cert-1");
        let tok = issuer.issue("alice", &[], 3600, "server-a", Some(&binding));
        let claims = verifier
            .verify(&tok, Some(&binding))
            .expect("matching binding verifies");
        assert_eq!(claims.sub, "alice");
        assert!(claims.cnf.is_some());
    }

    #[test]
    fn bound_token_rejected_with_different_binding() {
        let issuer = TokenIssuer::generate("ops-1");
        let mut verifier = TokenVerifier::new();
        verifier.trust("ops-1", issuer.verifying_key());
        let binding = fake_binding(b"peer-cert-1");
        let other = fake_binding(b"peer-cert-2");
        let tok = issuer.issue("alice", &[], 3600, "server-a", Some(&binding));
        let err = verifier.verify(&tok, Some(&other)).unwrap_err();
        matches!(err, TokenError::BindingMismatch)
            .then_some(())
            .expect("expected BindingMismatch for different binding");
    }

    #[test]
    fn bound_token_rejected_with_absent_binding() {
        let issuer = TokenIssuer::generate("ops-1");
        let mut verifier = TokenVerifier::new();
        verifier.trust("ops-1", issuer.verifying_key());
        let binding = fake_binding(b"peer-cert-1");
        let tok = issuer.issue("alice", &[], 3600, "server-a", Some(&binding));
        let err = verifier.verify(&tok, None).unwrap_err();
        matches!(err, TokenError::BindingMismatch)
            .then_some(())
            .expect("expected BindingMismatch for absent binding");
    }

    #[test]
    fn unbound_token_rejected_even_with_binding() {
        let issuer = TokenIssuer::generate("ops-1");
        let mut verifier = TokenVerifier::new();
        verifier.trust("ops-1", issuer.verifying_key());
        let binding = fake_binding(b"peer-cert-1");
        // Token minted with no binding.
        let tok = issuer.issue("alice", &[], 3600, "server-a", None);
        let err = verifier.verify(&tok, Some(&binding)).unwrap_err();
        matches!(err, TokenError::Unbound)
            .then_some(())
            .expect("expected Unbound for token minted without a binding");
    }

    #[test]
    fn audience_mismatch_rejected() {
        let issuer = TokenIssuer::generate("ops-1");
        let mut verifier = TokenVerifier::new().with_audience("server-a");
        verifier.trust("ops-1", issuer.verifying_key());
        let binding = fake_binding(b"peer-cert-1");
        let tok = issuer.issue("alice", &[], 3600, "server-b", Some(&binding));
        let err = verifier.verify(&tok, Some(&binding)).unwrap_err();
        matches!(err, TokenError::AudienceMismatch)
            .then_some(())
            .expect("expected AudienceMismatch");
    }

    #[test]
    fn audience_match_passes() {
        let issuer = TokenIssuer::generate("ops-1");
        let mut verifier = TokenVerifier::new().with_audience("server-a");
        verifier.trust("ops-1", issuer.verifying_key());
        let binding = fake_binding(b"peer-cert-1");
        let tok = issuer.issue("alice", &[], 3600, "server-a", Some(&binding));
        let claims = verifier
            .verify(&tok, Some(&binding))
            .expect("matching audience verifies");
        assert_eq!(claims.aud, "server-a");
    }
}
