//! VXI-11 driver scaffold (`drvVxi11` equivalent).
//!
//! VXI-11 is an ONC RPC protocol for LAN-based test & measurement
//! instruments (oscilloscopes, signal generators, network analysers
//! made before HiSLIP became the standard). C asyn ships `drvVxi11`
//! built on the `oncrpc` library.
//!
//! This is a **scaffold** mirroring [`super::ftdi`] / [`super::usbtmc`]:
//! the configuration string parser is complete and `connect()` fails
//! with a clear "feature not enabled" message until the `vxi11-hw`
//! Cargo feature lands a real ONC RPC stack (likely `onc-rpc` crate
//! or a custom XDR implementation — VXI-11 only needs a few RPCs:
//! `create_link`, `device_write`, `device_read`, `device_clear`,
//! `destroy_link`, plus the abort channel).
//!
//! ## Configuration string
//!
//! `"hostname[:device_name] [lockTimeout=ms] [ioTimeout=ms]"` —
//! `device_name` defaults to `"inst0"` (the IEEE 488.2 standard for
//! a single-instrument bridge); the explicit form is needed for
//! GPIB-Ethernet bridges that expose multiple buses (`gpib0,5`).

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::port::{PortDriver, PortDriverBase, PortFlags};
use crate::user::AsynUser;

#[derive(Debug, Clone)]
pub struct Vxi11Config {
    pub host: String,
    /// VXI-11 logical device name. `"inst0"` is the IEEE 488.2
    /// standard default and matches the C asyn convention.
    pub device_name: String,
    /// Lock acquisition timeout. C asyn calls this `lock_timeout`
    /// and exposes it as a configurable parameter; default 0 means
    /// no-wait (immediate lock or fail).
    pub lock_timeout_ms: u32,
    /// Per-operation I/O timeout (read/write). C asyn default
    /// matches asyn's general `timeout` field, here 2000 ms.
    pub io_timeout_ms: u32,
}

impl Vxi11Config {
    pub fn parse(spec: &str) -> AsynResult<Self> {
        let mut tokens = spec.split_whitespace();
        let host_part = tokens.next().ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "empty VXI-11 spec".into(),
        })?;

        // host[:device_name] — the rightmost colon-delimited segment is the
        // device name if present, otherwise default to inst0.
        let (host, device_name) = if let Some((h, d)) = host_part.split_once(':') {
            (h.to_string(), d.to_string())
        } else {
            (host_part.to_string(), "inst0".to_string())
        };

        let mut lock_timeout_ms: u32 = 0;
        let mut io_timeout_ms: u32 = 2000;
        for tok in tokens {
            let (k, v) = tok.split_once('=').ok_or_else(|| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("expected key=value, got '{tok}'"),
            })?;
            match k.to_ascii_lowercase().as_str() {
                "locktimeout" | "lock_timeout" => {
                    lock_timeout_ms = v.parse().map_err(|_| AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!("invalid lockTimeout '{v}'"),
                    })?;
                }
                "iotimeout" | "io_timeout" => {
                    io_timeout_ms = v.parse().map_err(|_| AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!("invalid ioTimeout '{v}'"),
                    })?;
                }
                _ => {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!("unknown VXI-11 config key '{k}'"),
                    });
                }
            }
        }

        Ok(Self {
            host,
            device_name,
            lock_timeout_ms,
            io_timeout_ms,
        })
    }
}

pub struct DrvAsynVxi11Port {
    base: PortDriverBase,
    config: Vxi11Config,
}

impl DrvAsynVxi11Port {
    pub fn new(port_name: &str, spec: &str) -> AsynResult<Self> {
        let config = Vxi11Config::parse(spec)?;
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

    pub fn config(&self) -> &Vxi11Config {
        &self.config
    }

    pub fn has_hw_support() -> bool {
        cfg!(feature = "vxi11-hw")
    }
}

impl PortDriver for DrvAsynVxi11Port {
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
                    "VXI-11 driver scaffold: ONC RPC feature 'vxi11-hw' not enabled \
                     (host={}, device={}). Rebuild with \
                     `--features asyn-rs/vxi11-hw` once the RPC stack is wired.",
                    self.config.host, self.config.device_name,
                ),
            });
        }
        Err(AsynError::Status {
            status: AsynStatus::Error,
            message: "VXI-11 RPC path not yet implemented".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_defaults_to_inst0() {
        let cfg = Vxi11Config::parse("192.168.1.100").unwrap();
        assert_eq!(cfg.host, "192.168.1.100");
        assert_eq!(cfg.device_name, "inst0");
        assert_eq!(cfg.lock_timeout_ms, 0);
        assert_eq!(cfg.io_timeout_ms, 2000);
    }

    #[test]
    fn parse_with_device_name() {
        let cfg = Vxi11Config::parse("scope.lab:gpib0,5").unwrap();
        assert_eq!(cfg.host, "scope.lab");
        assert_eq!(cfg.device_name, "gpib0,5");
    }

    #[test]
    fn parse_with_timeouts() {
        let cfg = Vxi11Config::parse("h lockTimeout=500 ioTimeout=10000").unwrap();
        assert_eq!(cfg.lock_timeout_ms, 500);
        assert_eq!(cfg.io_timeout_ms, 10000);
    }

    #[test]
    fn parse_rejects_unknown_key() {
        assert!(Vxi11Config::parse("h foo=1").is_err());
    }

    #[test]
    fn driver_constructible_no_hw() {
        let drv = DrvAsynVxi11Port::new("v1", "192.168.0.5").unwrap();
        assert_eq!(drv.config().device_name, "inst0");
    }

    #[test]
    fn connect_without_feature_errors_clearly() {
        let mut drv = DrvAsynVxi11Port::new("v1", "192.168.0.5").unwrap();
        let err = drv.connect(&AsynUser::default()).unwrap_err();
        match err {
            AsynError::Status { message, .. } => {
                assert!(message.contains("vxi11-hw"));
            }
            _ => panic!("expected Status err"),
        }
    }
}
