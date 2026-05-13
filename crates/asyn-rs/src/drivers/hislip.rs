//! HiSLIP driver scaffold (asyn Issue #130 equivalent).
//!
//! HiSLIP (High-Speed LAN Instrument Protocol) is the IVI-foundation
//! successor to VXI-11 — same purpose (LAN-attached test &
//! measurement instruments) but a framed TCP protocol with
//! separate sync/async channels, server-initiated service requests,
//! and higher throughput. Default TCP port is 4880.
//!
//! This is a **scaffold** mirroring [`super::vxi11`]: the config
//! parser is complete and `connect()` fails with a clear "feature
//! not enabled" message until the `hislip-hw` Cargo feature lands
//! the real protocol-frame implementation (header parser,
//! sync/async channel split, message-type dispatch — see IVI-6.1
//! HiSLIP spec rev 2.0 §6).
//!
//! ## Configuration string
//!
//! `"hostname[:port] [subaddress=hislip0,N] [maxMessageSize=N]"` —
//! HiSLIP listens on TCP/4880 by default. The `subaddress` selects
//! a logical instrument (default `"hislip0"`); GPIB-bridge gateways
//! use the trailing `,N` notation to address GPIB sub-instruments.

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::port::{PortDriver, PortDriverBase, PortFlags};
use crate::user::AsynUser;

/// Default HiSLIP TCP port assigned by IANA (`hislip-srv`).
pub const HISLIP_DEFAULT_PORT: u16 = 4880;

#[derive(Debug, Clone)]
pub struct HislipConfig {
    pub host: String,
    pub port: u16,
    /// HiSLIP sub-address selecting the logical instrument.
    /// `"hislip0"` is the IVI-6.1 default for a single-instrument
    /// gateway. GPIB-bridge style: `"hislip0,5"`.
    pub subaddress: String,
    /// Maximum HiSLIP message payload size negotiated at connect.
    /// 64 KiB matches the C asyn / NI-VISA default.
    pub max_message_size: u32,
}

impl HislipConfig {
    pub fn parse(spec: &str) -> AsynResult<Self> {
        let mut tokens = spec.split_whitespace();
        let host_part = tokens.next().ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "empty HiSLIP spec".into(),
        })?;

        // host[:port]
        let (host, port) = if let Some((h, p)) = host_part.rsplit_once(':') {
            let port: u16 = p.parse().map_err(|_| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("invalid port '{p}'"),
            })?;
            (h.to_string(), port)
        } else {
            (host_part.to_string(), HISLIP_DEFAULT_PORT)
        };

        let mut subaddress = "hislip0".to_string();
        let mut max_message_size: u32 = 65536;
        for tok in tokens {
            let (k, v) = tok.split_once('=').ok_or_else(|| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("expected key=value, got '{tok}'"),
            })?;
            match k.to_ascii_lowercase().as_str() {
                "subaddress" => subaddress = v.to_string(),
                "maxmessagesize" | "max_message_size" => {
                    max_message_size = v.parse().map_err(|_| AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!("invalid maxMessageSize '{v}'"),
                    })?;
                    if max_message_size < 256 {
                        return Err(AsynError::Status {
                            status: AsynStatus::Error,
                            message: format!(
                                "maxMessageSize {max_message_size} too small (IVI-6.1 §F minimum 256)"
                            ),
                        });
                    }
                }
                _ => {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!("unknown HiSLIP config key '{k}'"),
                    });
                }
            }
        }

        Ok(Self {
            host,
            port,
            subaddress,
            max_message_size,
        })
    }
}

pub struct DrvAsynHislipPort {
    base: PortDriverBase,
    config: HislipConfig,
}

impl DrvAsynHislipPort {
    pub fn new(port_name: &str, spec: &str) -> AsynResult<Self> {
        let config = HislipConfig::parse(spec)?;
        let mut base = PortDriverBase::new(
            port_name,
            1,
            PortFlags {
                multi_device: false,
                can_block: true,
                destructible: true,
            },
        );
        base.connected = false;
        base.auto_connect = true;
        Ok(Self { base, config })
    }

    pub fn config(&self) -> &HislipConfig {
        &self.config
    }

    pub fn has_hw_support() -> bool {
        cfg!(feature = "hislip-hw")
    }
}

impl PortDriver for DrvAsynHislipPort {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
        if !Self::has_hw_support() {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!(
                    "HiSLIP driver scaffold: protocol-frame feature 'hislip-hw' not enabled \
                     (host={}:{}, subaddress={}). Rebuild with \
                     `--features asyn-rs/hislip-hw` once the IVI-6.1 frame parser is wired.",
                    self.config.host, self.config.port, self.config.subaddress,
                ),
            });
        }
        Err(AsynError::Status {
            status: AsynStatus::Error,
            message: "HiSLIP protocol path not yet implemented".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_uses_default_port_and_subaddress() {
        let cfg = HislipConfig::parse("scope.lab").unwrap();
        assert_eq!(cfg.host, "scope.lab");
        assert_eq!(cfg.port, HISLIP_DEFAULT_PORT);
        assert_eq!(cfg.subaddress, "hislip0");
        assert_eq!(cfg.max_message_size, 65536);
    }

    #[test]
    fn parse_with_explicit_port() {
        let cfg = HislipConfig::parse("10.0.0.5:5000").unwrap();
        assert_eq!(cfg.port, 5000);
    }

    #[test]
    fn parse_with_subaddress_and_size() {
        let cfg = HislipConfig::parse("h subaddress=hislip0,7 maxMessageSize=131072").unwrap();
        assert_eq!(cfg.subaddress, "hislip0,7");
        assert_eq!(cfg.max_message_size, 131072);
    }

    #[test]
    fn parse_rejects_below_min_message_size() {
        assert!(HislipConfig::parse("h maxMessageSize=128").is_err());
    }

    #[test]
    fn parse_rejects_unknown_key() {
        assert!(HislipConfig::parse("h foo=1").is_err());
    }

    #[test]
    fn driver_constructible_no_hw() {
        let drv = DrvAsynHislipPort::new("h1", "scope.lab").unwrap();
        assert_eq!(drv.config().port, HISLIP_DEFAULT_PORT);
    }

    #[test]
    fn connect_without_feature_errors_clearly() {
        let mut drv = DrvAsynHislipPort::new("h1", "scope.lab").unwrap();
        let err = drv.connect(&AsynUser::default()).unwrap_err();
        match err {
            AsynError::Status { message, .. } => {
                assert!(message.contains("hislip-hw"));
            }
            _ => panic!("expected Status err"),
        }
    }
}
