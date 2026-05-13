//! USBTMC driver scaffold (`drvAsynUSBTMC` equivalent).
//!
//! USBTMC (USB Test & Measurement Class) is a USB protocol class
//! used by oscilloscopes, spectrum analysers, signal generators,
//! and most modern bench instruments that expose USB connectivity.
//! C asyn ships `drvAsynUSBTMC` on top of `libusb`.
//!
//! This is a **scaffold** mirroring [`super::ftdi`]: parser is
//! complete and validates spec strings, but `connect()` returns
//! `AsynStatus::Error` until the `usbtmc-hw` Cargo feature lands a
//! real `rusb` / `nusb` binding. This keeps the API surface stable
//! so application code can be feature-gated cleanly.
//!
//! ## Configuration string
//!
//! `"vid=0x0699:pid=0x0408:serial=ABC [interface=0]"` — VID/PID
//! pin the device, optional serial disambiguates multiple
//! identical instruments, optional interface number selects the
//! USB interface (defaults to the first USBTMC-class interface).

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::port::{PortDriver, PortDriverBase, PortFlags};
use crate::user::AsynUser;

/// Configuration parsed from `vid=...:pid=...[:serial=...] [interface=N]`.
#[derive(Debug, Clone)]
pub struct UsbTmcConfig {
    pub vid: u16,
    pub pid: u16,
    pub serial: Option<String>,
    /// USB interface number to claim. `None` selects the first
    /// USBTMC-class interface — the standard case for single-
    /// interface instruments.
    pub interface: Option<u8>,
}

impl UsbTmcConfig {
    pub fn parse(spec: &str) -> AsynResult<Self> {
        let mut vid: Option<u16> = None;
        let mut pid: Option<u16> = None;
        let mut serial: Option<String> = None;
        let mut interface: Option<u8> = None;

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
                    "interface" => interface = v.parse::<u8>().ok(),
                    _ => {
                        return Err(AsynError::Status {
                            status: AsynStatus::Error,
                            message: format!("unknown USBTMC config key '{k}'"),
                        });
                    }
                }
            }
        }

        Ok(Self {
            vid: vid.ok_or_else(|| AsynError::Status {
                status: AsynStatus::Error,
                message: "vid required (e.g. vid=0x0699)".into(),
            })?,
            pid: pid.ok_or_else(|| AsynError::Status {
                status: AsynStatus::Error,
                message: "pid required (e.g. pid=0x0408)".into(),
            })?,
            serial,
            interface,
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

pub struct DrvAsynUsbTmcPort {
    base: PortDriverBase,
    config: UsbTmcConfig,
}

impl DrvAsynUsbTmcPort {
    pub fn new(port_name: &str, spec: &str) -> AsynResult<Self> {
        let config = UsbTmcConfig::parse(spec)?;
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

    pub fn config(&self) -> &UsbTmcConfig {
        &self.config
    }

    pub fn has_hw_support() -> bool {
        cfg!(feature = "usbtmc-hw")
    }
}

impl PortDriver for DrvAsynUsbTmcPort {
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
                    "USBTMC driver scaffold: hardware feature 'usbtmc-hw' not enabled \
                     (vid=0x{:04X}, pid=0x{:04X}). Rebuild with \
                     `--features asyn-rs/usbtmc-hw` once the rusb wiring is added.",
                    self.config.vid, self.config.pid,
                ),
            });
        }
        Err(AsynError::Status {
            status: AsynStatus::Error,
            message: "USBTMC hardware path not yet implemented".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal() {
        let cfg = UsbTmcConfig::parse("vid=0x0699:pid=0x0408").unwrap();
        assert_eq!(cfg.vid, 0x0699);
        assert_eq!(cfg.pid, 0x0408);
        assert!(cfg.serial.is_none());
        assert!(cfg.interface.is_none());
    }

    #[test]
    fn parse_with_serial_and_interface() {
        let cfg =
            UsbTmcConfig::parse("vid=0x0699:pid=0x0408:serial=C012345 interface=2").unwrap();
        assert_eq!(cfg.serial.as_deref(), Some("C012345"));
        assert_eq!(cfg.interface, Some(2));
    }

    #[test]
    fn parse_rejects_missing_required_keys() {
        assert!(UsbTmcConfig::parse("pid=0x0408").is_err());
        assert!(UsbTmcConfig::parse("vid=0x0699").is_err());
    }

    #[test]
    fn parse_rejects_unknown_key() {
        assert!(UsbTmcConfig::parse("vid=0x0699:pid=0x0408:foo=1").is_err());
    }

    #[test]
    fn driver_constructible_no_hw() {
        let drv = DrvAsynUsbTmcPort::new("usb1", "vid=0x0699:pid=0x0408").unwrap();
        assert_eq!(drv.config().vid, 0x0699);
    }

    #[test]
    fn connect_without_feature_errors_clearly() {
        let mut drv = DrvAsynUsbTmcPort::new("usb1", "vid=0x0699:pid=0x0408").unwrap();
        let err = drv.connect(&AsynUser::default()).unwrap_err();
        match err {
            AsynError::Status { message, .. } => {
                assert!(message.contains("usbtmc-hw"));
            }
            _ => panic!("expected Status err"),
        }
    }
}
