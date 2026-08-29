//! FTDI MPSSE / serial bridge driver — port of `drvAsynFTDIPort.cpp`.
//!
//! ## C compatibility
//!
//! The C driver registers the iocsh function
//!
//! ```text
//! drvAsynFTDIPortConfigure(portName, vendor, product, baudrate,
//!                          latency, priority, noAutoConnect,
//!                          noProcessEos, mode)
//! ```
//!
//! with nine positional `iocshArg` entries (`drvAsynFTDIPort.cpp:641-655`).
//! `mode` is a bit-field: bit 0 (`UART_SPI_BIT = 0x01`) selects SPI
//! when set, UART when clear (`drvAsynFTDIPort.cpp:520`,
//! `drvAsynFTDIPort.h:11`). `latency` is clamped to `1..=255`
//! (`drvAsynFTDIPort.cpp:535-538`). When `noProcessEos` is zero the C
//! driver auto-installs `asynInterposeEos`
//! (`drvAsynFTDIPort.cpp:622-623`).
//!
//! This module mirrors that contract via [`DrvAsynFtdiPort::configure`].
//! Hardware I/O is gated behind the optional `ftdi-mpsse` Cargo
//! feature; without it `connect()` returns an error explaining the
//! feature must be enabled, but configuration validation runs
//! everywhere so iocsh-style startup scripts can be unit-tested
//! without `libusb`.

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::port::{PortDriver, PortDriverBase, PortFlags};
use crate::user::AsynUser;

/// `UART_SPI_BIT` — bit-0 of the `mode` argument. Matches
/// `drvAsynFTDIPort.h:11`.
pub const UART_SPI_BIT: i32 = 0x01;

/// Effective bit-mode derived from the C `mode` integer.
///
/// C asyn defines only two modes — UART (bit 0 clear) and SPI (bit 0
/// set) — even though `mode` is passed as an `int` for future
/// expansion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FtdiBitMode {
    /// UART — `mode & UART_SPI_BIT == 0`.
    #[default]
    Uart,
    /// SPI / MPSSE — `mode & UART_SPI_BIT == 1`.
    Spi,
}

impl FtdiBitMode {
    /// Decode the C `mode` argument the same way `drvAsynFTDIPort.cpp:520`
    /// does.
    pub fn from_c_mode(mode: i32) -> Self {
        if (mode & UART_SPI_BIT) == UART_SPI_BIT {
            FtdiBitMode::Spi
        } else {
            FtdiBitMode::Uart
        }
    }
}

/// Parsed configuration matching the C `ftdiController_t` config
/// fields (`drvAsynFTDIPort.cpp:568-572`).
#[derive(Debug, Clone)]
pub struct FtdiConfig {
    pub vendor: i32,
    pub product: i32,
    pub baudrate: i32,
    /// 1..=255 (clamped on construction).
    pub latency: i32,
    pub bitmode: FtdiBitMode,
    /// Raw `mode` integer kept for diagnostic logging.
    pub mode_raw: i32,
}

impl FtdiConfig {
    /// Build a config from the same positional arguments the C
    /// `drvAsynFTDIPortConfigure` accepts. Latency is clamped to
    /// `1..=255` to match `drvAsynFTDIPort.cpp:535-538`.
    pub fn from_positional(
        vendor: i32,
        product: i32,
        baudrate: i32,
        latency: i32,
        mode: i32,
    ) -> Self {
        let latency = latency.clamp(1, 255);
        Self {
            vendor,
            product,
            baudrate,
            latency,
            bitmode: FtdiBitMode::from_c_mode(mode),
            mode_raw: mode,
        }
    }
}

/// FTDI driver — scaffold matching C iocsh signature.
///
/// Hardware I/O requires the `ftdi-mpsse` Cargo feature. Without it,
/// [`PortDriver::connect`] returns an error so application code
/// surfaces the missing feature immediately. The driver remains
/// constructible everywhere so iocsh startup scripts parse and
/// validate identically on minimal hosts.
pub struct DrvAsynFtdiPort {
    base: PortDriverBase,
    config: FtdiConfig,
    priority: u32,
    no_process_eos: bool,
}

impl DrvAsynFtdiPort {
    /// Configure an FTDI port. One-to-one with C
    /// `drvAsynFTDIPortConfigure(portName, vendor, product, baudrate,
    /// latency, priority, noAutoConnect, noProcessEos, mode)`
    /// (`drvAsynFTDIPort.cpp:509-518`).
    ///
    /// * `latency` is clamped to `1..=255`.
    /// * `no_auto_connect` defers framework-driven connect (C
    ///   `noAutoConnect`).
    /// * `no_process_eos` suppresses the automatic EOS interpose
    ///   install (C `noProcessEos`). Mirror of
    ///   `drvAsynFTDIPort.cpp:622-623`: when zero, `configure` installs an
    ///   `EosInterpose` (empty terminator until `setInputEos`/`OEOS`), the
    ///   Rust octet-stack equivalent of C's `asynInterposeEosConfig`.
    #[allow(clippy::too_many_arguments)] // intentional 1:1 mirror of C iocshArg list
    pub fn configure(
        port_name: &str,
        vendor: i32,
        product: i32,
        baudrate: i32,
        latency: i32,
        priority: u32,
        no_auto_connect: bool,
        no_process_eos: bool,
        mode: i32,
    ) -> AsynResult<Self> {
        let config = FtdiConfig::from_positional(vendor, product, baudrate, latency, mode);
        let mut base = PortDriverBase::new(
            port_name,
            1,
            PortFlags {
                multi_device: false,
                can_block: true,
                ..PortFlags::default()
            },
        );
        base.init_connected(false);
        base.auto_connect = !no_auto_connect;
        // C passes `interruptProcess = 1` to `pasynOctetBase->initialize`
        // (drvAsynFTDIPort.cpp:616): every successful octet read on this port fans out to
        // its octet interrupt users, which is what drives a SCAN="I/O Intr"
        // record. See `PortDriverBase::octet_interrupt_process`.
        base.octet_interrupt_process = true;
        // C drvAsynFTDIPort.cpp:622-623: install asynInterposeEos by default
        // (empty terminator until setInputEos/OEOS), suppressed by
        // noProcessEos.
        if !no_process_eos {
            base.install_octet_interpose(Box::new(crate::interpose::eos::EosInterpose::default()));
        }
        Ok(Self {
            base,
            config,
            priority,
            no_process_eos,
        })
    }

    /// Inspect the parsed config.
    pub fn config(&self) -> &FtdiConfig {
        &self.config
    }

    /// Configured scheduling priority (C `unsigned int priority`).
    pub fn priority(&self) -> u32 {
        self.priority
    }

    /// Whether EOS interpose auto-install was suppressed.
    pub fn no_process_eos(&self) -> bool {
        self.no_process_eos
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

    /// C drvAsynFTDIPort registers asynCommon, asynOption and asynOctet
    /// (drvAsynFTDIPort.cpp:579-613).
    fn capabilities(&self) -> Vec<crate::interfaces::Capability> {
        crate::interfaces::octet_transport_capabilities()
    }

    fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
        if !Self::has_hw_support() {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!(
                    "FTDI driver scaffold: hardware feature 'ftdi-mpsse' not enabled \
                     in this build. Config parsed (vid=0x{:04X}, pid=0x{:04X}, \
                     baudrate={}, latency={}, mode={:?}) — rebuild with \
                     `--features asyn-rs/ftdi-mpsse` to enable.",
                    self.config.vendor as u16,
                    self.config.product as u16,
                    self.config.baudrate,
                    self.config.latency,
                    self.config.bitmode,
                ),
            });
        }
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
    fn mode_bit0_clear_selects_uart() {
        assert_eq!(FtdiBitMode::from_c_mode(0), FtdiBitMode::Uart);
        assert_eq!(FtdiBitMode::from_c_mode(0x10), FtdiBitMode::Uart);
    }

    #[test]
    fn mode_bit0_set_selects_spi() {
        assert_eq!(FtdiBitMode::from_c_mode(1), FtdiBitMode::Spi);
        assert_eq!(FtdiBitMode::from_c_mode(0x11), FtdiBitMode::Spi);
    }

    #[test]
    fn latency_clamped_low() {
        let cfg = FtdiConfig::from_positional(0x0403, 0x6014, 115_200, 0, 0);
        assert_eq!(cfg.latency, 1);
    }

    #[test]
    fn latency_clamped_high() {
        let cfg = FtdiConfig::from_positional(0x0403, 0x6014, 115_200, 999, 0);
        assert_eq!(cfg.latency, 255);
    }

    #[test]
    fn latency_passthrough_in_range() {
        let cfg = FtdiConfig::from_positional(0x0403, 0x6014, 115_200, 16, 0);
        assert_eq!(cfg.latency, 16);
    }

    #[test]
    fn configure_records_all_positional_fields() {
        let drv =
            DrvAsynFtdiPort::configure("ftdi0", 0x0403, 0x6014, 921_600, 4, 50, true, false, 1)
                .unwrap();
        assert_eq!(drv.config().vendor, 0x0403);
        assert_eq!(drv.config().product, 0x6014);
        assert_eq!(drv.config().baudrate, 921_600);
        assert_eq!(drv.config().latency, 4);
        assert_eq!(drv.config().bitmode, FtdiBitMode::Spi);
        assert_eq!(drv.config().mode_raw, 1);
        assert_eq!(drv.priority(), 50);
        assert!(!drv.no_process_eos());
        // no_auto_connect=true → base.auto_connect should be false
        assert!(!drv.base().auto_connect);
        // no_process_eos=false → EOS interpose auto-installed (DRV-2 family,
        // C drvAsynFTDIPort.cpp:622-623).
        assert_eq!(drv.base().interpose_octet.len(), 1);
    }

    #[test]
    fn configure_no_process_eos_suppresses_eos_interpose() {
        let drv =
            DrvAsynFtdiPort::configure("ftdi0", 0x0403, 0x6014, 115_200, 16, 0, false, true, 0)
                .unwrap();
        assert!(drv.no_process_eos());
        assert_eq!(
            drv.base().interpose_octet.len(),
            0,
            "noProcessEos must suppress the EOS interpose"
        );
    }

    #[test]
    fn configure_no_auto_connect_false_enables_auto() {
        let drv =
            DrvAsynFtdiPort::configure("ftdi0", 0x0403, 0x6014, 115_200, 16, 0, false, false, 0)
                .unwrap();
        assert!(drv.base().auto_connect);
    }

    // Only meaningful in a build without the hardware feature — with
    // `ftdi-mpsse` enabled `connect()` reaches the (unimplemented) HW path.
    #[cfg(not(feature = "ftdi-mpsse"))]
    #[test]
    fn connect_without_hw_feature_reports_error() {
        let mut drv =
            DrvAsynFtdiPort::configure("ftdi0", 0x0403, 0x6014, 115_200, 16, 0, false, false, 0)
                .unwrap();
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
