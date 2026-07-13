//! VXI-11 (TCP/IP Instrument Protocol) bridge driver — port of
//! `drvVxi11.c`.
//!
//! ## C compatibility
//!
//! The C driver registers iocsh as
//!
//! ```text
//! vxi11Configure(portName, hostName, flags, defTimeoutString,
//!                vxiName, priority, noAutoConnect)
//! ```
//!
//! — seven positional `iocshArg` entries (`drvVxi11.c:1789-1795`).
//! `flags` bitfield (`drvVxi11.c:56-58`):
//!
//! | Bit | Name                     | Effect                        |
//! |-----|--------------------------|-------------------------------|
//! | 0   | `FLAG_RECOVER_WITH_IFC`  | Recover stuck bus via IFC     |
//! | 1   | `FLAG_LOCK_DEVICES`      | Lock device during transactions |
//! | 2   | `FLAG_NO_SRQ`            | Skip SRQ interrupt channel     |
//!
//! `vxiName` selects the VXI-11 link type — typically `"inst0"`
//! (single instrument), `"gpib0"` (GPIB gateway), `"hpib0"` (HP-IB),
//! `"com1"` (serial gateway). The C heuristic at `drvVxi11.c:1754-1757`:
//!
//! ```c
//!     if (strncmp("gpib", vxiName, 4) == 0) isGpibLink = 1;
//!     if (strncmp("hpib", vxiName, 4) == 0) isGpibLink = 1;
//!     if (strncmp("inst", vxiName, 4) == 0) isSingleLink = 1;
//!     if (strncmp("com",  vxiName, 3) == 0) isSingleLink = 1;
//! ```
//!
//! ## VXI-11 RPC programs (`vxi11core.rpcl`)
//!
//! - `DEVICE_CORE` (program `0x0607AF`, version 1) — `create_link`
//!   (10), `device_write` (11), `device_read` (12),
//!   `device_clear` (15), `destroy_link` (23), etc.
//! - `DEVICE_ASYNC` (program `0x0607B0`, version 1) — `device_abort` (1).
//! - `DEVICE_INTR` (program `0x0607B1`) — SRQ delivery channel.
//!
//! ## Hardware feature gate
//!
//! Hardware I/O requires the `vxi11` Cargo feature (which would pull
//! `onc-rpc` or `sunrpc` once a deployment lands). Without it,
//! [`PortDriver::connect`] returns an explanatory error and the
//! driver remains constructible everywhere so iocsh-style config
//! parsing is testable on minimal hosts. Same scaffold convention
//! as [`super::ftdi`] and [`super::usbtmc`].

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::port::{PortDriver, PortDriverBase, PortFlags};
use crate::user::AsynUser;
use std::time::Duration;

// --- iocshArg flags bitfield — `drvVxi11.c:56-58` ---

/// `flags & 0x1` — attempt bus recovery via IFC on stuck transactions.
pub const FLAG_RECOVER_WITH_IFC: i32 = 0x1;
/// `flags & 0x2` — lock the device during read/write transactions.
pub const FLAG_LOCK_DEVICES: i32 = 0x2;
/// `flags & 0x4` — skip the SRQ interrupt channel (no async events).
pub const FLAG_NO_SRQ: i32 = 0x4;

/// Default RPC timeout — `drvVxi11.c:60` (`DEFAULT_RPC_TIMEOUT = 4`
/// seconds).
pub const DEFAULT_RPC_TIMEOUT_SECS: u64 = 4;

// --- RPC program identifiers — `vxi11core.rpcl` ---

/// `DEVICE_CORE` RPC program — `0x0607AF`, version 1. Handles
/// create_link, device_write, device_read, destroy_link, etc.
pub const DEVICE_CORE_PROG: u32 = 0x0006_07AF;
pub const DEVICE_CORE_VERS: u32 = 1;

/// `DEVICE_ASYNC` RPC program — `0x0607B0`, version 1. Handles
/// device_abort.
pub const DEVICE_ASYNC_PROG: u32 = 0x0006_07B0;
pub const DEVICE_ASYNC_VERS: u32 = 1;

/// `DEVICE_INTR` RPC program — `0x0607B1`. SRQ delivery channel.
pub const DEVICE_INTR_PROG: u32 = 0x0006_07B1;
pub const DEVICE_INTR_VERS: u32 = 1;

// --- DEVICE_CORE procedure numbers — `vxi11core.rpcl:155-169` ---

pub const PROC_CREATE_LINK: u32 = 10;
pub const PROC_DEVICE_WRITE: u32 = 11;
pub const PROC_DEVICE_READ: u32 = 12;
pub const PROC_DEVICE_READSTB: u32 = 13;
pub const PROC_DEVICE_TRIGGER: u32 = 14;
pub const PROC_DEVICE_CLEAR: u32 = 15;
pub const PROC_DEVICE_REMOTE: u32 = 16;
pub const PROC_DEVICE_LOCAL: u32 = 17;
pub const PROC_DEVICE_LOCK: u32 = 18;
pub const PROC_DEVICE_UNLOCK: u32 = 19;
pub const PROC_DEVICE_ENABLE_SRQ: u32 = 20;
pub const PROC_DEVICE_DOCMD: u32 = 22;
pub const PROC_DESTROY_LINK: u32 = 23;
pub const PROC_CREATE_INTR_CHAN: u32 = 25;
pub const PROC_DESTROY_INTR_CHAN: u32 = 26;

/// VXI-11 connection topology, derived from the `vxiName` prefix
/// (`drvVxi11.c:1754-1757`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VxiLinkKind {
    /// `inst*` / `com*` — single-link (per-port) device. C `isSingleLink`.
    Single,
    /// `gpib*` / `hpib*` — gateway exposing multiple GPIB addresses.
    /// C `isGpibLink`. Implies `ASYN_MULTIDEVICE`.
    Gpib,
    /// Anything else — fallback: treat as multi-device gateway with no
    /// special GPIB handling.
    Other,
}

impl VxiLinkKind {
    /// Classify from the same vxiName prefix the C driver checks
    /// (case-insensitive).
    pub fn from_vxi_name(name: &str) -> Self {
        let lc = name.to_ascii_lowercase();
        if lc.starts_with("gpib") || lc.starts_with("hpib") {
            VxiLinkKind::Gpib
        } else if lc.starts_with("inst") || lc.starts_with("com") {
            VxiLinkKind::Single
        } else {
            VxiLinkKind::Other
        }
    }
}

/// Parsed config — fields match `vxi11Configure` positional args.
#[derive(Debug, Clone)]
pub struct Vxi11Config {
    pub host_name: String,
    pub flags: i32,
    /// Default RPC timeout in seconds (`drvVxi11.c::defTimeout` parsed
    /// from a string with `epicsStrtod`). Zero / unparseable falls back
    /// to [`DEFAULT_RPC_TIMEOUT_SECS`].
    pub default_timeout: Duration,
    pub vxi_name: String,
    pub priority: u32,
    pub link_kind: VxiLinkKind,
}

impl Vxi11Config {
    pub fn from_positional(
        host_name: &str,
        flags: i32,
        def_timeout_string: &str,
        vxi_name: &str,
        priority: i32,
    ) -> Self {
        // C drvVxi11.c:1745-1747:
        //   if (defTimeoutString) defTimeout = epicsStrtod(...)
        //   pvxiPort->defTimeout = (defTimeout > .0001)
        //       ? defTimeout : (double)DEFAULT_RPC_TIMEOUT;
        let parsed: f64 = def_timeout_string.trim().parse().unwrap_or(0.0);
        let default_timeout = if parsed > 0.0001 {
            Duration::from_secs_f64(parsed)
        } else {
            Duration::from_secs(DEFAULT_RPC_TIMEOUT_SECS)
        };
        Self {
            host_name: host_name.to_string(),
            flags,
            default_timeout,
            vxi_name: vxi_name.to_string(),
            priority: priority.max(0) as u32,
            link_kind: VxiLinkKind::from_vxi_name(vxi_name),
        }
    }

    pub fn recover_with_ifc(&self) -> bool {
        (self.flags & FLAG_RECOVER_WITH_IFC) != 0
    }
    pub fn lock_devices(&self) -> bool {
        (self.flags & FLAG_LOCK_DEVICES) != 0
    }
    /// SRQ is enabled by default; `flags & FLAG_NO_SRQ` disables it
    /// (note the inverted semantics — matches C `!(flags & FLAG_NO_SRQ)`
    /// at `drvVxi11.c:1750`).
    pub fn has_srq(&self) -> bool {
        (self.flags & FLAG_NO_SRQ) == 0
    }
}

/// VXI-11 driver — scaffold matching C iocsh signature.
///
/// Hardware I/O requires the `vxi11` Cargo feature. Without it,
/// [`PortDriver::connect`] returns a feature-not-enabled error. The
/// config parser, link-kind classification, and RPC constants remain
/// available everywhere so iocsh-script unit tests run on minimal
/// hosts.
pub struct DrvVxi11Port {
    base: PortDriverBase,
    config: Vxi11Config,
}

impl DrvVxi11Port {
    /// Configure a VXI-11 port. One-to-one with C
    /// `vxi11Configure(portName, hostName, flags, defTimeoutString,
    /// vxiName, priority, noAutoConnect)` (`drvVxi11.c:1701-1705`).
    ///
    /// `flags` bits: see [`FLAG_RECOVER_WITH_IFC`] (0x1),
    /// [`FLAG_LOCK_DEVICES`] (0x2), [`FLAG_NO_SRQ`] (0x4).
    /// `vxiName` prefix maps to [`VxiLinkKind`].
    /// Gateway-class links (`gpib*` / `hpib*`) enable
    /// `ASYN_MULTIDEVICE` (C `drvVxi11.c:1759-1760`).
    #[allow(clippy::too_many_arguments)] // intentional 1:1 mirror of C iocshArg list
    pub fn configure(
        port_name: &str,
        host_name: &str,
        flags: i32,
        def_timeout_string: &str,
        vxi_name: &str,
        priority: i32,
        no_auto_connect: bool,
    ) -> AsynResult<Self> {
        let config =
            Vxi11Config::from_positional(host_name, flags, def_timeout_string, vxi_name, priority);

        // C `drvVxi11.c:1754-1760`: only `gpib*`/`hpib*` set `isGpibLink`
        // and enable `ASYN_MULTIDEVICE`. An unrecognized prefix leaves
        // both `isGpibLink` and `isSingleLink` clear — it must NOT be
        // registered as a 31-address gateway.
        let multi_device = matches!(config.link_kind, VxiLinkKind::Gpib);
        // GPIB addresses 0..30 (NUM_GPIB_ADDRESSES = 31, drvVxi11.c).
        let max_addr = if multi_device { 31 } else { 1 };

        let mut base = PortDriverBase::new(
            port_name,
            max_addr,
            PortFlags {
                multi_device,
                can_block: true,
                destructible: true,
            },
        );
        base.init_connected(false);
        base.auto_connect = !no_auto_connect;
        Ok(Self { base, config })
    }

    pub fn config(&self) -> &Vxi11Config {
        &self.config
    }

    /// Whether this build was compiled with VXI-11 hardware support.
    pub fn has_hw_support() -> bool {
        cfg!(feature = "vxi11")
    }

    /// The failure every VXI-11 bus operation takes while the RPC transport is
    /// a scaffold: the C method exists and is real (`vxiWriteCmd` →
    /// `device_docmd`), but there is nothing here to send it over.
    fn no_transport(c_method: &str) -> AsynError {
        AsynError::Status {
            status: AsynStatus::Error,
            message: format!("{c_method}: VXI-11 RPC transport not implemented"),
        }
    }
}

impl PortDriver for DrvVxi11Port {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    /// C drvVxi11 takes asynCommon + asynOctet + asynGpib + asynInt32 from
    /// `pasynGpib->registerPort` (drvVxi11.c:1761) and registers asynOption
    /// itself (:1777). No register interface.
    fn capabilities(&self) -> Vec<crate::interfaces::Capability> {
        let mut caps = crate::interfaces::gpib::gpib_port_capabilities();
        caps.push(crate::interfaces::Capability::Option);
        caps
    }

    /// asynInt32 on a GPIB port is asynGpib's SRQ interrupt source, not a
    /// readable register: `read`/`write` are the asynInt32Base defaults and
    /// fail. See [`crate::interfaces::gpib::int32_not_supported`].
    fn read_int32(&mut self, _user: &AsynUser) -> AsynResult<i32> {
        Err(crate::interfaces::gpib::int32_not_supported())
    }

    /// See [`Self::read_int32`].
    fn write_int32(&mut self, _user: &mut AsynUser, _value: i32) -> AsynResult<()> {
        Err(crate::interfaces::gpib::int32_not_supported())
    }

    fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
        if !Self::has_hw_support() {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!(
                    "VXI-11 driver scaffold: hardware feature 'vxi11' not enabled \
                     in this build. Config parsed (host={:?}, vxiName={:?}, \
                     link_kind={:?}, flags=0x{:X}, defTimeout={:?}) — rebuild \
                     with `--features asyn-rs/vxi11` to enable.",
                    self.config.host_name,
                    self.config.vxi_name,
                    self.config.link_kind,
                    self.config.flags,
                    self.config.default_timeout,
                ),
            });
        }
        Err(AsynError::Status {
            status: AsynStatus::Error,
            message: "VXI-11 hardware path not yet implemented".into(),
        })
    }

    // --- asynGpib, C `vxi11` asynGpibPort (drvVxi11.c:170-186) ---
    //
    // All four command methods are real in C: each frames its bytes and hands
    // them to `vxiWriteCmd` (drvVxi11.c:454-469), which sends them over the
    // VXI-11 `device_docmd` RPC with ATN asserted — `vxiUniversalCmd` (:1406),
    // `vxiAddressedCmd` (:1360), `vxiIfc` (:1426), `vxiRen` (:1472).
    //
    // This driver is an iocsh-parity scaffold with no RPC transport (see
    // `connect`), so every bus command fails where the transport is missing,
    // exactly as `connect` does. The interface is still *declared*
    // (`capabilities`): C registers asynGpib for this port, so its GPIBIV is 1,
    // and a UCMD/ACMD must reach the driver and report the driver's own failure
    // rather than take asynRecord's "No asynGpib interface" branch.

    fn gpib_universal_cmd(&mut self, _user: &mut AsynUser, _cmd: u8) -> AsynResult<()> {
        Err(Self::no_transport("vxiUniversalCmd"))
    }

    fn gpib_addressed_cmd(&mut self, _user: &mut AsynUser, _data: &[u8]) -> AsynResult<()> {
        Err(Self::no_transport("vxiAddressedCmd"))
    }

    fn gpib_ifc(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
        Err(Self::no_transport("vxiIfc"))
    }

    fn gpib_ren(&mut self, _user: &mut AsynUser, _enable: bool) -> AsynResult<()> {
        Err(Self::no_transport("vxiRen"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R10-55. `pasynGpib->registerPort` (drvVxi11.c:1761) gives the port
    /// asynCommon + asynOctet + asynGpib + asynInt32; the driver registers
    /// asynOption itself (:1777). The asynInt32 it gets is asynGpib's SRQ
    /// interrupt source with a NULL vtable (asynGpib.c:140), so a read or a
    /// write of it lands in the asynInt32Base defaults and fails.
    #[test]
    fn vxi11_registers_the_gpib_port_interfaces() {
        use crate::interfaces::Capability;

        let drv = DrvVxi11Port::configure("v", "192.0.2.1", 0, "", "gpib0", 0, true).unwrap();
        let caps = drv.capabilities();
        for cap in [
            Capability::Gpib,
            Capability::Int32Read,
            Capability::Int32Write,
            Capability::OctetRead,
            Capability::OctetWrite,
            Capability::Option,
        ] {
            assert!(caps.contains(&cap), "vxi11 must declare {cap:?}");
        }

        let mut drv = drv;
        let mut user = AsynUser::default();
        assert_eq!(
            drv.read_int32(&user).unwrap_err().message(),
            "write is not supported"
        );
        assert_eq!(
            drv.write_int32(&mut user, 1).unwrap_err().message(),
            "write is not supported"
        );
    }

    #[test]
    fn link_kind_recognises_gpib_hpib_inst_com() {
        // C drvVxi11.c:1754-1757 prefixes — case-insensitive.
        assert_eq!(VxiLinkKind::from_vxi_name("gpib0"), VxiLinkKind::Gpib);
        assert_eq!(VxiLinkKind::from_vxi_name("GPIB1"), VxiLinkKind::Gpib);
        assert_eq!(VxiLinkKind::from_vxi_name("hpib0"), VxiLinkKind::Gpib);
        assert_eq!(VxiLinkKind::from_vxi_name("HPIB7"), VxiLinkKind::Gpib);
        assert_eq!(VxiLinkKind::from_vxi_name("inst0"), VxiLinkKind::Single);
        assert_eq!(VxiLinkKind::from_vxi_name("INST7"), VxiLinkKind::Single);
        assert_eq!(VxiLinkKind::from_vxi_name("com1"), VxiLinkKind::Single);
        assert_eq!(VxiLinkKind::from_vxi_name("COM2"), VxiLinkKind::Single);
        assert_eq!(VxiLinkKind::from_vxi_name("foo"), VxiLinkKind::Other);
    }

    #[test]
    fn flag_bits_decode() {
        let cfg = Vxi11Config::from_positional("h", 0, "", "inst0", 0);
        assert!(!cfg.recover_with_ifc());
        assert!(!cfg.lock_devices());
        // Default: SRQ enabled (NO_SRQ not set).
        assert!(cfg.has_srq());

        let cfg = Vxi11Config::from_positional("h", 0x7, "", "inst0", 0);
        assert!(cfg.recover_with_ifc());
        assert!(cfg.lock_devices());
        // FLAG_NO_SRQ set → has_srq() returns false.
        assert!(!cfg.has_srq());
    }

    #[test]
    fn default_timeout_falls_back_when_unparseable() {
        // C drvVxi11.c:1745-1747 — strtod of "" or junk → 0, then the
        // `> 0.0001` guard substitutes DEFAULT_RPC_TIMEOUT.
        let cfg = Vxi11Config::from_positional("h", 0, "", "inst0", 0);
        assert_eq!(cfg.default_timeout, Duration::from_secs(4));

        let cfg = Vxi11Config::from_positional("h", 0, "garbage", "inst0", 0);
        assert_eq!(cfg.default_timeout, Duration::from_secs(4));

        // Sub-threshold (0.0001) also falls back.
        let cfg = Vxi11Config::from_positional("h", 0, "0.00005", "inst0", 0);
        assert_eq!(cfg.default_timeout, Duration::from_secs(4));
    }

    #[test]
    fn default_timeout_honours_user_value() {
        let cfg = Vxi11Config::from_positional("h", 0, "1.5", "inst0", 0);
        assert!((cfg.default_timeout.as_secs_f64() - 1.5).abs() < 1e-9);

        let cfg = Vxi11Config::from_positional("h", 0, "10", "inst0", 0);
        assert_eq!(cfg.default_timeout, Duration::from_secs(10));
    }

    #[test]
    fn gateway_link_is_multi_device() {
        let drv = DrvVxi11Port::configure("vxi0", "10.0.0.1", 0, "", "gpib0", 0, false).unwrap();
        assert!(drv.base().flags.multi_device);
        // C NUM_GPIB_ADDRESSES = 31 → max_addr 31.
        assert_eq!(drv.base().max_addr, 31);
    }

    #[test]
    fn single_link_is_not_multi_device() {
        let drv = DrvVxi11Port::configure("vxi0", "10.0.0.1", 0, "", "inst0", 0, false).unwrap();
        assert!(!drv.base().flags.multi_device);
        assert_eq!(drv.base().max_addr, 1);
    }

    #[test]
    fn unrecognized_link_is_not_multi_device() {
        // C `drvVxi11.c`: an unrecognized vxiName prefix sets neither
        // isGpibLink nor isSingleLink and does NOT enable ASYN_MULTIDEVICE.
        let drv = DrvVxi11Port::configure("vxi0", "10.0.0.1", 0, "", "foo0", 0, false).unwrap();
        assert_eq!(VxiLinkKind::from_vxi_name("foo0"), VxiLinkKind::Other);
        assert!(!drv.base().flags.multi_device);
        assert_eq!(drv.base().max_addr, 1);
    }

    #[test]
    fn no_auto_connect_disables_framework_auto() {
        let drv = DrvVxi11Port::configure("vxi0", "10.0.0.1", 0, "", "inst0", 0, true).unwrap();
        assert!(!drv.base().auto_connect);
        let drv = DrvVxi11Port::configure("vxi0", "10.0.0.1", 0, "", "inst0", 0, false).unwrap();
        assert!(drv.base().auto_connect);
    }

    #[test]
    fn rpc_program_numbers_match_vxi11_spec() {
        // From vxi11core.rpcl: DEVICE_CORE = 0x0607AF, version 1;
        // DEVICE_ASYNC = 0x0607B0, version 1; DEVICE_INTR = 0x0607B1.
        assert_eq!(DEVICE_CORE_PROG, 0x0006_07AF);
        assert_eq!(DEVICE_CORE_VERS, 1);
        assert_eq!(DEVICE_ASYNC_PROG, 0x0006_07B0);
        assert_eq!(DEVICE_INTR_PROG, 0x0006_07B1);
    }

    #[test]
    fn rpc_procedure_numbers_match_vxi11_rpcl() {
        // vxi11core.rpcl:155-169.
        assert_eq!(PROC_CREATE_LINK, 10);
        assert_eq!(PROC_DEVICE_WRITE, 11);
        assert_eq!(PROC_DEVICE_READ, 12);
        assert_eq!(PROC_DEVICE_CLEAR, 15);
        assert_eq!(PROC_DESTROY_LINK, 23);
    }

    // Only meaningful in a build without the hardware feature — with
    // `vxi11` enabled `connect()` reaches the (unimplemented) HW path.
    #[cfg(not(feature = "vxi11"))]
    #[test]
    fn connect_without_hw_feature_reports_error() {
        let mut drv =
            DrvVxi11Port::configure("vxi0", "10.0.0.1", 0, "", "inst0", 0, false).unwrap();
        let err = drv.connect(&AsynUser::default()).unwrap_err();
        match err {
            AsynError::Status { message, .. } => {
                assert!(message.contains("vxi11"), "must mention feature: {message}");
            }
            _ => panic!("expected Status error"),
        }
    }

    #[test]
    fn has_hw_support_matches_feature_flag() {
        assert_eq!(DrvVxi11Port::has_hw_support(), cfg!(feature = "vxi11"));
    }
}
