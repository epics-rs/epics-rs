//! TSIG key material and the BIND key-file parser.
//!
//! Split out of `dns_update` because none of it needs a reactor: it is file
//! text, base64 and two enum variants, while `DnsUpdater` next door awaits a
//! `tokio::net::TcpStream`. Under one gate for both, loading a key file — the
//! thing `softioc-rs` does at startup, before any UPDATE is sent — was absent
//! from every reactor-free build along with the sending.
//!
//! The mapping to `hickory_proto`'s own `TsigAlgorithm` stays in `dns_update`,
//! with the signer that is the only caller.

#![cfg(feature = "discovery-dns-update")]

use base64::Engine;

/// Algorithms supported by the TSIG signer. Mirrors RFC 4635.
#[derive(Debug, Clone, Copy)]
pub enum TsigAlgo {
    HmacSha256,
    HmacSha512,
}

/// TSIG key material loaded from a BIND-format key file or supplied
/// programmatically.
#[derive(Debug, Clone)]
pub struct TsigKey {
    pub name: String,
    pub algorithm: TsigAlgo,
    /// Raw HMAC secret (post base64-decode).
    pub secret: Vec<u8>,
}

impl TsigKey {
    /// Parse a BIND-style key file:
    ///
    /// ```text
    /// key "epics-key" {
    ///     algorithm hmac-sha256;
    ///     secret "x7K2pL...base64...==";
    /// };
    /// ```
    pub fn from_bind_file(path: impl AsRef<std::path::Path>) -> Result<Self, std::io::Error> {
        let content = std::fs::read_to_string(path)?;
        Self::from_bind_str(&content)
    }

    pub fn from_bind_str(s: &str) -> Result<Self, std::io::Error> {
        let mut name: Option<String> = None;
        let mut algorithm: Option<TsigAlgo> = None;
        let mut secret: Option<Vec<u8>> = None;
        for line in s.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("key ") {
                if let Some((quoted, _)) = rest.split_once(' ') {
                    name = Some(quoted.trim_matches('"').to_string());
                }
            } else if let Some(rest) = line.strip_prefix("algorithm ") {
                let v = rest.trim_end_matches(';').trim();
                algorithm = match v {
                    "hmac-sha256" => Some(TsigAlgo::HmacSha256),
                    "hmac-sha512" => Some(TsigAlgo::HmacSha512),
                    _ => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("unsupported TSIG algorithm: {v}"),
                        ));
                    }
                };
            } else if let Some(rest) = line.strip_prefix("secret ") {
                let v = rest.trim_end_matches(';').trim().trim_matches('"');
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(v)
                    .map_err(|e| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("base64 decode of TSIG secret failed: {e}"),
                        )
                    })?;
                secret = Some(bytes);
            }
        }
        let name = name.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "missing 'key' line")
        })?;
        let algorithm = algorithm.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "missing 'algorithm'")
        })?;
        let secret = secret.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "missing 'secret'")
        })?;
        Ok(Self {
            name,
            algorithm,
            secret,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bind_key_file() {
        let content = r#"
            key "epics-key" {
                algorithm hmac-sha256;
                secret "dGVzdC1zZWNyZXQ=";
            };
        "#;
        let key = TsigKey::from_bind_str(content).expect("parse");
        assert_eq!(key.name, "epics-key");
        assert!(matches!(key.algorithm, TsigAlgo::HmacSha256));
        assert_eq!(key.secret, b"test-secret");
    }

    #[test]
    fn parse_bind_key_rejects_bad_algo() {
        let content = r#"
            key "k" { algorithm foo-bar; secret "AAAA"; };
        "#;
        let err = TsigKey::from_bind_str(content).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
