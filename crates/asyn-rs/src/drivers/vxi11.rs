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
//! `"com1"` (serial gateway). The C heuristic at `drvVxi11.c:1754-1760`,
//! quoted through the line that consumes it:
//!
//! ```c
//!     if(epicsStrnCaseCmp("gpib", vxiName, 4) == 0) pvxiPort->isGpibLink = 1;
//!     if(epicsStrnCaseCmp("hpib", vxiName, 4) == 0) pvxiPort->isGpibLink = 1;
//!     if(epicsStrnCaseCmp("inst", vxiName, 4) == 0) pvxiPort->isSingleLink = 1;
//!     if(epicsStrnCaseCmp("com",  vxiName, 3) == 0) pvxiPort->isSingleLink = 1;
//!     attributes = ASYN_CANBLOCK;
//!     if(!pvxiPort->isSingleLink) attributes |= ASYN_MULTIDEVICE;
//! ```
//!
//! `isGpibLink` never reaches the attribute test: multi-device is the
//! default and only `inst*` / `com*` opt out, so an unrecognised name
//! (`hislip0`, `vxi0`, `TCPIP0`, or the empty string older st.cmd files
//! pass) registers multi-device. Stopping the quote at the four setters is
//! what made the port read the gate off the GPIB arm.
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
use std::collections::BTreeMap;
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

/// `device_read` flag bit 7 — stop the transfer on the termination character
/// carried in the request's `termChar` field (`vxi11.h:46`). C sets it, and
/// `termChar`, only while the device link holds an EOS
/// (`drvVxi11.c:1169-1173`).
pub const VXI_TERMCHRSET: u32 = 128;

/// VXI-11 connection topology, derived from the `vxiName` prefix
/// (`drvVxi11.c:1754-1757`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VxiLinkKind {
    /// `inst*` / `com*` — single-link (per-port) device. C `isSingleLink`.
    Single,
    /// `gpib*` / `hpib*` — gateway exposing multiple GPIB addresses.
    /// C `isGpibLink`, which selects the GPIB handling only; the
    /// `ASYN_MULTIDEVICE` attribute comes from not being `Single`.
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
    /// C `pdevLink->eos` (`drvVxi11.c:65`), one terminator per device link
    /// rather than per port — `:1737-1741` initialises the server link and
    /// every primary and secondary address to -1 independently. An absent
    /// entry is that -1.
    ///
    /// The port owns this instead of `PortDriverBase`'s EOS cache because a
    /// VXI-11 port has no software EOS interpose to hand it to: `asynGpib`
    /// registers the octet interface with `initialize(portName, &octet, 0, 0,
    /// 0)` (`asynGpib.c:601`), and the terminator is instead stamped into the
    /// `device_read` request (`drvVxi11.c:1169-1173`).
    eos: BTreeMap<i32, u8>,
}

impl DrvVxi11Port {
    /// Configure a VXI-11 port. One-to-one with C
    /// `vxi11Configure(portName, hostName, flags, defTimeoutString,
    /// vxiName, priority, noAutoConnect)` (`drvVxi11.c:1701-1705`).
    ///
    /// `flags` bits: see [`FLAG_RECOVER_WITH_IFC`] (0x1),
    /// [`FLAG_LOCK_DEVICES`] (0x2), [`FLAG_NO_SRQ`] (0x4).
    /// `vxiName` prefix maps to [`VxiLinkKind`]. `ASYN_MULTIDEVICE` is the
    /// default: only a single-link `inst*` / `com*` name opts out
    /// (C `drvVxi11.c:1756-1760`).
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

        // C `drvVxi11.c:1754-1760` classifies the prefix and then makes
        // MULTIDEVICE the DEFAULT — only a single link opts out:
        //
        //     if(epicsStrnCaseCmp("gpib", vxiName, 4) == 0) isGpibLink   = 1;
        //     if(epicsStrnCaseCmp("hpib", vxiName, 4) == 0) isGpibLink   = 1;
        //     if(epicsStrnCaseCmp("inst", vxiName, 4) == 0) isSingleLink = 1;
        //     if(epicsStrnCaseCmp("com",  vxiName, 3) == 0) isSingleLink = 1;
        //     if(!pvxiPort->isSingleLink) attributes |= ASYN_MULTIDEVICE;
        //
        // `isGpibLink` never enters that test, so an unrecognized — or EMPTY —
        // `vxiName` leaves `isSingleLink` clear and IS a 32-address gateway.
        // The empty name is the common older-`st.cmd` form
        // `vxi11Configure("L0","host",0,"1.0","",0,0)`; deriving the flag from
        // the GPIB arm instead capped that port at address 0 and lost every
        // device above it.
        //
        // Phrase it as the single-link test negated, not as a list of the
        // multi-device kinds, so any link kind this port learns to recognise
        // later is multi-device by construction, as it is in C.
        let single_link = matches!(config.link_kind, VxiLinkKind::Single);
        let multi_device = !single_link;
        // GPIB addresses 0..31 (NUM_GPIB_ADDRESSES = 32, asynGpibDriver.h:37;
        // drvVxi11.c uses it, e.g. the `addr < NUM_GPIB_ADDRESSES` scan loops
        // at :1016 and :1738, but does not define it).
        let max_addr = if multi_device { 32 } else { 1 };

        let mut base = PortDriverBase::new(
            port_name,
            max_addr,
            PortFlags {
                multi_device,
                can_block: true,
                ..PortFlags::default()
            },
        );
        base.init_connected(false);
        base.auto_connect = !no_auto_connect;
        Ok(Self {
            base,
            config,
            eos: BTreeMap::new(),
        })
    }

    pub fn config(&self) -> &Vxi11Config {
        &self.config
    }

    /// The terminator this address's `device_read` must carry, C
    /// `pdevLink->eos != -1` at `drvVxi11.c:1170`. `None` is C's -1: the
    /// request goes out with `flags = 0` and no `termChar`.
    pub fn input_eos_for(&self, addr: i32) -> Option<u8> {
        self.eos.get(&addr).copied()
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
    /// fail. See [`crate::interfaces::gpib::int32_read_not_supported`], which
    /// documents why the read reports the READ (CBUG-B10 — C said "write"
    /// until asyn #237; it says "read" too now).
    fn read_int32(&mut self, _user: &AsynUser) -> AsynResult<i32> {
        Err(crate::interfaces::gpib::int32_read_not_supported())
    }

    /// See [`Self::read_int32`].
    fn write_int32(&mut self, _user: &mut AsynUser, _value: i32) -> AsynResult<()> {
        Err(crate::interfaces::gpib::int32_write_not_supported())
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

    /// C `vxiSetEos` (`drvVxi11.c:1302-1329`), which resolves the address
    /// through `vxiGetDevLink` and stores the terminator on that link. The
    /// `PortDriver` default cannot stand in for it: it caches into
    /// `base.eos_entry` and forwards to the interpose stack, and a GPIB port
    /// has none — `asynGpib.c:601` registers the octet interface with
    /// `processEosIn = 0` because the terminator belongs in the `device_read`
    /// request. Its two-byte allowance is wrong here too; `vxiSetEos`'s
    /// `default:` arm and `asynGpib::setInputEos` (`asynGpib.c:441-446`) both
    /// refuse anything past one character.
    fn set_input_eos(&mut self, user: &AsynUser, eos: &[u8]) -> AsynResult<()> {
        match eos.len() {
            0 => {
                self.eos.remove(&user.addr);
                Ok(())
            }
            1 => {
                self.eos.insert(user.addr, eos[0]);
                Ok(())
            }
            n => Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!("{} vxiSetEos illegal eoslen {n}", self.base.port_name),
            }),
        }
    }

    /// C `vxiGetEos` (`drvVxi11.c:1331-1358`): `eos == -1` reports length 0,
    /// otherwise the one byte the link holds. Same field `set_input_eos`
    /// writes and the `device_read` request will read.
    fn get_input_eos(&self, user: &AsynUser) -> Vec<u8> {
        match self.input_eos_for(user.addr) {
            Some(c) => vec![c],
            None => Vec::new(),
        }
    }

    /// C parity: `asynGpib`'s octet vtable leaves `setOutputEos`/`getOutputEos`
    /// NULL (`asynGpib.c:132` — `...setInputEos, getInputEos, 0, 0`), so a GPIB
    /// port has no output terminator to set. Refuse rather than cache bytes
    /// nothing will ever append, exactly as the sibling GPIB port does
    /// (`super::prologix`).
    fn set_output_eos(&mut self, _user: &AsynUser, _eos: &[u8]) -> AsynResult<()> {
        Err(AsynError::Status {
            status: AsynStatus::Error,
            message: "output EOS not supported on a GPIB port".into(),
        })
    }

    fn get_output_eos(&self, _user: &AsynUser) -> Vec<u8> {
        Vec::new()
    }

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
        // CBUG-B10: C's asynInt32Base `readDefault` reported "write is not
        // supported" (a copy-paste from `writeDefault`) until asyn #237
        // merged; the read path here named the read first, and now agrees.
        assert_eq!(
            drv.read_int32(&user).unwrap_err().message(),
            "read is not supported"
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
        // C NUM_GPIB_ADDRESSES = 32 (asynGpibDriver.h:37) → max_addr 32.
        assert_eq!(drv.base().max_addr, 32);
    }

    #[test]
    fn single_link_is_not_multi_device() {
        let drv = DrvVxi11Port::configure("vxi0", "10.0.0.1", 0, "", "inst0", 0, false).unwrap();
        assert!(!drv.base().flags.multi_device);
        assert_eq!(drv.base().max_addr, 1);
    }

    /// One case per branch of C's rule (`drvVxi11.c:1754-1760`): the two
    /// `isGpibLink` prefixes, the two `isSingleLink` prefixes, an unrecognized
    /// name, and the empty name that older `st.cmd` files pass. MULTIDEVICE is
    /// the default and only `inst*`/`com*` opt out, so four of the six are
    /// 32-address gateways.
    #[test]
    fn multi_device_is_the_default_and_only_a_single_link_opts_out() {
        for (vxi_name, kind, multi) in [
            ("gpib0", VxiLinkKind::Gpib, true),
            ("hpib0", VxiLinkKind::Gpib, true),
            ("inst0", VxiLinkKind::Single, false),
            ("com1", VxiLinkKind::Single, false),
            ("foo0", VxiLinkKind::Other, true),
            ("", VxiLinkKind::Other, true),
        ] {
            assert_eq!(
                VxiLinkKind::from_vxi_name(vxi_name),
                kind,
                "classification of {vxi_name:?}"
            );
            let drv =
                DrvVxi11Port::configure("vxi0", "10.0.0.1", 0, "", vxi_name, 0, false).unwrap();
            assert_eq!(
                drv.base().flags.multi_device,
                multi,
                "ASYN_MULTIDEVICE for {vxi_name:?}"
            );
            assert_eq!(
                drv.base().max_addr,
                if multi { 32 } else { 1 },
                "max_addr for {vxi_name:?}"
            );
        }
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

    /// C keeps the terminator on the *device link*, not the port
    /// (`drvVxi11.c:65`, initialised per address at `:1737-1741`), and stamps
    /// it into that address's `device_read` at `:1169-1173`. The `PortDriver`
    /// default kept one two-byte cache per address in `PortDriverBase` and
    /// forwarded it to an interpose stack a GPIB port does not have, so the
    /// terminator reached neither the request nor the readback.
    #[test]
    fn input_eos_is_per_device_link_and_capped_at_one_character() {
        let mut drv =
            DrvVxi11Port::configure("V0", "gw.example", 0, "1.0", "gpib0", 0, false).unwrap();

        let a3 = AsynUser::default().with_addr(3);
        let a7 = AsynUser::default().with_addr(7);
        drv.set_input_eos(&a3, b"\n").unwrap();
        drv.set_input_eos(&a7, b";").unwrap();

        assert_eq!(drv.input_eos_for(3), Some(b'\n'));
        assert_eq!(drv.input_eos_for(7), Some(b';'));
        assert_eq!(drv.input_eos_for(11), None, "an untouched link stays at -1");
        assert_eq!(drv.get_input_eos(&a3), b"\n".to_vec());
        assert!(
            drv.get_input_eos(&AsynUser::default().with_addr(11))
                .is_empty()
        );

        // C `vxiSetEos`'s `default:` arm, and `asynGpib::setInputEos` above it,
        // both refuse more than one character.
        let err = drv
            .set_input_eos(&a3, b"\r\n")
            .expect_err("VXI-11 holds one terminating character per link");
        assert!(
            err.message().contains("illegal eoslen 2"),
            "expected C's wording, got {}",
            err.message()
        );
        assert_eq!(
            drv.input_eos_for(3),
            Some(b'\n'),
            "a refusal must not clear it"
        );

        // eoslen 0 is C's `eos = -1`, and only for the address that asked.
        drv.set_input_eos(&a3, b"").unwrap();
        assert_eq!(drv.input_eos_for(3), None);
        assert_eq!(drv.input_eos_for(7), Some(b';'));
    }

    /// `asynGpib`'s octet vtable leaves the output-EOS slots NULL
    /// (`asynGpib.c:132`), so a GPIB port must refuse rather than cache bytes
    /// nothing appends — the rule the sibling prologix port already holds.
    #[test]
    fn output_eos_is_refused_on_a_gpib_port() {
        let mut drv =
            DrvVxi11Port::configure("V1", "gw.example", 0, "1.0", "gpib0", 0, false).unwrap();
        let user = AsynUser::default();
        assert!(drv.set_output_eos(&user, b"\n").is_err());
        assert!(drv.get_output_eos(&user).is_empty());
    }
}
