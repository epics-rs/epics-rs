//! GPIB (IEEE-488) interface — the C `asynGpib` interface surface.
//!
//! C splits the GPIB support in two (`asynGpib/asynGpibDriver.h`):
//!
//! * `struct asynGpib` (asynGpibDriver.h:45-62) is what a *gpib-aware user*
//!   calls — `addressedCmd`, `universalCmd`, `ifc`, `ren` — after finding the
//!   interface with `findInterface(asynGpibType)`. asynGpib.c passes each one
//!   straight through to the driver (asynGpib.c:472-496).
//! * `struct asynGpibPort` (asynGpibDriver.h:65-93) is what the *driver*
//!   implements. Its four command methods are the same four; the rest
//!   (`srqStatus`, `srqEnable`, `serialPoll*`) exist only to feed asynGpib's own
//!   SRQ poll thread (asynGpib.c:303-356), which has no consumer in this tree.
//!
//! The pass-through means the two halves are one interface here: the driver
//! implements [`crate::port::PortDriver::gpib_universal_cmd`] and its three
//! siblings, declares [`super::Capability::Gpib`], and callers reach them
//! through [`crate::port_handle::PortHandle`] — the port actor is the
//! `pasynGpib` vtable.
//!
//! This module owns the *encoding*: the command bytes and the exact bus frames
//! C's asynRecord builds for UCMD / ACMD — `gpibUniversalCmd`'s `ucmd` switch
//! (asynRecord.c:1652-1671) and the `acmd[]` frame `gpibAddressedCmd` assembles
//! (:1698-1747) — so the record only chooses a command and the driver writes bytes.

// --- Addressed commands (asynGpibDriver.h:19-22) ---

/// Go To Local.
pub const IBGTL: u8 = 0x01;
/// Selective Device Clear.
pub const IBSDC: u8 = 0x04;
/// Group Execute Trigger.
pub const IBGET: u8 = 0x08;
/// Take Control.
pub const IBTCT: u8 = 0x09;

// --- Universal commands (asynGpibDriver.h:25-30) ---

/// Local Lockout.
pub const IBLLO: u8 = 0x11;
/// Device Clear (all devices).
pub const IBDCL: u8 = 0x14;
/// Serial Poll Enable.
pub const IBSPE: u8 = 0x18;
/// Serial Poll Disable.
pub const IBSPD: u8 = 0x19;
/// Unlisten.
pub const IBUNL: u8 = 0x3f;
/// Untalk.
pub const IBUNT: u8 = 0x5f;

// --- Address bases (asynGpibDriver.h:33-35) ---
//
// The two comments in the C header are swapped (`TADBASE` is annotated "listen"
// and `LADBASE` "talk"); the values are the standard IEEE-488 ones and the C
// call sites use them correctly — asynRecord sends `addr + LADBASE` as the
// listen address and `addr + TADBASE` for Take Control (asynRecord.c:1699,1713).

/// Listen-address base: listener 0 is `0x20`.
pub const LADBASE: u8 = 0x20;
/// Talk-address base: talker 0 is `0x40`.
pub const TADBASE: u8 = 0x40;
/// Secondary-address base: secondary 0 is `0x60`.
pub const SADBASE: u8 = 0x60;

/// Number of GPIB addresses (asynGpibDriver.h:37).
pub const NUM_GPIB_ADDRESSES: i32 = 32;

/// The asynRecord `UCMD` menu (`menuGpibUCMD`, asynRecord.dbd) — a universal
/// command, sent with no addressing. Discriminants are the menu indices; index 0
/// is `None` and is modelled as the absence of a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum GpibUniversalCommand {
    /// Device Clear (DCL).
    DeviceClear = 1,
    /// Local Lockout (LL0).
    LocalLockout = 2,
    /// Serial Poll Disable (SPD).
    SerialPollDisable = 3,
    /// Serial Poll Enable (SPE).
    SerialPollEnable = 4,
    /// Unlisten (UNL).
    Unlisten = 5,
    /// Untalk (UNT).
    Untalk = 6,
}

impl GpibUniversalCommand {
    /// Decode a UCMD menu index. `None` for `gpibUCMD_None` (0) and for any
    /// value outside the menu.
    pub fn from_menu(menu: i32) -> Option<Self> {
        match menu {
            1 => Some(Self::DeviceClear),
            2 => Some(Self::LocalLockout),
            3 => Some(Self::SerialPollDisable),
            4 => Some(Self::SerialPollEnable),
            5 => Some(Self::Unlisten),
            6 => Some(Self::Untalk),
            _ => None,
        }
    }

    /// The command byte C's `gpibUniversalCmd` switch selects
    /// (asynRecord.c:1652-1671).
    pub fn cmd_byte(self) -> u8 {
        match self {
            Self::DeviceClear => IBDCL,
            Self::LocalLockout => IBLLO,
            Self::SerialPollDisable => IBSPD,
            Self::SerialPollEnable => IBSPE,
            Self::Unlisten => IBUNL,
            Self::Untalk => IBUNT,
        }
    }
}

/// The command byte a UCMD menu value dispatches, C's `gpibUniversalCmd`
/// (asynRecord.c:1645-1672) in one call: `cmd_char` starts at 0 and the switch
/// has no default, so a value outside the menu is sent as the byte `0` — not
/// skipped.
pub fn universal_cmd_byte(menu: i32) -> u8 {
    GpibUniversalCommand::from_menu(menu).map_or(0, GpibUniversalCommand::cmd_byte)
}

/// The asynRecord `ACMD` menu (`menuGpibACMD`, asynRecord.dbd) — an addressed
/// command. Discriminants are the menu indices; index 0 is `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum GpibAddressedCommand {
    /// Group Execute Trigger (GET).
    GroupExecuteTrigger = 1,
    /// Go To Local (GTL).
    GoToLocal = 2,
    /// Selected Device Clear (SDC).
    SelectedDeviceClear = 3,
    /// Take Control (TCT).
    TakeControl = 4,
    /// Serial Poll — not a single frame; see [`GpibAddressedRequest::SerialPoll`].
    SerialPoll = 5,
}

impl GpibAddressedCommand {
    /// Decode an ACMD menu index. `None` for `gpibACMD_None` (0) and for any
    /// value outside the menu.
    pub fn from_menu(menu: i32) -> Option<Self> {
        match menu {
            1 => Some(Self::GroupExecuteTrigger),
            2 => Some(Self::GoToLocal),
            3 => Some(Self::SelectedDeviceClear),
            4 => Some(Self::TakeControl),
            5 => Some(Self::SerialPoll),
            _ => None,
        }
    }
}

/// What one ACMD put asks the port to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpibAddressedRequest {
    /// A single `addressedCmd(frame)` — the bytes C assembles in `acmd[]`
    /// (asynRecord.c:1698-1716).
    Frame(Vec<u8>),
    /// Serial Poll is three operations, not one frame: universal SPE, a
    /// one-byte octet read into `SPR`, universal SPD (asynRecord.c:1717-1746).
    /// The caller owns the sequence because the middle step is an octet read,
    /// not a GPIB command.
    SerialPoll,
}

/// The ATN frame C builds for an addressed command (asynRecord.c:1698-1716).
///
/// `[UNT, UNL, listen-address, cmd, UNT, UNL]` — the target is made the sole
/// listener, the command byte is sent, and the bus is untalked/unlistened
/// again. Take Control is the exception: it addresses the target as a *talker*
/// and stops after the command byte (`lenCmd = 4`), because the controller is
/// handing over control and must not keep driving the bus.
pub fn addressed_frame(cmd_byte: u8, addr: i32, take_control: bool) -> Vec<u8> {
    let addr_byte = (addr as u8).wrapping_add(if take_control { TADBASE } else { LADBASE });
    let mut frame = vec![IBUNT, IBUNL, addr_byte, cmd_byte];
    if !take_control {
        frame.push(IBUNT);
        frame.push(IBUNL);
    }
    frame
}

/// What an ACMD menu value dispatches at `addr`, C's `gpibAddressedCmd`
/// (asynRecord.c:1689-1749) in one call.
///
/// A value outside the menu still sends a frame, as C does — its `acmd[3]` is
/// never assigned on that path (the switch has no default and `acmd` is an
/// uninitialized local, asynRecord.c:1689), so C puts whatever the stack held on
/// the bus. This port sends a deterministic `0` command byte instead of
/// reproducing that read of uninitialized memory.
pub fn addressed_request(menu: i32, addr: i32) -> GpibAddressedRequest {
    match GpibAddressedCommand::from_menu(menu) {
        Some(GpibAddressedCommand::SerialPoll) => GpibAddressedRequest::SerialPoll,
        Some(GpibAddressedCommand::TakeControl) => {
            GpibAddressedRequest::Frame(addressed_frame(IBTCT, addr, true))
        }
        Some(GpibAddressedCommand::GroupExecuteTrigger) => {
            GpibAddressedRequest::Frame(addressed_frame(IBGET, addr, false))
        }
        Some(GpibAddressedCommand::GoToLocal) => {
            GpibAddressedRequest::Frame(addressed_frame(IBGTL, addr, false))
        }
        Some(GpibAddressedCommand::SelectedDeviceClear) => {
            GpibAddressedRequest::Frame(addressed_frame(IBSDC, addr, false))
        }
        None => GpibAddressedRequest::Frame(addressed_frame(0, addr, false)),
    }
}

/// The interface set `pasynGpib->registerPort` gives every GPIB driver
/// (asynGpib.c:562-631): `asynCommon` + `asynOctet` + `asynGpib` + `asynInt32`.
/// One owner for that C fact — drvVxi11 and drvPrologixGPIB both obtain their
/// interfaces from this one call (drvVxi11.c:1761, drvPrologixGPIB.c:592), and
/// a driver that registers more (vxi11 adds `asynOption`, drvVxi11.c:1777) says
/// so at its own `capabilities`.
///
/// `asynInt32` is in the set even though no GPIB driver implements it: asynGpib
/// registers an all-NULL vtable (`asynInt32 int32 = {0,0,0,0,0}`,
/// asynGpib.c:140) through `pasynInt32Base->initialize`, which fills in the
/// asynInt32Base defaults. It is registered for the SRQ interrupt source
/// (asynGpib.c:620), so asynRecord's I32IV reads 1 on a GPIB port while an
/// actual read reports [`int32_read_not_supported`] and an actual write
/// [`int32_write_not_supported`].
pub fn gpib_port_capabilities() -> Vec<super::Capability> {
    use super::Capability::*;
    vec![
        OctetRead, OctetWrite, Gpib, Int32Read, Int32Write, Flush, Connect,
    ]
}

/// C's asynInt32Base default WRITE for a port that registered `asynInt32` with a
/// NULL vtable — `writeDefault` (asynInt32Base.c:52-68) sets `errorMessage` to
/// "write is not supported" and returns `asynError`. `getBounds` is the one
/// default that succeeds (low = high = 0), which is already
/// [`crate::port::PortDriver::get_bounds_int32`]'s default.
pub fn int32_write_not_supported() -> crate::error::AsynError {
    crate::error::AsynError::Status {
        status: crate::error::AsynStatus::Error,
        message: "write is not supported".into(),
    }
}

/// The same default for a READ.
///
/// **CBUG-B10 — now matches upstream.** At the `R4-45-19-ge2a281e2` pin this
/// catalogue was read against, C's `readDefault` (asynInt32Base.c:70-86) set
/// `errorMessage` to **"write is not supported"** — a copy-paste from the
/// `writeDefault` directly above it — while the `asynPrint` trace two lines
/// down correctly said "read is not supported", and the same shape stood in
/// all six `asyn*Base.c` files. asyn **#237** merged, and at the worktree
/// revision `R4-45-74-g731d616e` every one of those six lines reads "read is
/// not supported" (asynEnumBase.c:79, asynFloat64Base.c:78,
/// asynGenericPointerBase.c:77, asynInt32Base.c:82, asynInt64Base.c:82,
/// asynUInt32DigitalBase.c:91 — the cited lines are now the FIXED lines). So
/// this is no longer a deviation: the port said "read" first and C has since
/// agreed. The string lands in asynRecord's ERRS field and is purely
/// diagnostic — no client parses it, so it was never a wire contract.
///
/// Two functions, not one with a flag: the direction is fixed at the call site,
/// so a read path cannot reach for the write text.
pub fn int32_read_not_supported() -> crate::error::AsynError {
    crate::error::AsynError::Status {
        status: crate::error::AsynStatus::Error,
        message: "read is not supported".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn universal_cmd_bytes_match_the_c_switch() {
        // asynRecord.c:1652-1671, menu order.
        assert_eq!(universal_cmd_byte(1), 0x14, "DCL");
        assert_eq!(universal_cmd_byte(2), 0x11, "LLO");
        assert_eq!(universal_cmd_byte(3), 0x19, "SPD");
        assert_eq!(universal_cmd_byte(4), 0x18, "SPE");
        assert_eq!(universal_cmd_byte(5), 0x3f, "UNL");
        assert_eq!(universal_cmd_byte(6), 0x5f, "UNT");
    }

    #[test]
    fn an_out_of_menu_universal_command_is_the_zero_byte() {
        // C's `cmd_char = 0` initializer with no `default:` arm — the command is
        // still dispatched (asynRecord.c:1645,1672).
        assert_eq!(universal_cmd_byte(99), 0);
    }

    #[test]
    fn addressed_frames_match_the_c_acmd_buffer() {
        // C acmd[] = {IBUNT, IBUNL, addr+LADBASE, cmd, IBUNT, IBUNL}, lenCmd 6.
        assert_eq!(
            addressed_request(1, 5),
            GpibAddressedRequest::Frame(vec![0x5f, 0x3f, 0x20 + 5, 0x08, 0x5f, 0x3f]),
            "GET"
        );
        assert_eq!(
            addressed_request(2, 5),
            GpibAddressedRequest::Frame(vec![0x5f, 0x3f, 0x20 + 5, 0x01, 0x5f, 0x3f]),
            "GTL"
        );
        assert_eq!(
            addressed_request(3, 5),
            GpibAddressedRequest::Frame(vec![0x5f, 0x3f, 0x20 + 5, 0x04, 0x5f, 0x3f]),
            "SDC"
        );
    }

    #[test]
    fn take_control_uses_the_talk_address_and_stops_at_four_bytes() {
        // C: acmd[2] = addr + TADBASE; acmd[3] = IBTCT; lenCmd = 4
        // (asynRecord.c:1711-1716).
        assert_eq!(
            addressed_request(4, 5),
            GpibAddressedRequest::Frame(vec![0x5f, 0x3f, 0x40 + 5, 0x09]),
            "TCT"
        );
    }

    #[test]
    fn serial_poll_is_a_sequence_not_a_frame() {
        assert_eq!(addressed_request(5, 5), GpibAddressedRequest::SerialPoll);
    }

    #[test]
    fn an_out_of_menu_addressed_command_still_frames_the_address() {
        // C sends the frame with an unassigned command byte; this port sends 0.
        assert_eq!(
            addressed_request(99, 7),
            GpibAddressedRequest::Frame(vec![0x5f, 0x3f, 0x20 + 7, 0x00, 0x5f, 0x3f])
        );
    }
}
