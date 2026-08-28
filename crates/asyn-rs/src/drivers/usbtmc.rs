//! USBTMC (USB Test & Measurement Class) bridge driver — port of
//! `drvAsynUSBTMC.c`.
//!
//! ## C compatibility
//!
//! The C driver registers iocsh as
//!
//! ```text
//! usbtmcConfigure(portName, vendorId, productId, serialNumber,
//!                 priority, flags)
//! ```
//!
//! — six positional `iocshArg` entries
//! (`drvAsynUSBTMC.c:1332-1349`). `flags & 0x1 == 0` enables the
//! framework's auto-connect (mirrors C's `(flags & 0x1) == 0` at
//! `registerPort` — line 1275-1276). Empty `serialNumber` means
//! "first matching device".
//!
//! The protocol is USB Bulk-OUT / Bulk-IN with a 12-byte header
//! (`drvAsynUSBTMC.c:35`, `BULK_IO_HEADER_SIZE`):
//!
//! ```text
//!   buf[0]    MESSAGE_ID (1=DEV_DEP_MSG_OUT, 2=REQUEST_DEV_DEP_MSG_IN)
//!   buf[1]    bTag (1..0xFF, advancing per transaction)
//!   buf[2]    ~bTag
//!   buf[3]    reserved (0)
//!   buf[4-7]  transferSize little-endian (u32)
//!   buf[8]    bmTransferAttributes — bit 0 = EOM on DEV_DEP_MSG_OUT,
//!             bit 1 = TermCharEnabled on REQUEST_DEV_DEP_MSG_IN
//!   buf[9]    TermChar, when bit 1 of buf[8] is set (IN only)
//!   buf[10-11] reserved (0)
//! ```
//!
//! followed by `transferSize` payload bytes, padded to a 4-byte
//! boundary with zeros (lines 773-775).
//!
//! ## Hardware feature gate
//!
//! Hardware I/O requires the `usbtmc` Cargo feature (which would
//! pull `nusb` or `rusb` once a deployment lands). Without it,
//! [`PortDriver::connect`] returns an explanatory error and the
//! driver remains constructible everywhere so iocsh startup scripts
//! can validate VID/PID/serial parsing on minimal hosts. Same
//! scaffold convention as [`super::ftdi`].

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::port::{PortDriver, PortDriverBase, PortFlags};
use crate::user::AsynUser;

// --- Protocol constants — `drvAsynUSBTMC.c:28-37` ---

/// USB-IF assigned interface class for Test & Measurement.
pub const USBTMC_INTERFACE_CLASS: u8 = 0xFE;
/// USB-IF assigned interface sub-class for USBTMC.
pub const USBTMC_INTERFACE_SUBCLASS: u8 = 0x03;

/// C `BULK_IO_OUTPUT_EOS_CAPACITY` (`drvAsynUSBTMC.c:37`) — how many
/// terminator bytes `asynOctetSetOutputEos` will hold.
pub const OUTPUT_EOS_CAPACITY: usize = 2;

/// `MESSAGE_ID_DEV_DEP_MSG_OUT` — Bulk-OUT data transfer from host.
pub const MESSAGE_ID_DEV_DEP_MSG_OUT: u8 = 1;
/// `MESSAGE_ID_REQUEST_DEV_DEP_MSG_IN` — host requesting Bulk-IN data.
pub const MESSAGE_ID_REQUEST_DEV_DEP_MSG_IN: u8 = 2;
/// `MESSAGE_ID_DEV_DEP_MSG_IN` — device-to-host Bulk-IN response.
pub const MESSAGE_ID_DEV_DEP_MSG_IN: u8 = 2;

/// USBTMC bulk-transfer header is 12 bytes.
pub const BULK_IO_HEADER_SIZE: usize = 12;
/// Maximum payload size per bulk transaction.
pub const BULK_IO_PAYLOAD_CAPACITY: usize = 1024 * 1024;

/// `bmTransferAttributes` bit 1 of a `REQUEST_DEV_DEP_MSG_IN` header — the
/// device ends the transfer when it sends `TermChar` (`buf[9]`).
/// C writes it as the literal `2` at `drvAsynUSBTMC.c:863-866`.
pub const BULK_IN_TERM_CHAR_ENABLED: u8 = 0x02;

/// Bit 0 of the USBTMC device-capabilities byte: the device supports a
/// bulk-IN terminating character. C refuses an input EOS without it
/// (`drvAsynUSBTMC.c:971-975`).
pub const TMC_CAP_TERM_CHAR: u8 = 0x01;

/// Offset of the device-capabilities byte in the 0x18-byte `GET_CAPABILITIES`
/// control-transfer response (`drvAsynUSBTMC.c:544`, `pdpvt->buf[5]`).
pub const GET_CAPABILITIES_DEVICE_CAP_OFFSET: usize = 5;

// --- iocshArg flags bitfield — `drvAsynUSBTMC.c:1275` ---

/// `flags & 0x1` — when set, *disable* the framework's auto-connect.
/// Inverted from the `noAutoConnect` argument style used by other C
/// asyn drivers (FTDI / IP) because USBTMC packs both auto-connect
/// and future expansion knobs into one `int flags` slot.
pub const USBTMC_FLAG_NO_AUTO_CONNECT: i32 = 0x1;

/// Build the 12-byte BULK-OUT header for a `DEV_DEP_MSG_OUT`
/// transaction. C parity: `drvAsynUSBTMC.c:737-770` —
///
/// ```text
///   buf[0] = MESSAGE_ID_DEV_DEP_MSG_OUT;
///   buf[1] = bTag;
///   buf[2] = ~bTag;
///   buf[3] = 0;
///   buf[4..8] = transferSize (LE u32);
///   buf[8] = transferAttributes (EOM bit);
///   buf[9..12] = 0;
/// ```
pub fn build_bulk_out_header(
    b_tag: u8,
    transfer_size: u32,
    end_of_message: bool,
) -> [u8; BULK_IO_HEADER_SIZE] {
    let mut h = [0u8; BULK_IO_HEADER_SIZE];
    h[0] = MESSAGE_ID_DEV_DEP_MSG_OUT;
    h[1] = b_tag;
    h[2] = !b_tag;
    h[3] = 0;
    h[4..8].copy_from_slice(&transfer_size.to_le_bytes());
    h[8] = if end_of_message { 0x01 } else { 0x00 };
    // h[9..12] already zero.
    h
}

/// Build the 12-byte BULK-OUT header for a `REQUEST_DEV_DEP_MSG_IN`
/// transaction. C parity: `drvAsynUSBTMC.c:855-871` —
///
/// ```text
///   buf[0] = MESSAGE_ID_REQUEST_DEV_DEP_MSG_IN;
///   buf[1] = bTag;   buf[2] = ~bTag;   buf[3] = 0;
///   buf[4..8] = transferSize (LE u32);
///   if (termChar >= 0) { buf[8] = 2; buf[9] = termChar; }
///   else               { buf[8] = 0; buf[9] = 0; }
///   buf[10..12] = 0;
/// ```
///
/// `term_char` is the whole point of the request header: with the attribute
/// bit clear the device returns the full `max_transfer_size` or times out, so
/// a read that is supposed to stop on a terminator never does. The earlier
/// version of this function had no such parameter and cited `:855-863` as
/// authority for it — a range that stops one line before `:864`, the line that
/// sets the bit.
pub fn build_request_bulk_in_header(
    b_tag: u8,
    max_transfer_size: u32,
    term_char: Option<u8>,
) -> [u8; BULK_IO_HEADER_SIZE] {
    let mut h = [0u8; BULK_IO_HEADER_SIZE];
    h[0] = MESSAGE_ID_REQUEST_DEV_DEP_MSG_IN;
    h[1] = b_tag;
    h[2] = !b_tag;
    h[3] = 0;
    h[4..8].copy_from_slice(&max_transfer_size.to_le_bytes());
    if let Some(c) = term_char {
        h[8] = BULK_IN_TERM_CHAR_ENABLED;
        h[9] = c;
    }
    // h[10..12] reserved.
    h
}

/// Advance bTag using the same wrap rule the C driver uses
/// (`drvAsynUSBTMC.c:772`): `bTag = (bTag == 0xFF) ? 1 : bTag + 1` —
/// bTag must be non-zero per the USBTMC spec.
pub fn next_b_tag(prev: u8) -> u8 {
    if prev == 0xFF { 1 } else { prev + 1 }
}

/// Round up a packet length to the next 4-byte boundary (USBTMC pads
/// with zeros per `drvAsynUSBTMC.c:774`).
pub fn pad4(n: usize) -> usize {
    (n + 3) & !3
}

/// Parsed config — fields match `usbtmcConfigure` positional args.
#[derive(Debug, Clone)]
pub struct UsbtmcConfig {
    pub vendor_id: u16,
    pub product_id: u16,
    /// Empty means "first matching device" (C `serialNumber == NULL`).
    pub serial_number: String,
    pub priority: u32,
    pub flags: i32,
}

impl UsbtmcConfig {
    pub fn from_positional(
        vendor_id: i32,
        product_id: i32,
        serial_number: &str,
        priority: i32,
        flags: i32,
    ) -> Self {
        Self {
            vendor_id: vendor_id as u16,
            product_id: product_id as u16,
            serial_number: serial_number.to_string(),
            // C `if (priority == 0) priority = epicsThreadPriorityMedium`
            // — record the original; the OS-thread priority mapping
            // is platform-specific and we don't expose it from Rust.
            priority: priority.max(0) as u32,
            flags,
        }
    }

    /// True when the caller wants framework auto-connect disabled
    /// (C `flags & 0x1`).
    pub fn no_auto_connect(&self) -> bool {
        (self.flags & USBTMC_FLAG_NO_AUTO_CONNECT) != 0
    }
}

/// USBTMC driver — scaffold matching C iocsh signature.
///
/// Hardware I/O requires the `usbtmc` Cargo feature. Without it,
/// [`PortDriver::connect`] returns a feature-not-enabled error so
/// application code surfaces the missing dep at iocsh boot rather
/// than later. The config parser and header-builder functions
/// remain available everywhere so wire-protocol unit tests run on
/// minimal hosts.
pub struct DrvAsynUsbtmcPort {
    base: PortDriverBase,
    config: UsbtmcConfig,
    /// Current bTag value — advances per transaction.
    b_tag: u8,
    /// C `pdpvt->termChar` (`drvAsynUSBTMC.c:85`, initialised to -1 at
    /// `:1259`): the bulk-IN terminating character, or `None` for C's -1.
    /// This is the port's only input-EOS state — C registers the octet
    /// interface with `initialize(portName, &octet, 0, 0, 0)` (`:1297`), so a
    /// USBTMC port has no software EOS interpose and the terminator has
    /// nowhere else to live.
    term_char: Option<u8>,
    /// C `pdpvt->outputEOS` / `outputEOSlen` (`drvAsynUSBTMC.c:123-124`).
    output_eos: Vec<u8>,
    /// C `pdpvt->tmcDeviceCapabilities` (`:106`), byte 5 of the
    /// `GET_CAPABILITIES` response (`:544`). Zero until a connect has read it,
    /// which is also C's state on a port that has not connected — and an EOS
    /// request in that state is refused, there as here.
    tmc_device_capabilities: u8,
}

impl DrvAsynUsbtmcPort {
    /// Configure a USBTMC port. One-to-one with C
    /// `usbtmcConfigure(portName, vendorId, productId, serialNumber,
    /// priority, flags)` (`drvAsynUSBTMC.c:1222-1224`).
    #[allow(clippy::too_many_arguments)] // intentional 1:1 mirror of C iocshArg list
    pub fn configure(
        port_name: &str,
        vendor_id: i32,
        product_id: i32,
        serial_number: &str,
        priority: i32,
        flags: i32,
    ) -> AsynResult<Self> {
        let config =
            UsbtmcConfig::from_positional(vendor_id, product_id, serial_number, priority, flags);
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
        base.auto_connect = !config.no_auto_connect();
        Ok(Self {
            base,
            config,
            b_tag: 1,                   // C initializes pdpvt->bTag = 1
            term_char: None,            // C `pdpvt->termChar = -1` (:1259)
            output_eos: Vec::new(),     // C `outputEOSlen = 0` from the calloc
            tmc_device_capabilities: 0, // C callocs pdpvt; connect fills it in
        })
    }

    pub fn config(&self) -> &UsbtmcConfig {
        &self.config
    }

    pub fn current_b_tag(&self) -> u8 {
        self.b_tag
    }

    /// The bulk-IN terminating character to stamp into the next
    /// `REQUEST_DEV_DEP_MSG_IN` header, C `pdpvt->termChar >= 0`.
    pub fn term_char(&self) -> Option<u8> {
        self.term_char
    }

    /// Record what `GET_CAPABILITIES` reported, C `:543-544`. The hardware
    /// path calls this with the 0x18-byte control-transfer response; the
    /// device-capabilities byte is what gates an input EOS.
    pub fn apply_capabilities(&mut self, response: &[u8]) -> AsynResult<()> {
        let byte = response
            .get(GET_CAPABILITIES_DEVICE_CAP_OFFSET)
            .ok_or_else(|| AsynError::Status {
                status: AsynStatus::Error,
                message: "GET_CAPABILITIES response too short".into(),
            })?;
        self.tmc_device_capabilities = *byte;
        Ok(())
    }

    /// Whether this build was compiled with USBTMC hardware support.
    pub fn has_hw_support() -> bool {
        cfg!(feature = "usbtmc")
    }
}

impl PortDriver for DrvAsynUsbtmcPort {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    /// C drvAsynUSBTMC registers asynCommon, asynOctet and asynInt32
    /// (drvAsynUSBTMC.c:1285-1322) — asynInt32 carries the status-byte /
    /// remote-local parameters. It registers no asynOption.
    fn capabilities(&self) -> Vec<crate::interfaces::Capability> {
        use crate::interfaces::Capability::*;
        // C drvAsynUSBTMC registers asynDrvUser (drvAsynUSBTMC.c:1319-1324) —
        // its drvInfo strings (SRQ, STATUS_BYTE, …) resolve to reasons.
        vec![
            OctetRead, OctetWrite, Int32Read, Int32Write, DrvUser, Flush, Connect,
        ]
    }

    /// C `asynOctetSetInputEos` (`drvAsynUSBTMC.c:963-985`). The terminator is
    /// the device's, not a software scan: it rides in the `REQUEST_DEV_DEP_MSG_IN`
    /// header and the device stops the transfer on it. So this overrides the
    /// `PortDriver` default, which caches into `base.eos_entry` and forwards to
    /// the interpose stack — a USBTMC port has no EOS interpose to forward to
    /// (`:1297` registers the octet interface with `processEosIn = 0`), and the
    /// cached bytes would terminate nothing.
    ///
    /// Both of C's refusals are here. An EOS longer than one character is not
    /// representable in the header at all, and an EOS on a device whose
    /// `GET_CAPABILITIES` did not advertise `TermChar` is refused rather than
    /// silently ignored — a device that never honours the terminator would
    /// otherwise turn every terminated read into a full-length read or a
    /// timeout, which is the failure this rejection exists to make loud.
    fn set_input_eos(&mut self, _user: &AsynUser, eos: &[u8]) -> AsynResult<()> {
        match eos.len() {
            0 => {
                self.term_char = None;
                Ok(())
            }
            1 => {
                if self.tmc_device_capabilities & TMC_CAP_TERM_CHAR == 0 {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: "Device does not support bulk-IN terminating character".into(),
                    });
                }
                self.term_char = Some(eos[0]);
                Ok(())
            }
            _ => Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "USBTMC does not support multiple input terminating characters".into(),
            }),
        }
    }

    /// C `asynOctetGetInputEos` (`drvAsynUSBTMC.c:987-1001`): `termChar < 0`
    /// reports `eoslen` 0, otherwise the single byte. Reads the same field
    /// `set_input_eos` writes, so the readback cannot disagree with what the
    /// next request header will carry.
    fn get_input_eos(&self, _user: &AsynUser) -> Vec<u8> {
        match self.term_char {
            Some(c) => vec![c],
            None => Vec::new(),
        }
    }

    /// C `asynOctetSetOutputEos` (`drvAsynUSBTMC.c:1004-1016`): up to
    /// `BULK_IO_OUTPUT_EOS_CAPACITY` (2, :37) bytes stored on the driver.
    /// USBTMC has a real entry in C's `asynOctet` table for this (:1036), so
    /// it must not fall through to [`PortDriver`]'s Fail-stub default — a
    /// USBTMC port is not a port without output-EOS support.
    ///
    /// KNOWN GAP, unchanged by this method: C's `asynOctetWrite` appends the
    /// stored bytes to the last packet of a transfer (:759-762) and this
    /// driver's write does not, so the terminator is stored and read back but
    /// not yet sent. Storing it here keeps the readback honest and keeps the
    /// refusal for ports that really have no output EOS.
    fn set_output_eos(&mut self, _user: &AsynUser, eos: &[u8]) -> AsynResult<()> {
        if eos.len() > OUTPUT_EOS_CAPACITY {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!(
                    "USBTMC output EOS is at most {OUTPUT_EOS_CAPACITY} bytes, got {}",
                    eos.len()
                ),
            });
        }
        self.output_eos = eos.to_vec();
        Ok(())
    }

    /// C `asynOctetGetOutputEos` (`drvAsynUSBTMC.c:1018-1028`) — reads back
    /// the same field the setter wrote.
    fn get_output_eos(&self, _user: &AsynUser) -> Vec<u8> {
        self.output_eos.clone()
    }

    fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
        if !Self::has_hw_support() {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!(
                    "USBTMC driver scaffold: hardware feature 'usbtmc' not enabled \
                     in this build. Config parsed (vid=0x{:04X}, pid=0x{:04X}, \
                     serial={:?}, flags=0x{:X}) — rebuild with \
                     `--features asyn-rs/usbtmc` to enable.",
                    self.config.vendor_id,
                    self.config.product_id,
                    self.config.serial_number,
                    self.config.flags,
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
    fn bulk_out_header_matches_c_layout() {
        // C drvAsynUSBTMC.c:737-770:
        //   buf[0] = MESSAGE_ID_DEV_DEP_MSG_OUT (=1)
        //   buf[1] = bTag,  buf[2] = ~bTag
        //   buf[3] = 0
        //   buf[4..8] = transferSize little-endian
        //   buf[8] = 1 if EOM else 0
        //   buf[9..12] = 0
        let h = build_bulk_out_header(0x42, 0x1234_5678, true);
        assert_eq!(h[0], MESSAGE_ID_DEV_DEP_MSG_OUT);
        assert_eq!(h[1], 0x42);
        assert_eq!(h[2], !0x42);
        assert_eq!(h[3], 0);
        assert_eq!(&h[4..8], &[0x78, 0x56, 0x34, 0x12]);
        assert_eq!(h[8], 0x01);
        assert_eq!(&h[9..12], &[0, 0, 0]);
    }

    #[test]
    fn bulk_out_header_eom_bit_clear_when_not_end() {
        let h = build_bulk_out_header(0x10, 256, false);
        assert_eq!(h[8], 0x00);
    }

    #[test]
    fn request_bulk_in_header_matches_c_layout() {
        // drvAsynUSBTMC.c:855-871 — MESSAGE_ID=2, bTag/~bTag, transferSize in
        // bytes 4..8, and the `else` arm at :867-870 zeroing buf[8]/buf[9]
        // when there is no terminator.
        let h = build_request_bulk_in_header(0x10, BULK_IO_PAYLOAD_CAPACITY as u32, None);
        assert_eq!(h[0], MESSAGE_ID_REQUEST_DEV_DEP_MSG_IN);
        assert_eq!(h[1], 0x10);
        assert_eq!(h[2], !0x10);
        let cap = BULK_IO_PAYLOAD_CAPACITY as u32;
        assert_eq!(&h[4..8], &cap.to_le_bytes());
        // attributes / reserved bytes are zero.
        assert!(h[8..12].iter().all(|&b| b == 0));
    }

    /// C `:863-866` sets `buf[8] = 2` (TermCharEnabled) and `buf[9] = termChar`
    /// whenever an input EOS is in force. Without the bit the device returns the
    /// full requested transfer size or times out, so a `camonitor` on a
    /// terminator-delimited instrument reply either blocks for TMOT or reports
    /// a megabyte-long read instead of the line.
    #[test]
    fn request_bulk_in_header_carries_the_terminator() {
        let h = build_request_bulk_in_header(0x10, 512, Some(b'\n'));
        assert_eq!(
            h[8], BULK_IN_TERM_CHAR_ENABLED,
            "bmTransferAttributes must set TermCharEnabled"
        );
        assert_eq!(h[9], b'\n');
        assert_eq!(&h[10..12], &[0, 0]);
    }

    /// C `asynOctetSetInputEos` (`:963-985`) has two hard refusals and one
    /// disable, and the port carries no other input-EOS state, so the readback
    /// has to come from the same field.
    #[test]
    fn set_input_eos_ports_both_c_refusals() {
        let mut drv = DrvAsynUsbtmcPort::configure("u_eos", 0x0699, 0x0401, "", 0, 0).unwrap();
        let user = AsynUser::default();

        // A device that has not advertised TermChar support is refused, which
        // is also the state of a port that has not connected.
        let err = drv
            .set_input_eos(&user, b"\n")
            .expect_err("no capabilities byte yet");
        assert_eq!(
            err.message(),
            "Device does not support bulk-IN terminating character"
        );
        assert_eq!(drv.term_char(), None);

        // GET_CAPABILITIES reports TermChar support in byte 5.
        let mut caps = [0u8; 0x18];
        caps[GET_CAPABILITIES_DEVICE_CAP_OFFSET] = TMC_CAP_TERM_CHAR;
        drv.apply_capabilities(&caps).unwrap();

        drv.set_input_eos(&user, b"\n").unwrap();
        assert_eq!(drv.term_char(), Some(b'\n'));
        assert_eq!(drv.get_input_eos(&user), b"\n".to_vec());
        assert_eq!(
            build_request_bulk_in_header(1, 512, drv.term_char())[8],
            BULK_IN_TERM_CHAR_ENABLED,
            "the stored terminator has to reach the request header"
        );

        // Two characters cannot be represented in the header at all.
        let err = drv
            .set_input_eos(&user, b"\r\n")
            .expect_err("USBTMC holds one terminating character");
        assert_eq!(
            err.message(),
            "USBTMC does not support multiple input terminating characters"
        );
        assert_eq!(drv.term_char(), Some(b'\n'), "a refusal must not clear it");

        // eoslen 0 is C's `termChar = -1`.
        drv.set_input_eos(&user, b"").unwrap();
        assert_eq!(drv.term_char(), None);
        assert!(drv.get_input_eos(&user).is_empty());
    }

    #[test]
    fn b_tag_wraps_at_ff_to_1_not_0() {
        // USBTMC bTag must be non-zero; C wraps 0xFF → 1 (NOT 0).
        assert_eq!(next_b_tag(0x01), 0x02);
        assert_eq!(next_b_tag(0xFE), 0xFF);
        assert_eq!(next_b_tag(0xFF), 0x01);
    }

    #[test]
    fn pad4_rounds_up_to_4_byte_boundary() {
        assert_eq!(pad4(0), 0);
        assert_eq!(pad4(1), 4);
        assert_eq!(pad4(2), 4);
        assert_eq!(pad4(3), 4);
        assert_eq!(pad4(4), 4);
        assert_eq!(pad4(5), 8);
        assert_eq!(pad4(BULK_IO_HEADER_SIZE), BULK_IO_HEADER_SIZE);
        // A 100-byte payload + 12-byte header = 112 — already aligned.
        assert_eq!(pad4(112), 112);
        // 113 → 116.
        assert_eq!(pad4(113), 116);
    }

    #[test]
    fn config_no_auto_connect_flag_bit() {
        // C drvAsynUSBTMC.c:1275 — `(flags & 0x1) == 0` argument to
        // registerPort enables auto-connect. So flags & 0x1 == 1 →
        // no auto-connect.
        let cfg = UsbtmcConfig::from_positional(0x0957, 0x0407, "MY01234", 0, 0);
        assert!(!cfg.no_auto_connect());
        let cfg = UsbtmcConfig::from_positional(0x0957, 0x0407, "MY01234", 0, 0x1);
        assert!(cfg.no_auto_connect());
    }

    #[test]
    fn configure_records_all_positional_fields() {
        let drv =
            DrvAsynUsbtmcPort::configure("usbtmc0", 0x0957, 0x0407, "MY12345", 50, 0).unwrap();
        assert_eq!(drv.config().vendor_id, 0x0957);
        assert_eq!(drv.config().product_id, 0x0407);
        assert_eq!(drv.config().serial_number, "MY12345");
        assert_eq!(drv.config().priority, 50);
        assert_eq!(drv.config().flags, 0);
        // bTag starts at 1 (C drvAsynUSBTMC.c:1260).
        assert_eq!(drv.current_b_tag(), 1);
        // Auto-connect: flags=0 → auto on.
        assert!(drv.base().auto_connect);
    }

    #[test]
    fn configure_flags_bit0_disables_auto_connect() {
        let drv = DrvAsynUsbtmcPort::configure(
            "usbtmc0",
            0x0957,
            0x0407,
            "",
            0,
            USBTMC_FLAG_NO_AUTO_CONNECT,
        )
        .unwrap();
        assert!(!drv.base().auto_connect);
    }

    // Only meaningful in a build without the hardware feature — with
    // `usbtmc` enabled `connect()` reaches the (unimplemented) HW path.
    #[cfg(not(feature = "usbtmc"))]
    #[test]
    fn connect_without_hw_feature_reports_error() {
        let mut drv = DrvAsynUsbtmcPort::configure("usbtmc0", 0x0957, 0x0407, "", 0, 0).unwrap();
        let err = drv.connect(&AsynUser::default()).unwrap_err();
        match err {
            AsynError::Status { message, .. } => {
                assert!(
                    message.contains("usbtmc"),
                    "must mention feature: {message}"
                );
            }
            _ => panic!("expected Status error"),
        }
    }

    #[test]
    fn has_hw_support_matches_feature_flag() {
        assert_eq!(
            DrvAsynUsbtmcPort::has_hw_support(),
            cfg!(feature = "usbtmc")
        );
    }
}
