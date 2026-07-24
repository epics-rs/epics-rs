//! Ed25519-signed beacon authentication.
//!
//! `CA_PROTO_RSRV_IS_UP` itself has no authentication — anyone on the
//! beacon-broadcast segment can claim to be a CA server, which a
//! malicious actor uses to redirect clients to a poisoned IOC. This
//! module fixes that without breaking C interop, by emitting a signed
//! *companion datagram* immediately after each beacon. C clients
//! receive an unrecognized command (0xCAFE) and ignore it; Rust
//! clients with a configured verifier match the companion to the
//! beacon by `(source_ip, server_ip, server_port, beacon_id)` and
//! reject the beacon if no valid signature lands within the time
//! window.
//!
//! The signature covers `(server_ip‖server_port‖beacon_id‖ts)` so it
//! cannot be replayed across hosts, ports, or sequence numbers.
//!
//! Companion wire format (96-byte payload after the 16-byte header):
//!
//! ```text
//! 0..16   header (cmmd=0xCAFE, postsize=80, cid=beacon_id, available=server_ip)
//! 16..24  ts (u64 unix seconds, big-endian)
//! 24..88  signature (64 bytes, Ed25519)
//! 88..96  issuer key id (8 bytes, identifies the signing key)
//! ```
//!
//! Gated behind the `cap-tokens` feature because it reuses the
//! Ed25519 primitives from that module.

#![cfg(feature = "cap-tokens")]

// RTEMS-EXEC-MODEL-ALLOW(4): not built feature-ON by default - this module is behind the `cap-tokens` feature.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use tokio::net::UdpSocket;

/// Custom CA command for the signed beacon companion datagram. Picked
/// outside the libca/rsrv reserved range. C clients ignore unknown
/// commands (libca's `cac_recv_msg` skips them).
pub const CA_PROTO_RSRV_BEACON_SIG: u16 = 0xCAFE;

const PAYLOAD_SIZE: usize = 80;
const HEADER_SIZE: usize = 16;
const COMPANION_SIZE: usize = HEADER_SIZE + PAYLOAD_SIZE;

/// 8-byte stable key identifier. Clients carry a keyring keyed by
/// these. Convention: first 8 bytes of `sha256(verifying_key_bytes)`.
pub type KeyId = [u8; 8];

pub fn key_id(vk: &VerifyingKey) -> KeyId {
    // G6: first 8 bytes of SHA256(vk_bytes). The previous FNV-1a
    // fold (despite a "SplitMix64-style" comment, the magic numbers
    // 0xCBF29CE484222325 / 0x100000001B3 are FNV's) is not collision-
    // resistant — preimages for an 8-byte target can be ground in
    // seconds. Doc comment at line 48 already promised SHA256[..8],
    // implementation now matches. sha2 is already a transitive dep
    // via ed25519_dalek so no new crate.
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(vk.to_bytes());
    let digest = hasher.finalize();
    let mut id = [0u8; 8];
    id.copy_from_slice(&digest[..8]);
    id
}

/// Builds and emits signed companion datagrams. Holds the signing key
/// and the destination list (same destinations as the regular beacon).
pub struct SignedBeaconEmitter {
    key: SigningKey,
    issuer_id: KeyId,
    socket: Arc<UdpSocket>,
    addrs: Vec<SocketAddr>,
}

impl SignedBeaconEmitter {
    pub fn new(key: SigningKey, socket: Arc<UdpSocket>, addrs: Vec<SocketAddr>) -> Self {
        let issuer_id = key_id(&key.verifying_key());
        Self {
            key,
            issuer_id,
            socket,
            addrs,
        }
    }

    pub fn issuer_id(&self) -> KeyId {
        self.issuer_id
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.key.verifying_key()
    }

    /// Emit one companion datagram tied to a regular beacon.
    pub async fn emit(&self, server_ip: u32, server_port: u16, beacon_id: u32) {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let bytes = self.build_packet(server_ip, server_port, beacon_id, ts);
        for addr in &self.addrs {
            let _ = self.socket.send_to(&bytes, addr).await;
        }
    }

    pub(crate) fn build_packet(
        &self,
        server_ip: u32,
        server_port: u16,
        beacon_id: u32,
        ts: u64,
    ) -> Vec<u8> {
        let mut signed = [0u8; 18];
        signed[0..4].copy_from_slice(&server_ip.to_be_bytes());
        signed[4..6].copy_from_slice(&server_port.to_be_bytes());
        signed[6..10].copy_from_slice(&beacon_id.to_be_bytes());
        signed[10..18].copy_from_slice(&ts.to_be_bytes());
        let sig: Signature = self.key.sign(&signed);

        let mut buf = Vec::with_capacity(COMPANION_SIZE);
        // Header (16 bytes, big-endian, mirrors CaHeader layout)
        buf.extend_from_slice(&CA_PROTO_RSRV_BEACON_SIG.to_be_bytes());
        buf.extend_from_slice(&(PAYLOAD_SIZE as u16).to_be_bytes());
        buf.extend_from_slice(&0u16.to_be_bytes()); // data_type
        buf.extend_from_slice(&server_port.to_be_bytes()); // count = port (mirror beacon)
        buf.extend_from_slice(&beacon_id.to_be_bytes()); // cid
        buf.extend_from_slice(&server_ip.to_be_bytes()); // available
        // Payload
        buf.extend_from_slice(&ts.to_be_bytes());
        buf.extend_from_slice(&sig.to_bytes());
        buf.extend_from_slice(&self.issuer_id);
        buf
    }
}

/// Maintains a keyring of trusted issuers and verifies companion
/// datagrams. The verifier is intentionally stateless past keyring
/// lookup; rate-limit / replay-protection is the caller's problem.
#[derive(Default)]
pub struct SignedBeaconVerifier {
    keys: std::collections::HashMap<KeyId, VerifyingKey>,
    /// Maximum age (seconds) of a beacon's `ts` we'll accept. Defaults
    /// to 30 s — long enough for clock skew, short enough to make
    /// replay attacks expensive.
    pub max_age_secs: u64,
}

impl SignedBeaconVerifier {
    pub fn new() -> Self {
        Self {
            keys: Default::default(),
            max_age_secs: 30,
        }
    }

    pub fn trust(&mut self, vk: VerifyingKey) {
        self.keys.insert(key_id(&vk), vk);
    }

    /// Parse and verify a companion datagram. Returns the
    /// `(server_ip, server_port, beacon_id)` tuple on success so the
    /// caller can match it to a regular beacon.
    pub fn verify(&self, packet: &[u8]) -> Result<(u32, u16, u32), VerifyError> {
        if packet.len() != COMPANION_SIZE {
            return Err(VerifyError::WrongSize);
        }
        let cmmd = u16::from_be_bytes([packet[0], packet[1]]);
        if cmmd != CA_PROTO_RSRV_BEACON_SIG {
            return Err(VerifyError::WrongCommand);
        }
        let beacon_id = u32::from_be_bytes(packet[8..12].try_into().unwrap());
        let server_ip = u32::from_be_bytes(packet[12..16].try_into().unwrap());
        let server_port = u16::from_be_bytes(packet[6..8].try_into().unwrap());
        let ts = u64::from_be_bytes(packet[16..24].try_into().unwrap());
        let mut sig_arr = [0u8; 64];
        sig_arr.copy_from_slice(&packet[24..88]);
        let signature = Signature::from_bytes(&sig_arr);
        let mut kid = [0u8; 8];
        kid.copy_from_slice(&packet[88..96]);
        let vk = self.keys.get(&kid).ok_or(VerifyError::UnknownIssuer)?;

        // Reject stale / future-dated signatures.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if ts > now + self.max_age_secs || (now > ts && now - ts > self.max_age_secs) {
            return Err(VerifyError::Stale);
        }

        let mut signed = [0u8; 18];
        signed[0..4].copy_from_slice(&server_ip.to_be_bytes());
        signed[4..6].copy_from_slice(&server_port.to_be_bytes());
        signed[6..10].copy_from_slice(&beacon_id.to_be_bytes());
        signed[10..18].copy_from_slice(&ts.to_be_bytes());
        // verify_strict (not verify): rejects low-order / non-canonical
        // public keys — same Ed25519 signature-malleability hardening
        // applied to cap_token verification. Honest keys are unaffected.
        vk.verify_strict(&signed, &signature)
            .map_err(|_| VerifyError::BadSignature)?;
        Ok((server_ip, server_port, beacon_id))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("companion packet wrong size")]
    WrongSize,
    #[error("not a beacon-signature command")]
    WrongCommand,
    #[error("issuer key id not in keyring")]
    UnknownIssuer,
    #[error("signature timestamp out of window")]
    Stale,
    #[error("Ed25519 signature verification failed")]
    BadSignature,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_core::OsRng;

    fn fresh() -> (SigningKey, SignedBeaconVerifier) {
        let mut csprng = OsRng;
        let key = SigningKey::generate(&mut csprng);
        let mut v = SignedBeaconVerifier::new();
        v.trust(key.verifying_key());
        (key, v)
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn round_trip_valid() {
        let (key, verifier) = fresh();
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let emitter = SignedBeaconEmitter::new(key, socket, vec![]);
        let pkt = emitter.build_packet(0x0a000005, 5064, 42, now());
        assert_eq!(pkt.len(), COMPANION_SIZE);
        let (ip, port, bid) = verifier.verify(&pkt).expect("verifies");
        assert_eq!(ip, 0x0a000005);
        assert_eq!(port, 5064);
        assert_eq!(bid, 42);
    }

    #[tokio::test]
    async fn rejects_tampered_payload() {
        let (key, verifier) = fresh();
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let emitter = SignedBeaconEmitter::new(key, socket, vec![]);
        let mut pkt = emitter.build_packet(0x0a000005, 5064, 42, now());
        // Flip a byte in the server_ip area.
        pkt[12] ^= 0xFF;
        let r = verifier.verify(&pkt);
        assert!(r.is_err(), "tampered packet must fail: {r:?}");
    }

    #[tokio::test]
    async fn rejects_unknown_issuer() {
        let mut csprng = OsRng;
        let key = SigningKey::generate(&mut csprng);
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let emitter = SignedBeaconEmitter::new(key, socket, vec![]);
        let pkt = emitter.build_packet(0x0a000005, 5064, 42, now());
        let verifier = SignedBeaconVerifier::new(); // empty keyring
        assert!(verifier.verify(&pkt).is_err());
    }

    #[tokio::test]
    async fn rejects_stale() {
        let (key, mut verifier) = fresh();
        verifier.max_age_secs = 1;
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let emitter = SignedBeaconEmitter::new(key, socket, vec![]);
        // ts well in the past
        let pkt = emitter.build_packet(0x0a000005, 5064, 42, 0);
        let r = verifier.verify(&pkt);
        assert!(matches!(r, Err(VerifyError::Stale)));
    }
}
