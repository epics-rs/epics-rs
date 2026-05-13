//! Prologix GPIB-Ethernet controller driver (asyn PR #129 equivalent).
//!
//! The Prologix GPIB-Ethernet bridge is a small box that exposes
//! GPIB instruments as a TCP socket — text commands prefixed with
//! `++` configure the bridge (address selection, EOI, EOS), plain
//! lines are forwarded to the currently-addressed GPIB instrument.
//!
//! Unlike [`super::ftdi`] / [`super::usbtmc`] this driver has no
//! external-dep gating: it's pure TCP atop [`super::ip_port`]'s
//! existing infrastructure. The wrapper here adds the Prologix-
//! specific addressing protocol so user code writes plain command
//! strings and the driver inserts the `++addr` headers.
//!
//! ## Configuration string
//!
//! `"hostname:1234 [addr=12]"` — TCP host:port for the Prologix
//! bridge plus optional default GPIB address. The address can be
//! changed at runtime via [`DrvAsynPrologixPort::set_gpib_address`]
//! before each instrument access.

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::port::{PortDriver, PortDriverBase, PortFlags};
use crate::user::AsynUser;
use std::sync::Mutex;

/// Configuration parsed from `"host:port [addr=N]"`.
#[derive(Debug, Clone)]
pub struct PrologixConfig {
    pub host: String,
    pub port: u16,
    /// Initial GPIB primary address (0..30). `None` defers selection
    /// until the first [`DrvAsynPrologixPort::set_gpib_address`].
    pub gpib_addr: Option<u8>,
}

impl PrologixConfig {
    pub fn parse(spec: &str) -> AsynResult<Self> {
        let mut tokens = spec.split_whitespace();
        let addr_part = tokens.next().ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "empty Prologix spec".into(),
        })?;
        let (host, port_str) = addr_part.rsplit_once(':').ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("expected host:port, got '{addr_part}'"),
        })?;
        let port: u16 = port_str.parse().map_err(|_| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("invalid port in '{addr_part}'"),
        })?;

        let mut gpib_addr: Option<u8> = None;
        for tok in tokens {
            let (k, v) = tok.split_once('=').ok_or_else(|| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("expected key=value, got '{tok}'"),
            })?;
            match k.to_ascii_lowercase().as_str() {
                "addr" | "gpib_addr" => {
                    let n: u8 = v.parse().map_err(|_| AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!("invalid GPIB address '{v}'"),
                    })?;
                    if n > 30 {
                        return Err(AsynError::Status {
                            status: AsynStatus::Error,
                            message: format!("GPIB address {n} out of range (0..30)"),
                        });
                    }
                    gpib_addr = Some(n);
                }
                _ => {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!("unknown Prologix config key '{k}'"),
                    });
                }
            }
        }

        Ok(Self {
            host: host.to_string(),
            port,
            gpib_addr,
        })
    }
}

pub struct DrvAsynPrologixPort {
    base: PortDriverBase,
    config: PrologixConfig,
    /// Currently-addressed GPIB primary address (0..30) or None
    /// when no address has been selected yet. Mutable for runtime
    /// switching between instruments on the same bridge.
    current_addr: Mutex<Option<u8>>,
}

impl DrvAsynPrologixPort {
    pub fn new(port_name: &str, spec: &str) -> AsynResult<Self> {
        let config = PrologixConfig::parse(spec)?;
        let initial = config.gpib_addr;
        let mut base = PortDriverBase::new(
            port_name,
            32, // 0..30 valid GPIB primary addresses + headroom
            PortFlags {
                multi_device: true,
                can_block: true,
                destructible: true,
            },
        );
        base.connected = false;
        base.auto_connect = true;
        Ok(Self {
            base,
            config,
            current_addr: Mutex::new(initial),
        })
    }

    pub fn config(&self) -> &PrologixConfig {
        &self.config
    }

    /// Select which GPIB instrument the next read/write targets.
    /// Wire-side this becomes a `++addr <N>\n` command sent to the
    /// bridge before the user payload. Validates the range so a
    /// typo doesn't get sent to the bridge.
    pub fn set_gpib_address(&self, addr: u8) -> AsynResult<()> {
        if addr > 30 {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!("GPIB address {addr} out of range (0..30)"),
            });
        }
        *self.current_addr.lock().unwrap() = Some(addr);
        Ok(())
    }

    /// Build the `++addr` prefix line that selects the current
    /// GPIB target. Returns an empty string when no address has been
    /// configured (caller's responsibility — the bridge will use
    /// its last-configured address otherwise).
    pub fn addr_select_line(&self) -> String {
        match *self.current_addr.lock().unwrap() {
            Some(a) => format!("++addr {a}\n"),
            None => String::new(),
        }
    }
}

impl PortDriver for DrvAsynPrologixPort {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
        // Runtime path is the standard `super::ip_port::DrvAsynIPPort`
        // TCP open against `(self.config.host, self.config.port)` —
        // wiring the actual TcpStream is intentionally deferred to
        // a follow-up that integrates with the existing IP port
        // adapter rather than duplicating the connect/read/write
        // plumbing here.
        Err(AsynError::Status {
            status: AsynStatus::Error,
            message: format!(
                "Prologix driver scaffold: TCP connect path delegates to ip_port \
                 (target={}:{}); follow-up will wire the IP port adapter",
                self.config.host, self.config.port,
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal() {
        let cfg = PrologixConfig::parse("192.168.1.10:1234").unwrap();
        assert_eq!(cfg.host, "192.168.1.10");
        assert_eq!(cfg.port, 1234);
        assert!(cfg.gpib_addr.is_none());
    }

    #[test]
    fn parse_with_addr() {
        let cfg = PrologixConfig::parse("prologix.lab:1234 addr=12").unwrap();
        assert_eq!(cfg.host, "prologix.lab");
        assert_eq!(cfg.gpib_addr, Some(12));
    }

    #[test]
    fn parse_rejects_out_of_range_addr() {
        assert!(PrologixConfig::parse("h:1 addr=31").is_err());
    }

    #[test]
    fn parse_rejects_unknown_key() {
        assert!(PrologixConfig::parse("h:1 foo=2").is_err());
    }

    #[test]
    fn set_address_validates_range() {
        let drv = DrvAsynPrologixPort::new("p", "h:1234").unwrap();
        assert!(drv.set_gpib_address(0).is_ok());
        assert!(drv.set_gpib_address(30).is_ok());
        assert!(drv.set_gpib_address(31).is_err());
    }

    #[test]
    fn addr_select_line_formats_correctly() {
        let drv = DrvAsynPrologixPort::new("p", "h:1234").unwrap();
        assert_eq!(drv.addr_select_line(), "");
        drv.set_gpib_address(7).unwrap();
        assert_eq!(drv.addr_select_line(), "++addr 7\n");
    }

    #[test]
    fn initial_addr_from_spec() {
        let drv = DrvAsynPrologixPort::new("p", "h:1234 addr=15").unwrap();
        assert_eq!(drv.addr_select_line(), "++addr 15\n");
    }
}
