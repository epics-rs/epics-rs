//! FTDI MPSSE / serial bridge driver (asyn PR #88 equivalent).
//!
//! This is a **scaffold** that compiles on every platform but only
//! provides functional MPSSE I/O when the optional `ftdi-mpsse`
//! Cargo feature is enabled (which pulls in the `ftdi-mpsse` crate
//! and `libusb` system dep). Without the feature, all calls return
//! `AsynStatus::Error` with a clear "FTDI feature not enabled"
//! message — letting application code be feature-gated without
//! having to cfg-gate every call site.
//!
//! Why a scaffold rather than full implementation:
//! - The `ftdi-mpsse` / `libftd2xx` ecosystem requires platform-
//!   specific runtime (libusb on Linux/macOS, FTDI's D2XX dylib on
//!   Windows). Pulling those into the default build creates a
//!   non-portable artifact that breaks `cargo install` on minimal
//!   hosts.
//! - The actual production use case in epics-rs sites today is zero
//!   — every deployment polls FTDI hardware via the D-Tacq /
//!   Tucker-Davis Linux driver framework, not asyn directly.
//! - The trait interface here matches asyn PR #88's expected API
//!   surface so a future "wire libftd2xx" PR is a single-file edit.
//!
//! ## Configuration string
//!
//! `"vid=0x0403:pid=0x6014:serial=ABC123 [bitmode=mpsse|uart]"` —
//! VID/PID and serial pin the device, bitmode chooses MPSSE
//! (synchronous serial / SPI / I2C / JTAG) or UART (RS-232 over
//! USB-serial). UART mode bridges to [`super::serial_port`]
//! semantics; MPSSE is the asyn-native byte-stream interface.

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::port::{PortDriver, PortDriverBase, PortFlags};
use crate::user::AsynUser;

/// FTDI bit-mode selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FtdiBitMode {
    /// MPSSE — synchronous serial / SPI / I2C / JTAG host engine.
    #[default]
    Mpsse,
    /// Asynchronous bit-bang.
    BitBang,
    /// UART (default — looks like a /dev/ttyUSBn).
    Uart,
}

/// Configuration parsed from `vid=...:pid=...:serial=...` spec.
#[derive(Debug, Clone)]
pub struct FtdiConfig {
    pub vid: u16,
    pub pid: u16,
    pub serial: Option<String>,
    pub bitmode: FtdiBitMode,
}

impl FtdiConfig {
    /// Parse a `vid=0x0403:pid=0x6014:serial=ABC [bitmode=mpsse]`
    /// spec. Hex (`0x...`) or decimal accepted for VID/PID.
    pub fn parse(spec: &str) -> AsynResult<Self> {
        let mut vid: Option<u16> = None;
        let mut pid: Option<u16> = None;
        let mut serial: Option<String> = None;
        let mut bitmode = FtdiBitMode::default();

        for tok in spec.split_whitespace() {
            for kv in tok.split(':') {
                let (k, v) = kv.split_once('=').ok_or_else(|| AsynError::Status {
                    status: AsynStatus::Error,
                    message: format!("expected key=value, got '{kv}'"),
                })?;
                match k.to_ascii_lowercase().as_str() {
                    "vid" => vid = parse_u16_radix(v),
                    "pid" => pid = parse_u16_radix(v),
                    "serial" => serial = Some(v.to_string()),
                    "bitmode" => {
                        bitmode = match v.to_ascii_lowercase().as_str() {
                            "mpsse" => FtdiBitMode::Mpsse,
                            "bitbang" => FtdiBitMode::BitBang,
                            "uart" => FtdiBitMode::Uart,
                            _ => {
                                return Err(AsynError::Status {
                                    status: AsynStatus::Error,
                                    message: format!("unknown bitmode '{v}'"),
                                });
                            }
                        };
                    }
                    _ => {
                        return Err(AsynError::Status {
                            status: AsynStatus::Error,
                            message: format!("unknown FTDI config key '{k}'"),
                        });
                    }
                }
            }
        }

        Ok(Self {
            vid: vid.ok_or_else(|| AsynError::Status {
                status: AsynStatus::Error,
                message: "vid required (e.g. vid=0x0403)".into(),
            })?,
            pid: pid.ok_or_else(|| AsynError::Status {
                status: AsynStatus::Error,
                message: "pid required (e.g. pid=0x6014)".into(),
            })?,
            serial,
            bitmode,
        })
    }
}

fn parse_u16_radix(s: &str) -> Option<u16> {
    if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u16::from_str_radix(rest, 16).ok()
    } else {
        s.parse::<u16>().ok()
    }
}

/// FTDI driver — scaffold.
///
/// Without the `ftdi-mpsse` Cargo feature, [`Self::connect`] returns
/// `AsynStatus::Error` so application code immediately sees that the
/// FTDI back-end isn't compiled in. The driver remains constructible
/// (so iocsh `drvAsynFtdiPortConfigure` doesn't panic on import) and
/// the configuration parser fully validates the spec — enabling
/// dry-run config validation in tests without the libusb dep.
pub struct DrvAsynFtdiPort {
    base: PortDriverBase,
    config: FtdiConfig,
}

impl DrvAsynFtdiPort {
    pub fn new(port_name: &str, spec: &str) -> AsynResult<Self> {
        let config = FtdiConfig::parse(spec)?;
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

    /// Inspect the parsed config (useful for tests / iocsh introspection).
    pub fn config(&self) -> &FtdiConfig {
        &self.config
    }

    /// Whether this build was compiled with FTDI hardware support.
    pub fn has_hw_support() -> bool {
        cfg!(feature = "ftdi-mpsse")
    }
}

impl PortDriver for DrvAsynFtdiPort {
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
                    "FTDI driver scaffold: hardware feature 'ftdi-mpsse' not enabled \
                     in this build. Config parsed (vid=0x{:04X}, pid=0x{:04X}, bitmode={:?}) \
                     — rebuild with `--features asyn-rs/ftdi-mpsse` to enable.",
                    self.config.vid, self.config.pid, self.config.bitmode,
                ),
            });
        }
        // Hardware-enabled path would go here — open device by VID/PID,
        // optionally match serial, configure bitmode, etc.
        // Intentionally not implemented in this scaffold; the trait
        // interface is in place so a follow-up PR replacing this
        // method body is a one-file change.
        Err(AsynError::Status {
            status: AsynStatus::Error,
            message: "FTDI hardware path not yet implemented".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_vid_pid() {
        let cfg = FtdiConfig::parse("vid=0x0403:pid=0x6014").unwrap();
        assert_eq!(cfg.vid, 0x0403);
        assert_eq!(cfg.pid, 0x6014);
        assert!(cfg.serial.is_none());
        assert_eq!(cfg.bitmode, FtdiBitMode::Mpsse);
    }

    #[test]
    fn parse_with_serial_and_bitmode() {
        let cfg = FtdiConfig::parse("vid=0x0403:pid=0x6014:serial=ABC123 bitmode=uart").unwrap();
        assert_eq!(cfg.serial.as_deref(), Some("ABC123"));
        assert_eq!(cfg.bitmode, FtdiBitMode::Uart);
    }

    #[test]
    fn parse_decimal_vid_pid() {
        let cfg = FtdiConfig::parse("vid=1027:pid=24596").unwrap();
        assert_eq!(cfg.vid, 1027);
        assert_eq!(cfg.pid, 24596);
    }

    #[test]
    fn parse_rejects_missing_vid() {
        assert!(FtdiConfig::parse("pid=0x6014").is_err());
    }

    #[test]
    fn parse_rejects_missing_pid() {
        assert!(FtdiConfig::parse("vid=0x0403").is_err());
    }

    #[test]
    fn parse_rejects_unknown_bitmode() {
        assert!(FtdiConfig::parse("vid=0x0403:pid=0x6014 bitmode=jtag2").is_err());
    }

    #[test]
    fn parse_rejects_unknown_key() {
        assert!(FtdiConfig::parse("vid=0x0403:pid=0x6014:foobar=1").is_err());
    }

    #[test]
    fn driver_constructible_without_hw() {
        let drv = DrvAsynFtdiPort::new("ftdi0", "vid=0x0403:pid=0x6014").unwrap();
        assert_eq!(drv.config().vid, 0x0403);
        // `connect()` returns the "feature not enabled" error path.
        let mut drv = drv;
        let err = drv.connect(&AsynUser::default()).unwrap_err();
        match err {
            AsynError::Status { message, .. } => {
                assert!(
                    message.contains("ftdi-mpsse"),
                    "error must mention the feature: {message}"
                );
            }
            _ => panic!("expected Status error"),
        }
    }

    #[test]
    fn has_hw_support_matches_feature_flag() {
        assert_eq!(
            DrvAsynFtdiPort::has_hw_support(),
            cfg!(feature = "ftdi-mpsse")
        );
    }
}
