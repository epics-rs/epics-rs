//! COM interpose — RFC 2217 telnet COM-port-option negotiation for remote
//! serial ports reached over TCP (a "terminal server" / serial-to-ethernet
//! converter).
//!
//! Wire-faithful port of C `asynInterposeCom.c` (856 lines). C installs it from
//! `drvAsynIPPort.c:1061` when the configure string's protocol token is `COM`;
//! [`crate::drivers::ip_port::DrvAsynIPPort`] does the same.
//!
//! The C file interposes **two** interfaces on the port, and this module keeps
//! that split rather than fusing them, because the state they touch is
//! disjoint:
//!
//! * the `asynOctet` interface — IAC (0xFF) doubling on write and unstuffing on
//!   read ([`ComInterpose`], C `writeIt`/`readIt`, :136-245). Pure byte
//!   transformation: C's only per-port state here is the `xBuf` scratch buffer.
//! * the `asynOption` interface — the telnet negotiation and the serial-line
//!   settings it carries ([`ComPortOptions`], C `willdo` / `sbComPortOption` /
//!   `setOption` / `getOption` / `restoreSettings`, :327-758).
//!
//! # The negotiation bypasses the octet interpose
//!
//! Every negotiation byte C sends goes to `pinterposePvt->pasynOctetDrv->write`
//! — the driver *below* the interpose (:339, :430) — and every byte it reads
//! comes from `pasynOctetDrv->read` (`nextChar`, :103). So the IAC framing of
//! the negotiation is **not** IAC-stuffed by `writeIt`, and the replies are
//! **not** unstuffed by `readIt`. [`ComPortOptions`] therefore takes the lower
//! [`OctetNext`] link directly; it is not reachable through the interpose
//! stack, exactly as in C.
//!
//! One consequence in C is a real defect, and this port does NOT reproduce it
//! (CBUG-B8): because the negotiation bypasses `writeIt`, C never IAC-stuffs the
//! subnegotiation PAYLOAD, so a payload byte that happens to equal 0xFF — a baud
//! rate whose big-endian encoding contains 0xFF, e.g. `baud=255`; a line state of
//! 0xFF — goes out raw and a compliant RFC-2217 server reads it as a command
//! byte and desynchronises. Here the payload is escaped on the way out
//! (`write_subnegotiation`) and un-escaped on the way in (`next_payload_char`),
//! which is RFC 2217 §3. The IAC bytes of the FRAMING (`IAC SB … IAC SE`) are
//! commands and stay raw, in C and here alike.

use std::time::Duration;

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::interpose::{EomReason, OctetInterpose, OctetNext, OctetReadResult};
use crate::trace::TraceMask;
use crate::user::AsynUser;

// TELNET special characters — C asynInterposeCom.c:32-38.
/// "Interpret As Command".
pub const IAC: u8 = 255;
pub const DONT: u8 = 254;
pub const DO: u8 = 253;
pub const WONT: u8 = 252;
pub const WILL: u8 = 251;
/// Subnegotiation Begin.
pub const SB: u8 = 250;
/// Subnegotiation End.
pub const SE: u8 = 240;

/// Will/Do transmit binary — C :40.
const WD_TRANSMIT_BINARY: u8 = 0;

/// Subnegotiation command port option — C :42.
const SB_COM_PORT_OPTION: u8 = 44;

// COM-PORT-OPTION subcommands — C :43-62.
const CPO_SET_BAUDRATE: u8 = 1;
const CPO_SET_DATASIZE: u8 = 2;
const CPO_SET_PARITY: u8 = 3;
const CPO_PARITY_NONE: u8 = 1;
const CPO_PARITY_ODD: u8 = 2;
const CPO_PARITY_EVEN: u8 = 3;
const CPO_PARITY_MARK: u8 = 4;
const CPO_PARITY_SPACE: u8 = 5;
const CPO_SET_STOPSIZE: u8 = 4;
const CPO_SET_CONTROL: u8 = 5;
const CPO_CONTROL_NOFLOW: u8 = 1;
const CPO_CONTROL_IXON: u8 = 2;
const CPO_CONTROL_HWFLOW: u8 = 3;
const CPO_CONTROL_BREAK_ON: u8 = 5;
const CPO_CONTROL_BREAK_OFF: u8 = 6;
const CPO_SET_MODEMSTATE_MASK: u8 = 11;
const CPO_SERVER_NOTIFY_LINESTATE: u8 = 106;
const CPO_SERVER_NOTIFY_MODEMSTATE: u8 = 107;

/// A server's reply to COM-PORT-OPTION subcommand `n` is `n + 100` — C :450.
const CPO_REPLY_OFFSET: i32 = 100;

/// C `nextChar`'s `EOF` (:105). `int` in C, so a failing read is distinguishable
/// from the byte 0xFF, which is why this type is `i32` and not `u8`.
const EOF: i32 = -1;

/// The timeout on the interpose's *own* `asynUser` (`asynInterposeCom.c:836`).
///
/// It bounds the two negotiations the interpose runs for itself — `restoreSettings`
/// at configure time and from the reconnect `exceptionHandler` — and nothing else.
/// A negotiation driven by a *caller* (an asynRecord option put, an iocsh
/// `asynSetOption`) runs under that caller's `asynUser`, whose timeout C threads
/// all the way down through `setOption` → `sbComPortOption` → `nextChar` (:475,
/// :417-431, :95-106).
const INTERPOSE_USER_TIMEOUT: Duration = Duration::from_secs(2);

/// The interpose's own `asynUser` — C `asynInterposeCom.c:833-836`.
fn interpose_user() -> AsynUser {
    AsynUser::new(0).with_timeout(INTERPOSE_USER_TIMEOUT)
}

/// The option keys C's COM interpose claims — `setOption` (:481-643) and
/// `getOption` (:664-708) test the same seven with `epicsStrCaseCmp`. Anything
/// else falls through to the option interface of the driver below (:645-652,
/// :710-717).
const COM_OPTION_KEYS: [&str; 7] = ["baud", "bits", "parity", "stop", "crtscts", "ixon", "break"];

/// C `%#x` — lowercase hex with an `0x` prefix. printf's `#` flag adds the
/// prefix only for a nonzero value, so 0 prints bare.
fn hash_hex_lower(v: i32) -> String {
    if v == 0 {
        "0".to_string()
    } else {
        format!("0x{v:x}")
    }
}

/// C `%#X` — the uppercase form, used by `expectChar` (:121) and `getOption`'s
/// unknown-flow-code diagnostic (:689).
fn hash_hex_upper(v: i32) -> String {
    if v == 0 {
        "0".to_string()
    } else {
        format!("0X{v:X}")
    }
}

/// C `sscanf(val, "%d", &x)`: skip leading whitespace, take an optional sign and
/// then a run of decimal digits, stopping at the first character that cannot
/// continue the number. A match of zero digits is `sscanf` returning 0, i.e. C's
/// "Bad number" branch. `"9600 baud"` therefore reads 9600, and `"abc"` reads
/// nothing.
///
/// Deviation: a digit run that overflows `i32` is undefined behaviour in C; here
/// it is rejected as "Bad number" rather than silently wrapping.
fn scan_int(s: &str) -> Option<i32> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && epics_libcom_rs::runtime::stdlib::c_isspace(b[i] as char) {
        i += 1;
    }
    let start = i;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let first_digit = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == first_digit {
        return None;
    }
    s[start..i].parse::<i32>().ok()
}

/// C `sscanf(val, "%u", &x)` — as [`scan_int`], into an unsigned.
///
/// Deviation: C's `%u` accepts a leading `-` and wraps ("-1" becomes
/// 4294967295, which as a break length would sleep for 49 days). A negative run
/// is rejected here as "Bad number".
fn scan_uint(s: &str) -> Option<u32> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && epics_libcom_rs::runtime::stdlib::c_isspace(b[i] as char) {
        i += 1;
    }
    let start = i;
    if i < b.len() && b[i] == b'+' {
        i += 1;
    }
    let first_digit = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == first_digit {
        return None;
    }
    s[start..i].parse::<u32>().ok()
}

/// C `sscanf(val, "%g", &x)` — the longest prefix that forms a float, after
/// leading whitespace. Implemented as C specifies it (longest valid prefix)
/// rather than by re-deriving the grammar, so `"1.5xyz"` reads 1.5 and the
/// `inf` / `nan` forms C's `%g` accepts are accepted here too.
fn scan_float(s: &str) -> Option<f32> {
    let t = s.trim_start();
    let mut end = t.len();
    while end > 0 {
        if t.is_char_boundary(end) {
            if let Ok(v) = t[..end].parse::<f32>() {
                return Some(v);
            }
        }
        end -= 1;
    }
    None
}

fn asyn_error(message: impl Into<String>) -> AsynError {
    AsynError::Status {
        status: AsynStatus::Error,
        message: message.into(),
    }
}

// ---------------------------------------------------------------------------
// asynOctet interface: IAC stuffing / unstuffing
// ---------------------------------------------------------------------------

/// The `asynOctet` half of C's COM interpose — IAC doubling on the way out,
/// IAC unstuffing on the way in (C `writeIt`/`readIt`, :136-245).
///
/// Carries no state: C's `interposePvt` fields that `writeIt` touches are only
/// the `xBuf`/`xBufCapacity` scratch buffer (:84-85), which exists to avoid a
/// per-write `malloc` and has no meaning across calls. The serial-line settings
/// live in [`ComPortOptions`], which the octet path never reads.
pub struct ComInterpose;

impl ComInterpose {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ComInterpose {
    fn default() -> Self {
        Self::new()
    }
}

/// C `nextChar` (:94-107) — one byte from the layer below, or `Err(message)` when
/// the read fails. C discards the failing `asynStatus` and keeps only the message
/// the lower layer left in `pasynUser->errorMessage`, which is what the caller's
/// `return asynError` then carries out. The message comes back to the caller here
/// because the two callers put it in different places: the negotiation lands it in
/// the user's message slot (C's buffer), while `readIt` discards it and reports
/// "Missing IAC" instead (:226-230).
///
/// Deviation: C does not check `nbytes`, so a lower read that reports success
/// having transferred nothing leaves `char c` uninitialized and C reads garbage.
/// That is treated as EOF here.
fn next_char(next: &mut dyn OctetNext, user: &AsynUser) -> Result<u8, String> {
    let mut c = [0u8; 1];
    match next.read(user, &mut c) {
        Ok(r) if r.nbytes_transferred >= 1 => Ok(c[0]),
        Ok(_) => Err("no data".into()),
        Err(e) => Err(e.message()),
    }
}

impl OctetInterpose for ComInterpose {
    /// C `readIt` (:197-245) — unstuff doubled IACs in place.
    ///
    /// The loop is C's, index-for-pointer. Two arms:
    ///
    /// * the IAC is *not* the last valid byte: its partner is already in the
    ///   buffer, so the count drops by one and the tail slides down over it.
    /// * the IAC *is* the last valid byte: its partner has not arrived yet, so C
    ///   pulls one more byte straight from the device (`nextChar`, bypassing this
    ///   layer) and steps `iac` back one — which makes the `nCheck` arithmetic
    ///   land on zero and end the loop. The count does **not** drop, because the
    ///   partner was never in the buffer to begin with.
    ///
    /// Either way the partner must be another IAC; C rejects anything else with
    /// "Missing IAC" rather than interpreting it as a telnet command, so an
    /// unsolicited `IAC WILL x` arriving mid-stream is an error, not a
    /// negotiation. That refusal is the contract, quirk and all.
    fn read(
        &mut self,
        user: &AsynUser,
        buf: &mut [u8],
        next: &mut dyn OctetNext,
    ) -> AsynResult<OctetReadResult> {
        let maxchars = buf.len();
        let r = next.read(user, buf)?;
        let mut n_read = r.nbytes_transferred;
        let mut eom = r.eom_reason;

        // C's `data` cursor and `nCheck` span. `iac` goes signed because the
        // last-byte arm decrements it below `data` (:220), and `data` itself can
        // be the start of the buffer.
        let mut d: isize = 0;
        let mut n_check: isize = n_read as isize;
        let mut unstuffed = false;

        while n_check > 0 {
            let span = &buf[d as usize..(d + n_check) as usize];
            let Some(rel) = span.iter().position(|&b| b == IAC) else {
                break;
            };
            let mut iac = d + rel as isize;

            unstuffed = true;
            // C :217 — any unstuffing clears CNT: the count the base layer
            // reported is no longer the count being returned.
            eom.remove(EomReason::CNT);

            let c = if iac == d + n_check - 1 {
                // :218-221 — partner not yet read; pull it from the device. C
                // keeps the lower layer's message in `pasynUser->errorMessage`
                // here, but the only exit from this arm overwrites it with
                // "Missing IAC" (:226-230), so the message is dropped on the
                // floor exactly as C drops it.
                let c = next_char(next, user).map_or(EOF, i32::from);
                iac -= 1;
                c
            } else {
                // :222-225 — partner is in the buffer; it is about to be dropped.
                let c = i32::from(buf[(iac + 1) as usize]);
                n_read -= 1;
                c
            };
            if c != i32::from(IAC) {
                // :226-230 — C overwrites the lower layer's message with this
                // one, including when `c` was EOF.
                return Err(asyn_error("Missing IAC"));
            }

            // :231-232
            n_check -= (iac - d) + 2;
            d = iac + 1;
            if n_check == 0 {
                break;
            }
            // :235 — memmove(data, data + 1, nCheck): slide the tail down over
            // the escape byte. Bounds hold because `d + n_check` is invariant
            // under this update minus one, and started at `n_read <= buf.len()`.
            let dst = d as usize;
            buf.copy_within(dst + 1..dst + 1 + n_check as usize, dst);
        }

        // :237-239 — a read that unstuffed anything is traced at
        // ASYN_TRACEIO_FILTER, with the buffer as it now stands. The layer has no
        // handle on the port; the trace config comes through the asynUser, as it
        // does in C (`asynPrintIO(pasynUser, …)`).
        if unstuffed {
            user.print_io(
                TraceMask::IO_FILTER,
                &buf[..n_read],
                &format!("nRead {n_read} after IAC unstuffing"),
            );
        }

        // :240-241 — restore CNT if, after unstuffing, the buffer is still full.
        if n_read == maxchars {
            eom.insert(EomReason::CNT);
        }
        Ok(OctetReadResult {
            nbytes_transferred: n_read,
            eom_reason: eom,
        })
    }

    /// C `writeIt` (:136-195) — double every IAC, then report the count the
    /// *caller* handed us, not the stuffed count.
    ///
    /// C only subtracts the stuffing back out when the lower layer took the
    /// whole stuffed buffer (`if (*nbytesTransfered == numchars)`, :192-193): a
    /// short write reports a count in stuffed-byte units, which the caller reads
    /// as un-stuffed bytes. That subtraction is unreachable on the failure path
    /// (a lower write that transferred every byte returns `asynSuccess`), so a
    /// failing write propagates its partial count untouched, as C's does.
    fn write(
        &mut self,
        user: &mut AsynUser,
        data: &[u8],
        next: &mut dyn OctetNext,
    ) -> AsynResult<usize> {
        let n_iac = data.iter().filter(|&&b| b == IAC).count();
        if n_iac == 0 {
            // C :146 — no IAC in the payload, so `data` goes down verbatim and
            // `nIAC` stays 0, making the tail adjustment a no-op.
            return next.write(user, data);
        }
        let mut stuffed = Vec::with_capacity(data.len() + n_iac);
        for &b in data {
            stuffed.push(b);
            if b == IAC {
                stuffed.push(IAC);
            }
        }
        let n = next.write(user, &stuffed)?;
        Ok(if n == stuffed.len() { n - n_iac } else { n })
    }

    fn flush(&mut self, user: &mut AsynUser, next: &mut dyn OctetNext) -> AsynResult<()> {
        // C `flushIt` (:247-253) — straight delegation.
        next.flush(user)
    }
}

// ---------------------------------------------------------------------------
// asynOption interface: the telnet negotiation
// ---------------------------------------------------------------------------

/// The negotiation's view of the link below the interpose, and the `asynUser`
/// whose message slot every C layer writes into.
///
/// The slot is [`AsynUser::error_message`] — C's `pasynUser->errorMessage`
/// itself, not a copy: `nextChar` leaves the lower read's message there on EOF
/// (:103-106), `expectChar` overwrites it on a mismatch (:120-122), `setOption`
/// leaves a *non-failing* advisory there (:571-573, :587-589), and each `return
/// asynError` carries out whatever it currently holds. Keeping the negotiation's
/// messages in the caller's user is what lets a failing read surface the *lower
/// driver's* message with C's `asynError` status, and what gives the advisories
/// somewhere to live at all — a `Result` cannot carry a message that does not
/// fail the call.
struct TelnetLink<'a> {
    next: &'a mut dyn OctetNext,
    /// The `asynUser` C threads through the whole negotiation — the *caller's*
    /// on `setOption` (an asynRecord option put carries TMOT; an iocsh
    /// `asynSetOption` carries its own 2 s, `asynShellCommands.c:119`), the
    /// interpose's own on `restoreSettings` ([`interpose_user`]). Its `timeout`
    /// bounds every wire read the handshake performs, so the negotiation has no
    /// timeout of its own to disagree with the caller's.
    user: &'a mut AsynUser,
}

impl<'a> TelnetLink<'a> {
    fn new(next: &'a mut dyn OctetNext, user: &'a mut AsynUser) -> Self {
        Self { next, user }
    }

    fn next_char(&mut self) -> i32 {
        match next_char(self.next, self.user) {
            Ok(b) => i32::from(b),
            Err(msg) => {
                self.user.error_message = msg;
                EOF
            }
        }
    }

    /// C's `epicsSnprintf(pasynUser->errorMessage, …)` on a path that does *not*
    /// return an error (:571-573, :587-589): the message is left for the caller
    /// to find and the call carries on.
    fn advise(&mut self, msg: &str) {
        self.user.error_message = msg.to_string();
    }

    /// C `expectChar` (:112-125) — fetch and compare. Returns false on EOF
    /// *without* touching the message (so the lower read's message survives),
    /// and false-with-message on a mismatch.
    fn expect_char(&mut self, expect: u8) -> bool {
        let c = self.next_char();
        if c == EOF {
            return false;
        }
        if c != i32::from(expect) {
            self.user.error_message = format!(
                "Expected {}, got {}",
                hash_hex_upper(i32::from(expect)),
                hash_hex_upper(c)
            );
            return false;
        }
        true
    }

    /// One PAYLOAD byte of a subnegotiation, with RFC 2217's IAC escape undone:
    /// a data byte equal to 0xFF travels as `IAC IAC`.
    ///
    /// DEVIATION from C, deliberate — CBUG-B8. See [`Self::write_subnegotiation`]
    /// for the write half. C reads every negotiation byte with a bare `nextChar`
    /// (:103), so a compliant server's escaped 0xFF is read as two bytes: the
    /// value byte comes out right but the frame is one byte long and the trailing
    /// `IAC SE` check fails. Reading the payload through this — the same escape
    /// rule the write half applies — is what makes a 0xFF round-trip at all.
    ///
    /// An IAC followed by anything else is a command in the middle of a payload:
    /// a framing error, reported as such rather than silently taken as data.
    fn next_payload_char(&mut self) -> i32 {
        let c = self.next_char();
        if c != i32::from(IAC) {
            return c;
        }
        let c2 = self.next_char();
        if c2 == i32::from(IAC) {
            return i32::from(IAC);
        }
        if c2 != EOF {
            self.user.error_message = format!(
                "Unescaped IAC in a COM-PORT-OPTION payload, followed by {}",
                hash_hex_upper(c2)
            );
        }
        EOF
    }

    /// C's `return asynError` — the status is always `asynError`, the message is
    /// whatever is in the slot.
    fn error(&self) -> AsynError {
        asyn_error(self.user.error_message.clone())
    }

    /// `IAC SB 44 <payload, IAC-stuffed> IAC SE` — RFC 2217 §3 framing.
    ///
    /// DEVIATION from C, deliberate — CBUG-B8. C builds this frame at :424-431
    /// and hands it to `pasynOctetDrv->write`, the driver BELOW the interpose,
    /// so it never passes through the interpose's own `writeIt` (:146-182) —
    /// the function that doubles IAC bytes. The payload is exactly where a 0xFF
    /// can occur: `CPO_SET_BAUDRATE` sends the rate as four big-endian bytes
    /// (:491), so `asynSetOption(port, 0, "baud", "255")` puts a raw 0xFF into
    /// the payload and a compliant terminal server reads it as a command byte
    /// and desynchronises. Only the payload is stuffed here; the IAC bytes of
    /// the framing itself are commands and must stay raw.
    fn write_subnegotiation(&mut self, payload: &[u8]) -> AsynResult<usize> {
        let mut cbuf = Vec::with_capacity(5 + payload.len());
        cbuf.extend_from_slice(&[IAC, SB, SB_COM_PORT_OPTION]);
        for &b in payload {
            cbuf.push(b);
            if b == IAC {
                cbuf.push(IAC);
            }
        }
        cbuf.extend_from_slice(&[IAC, SE]);
        self.write(&cbuf)
    }

    fn write(&mut self, bytes: &[u8]) -> AsynResult<usize> {
        self.next.write(self.user, bytes)
    }

    /// C `willdo` (:327-411). Two shapes depending on `command`:
    ///
    /// 1. `WILL x` — tell the server we will do x, and require it to answer
    ///    `DO x`. A `DONT x` is a refusal; a `WILL`/`WONT x` echoed back is a
    ///    protocol error ("Received response ... in response to WILL").
    /// 2. `DO x` — tell the server to do x, and require it to answer `WILL x`.
    ///    Mirror-image errors.
    ///
    /// Everything else on the wire is skipped: a reply about a *different*
    /// option code is ignored and the scan resumes, a bare `IAC`/`IAC SE` is
    /// ignored, and an `IAC SB 44 <NOTIFY-LINESTATE|NOTIFY-MODEMSTATE> <state>`
    /// is consumed and discarded — the server may volunteer those at any point.
    /// A byte that is none of the above ends the negotiation with "Unexpected
    /// character".
    fn willdo(&mut self, command: u8, code: u8) -> AsynResult<()> {
        // :336-340
        self.write(&[IAC, command, code])?;
        loop {
            // :344-346 — skip to the next IAC.
            loop {
                let c = self.next_char();
                if c == EOF {
                    return Err(self.error());
                }
                if c == i32::from(IAC) {
                    break;
                }
            }
            let c = self.next_char();
            if c == EOF {
                return Err(self.error());
            }
            match c as u8 {
                // :349-350 — a doubled IAC or a stray SE: resume scanning.
                IAC | SE => {}
                DO | DONT => {
                    // :352-370
                    let wd = c as u8;
                    let opt = self.next_char();
                    if opt == EOF {
                        return Err(self.error());
                    }
                    if opt != i32::from(code) {
                        continue;
                    }
                    if command == DO {
                        self.user.error_message = format!(
                            "Received response {} in response to DO.",
                            hash_hex_lower(opt)
                        );
                        return Err(self.error());
                    }
                    if wd == DONT {
                        self.user.error_message =
                            format!("Device says DON'T {}.", hash_hex_lower(opt));
                        return Err(self.error());
                    }
                    return Ok(());
                }
                WILL | WONT => {
                    // :372-390
                    let wd = c as u8;
                    let opt = self.next_char();
                    if opt == EOF {
                        return Err(self.error());
                    }
                    if opt != i32::from(code) {
                        continue;
                    }
                    if command == WILL {
                        self.user.error_message = format!(
                            "Received response {} in response to WILL.",
                            hash_hex_lower(opt)
                        );
                        return Err(self.error());
                    }
                    if wd == WONT {
                        self.user.error_message =
                            format!("Device says WON'T {}.", hash_hex_lower(opt));
                        return Err(self.error());
                    }
                    return Ok(());
                }
                SB => {
                    // :392-403. Note that C does not error out on EOF at :393 —
                    // EOF is simply not SB_COM_PORT_OPTION, so it breaks back to
                    // the scan loop, whose own `nextChar` then returns EOF and
                    // errors. Same outcome, one read later.
                    if self.next_char() != i32::from(SB_COM_PORT_OPTION) {
                        continue;
                    }
                    let c = self.next_char();
                    if c != i32::from(CPO_SERVER_NOTIFY_LINESTATE)
                        && c != i32::from(CPO_SERVER_NOTIFY_MODEMSTATE)
                    {
                        if c == EOF {
                            return Err(self.error());
                        }
                        continue;
                    }
                    // The state byte — payload, so IAC-escaped (CBUG-B8; a line
                    // state of 0xFF is a legal value). C consumes exactly one
                    // raw byte and does *not* consume the trailing IAC SE here —
                    // it lets the scan loop step over them (the IAC is found, and
                    // SE hits the `case C_SE: break` arm).
                    if self.next_payload_char() == EOF {
                        return Err(self.error());
                    }
                }
                _ => {
                    // :405-408
                    self.user.error_message =
                        format!("Unexpected character {} in TELNET reply", hash_hex_lower(c));
                    return Err(self.error());
                }
            }
        }
    }

    /// C `sbComPortOption` (:416-469) — send `IAC SB 44 <x…> IAC SE` and collect
    /// the server's `x.len() - 1` reply bytes into `r`.
    ///
    /// The server acknowledges subcommand `n` with `n + 100` (:450), echoing the
    /// value it actually applied — which may differ from the one requested, and
    /// which the callers below then check. An `IAC SB 44 <NOTIFY-…> <state> IAC
    /// SE` arriving instead is consumed and the wait resumes; any other reply
    /// code is an error.
    ///
    /// The payload `x` is IAC-stuffed and the reply payload is un-stuffed —
    /// CBUG-B8, a deliberate deviation from C, which does neither. See
    /// [`Self::write_subnegotiation`] and [`Self::next_payload_char`].
    fn sb_com_port_option(&mut self, x: &[u8], r: &mut [u8]) -> AsynResult<()> {
        debug_assert!(!x.is_empty() && r.len() >= x.len() - 1);
        // :424-431
        self.write_subnegotiation(x)?;

        loop {
            // :435-438
            loop {
                let c = self.next_char();
                if c == EOF {
                    return Err(self.error());
                }
                if c == i32::from(IAC) {
                    break;
                }
            }
            // :439-441
            if !self.expect_char(SB) || !self.expect_char(SB_COM_PORT_OPTION) {
                return Err(self.error());
            }
            let c = self.next_char();
            if c == i32::from(CPO_SERVER_NOTIFY_LINESTATE)
                || c == i32::from(CPO_SERVER_NOTIFY_MODEMSTATE)
            {
                // :443-449 — an unsolicited line/modem-state notification:
                // <state> IAC SE, discarded, and we keep waiting for our reply.
                // The state is payload, so it carries the IAC escape (CBUG-B8).
                if self.next_payload_char() == EOF
                    || !self.expect_char(IAC)
                    || !self.expect_char(SE)
                {
                    return Err(self.error());
                }
            } else if c == i32::from(x[0]) + CPO_REPLY_OFFSET {
                // :450-459 — `while (--xLen > 0)`: one reply byte per payload
                // byte after the subcommand. These are DATA, so they carry the
                // IAC escape (CBUG-B8) — C reads them raw.
                for slot in r.iter_mut().take(x.len() - 1) {
                    let b = self.next_payload_char();
                    if b == EOF {
                        return Err(self.error());
                    }
                    *slot = b as u8;
                }
                if !self.expect_char(IAC) || !self.expect_char(SE) {
                    return Err(self.error());
                }
                return Ok(());
            } else {
                // :461-465 — C prints `c` with %d, so an EOF here reports -1.
                self.user.error_message =
                    format!("Sent COM-PORT-OPTION {} but got reply {}", x[0], c);
                return Err(self.error());
            }
        }
    }
}

/// The `asynOption` half of C's COM interpose: the serial-line settings the
/// negotiation carries, and the negotiation itself.
///
/// The fields are C's `interposePvt` option state (:77-82) and hold what the
/// **server** last acknowledged, not what was last requested — every setter
/// stores the value echoed back in the reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComPortOptions {
    baud: i32,
    bits: i32,
    /// One of the `CPO_PARITY_*` codes.
    parity: u8,
    stop: i32,
    /// One of the `CPO_CONTROL_*` codes.
    flow: u8,
    break_active: bool,
}

impl Default for ComPortOptions {
    /// C :837-841 — the defaults `asynInterposeCOM` installs before its first
    /// `restoreSettings`.
    fn default() -> Self {
        Self {
            baud: 9600,
            bits: 8,
            parity: CPO_PARITY_NONE,
            stop: 1,
            flow: CPO_CONTROL_NOFLOW,
            break_active: false,
        }
    }
}

impl ComPortOptions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this layer answers `key` itself. A key outside the set falls
    /// through to the option interface of the driver below, which is what C does
    /// at :645-652 (`setOption`) and :710-717 (`getOption`).
    pub fn owns_key(key: &str) -> bool {
        COM_OPTION_KEYS.iter().any(|k| key.eq_ignore_ascii_case(k))
    }

    /// C `getOption` (:657-725). Reads back cached state only — no wire traffic,
    /// so this needs no link.
    ///
    /// Caller must have checked [`Self::owns_key`]; a key outside the set is C's
    /// delegate-to-lower-driver branch and is not this function's to answer.
    pub fn get_option(&self, key: &str) -> AsynResult<String> {
        if key.eq_ignore_ascii_case("baud") {
            Ok(self.baud.to_string())
        } else if key.eq_ignore_ascii_case("bits") {
            Ok(self.bits.to_string())
        } else if key.eq_ignore_ascii_case("parity") {
            // :671-677 — C's switch has no default: a parity code outside the
            // five leaves `val` untouched (`l` stays 0) and still returns
            // asynSuccess, handing the caller back an unwritten buffer. "Nothing
            // written" is the empty string here. It is reachable — `parity` holds
            // whatever the server echoed — and either way the value fails the
            // next `setOption("parity", …)` with "Invalid parity selection".
            Ok(match self.parity {
                CPO_PARITY_NONE => "none",
                CPO_PARITY_EVEN => "even",
                CPO_PARITY_ODD => "odd",
                CPO_PARITY_MARK => "mark",
                CPO_PARITY_SPACE => "space",
                _ => "",
            }
            .to_string())
        } else if key.eq_ignore_ascii_case("stop") {
            Ok(self.stop.to_string())
        } else if key.eq_ignore_ascii_case("crtscts") {
            // :682-691 — XON/XOFF reports as "no hardware flow control".
            match self.flow {
                CPO_CONTROL_NOFLOW | CPO_CONTROL_IXON => Ok("N".to_string()),
                CPO_CONTROL_HWFLOW => Ok("Y".to_string()),
                other => Err(asyn_error(format!(
                    "Unknown flow control code {}",
                    hash_hex_upper(i32::from(other))
                ))),
            }
        } else if key.eq_ignore_ascii_case("ixon") {
            // :693-703 — and hardware flow control reports as "no XON/XOFF".
            match self.flow {
                CPO_CONTROL_NOFLOW | CPO_CONTROL_HWFLOW => Ok("N".to_string()),
                CPO_CONTROL_IXON => Ok("Y".to_string()),
                other => Err(asyn_error(format!(
                    "Unknown flow control code {}",
                    hash_hex_upper(i32::from(other))
                ))),
            }
        } else if key.eq_ignore_ascii_case("break") {
            // :704-708
            Ok(if self.break_active { "on" } else { "off" }.to_string())
        } else {
            Err(AsynError::OptionNotFound(key.to_string()))
        }
    }

    /// The SET-CONTROL byte to transmit when `key_mode` is being turned OFF.
    ///
    /// **DEVIATION from C, deliberate — CBUG-B6.** C's `crtscts` and `ixon`
    /// branches both implement "n" as `xBuf[1] = pinterposePvt->flow`
    /// (`asynInterposeCom.c:575`, `:591`) — the value transmitted for "turn this
    /// off" is the port's **current** flow-control mode. So if RTS/CTS is on and
    /// you send `crtscts N`, C re-transmits SET-CONTROL HWFLOW, the server
    /// confirms HWFLOW, `:578` writes it back into `flow`, and `getOption` still
    /// answers "Y". Flow control can be turned on and never off; recovery needs
    /// an IOC restart or a terminal-server power cycle.
    ///
    /// `CPO_CONTROL_NOFLOW` ("No flow control", `:53`) is defined and is decoded
    /// in `getOption` (`:684`, `:695`) — it is simply never assigned to
    /// `xBuf[1]` anywhere in the file. Transmitting it is the intended
    /// behaviour, and it is what this does.
    ///
    /// The mode byte carries ONE flow-control mode, so the modes are mutually
    /// exclusive (which is why C advises "XON/XOFF already set. Now using
    /// RTS/CTS."). Disabling therefore means: if `key_mode` is what is currently
    /// in effect, turn flow control off; if some other mode is in effect, this
    /// key's mode is already off and the other one is not ours to touch.
    fn flow_mode_off(&self, key_mode: u8) -> u8 {
        if self.flow == key_mode {
            CPO_CONTROL_NOFLOW
        } else {
            self.flow
        }
    }

    /// C `setOption` (:474-655) — negotiate `key`/`val` with the server over
    /// `next`, the octet driver *below* the interpose, bounded by `user`'s
    /// timeout.
    ///
    /// `user` is the caller's `asynUser`, as in C: `setOption(drvPvt, pasynUser,
    /// key, val)` hands it to `sbComPortOption` (:495), which writes and reads the
    /// wire with it (:431, :435-457). An asynRecord option put therefore
    /// negotiates under TMOT and an iocsh `asynSetOption` under its own 2 s
    /// (`asynShellCommands.c:119`) — the layer has no timeout of its own.
    ///
    /// Caller must have checked [`Self::owns_key`].
    pub fn set_option(
        &mut self,
        user: &mut AsynUser,
        next: &mut dyn OctetNext,
        key: &str,
        val: &str,
    ) -> AsynResult<()> {
        let mut link = TelnetLink::new(next, user);
        self.set_option_on(&mut link, key, val)
    }

    /// C `restoreSettings` (:729-758) — the full handshake, run at connect and on
    /// every reconnect (C wires it to `asynExceptionConnect`, :763-774).
    ///
    /// `IAC DO BINARY`, `IAC WILL BINARY`, `IAC WILL COM-PORT-OPTION`, then a
    /// SET-MODEMSTATE-MASK of 0 (asking the server not to volunteer modem-state
    /// notifications), and finally each cached setting is read back out and
    /// pushed to the server — which is how the line comes up configured after a
    /// terminal server reboots. `break` is deliberately not in C's key list: a
    /// break is a momentary line condition, not a setting to restore.
    /// Runs on the interpose's own `asynUser` (C creates it at :833-836 and hands
    /// it to `restoreSettings` from both the configure path (:851) and the
    /// reconnect `exceptionHandler` (:770)), so this is the one negotiation with a
    /// timeout of its own — 2 s — and no caller to inherit one from.
    pub fn restore_settings(&mut self, next: &mut dyn OctetNext) -> AsynResult<()> {
        let mut user = interpose_user();
        let mut link = TelnetLink::new(next, &mut user);
        self.restore_settings_on(&mut link)
    }

    fn restore_settings_on(&mut self, link: &mut TelnetLink) -> AsynResult<()> {
        // :740-747
        link.willdo(DO, WD_TRANSMIT_BINARY)?;
        link.willdo(WILL, WD_TRANSMIT_BINARY)?;
        link.willdo(WILL, SB_COM_PORT_OPTION)?;
        let mut r = [0u8; 1];
        link.sb_com_port_option(&[CPO_SET_MODEMSTATE_MASK, 0], &mut r)?;

        // :749-756 — note `break` is absent from C's list.
        for key in ["baud", "bits", "parity", "stop", "crtscts", "ixon"] {
            let val = self.get_option(key)?;
            self.set_option_on(link, key, &val)?;
        }
        Ok(())
    }

    fn set_option_on(&mut self, link: &mut TelnetLink, key: &str, val: &str) -> AsynResult<()> {
        if key.eq_ignore_ascii_case("baud") {
            // :481-508 — the rate goes out big-endian in four bytes, and the
            // server echoes the rate it actually applied. C fails the call when
            // they differ rather than silently running at the wrong speed.
            let Some(b) = scan_int(val) else {
                return Err(asyn_error("Bad number"));
            };
            let baud = b as u32;
            let x = [
                CPO_SET_BAUDRATE,
                (baud >> 24) as u8,
                (baud >> 16) as u8,
                (baud >> 8) as u8,
                baud as u8,
            ];
            let mut r = [0u8; 4];
            link.sb_com_port_option(&x, &mut r)?;
            self.baud = i32::from_be_bytes(r);
            if self.baud != b {
                return Err(asyn_error(format!(
                    "Tried to set {b} baud, actually set {} baud.",
                    self.baud
                )));
            }
            Ok(())
        } else if key.eq_ignore_ascii_case("bits") {
            // :509-528
            let Some(b) = scan_int(val) else {
                return Err(asyn_error("Bad number"));
            };
            let mut r = [0u8; 1];
            link.sb_com_port_option(&[CPO_SET_DATASIZE, b as u8], &mut r)?;
            self.bits = i32::from(r[0]);
            if self.bits != b {
                return Err(asyn_error(format!(
                    "Tried to set {b} bits, actually set {} bits.",
                    self.bits
                )));
            }
            Ok(())
        } else if key.eq_ignore_ascii_case("parity") {
            // :529-543 — and note C does *not* check the echo here, unlike baud,
            // bits and stop: whatever the server answers becomes the cached
            // parity, even if it is not what was asked for.
            let code = if val.eq_ignore_ascii_case("none") {
                CPO_PARITY_NONE
            } else if val.eq_ignore_ascii_case("even") {
                CPO_PARITY_EVEN
            } else if val.eq_ignore_ascii_case("odd") {
                CPO_PARITY_ODD
            } else if val.eq_ignore_ascii_case("mark") {
                CPO_PARITY_MARK
            } else if val.eq_ignore_ascii_case("space") {
                CPO_PARITY_SPACE
            } else {
                return Err(asyn_error("Invalid parity selection"));
            };
            let mut r = [0u8; 1];
            link.sb_com_port_option(&[CPO_SET_PARITY, code], &mut r)?;
            self.parity = r[0];
            Ok(())
        } else if key.eq_ignore_ascii_case("stop") {
            // :544-568 — parsed as a float (so "1.5" is *read*), then required to
            // be exactly 1 or 2, then truncated to a byte.
            let Some(b) = scan_float(val) else {
                return Err(asyn_error("Bad number"));
            };
            if b != 1.0 && b != 2.0 {
                // C's message has the double space.
                return Err(asyn_error("Bad  stop bit count"));
            }
            let mut r = [0u8; 1];
            link.sb_com_port_option(&[CPO_SET_STOPSIZE, b as u8], &mut r)?;
            self.stop = i32::from(r[0]);
            if self.stop as f32 != b {
                return Err(asyn_error(format!(
                    "Tried to set {b} stop bits, actually set {} stop bits.",
                    self.stop
                )));
            }
            Ok(())
        } else if key.eq_ignore_ascii_case("crtscts") {
            // :569-584 — "y" turns hardware flow control on. C's "n" re-sends the
            // *current* flow mode; see `flow_mode_for` — CBUG-B6.
            //
            // :571-573 — switching to RTS/CTS over a live XON/XOFF setting is
            // announced in the message slot and does *not* fail the call.
            if self.flow == CPO_CONTROL_IXON {
                link.advise("XON/XOFF already set. Now using RTS/CTS.");
            }
            let mode = if val.eq_ignore_ascii_case("n") {
                self.flow_mode_off(CPO_CONTROL_HWFLOW)
            } else if val.eq_ignore_ascii_case("y") {
                CPO_CONTROL_HWFLOW
            } else {
                return Err(asyn_error("Bad  value"));
            };
            let mut r = [0u8; 1];
            link.sb_com_port_option(&[CPO_SET_CONTROL, mode], &mut r)?;
            self.flow = r[0];
            Ok(())
        } else if key.eq_ignore_ascii_case("ixon") {
            // :585-604 — mirror of crtscts, advisory (:587-589) included.
            if self.flow == CPO_CONTROL_HWFLOW {
                link.advise("RTS/CTS already set. Now using XON/XOFF.");
            }
            let mode = if val.eq_ignore_ascii_case("n") {
                self.flow_mode_off(CPO_CONTROL_IXON)
            } else if val.eq_ignore_ascii_case("y") {
                CPO_CONTROL_IXON
            } else {
                // DEVIATION. C writes "Bad option value" into the message and
                // then *falls through* without returning (:593-596), sending a
                // subnegotiation whose payload byte is the uninitialized
                // `xBuf[1]` — undefined behaviour, and an arbitrary flow-control
                // mode on the wire. Refusing the value is the only behaviour that
                // is both defined and safe; there is nothing to be faithful to.
                return Err(asyn_error("Bad option value"));
            };
            let mut r = [0u8; 1];
            match link.sb_com_port_option(&[CPO_SET_CONTROL, mode], &mut r) {
                Ok(()) => {
                    self.flow = r[0];
                    Ok(())
                }
                Err(e) => {
                    // :601-603 — and only on this key does C print the failure to
                    // stdout as well as returning it.
                    println!("XON/XOFF not set.");
                    Err(e)
                }
            }
        } else if key.eq_ignore_ascii_case("break") {
            self.set_break(link, val)
        } else {
            Err(AsynError::OptionNotFound(key.to_string()))
        }
    }

    /// C `setOption`'s `break` arm (:605-643) — assert / release the line break.
    ///
    /// `"on"` and `"off"` are edge-triggered against the cached state (asserting
    /// an already-asserted break sends nothing). Anything else is read as a
    /// millisecond count and runs the whole cycle: assert, sleep, release —
    /// blocking the caller for the duration, as C's `epicsThreadSleep` does. An
    /// empty value takes that path too, with C's 250 ms default.
    ///
    /// The cached flag tracks what the *server* acknowledged: it is set from the
    /// reply byte, not from the request.
    fn set_break(&mut self, link: &mut TelnetLink, val: &str) -> AsynResult<()> {
        let (on, off, sleep_for) = if val.eq_ignore_ascii_case("on") {
            (!self.break_active, false, None)
        } else if val.eq_ignore_ascii_case("off") {
            (false, self.break_active, None)
        } else {
            let break_len = if val.is_empty() {
                0
            } else {
                let Some(n) = scan_uint(val) else {
                    return Err(asyn_error("Bad number"));
                };
                n
            };
            // :632 — a zero/absent length is 250 ms.
            let ms = if break_len == 0 { 250 } else { break_len };
            (!self.break_active, true, Some(ms))
        };

        if on {
            let mut r = [0u8; 1];
            link.sb_com_port_option(&[CPO_SET_CONTROL, CPO_CONTROL_BREAK_ON], &mut r)?;
            self.break_active = r[0] == CPO_CONTROL_BREAK_ON;
        }
        if let Some(ms) = sleep_for {
            std::thread::sleep(Duration::from_millis(u64::from(ms)));
        }
        if off {
            let mut r = [0u8; 1];
            link.sb_com_port_option(&[CPO_SET_CONTROL, CPO_CONTROL_BREAK_OFF], &mut r)?;
            // :640 — yes, C tests the reply against BREAK_ON here too, so a
            // server that echoes BREAK_OFF leaves the flag clear and one that
            // echoes BREAK_ON leaves it set. Same predicate as the assert arm.
            self.break_active = r[0] == CPO_CONTROL_BREAK_ON;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interpose::OctetInterposeStack;
    use std::collections::VecDeque;

    /// A scripted terminal server: records everything written, and replays a
    /// queued byte stream one byte at a time (which is how `nextChar` reads).
    struct FakeServer {
        written: Vec<u8>,
        to_read: VecDeque<u8>,
    }

    impl FakeServer {
        fn new(reply: &[u8]) -> Self {
            Self {
                written: Vec::new(),
                to_read: reply.iter().copied().collect(),
            }
        }
    }

    impl OctetNext for FakeServer {
        fn read(&mut self, _user: &AsynUser, buf: &mut [u8]) -> AsynResult<OctetReadResult> {
            if buf.is_empty() {
                return Ok(OctetReadResult {
                    nbytes_transferred: 0,
                    eom_reason: EomReason::empty(),
                });
            }
            let mut n = 0;
            while n < buf.len() {
                match self.to_read.pop_front() {
                    Some(b) => {
                        buf[n] = b;
                        n += 1;
                    }
                    None => break,
                }
            }
            if n == 0 {
                return Err(AsynError::Status {
                    status: AsynStatus::Timeout,
                    message: "read timeout".into(),
                });
            }
            Ok(OctetReadResult {
                nbytes_transferred: n,
                eom_reason: if n == buf.len() {
                    EomReason::CNT
                } else {
                    EomReason::empty()
                },
            })
        }

        fn write(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
            self.written.extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
            Ok(())
        }
    }

    /// The server's acknowledgement of subcommand `x[0]`: `IAC SB 44 <x[0]+100>
    /// <values…> IAC SE`.
    fn ack(subcmd: u8, values: &[u8]) -> Vec<u8> {
        let mut v = vec![IAC, SB, SB_COM_PORT_OPTION, subcmd + 100];
        v.extend_from_slice(values);
        v.extend_from_slice(&[IAC, SE]);
        v
    }

    // -- asynOctet: IAC stuffing ------------------------------------------

    #[test]
    fn write_doubles_iac_and_reports_the_unstuffed_count() {
        let mut stack = OctetInterposeStack::new(false);
        stack.install(-1, Box::new(ComInterpose::new()));
        let mut base = FakeServer::new(&[]);
        let mut user = AsynUser::default();

        let n = stack
            .dispatch_write(&mut user, &[b'A', IAC, b'B'], &mut base)
            .unwrap();
        // C writeIt :175 — the escape byte goes on the wire...
        assert_eq!(base.written, vec![b'A', IAC, IAC, b'B']);
        // ...but :192-193 subtracts it back out, so the caller sees the count it
        // handed in, not the stuffed count.
        assert_eq!(n, 3);
    }

    #[test]
    fn write_without_iac_is_verbatim_passthrough() {
        let mut stack = OctetInterposeStack::new(false);
        stack.install(-1, Box::new(ComInterpose::new()));
        let mut base = FakeServer::new(&[]);
        let mut user = AsynUser::default();

        let n = stack
            .dispatch_write(&mut user, b"HELLO", &mut base)
            .unwrap();
        assert_eq!(base.written, b"HELLO");
        assert_eq!(n, 5);
    }

    #[test]
    fn write_stuffs_every_iac_in_a_run() {
        let mut stack = OctetInterposeStack::new(false);
        stack.install(-1, Box::new(ComInterpose::new()));
        let mut base = FakeServer::new(&[]);
        let mut user = AsynUser::default();

        let n = stack
            .dispatch_write(&mut user, &[IAC, IAC, IAC], &mut base)
            .unwrap();
        assert_eq!(base.written, vec![IAC, IAC, IAC, IAC, IAC, IAC]);
        assert_eq!(n, 3);
    }

    /// C `writeIt` :192-193 only subtracts the stuffing when the lower layer took
    /// the *whole stuffed buffer*. A short write reports the count in stuffed
    /// bytes — a genuine C wart, pinned here so a "helpful" correction to it
    /// would fail.
    #[test]
    fn short_write_reports_the_stuffed_count_c_reports() {
        struct ShortWrite;
        impl OctetNext for ShortWrite {
            fn read(&mut self, _u: &AsynUser, _b: &mut [u8]) -> AsynResult<OctetReadResult> {
                unreachable!()
            }
            fn write(&mut self, _u: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
                Ok(data.len() - 1)
            }
            fn flush(&mut self, _u: &mut AsynUser) -> AsynResult<()> {
                Ok(())
            }
        }
        let mut stack = OctetInterposeStack::new(false);
        stack.install(-1, Box::new(ComInterpose::new()));
        let mut base = ShortWrite;
        let mut user = AsynUser::default();

        // 3 caller bytes -> 4 stuffed bytes -> lower takes 3 -> C reports 3, NOT
        // 3 - nIAC = 2.
        let n = stack
            .dispatch_write(&mut user, &[b'A', IAC, b'B'], &mut base)
            .unwrap();
        assert_eq!(n, 3);
    }

    // -- asynOctet: IAC unstuffing ----------------------------------------

    #[test]
    fn read_unstuffs_a_doubled_iac_inside_the_buffer() {
        let mut stack = OctetInterposeStack::new(false);
        stack.install(-1, Box::new(ComInterpose::new()));
        let mut base = FakeServer::new(&[b'A', IAC, IAC, b'B']);
        let user = AsynUser::default();
        let mut buf = [0u8; 8];

        let r = stack.dispatch_read(&user, &mut buf, &mut base).unwrap();
        assert_eq!(r.nbytes_transferred, 3);
        assert_eq!(&buf[..3], &[b'A', IAC, b'B']);
    }

    /// C `readIt` :218-221 — the escape's partner has not arrived yet, so C pulls
    /// one more byte *from the device*, bypassing this layer, and the returned
    /// count does not drop (the partner was never in the buffer).
    #[test]
    fn read_pulls_the_partner_from_the_device_when_iac_lands_last() {
        let mut stack = OctetInterposeStack::new(false);
        stack.install(-1, Box::new(ComInterpose::new()));
        // A 2-byte buffer takes [A, IAC]; the partner IAC is still on the wire.
        let mut base = FakeServer::new(&[b'A', IAC, IAC]);
        let user = AsynUser::default();
        let mut buf = [0u8; 2];

        let r = stack.dispatch_read(&user, &mut buf, &mut base).unwrap();
        assert_eq!(r.nbytes_transferred, 2);
        assert_eq!(&buf[..2], &[b'A', IAC]);
        assert!(base.to_read.is_empty(), "the partner byte was consumed");
    }

    /// The same arm with the IAC at offset 0 — C decrements `iac` to *before* the
    /// buffer (:220), which is exactly what makes the `nCheck` arithmetic land on
    /// zero. Signed cursor, or this underflows.
    #[test]
    fn read_handles_a_lone_iac_as_the_only_byte() {
        let mut stack = OctetInterposeStack::new(false);
        stack.install(-1, Box::new(ComInterpose::new()));
        let mut base = FakeServer::new(&[IAC, IAC]);
        let user = AsynUser::default();
        let mut buf = [0u8; 1];

        let r = stack.dispatch_read(&user, &mut buf, &mut base).unwrap();
        assert_eq!(r.nbytes_transferred, 1);
        assert_eq!(buf[0], IAC);
    }

    #[test]
    fn read_unstuffs_consecutive_escapes() {
        let mut stack = OctetInterposeStack::new(false);
        stack.install(-1, Box::new(ComInterpose::new()));
        let mut base = FakeServer::new(&[IAC, IAC, IAC, IAC, b'Z']);
        let user = AsynUser::default();
        let mut buf = [0u8; 8];

        let r = stack.dispatch_read(&user, &mut buf, &mut base).unwrap();
        assert_eq!(r.nbytes_transferred, 3);
        assert_eq!(&buf[..3], &[IAC, IAC, b'Z']);
    }

    /// C :226-230 — a lone IAC followed by anything other than IAC is an error,
    /// *not* a telnet command to interpret. An unsolicited `IAC WILL x` in the
    /// data stream therefore fails the read.
    #[test]
    fn read_rejects_an_unescaped_iac() {
        let mut stack = OctetInterposeStack::new(false);
        stack.install(-1, Box::new(ComInterpose::new()));
        let mut base = FakeServer::new(&[b'A', IAC, WILL, 0]);
        let user = AsynUser::default();
        let mut buf = [0u8; 8];

        let err = stack.dispatch_read(&user, &mut buf, &mut base).unwrap_err();
        assert_eq!(err.status(), AsynStatus::Error);
        assert_eq!(err.message(), "Missing IAC");
    }

    /// C :217 + :240-241 — unstuffing clears CNT, and it is re-set only if the
    /// *post-unstuffing* count still fills the buffer. Here it does not, so a
    /// full base read comes back without CNT.
    #[test]
    fn unstuffing_clears_the_count_eom_reason() {
        let mut stack = OctetInterposeStack::new(false);
        stack.install(-1, Box::new(ComInterpose::new()));
        let mut base = FakeServer::new(&[b'A', IAC, IAC, b'B']);
        let user = AsynUser::default();
        let mut buf = [0u8; 4];

        let r = stack.dispatch_read(&user, &mut buf, &mut base).unwrap();
        // Base filled all 4 and reported CNT; after unstuffing only 3 remain.
        assert_eq!(r.nbytes_transferred, 3);
        assert!(!r.eom_reason.contains(EomReason::CNT));
    }

    #[test]
    fn read_without_iac_keeps_the_base_eom_reason() {
        let mut stack = OctetInterposeStack::new(false);
        stack.install(-1, Box::new(ComInterpose::new()));
        let mut base = FakeServer::new(b"ABCD");
        let user = AsynUser::default();
        let mut buf = [0u8; 4];

        let r = stack.dispatch_read(&user, &mut buf, &mut base).unwrap();
        assert_eq!(r.nbytes_transferred, 4);
        assert!(r.eom_reason.contains(EomReason::CNT));
    }

    // -- asynOption: negotiation bytes ------------------------------------

    /// The exact bytes C's `restoreSettings` (:729-758) puts on the wire for a
    /// freshly-configured port at its defaults (9600 8N1, no flow control).
    /// This is the byte contract of the whole subsystem.
    #[test]
    fn restore_settings_emits_c_s_exact_handshake() {
        let mut reply = Vec::new();
        // IAC DO 0    -> the server answers WILL 0
        reply.extend_from_slice(&[IAC, WILL, WD_TRANSMIT_BINARY]);
        // IAC WILL 0  -> the server answers DO 0
        reply.extend_from_slice(&[IAC, DO, WD_TRANSMIT_BINARY]);
        // IAC WILL 44 -> the server answers DO 44
        reply.extend_from_slice(&[IAC, DO, SB_COM_PORT_OPTION]);
        // SET-MODEMSTATE-MASK 0, then each setting, each echoed back verbatim.
        reply.extend_from_slice(&ack(CPO_SET_MODEMSTATE_MASK, &[0]));
        reply.extend_from_slice(&ack(CPO_SET_BAUDRATE, &[0x00, 0x00, 0x25, 0x80]));
        reply.extend_from_slice(&ack(CPO_SET_DATASIZE, &[8]));
        reply.extend_from_slice(&ack(CPO_SET_PARITY, &[CPO_PARITY_NONE]));
        reply.extend_from_slice(&ack(CPO_SET_STOPSIZE, &[1]));
        reply.extend_from_slice(&ack(CPO_SET_CONTROL, &[CPO_CONTROL_NOFLOW]));
        reply.extend_from_slice(&ack(CPO_SET_CONTROL, &[CPO_CONTROL_NOFLOW]));

        let mut server = FakeServer::new(&reply);
        let mut com = ComPortOptions::new();
        com.restore_settings(&mut server).unwrap();

        #[rustfmt::skip]
        let expected: Vec<u8> = vec![
            IAC, DO,   WD_TRANSMIT_BINARY,                                  // :740
            IAC, WILL, WD_TRANSMIT_BINARY,                                  // :742
            IAC, WILL, SB_COM_PORT_OPTION,                                  // :744
            IAC, SB, SB_COM_PORT_OPTION, CPO_SET_MODEMSTATE_MASK, 0, IAC, SE, // :738-746
            // 9600 == 0x2580, big-endian in four bytes (:490-494).
            IAC, SB, SB_COM_PORT_OPTION, CPO_SET_BAUDRATE, 0x00, 0x00, 0x25, 0x80, IAC, SE,
            IAC, SB, SB_COM_PORT_OPTION, CPO_SET_DATASIZE, 8, IAC, SE,
            IAC, SB, SB_COM_PORT_OPTION, CPO_SET_PARITY, CPO_PARITY_NONE, IAC, SE,
            IAC, SB, SB_COM_PORT_OPTION, CPO_SET_STOPSIZE, 1, IAC, SE,
            // crtscts "N" re-sends the *current* flow mode (:575), not NOFLOW by
            // name — they coincide at the default, and the ixon "N" that follows
            // sends the same byte for the same reason.
            IAC, SB, SB_COM_PORT_OPTION, CPO_SET_CONTROL, CPO_CONTROL_NOFLOW, IAC, SE,
            IAC, SB, SB_COM_PORT_OPTION, CPO_SET_CONTROL, CPO_CONTROL_NOFLOW, IAC, SE,
        ];
        assert_eq!(server.written, expected);
        assert_eq!(com, ComPortOptions::default());
    }

    #[test]
    fn set_baud_emits_a_big_endian_four_byte_subnegotiation() {
        let mut server = FakeServer::new(&ack(CPO_SET_BAUDRATE, &[0x00, 0x01, 0xC2, 0x00]));
        let mut com = ComPortOptions::new();
        com.set_option(&mut AsynUser::default(), &mut server, "baud", "115200")
            .unwrap();

        // 115200 == 0x0001C200.
        assert_eq!(
            server.written,
            vec![
                IAC,
                SB,
                SB_COM_PORT_OPTION,
                CPO_SET_BAUDRATE,
                0x00,
                0x01,
                0xC2,
                0x00,
                IAC,
                SE
            ]
        );
        assert_eq!(com.get_option("baud").unwrap(), "115200");
    }

    /// CBUG-B8 — a payload byte of 0xFF is IAC-escaped, in both directions.
    ///
    /// DEVIATION from C, deliberate. C writes the subnegotiation straight to the
    /// driver below the interpose (:430), skipping the `writeIt` that doubles
    /// IACs, so `baud=255` (0x000000FF) puts a raw IAC into the payload and a
    /// compliant RFC-2217 server mis-frames the rest of the negotiation.
    #[test]
    fn b8_a_payload_byte_of_0xff_is_escaped_both_ways() {
        // The compliant server escapes its echo of 0xFF as well.
        let reply = vec![
            IAC,
            SB,
            SB_COM_PORT_OPTION,
            CPO_SET_BAUDRATE + 100,
            0x00,
            0x00,
            0x00,
            IAC,
            IAC, // the echoed 0xFF
            IAC,
            SE,
        ];
        let mut server = FakeServer::new(&reply);
        let mut com = ComPortOptions::new();
        com.set_option(&mut AsynUser::default(), &mut server, "baud", "255")
            .unwrap();

        // Written: the 0xFF of 0x000000FF is doubled; the framing IACs are not.
        assert_eq!(
            server.written,
            vec![
                IAC,
                SB,
                SB_COM_PORT_OPTION,
                CPO_SET_BAUDRATE,
                0x00,
                0x00,
                0x00,
                0xFF,
                0xFF, // C emits a single, raw 0xFF here
                IAC,
                SE
            ]
        );
        // And the escaped echo reads back as the one byte it encodes, so the
        // "did the server apply what we asked?" check passes.
        assert_eq!(com.get_option("baud").unwrap(), "255");
    }

    /// The other half of the escape rule: an IAC that is NOT doubled inside a
    /// payload is a command, i.e. a framing error — not a data byte.
    #[test]
    fn b8_an_unescaped_iac_in_a_reply_payload_is_a_framing_error() {
        let reply = vec![
            IAC,
            SB,
            SB_COM_PORT_OPTION,
            CPO_SET_BAUDRATE + 100,
            0x00,
            0x00,
            0x00,
            IAC,
            SE, // an unescaped IAC where a payload byte belongs
        ];
        let mut server = FakeServer::new(&reply);
        let mut com = ComPortOptions::new();
        let err = com
            .set_option(&mut AsynUser::default(), &mut server, "baud", "255")
            .unwrap_err();
        assert_eq!(err.status(), AsynStatus::Error);
        assert_eq!(
            err.message(),
            "Unescaped IAC in a COM-PORT-OPTION payload, followed by 0XF0"
        );
    }

    /// C :501-506 — the server echoes the rate it *actually* applied, and C fails
    /// the call when it is not the one asked for.
    #[test]
    fn set_baud_fails_when_the_server_applies_a_different_rate() {
        // Asked 115200, server says 9600.
        let mut server = FakeServer::new(&ack(CPO_SET_BAUDRATE, &[0x00, 0x00, 0x25, 0x80]));
        let mut com = ComPortOptions::new();
        let err = com
            .set_option(&mut AsynUser::default(), &mut server, "baud", "115200")
            .unwrap_err();
        assert_eq!(err.status(), AsynStatus::Error);
        assert_eq!(
            err.message(),
            "Tried to set 115200 baud, actually set 9600 baud."
        );
        // C still commits the echoed value before erroring (:497-500).
        assert_eq!(com.get_option("baud").unwrap(), "9600");
    }

    #[test]
    fn set_bits_and_stop_check_the_echo() {
        let mut server = FakeServer::new(&ack(CPO_SET_DATASIZE, &[7]));
        let mut com = ComPortOptions::new();
        let err = com
            .set_option(&mut AsynUser::default(), &mut server, "bits", "8")
            .unwrap_err();
        assert_eq!(err.message(), "Tried to set 8 bits, actually set 7 bits.");

        let mut server = FakeServer::new(&ack(CPO_SET_STOPSIZE, &[2]));
        let mut com = ComPortOptions::new();
        let err = com
            .set_option(&mut AsynUser::default(), &mut server, "stop", "1")
            .unwrap_err();
        assert_eq!(
            err.message(),
            "Tried to set 1 stop bits, actually set 2 stop bits."
        );
    }

    /// Negative control for the asymmetry at C :529-543: parity is the one
    /// setting with **no** echo check. Ask for "even", get "odd" back, and C
    /// reports success while caching odd. If someone "fixes" that into a
    /// consistency check, this fails.
    #[test]
    fn set_parity_does_not_check_the_echo() {
        let mut server = FakeServer::new(&ack(CPO_SET_PARITY, &[CPO_PARITY_ODD]));
        let mut com = ComPortOptions::new();
        com.set_option(&mut AsynUser::default(), &mut server, "parity", "even")
            .unwrap();
        assert_eq!(
            server.written,
            vec![
                IAC,
                SB,
                SB_COM_PORT_OPTION,
                CPO_SET_PARITY,
                CPO_PARITY_EVEN,
                IAC,
                SE
            ]
        );
        assert_eq!(com.get_option("parity").unwrap(), "odd");
    }

    #[test]
    fn parity_names_map_to_the_rfc2217_codes() {
        for (name, code) in [
            ("none", CPO_PARITY_NONE),
            ("odd", CPO_PARITY_ODD),
            ("even", CPO_PARITY_EVEN),
            ("mark", CPO_PARITY_MARK),
            ("space", CPO_PARITY_SPACE),
        ] {
            let mut server = FakeServer::new(&ack(CPO_SET_PARITY, &[code]));
            let mut com = ComPortOptions::new();
            com.set_option(&mut AsynUser::default(), &mut server, "parity", name)
                .unwrap();
            assert_eq!(server.written[4], code, "parity {name}");
            assert_eq!(com.get_option("parity").unwrap(), name);
        }
    }

    /// Turning OFF the mode that is *not* the one in effect leaves the other one
    /// alone: XON/XOFF is on, `crtscts n` says "hardware flow control off", and
    /// hardware flow control is already off — so the cached mode is re-sent and
    /// XON/XOFF keeps running. This is C's byte for this case too (:575) and it
    /// is the right one; the mode byte carries a single mutually-exclusive mode,
    /// so disabling RTS/CTS is not a licence to disable XON/XOFF.
    ///
    /// The case C gets WRONG is the other one — see
    /// [`each_key_can_turn_its_own_flow_control_back_off`].
    #[test]
    fn turning_off_the_mode_that_is_not_in_effect_leaves_the_other_running() {
        let mut server = FakeServer::new(&ack(CPO_SET_CONTROL, &[CPO_CONTROL_IXON]));
        let mut com = ComPortOptions::new();
        com.set_option(&mut AsynUser::default(), &mut server, "ixon", "y")
            .unwrap();
        assert_eq!(com.get_option("ixon").unwrap(), "Y");
        assert_eq!(com.get_option("crtscts").unwrap(), "N");

        let mut server = FakeServer::new(&ack(CPO_SET_CONTROL, &[CPO_CONTROL_IXON]));
        com.set_option(&mut AsynUser::default(), &mut server, "crtscts", "n")
            .unwrap();
        assert_eq!(
            server.written,
            vec![
                IAC,
                SB,
                SB_COM_PORT_OPTION,
                CPO_SET_CONTROL,
                CPO_CONTROL_IXON,
                IAC,
                SE
            ]
        );
        assert_eq!(com.get_option("ixon").unwrap(), "Y");
    }

    /// CBUG-B6 — each key can turn ITS OWN flow control back off, by
    /// transmitting `CPO_CONTROL_NOFLOW`.
    ///
    /// In C, `xBuf[1] = pinterposePvt->flow` on both "n" branches (:575, :591),
    /// so `crtscts N` with RTS/CTS on re-transmits HWFLOW, the server confirms
    /// it, `:578` caches it back, and `getOption` still answers "Y" — flow
    /// control can be enabled and never disabled. `CPO_CONTROL_NOFLOW` is
    /// defined and decoded but assigned to `xBuf[1]` nowhere in the file.
    #[test]
    fn each_key_can_turn_its_own_flow_control_back_off() {
        for (key, on_mode) in [("crtscts", CPO_CONTROL_HWFLOW), ("ixon", CPO_CONTROL_IXON)] {
            let mut com = ComPortOptions::new();
            let mut server = FakeServer::new(&ack(CPO_SET_CONTROL, &[on_mode]));
            com.set_option(&mut AsynUser::default(), &mut server, key, "y")
                .unwrap();
            assert_eq!(com.get_option(key).unwrap(), "Y", "{key} on");

            // C would re-send `on_mode` here and stay "Y" forever.
            let mut server = FakeServer::new(&ack(CPO_SET_CONTROL, &[CPO_CONTROL_NOFLOW]));
            com.set_option(&mut AsynUser::default(), &mut server, key, "n")
                .unwrap();
            assert_eq!(
                server.written,
                vec![
                    IAC,
                    SB,
                    SB_COM_PORT_OPTION,
                    CPO_SET_CONTROL,
                    CPO_CONTROL_NOFLOW,
                    IAC,
                    SE
                ],
                "{key} off must transmit NOFLOW"
            );
            assert_eq!(com.get_option(key).unwrap(), "N", "{key} reads back off");
            // And neither key claims the other is on.
            assert_eq!(com.get_option("crtscts").unwrap(), "N");
            assert_eq!(com.get_option("ixon").unwrap(), "N");
        }
    }

    /// "n" when flow control is already off is a no-op: NOFLOW is re-sent.
    #[test]
    fn turning_flow_control_off_when_it_is_already_off_sends_noflow() {
        let mut com = ComPortOptions::new();
        let mut server = FakeServer::new(&ack(CPO_SET_CONTROL, &[CPO_CONTROL_NOFLOW]));
        com.set_option(&mut AsynUser::default(), &mut server, "crtscts", "n")
            .unwrap();
        assert_eq!(server.written[4], CPO_CONTROL_NOFLOW);
        assert_eq!(com.get_option("crtscts").unwrap(), "N");
    }

    #[test]
    fn crtscts_y_sets_hardware_flow_control() {
        let mut server = FakeServer::new(&ack(CPO_SET_CONTROL, &[CPO_CONTROL_HWFLOW]));
        let mut com = ComPortOptions::new();
        com.set_option(&mut AsynUser::default(), &mut server, "crtscts", "y")
            .unwrap();
        assert_eq!(
            server.written,
            vec![
                IAC,
                SB,
                SB_COM_PORT_OPTION,
                CPO_SET_CONTROL,
                CPO_CONTROL_HWFLOW,
                IAC,
                SE
            ]
        );
        assert_eq!(com.get_option("crtscts").unwrap(), "Y");
        assert_eq!(com.get_option("ixon").unwrap(), "N");
    }

    /// C :605-643 — "on" then "off", edge-triggered, and the cached flag comes
    /// from the server's reply byte, not the request.
    #[test]
    fn break_on_and_off_emit_set_control_break() {
        let mut server = FakeServer::new(&ack(CPO_SET_CONTROL, &[CPO_CONTROL_BREAK_ON]));
        let mut com = ComPortOptions::new();
        com.set_option(&mut AsynUser::default(), &mut server, "break", "on")
            .unwrap();
        assert_eq!(
            server.written,
            vec![
                IAC,
                SB,
                SB_COM_PORT_OPTION,
                CPO_SET_CONTROL,
                CPO_CONTROL_BREAK_ON,
                IAC,
                SE
            ]
        );
        assert_eq!(com.get_option("break").unwrap(), "on");

        let mut server = FakeServer::new(&ack(CPO_SET_CONTROL, &[CPO_CONTROL_BREAK_OFF]));
        com.set_option(&mut AsynUser::default(), &mut server, "break", "off")
            .unwrap();
        assert_eq!(
            server.written,
            vec![
                IAC,
                SB,
                SB_COM_PORT_OPTION,
                CPO_SET_CONTROL,
                CPO_CONTROL_BREAK_OFF,
                IAC,
                SE
            ]
        );
        assert_eq!(com.get_option("break").unwrap(), "off");
    }

    /// C :609-612 — asserting an already-asserted break sends nothing at all, and
    /// releasing an already-released one likewise.
    #[test]
    fn break_is_edge_triggered_against_the_cached_state() {
        let mut server = FakeServer::new(&[]);
        let mut com = ComPortOptions::new();
        // Already off; "off" is a no-op that touches neither the wire nor the
        // (empty) reply queue.
        com.set_option(&mut AsynUser::default(), &mut server, "break", "off")
            .unwrap();
        assert!(server.written.is_empty());
    }

    /// C :614-641 — a numeric value runs assert / sleep / release in one call.
    #[test]
    fn break_with_a_duration_asserts_then_releases() {
        let mut reply = ack(CPO_SET_CONTROL, &[CPO_CONTROL_BREAK_ON]);
        reply.extend_from_slice(&ack(CPO_SET_CONTROL, &[CPO_CONTROL_BREAK_OFF]));
        let mut server = FakeServer::new(&reply);
        let mut com = ComPortOptions::new();
        com.set_option(&mut AsynUser::default(), &mut server, "break", "1")
            .unwrap();

        #[rustfmt::skip]
        let expected: Vec<u8> = vec![
            IAC, SB, SB_COM_PORT_OPTION, CPO_SET_CONTROL, CPO_CONTROL_BREAK_ON,  IAC, SE,
            IAC, SB, SB_COM_PORT_OPTION, CPO_SET_CONTROL, CPO_CONTROL_BREAK_OFF, IAC, SE,
        ];
        assert_eq!(server.written, expected);
        assert_eq!(com.get_option("break").unwrap(), "off");
    }

    // -- asynOption: negotiation state machine ----------------------------

    /// C `sbComPortOption` :443-449 — a server may volunteer a modem/line-state
    /// notification at any point; it is consumed and the wait for our own reply
    /// resumes.
    #[test]
    fn a_volunteered_modemstate_notification_is_skipped() {
        let mut reply = vec![
            IAC,
            SB,
            SB_COM_PORT_OPTION,
            CPO_SERVER_NOTIFY_MODEMSTATE,
            0x30,
            IAC,
            SE,
        ];
        reply.extend_from_slice(&ack(CPO_SET_DATASIZE, &[8]));
        let mut server = FakeServer::new(&reply);
        let mut com = ComPortOptions::new();
        com.set_option(&mut AsynUser::default(), &mut server, "bits", "8")
            .unwrap();
        assert_eq!(com.get_option("bits").unwrap(), "8");
    }

    #[test]
    fn a_volunteered_linestate_notification_is_skipped() {
        let mut reply = vec![
            IAC,
            SB,
            SB_COM_PORT_OPTION,
            CPO_SERVER_NOTIFY_LINESTATE,
            0x60,
            IAC,
            SE,
        ];
        reply.extend_from_slice(&ack(CPO_SET_DATASIZE, &[8]));
        let mut server = FakeServer::new(&reply);
        let mut com = ComPortOptions::new();
        com.set_option(&mut AsynUser::default(), &mut server, "bits", "8")
            .unwrap();
        assert_eq!(com.get_option("bits").unwrap(), "8");
    }

    /// C :461-465 — a reply for the wrong subcommand ends the negotiation.
    #[test]
    fn a_reply_for_the_wrong_subcommand_is_an_error() {
        // Asked SET-DATASIZE (2, reply 102); server answers for SET-PARITY (103).
        let mut server = FakeServer::new(&ack(CPO_SET_PARITY, &[CPO_PARITY_NONE]));
        let mut com = ComPortOptions::new();
        let err = com
            .set_option(&mut AsynUser::default(), &mut server, "bits", "8")
            .unwrap_err();
        assert_eq!(err.status(), AsynStatus::Error);
        assert_eq!(err.message(), "Sent COM-PORT-OPTION 2 but got reply 103");
    }

    /// C `expectChar` :120-122 — and its `%#X` formatting, `0X` prefix and all.
    #[test]
    fn a_malformed_subnegotiation_frame_reports_c_s_expected_got() {
        // IAC then a data byte where SB (0xFA) must be.
        let mut server = FakeServer::new(&[IAC, b'A']);
        let mut com = ComPortOptions::new();
        let err = com
            .set_option(&mut AsynUser::default(), &mut server, "bits", "8")
            .unwrap_err();
        assert_eq!(err.message(), "Expected 0XFA, got 0X41");
    }

    /// C `willdo` :372-390 — we said `IAC DO BINARY`, so a `WONT` is the server
    /// refusing.
    #[test]
    fn willdo_reports_a_refusal() {
        let mut server = FakeServer::new(&[IAC, WONT, WD_TRANSMIT_BINARY]);
        let mut com = ComPortOptions::new();
        let err = com.restore_settings(&mut server).unwrap_err();
        assert_eq!(err.status(), AsynStatus::Error);
        // WD_TRANSMIT_BINARY is 0, and printf's `#` flag prints no prefix for 0.
        assert_eq!(err.message(), "Device says WON'T 0.");
    }

    #[test]
    fn willdo_reports_a_dont_refusal_for_a_will_request() {
        // Skip past the first two exchanges to reach `IAC WILL 44`, which the
        // server refuses with DONT 44.
        let mut reply = vec![IAC, WILL, WD_TRANSMIT_BINARY, IAC, DO, WD_TRANSMIT_BINARY];
        reply.extend_from_slice(&[IAC, DONT, SB_COM_PORT_OPTION]);
        let mut server = FakeServer::new(&reply);
        let mut com = ComPortOptions::new();
        let err = com.restore_settings(&mut server).unwrap_err();
        assert_eq!(err.message(), "Device says DON'T 0x2c.");
    }

    /// C :358-362 — we sent `DO`, so a `DO`/`DONT` echo for the same code is a
    /// protocol error, not an acknowledgement. (`restoreSettings` opens with
    /// `IAC DO BINARY`.)
    #[test]
    fn willdo_rejects_a_do_echoed_in_response_to_do() {
        let mut server = FakeServer::new(&[IAC, DO, WD_TRANSMIT_BINARY]);
        let mut com = ComPortOptions::new();
        let err = com.restore_settings(&mut server).unwrap_err();
        assert_eq!(err.message(), "Received response 0 in response to DO.");
    }

    /// C :344-346 / :349-350 — chatter about *other* option codes, and stray
    /// IAC/SE pairs, are skipped until our own code comes back.
    #[test]
    fn willdo_skips_replies_about_other_option_codes() {
        let mut reply = Vec::new();
        // Server volunteers WILL/WONT for unrelated options (ECHO=1, SGA=3).
        reply.extend_from_slice(&[IAC, WILL, 1, IAC, WONT, 3]);
        // ...and only then answers ours.
        reply.extend_from_slice(&[IAC, WILL, WD_TRANSMIT_BINARY]);
        reply.extend_from_slice(&[IAC, DO, WD_TRANSMIT_BINARY]);
        reply.extend_from_slice(&[IAC, DO, SB_COM_PORT_OPTION]);
        reply.extend_from_slice(&ack(CPO_SET_MODEMSTATE_MASK, &[0]));
        reply.extend_from_slice(&ack(CPO_SET_BAUDRATE, &[0x00, 0x00, 0x25, 0x80]));
        reply.extend_from_slice(&ack(CPO_SET_DATASIZE, &[8]));
        reply.extend_from_slice(&ack(CPO_SET_PARITY, &[CPO_PARITY_NONE]));
        reply.extend_from_slice(&ack(CPO_SET_STOPSIZE, &[1]));
        reply.extend_from_slice(&ack(CPO_SET_CONTROL, &[CPO_CONTROL_NOFLOW]));
        reply.extend_from_slice(&ack(CPO_SET_CONTROL, &[CPO_CONTROL_NOFLOW]));

        let mut server = FakeServer::new(&reply);
        let mut com = ComPortOptions::new();
        com.restore_settings(&mut server).unwrap();
        assert_eq!(com, ComPortOptions::default());
    }

    /// C :405-408 — a byte after IAC that is no telnet command at all.
    #[test]
    fn willdo_rejects_an_unexpected_command_byte() {
        let mut server = FakeServer::new(&[IAC, b'A']);
        let mut com = ComPortOptions::new();
        let err = com.restore_settings(&mut server).unwrap_err();
        assert_eq!(err.message(), "Unexpected character 0x41 in TELNET reply");
    }

    /// C `nextChar` :103-106 discards the failing status but keeps the lower
    /// layer's message, which the caller's `return asynError` then carries out.
    /// So a timed-out negotiation surfaces as asynError carrying the read's
    /// message — the status is *not* preserved as asynTimeout.
    #[test]
    fn a_silent_server_surfaces_the_lower_layers_message_as_asyn_error() {
        let mut server = FakeServer::new(&[]);
        let mut com = ComPortOptions::new();
        let err = com.restore_settings(&mut server).unwrap_err();
        assert_eq!(err.status(), AsynStatus::Error);
        assert_eq!(err.message(), "read timeout");
    }

    // -- asynOption: option plumbing --------------------------------------

    #[test]
    fn owns_exactly_c_s_seven_keys_case_insensitively() {
        for key in ["baud", "BITS", "Parity", "stop", "CRTSCTS", "ixon", "Break"] {
            assert!(ComPortOptions::owns_key(key), "{key}");
        }
        // Handled by the driver below, not here — C :645-652 / :710-717.
        for key in ["hostInfo", "disconnectOnReadTimeout", "clocal", "ixoff"] {
            assert!(!ComPortOptions::owns_key(key), "{key}");
        }
    }

    /// R9-55. C threads the *caller's* `pasynUser` through the whole
    /// negotiation: `setOption(drvPvt, pasynUser, key, val)` (:475) hands it to
    /// `sbComPortOption` (:495), which writes (:431) and reads (:435-457) the wire
    /// with it — so an asynRecord option put negotiates under TMOT and an iocsh
    /// `asynSetOption` under its own 2 s (`asynShellCommands.c:119`). The port
    /// pinned every negotiation to a private 2 s instead, so a record with
    /// TMOT=10 gave up on a slow terminal server after 2 s, and one with
    /// TMOT=0.05 sat waiting for 2 s.
    ///
    /// The 2 s is C's only where C's own asynUser drives — `restoreSettings`,
    /// from the configure path and the reconnect exceptionHandler (:836).
    #[test]
    fn the_negotiation_runs_under_the_callers_timeout() {
        /// Records the timeout on the asynUser every wire operation is given.
        struct TimeoutSpy {
            inner: FakeServer,
            seen: Vec<Duration>,
        }
        impl OctetNext for TimeoutSpy {
            fn read(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<OctetReadResult> {
                self.seen.push(user.timeout);
                self.inner.read(user, buf)
            }
            fn write(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
                self.seen.push(user.timeout);
                self.inner.write(user, data)
            }
            fn flush(&mut self, user: &mut AsynUser) -> AsynResult<()> {
                self.inner.flush(user)
            }
        }

        // One setOption("bits", "8"): write the subcommand, read the echo.
        let mut spy = TimeoutSpy {
            inner: FakeServer::new(&ack(CPO_SET_DATASIZE, &[8])),
            seen: Vec::new(),
        };
        let mut com = ComPortOptions::new();
        let caller = Duration::from_millis(250);
        com.set_option(
            &mut AsynUser::default().with_timeout(caller),
            &mut spy,
            "bits",
            "8",
        )
        .unwrap();
        assert!(!spy.seen.is_empty(), "the negotiation reached the wire");
        assert!(
            spy.seen.iter().all(|t| *t == caller),
            "every wire operation runs under the caller's timeout, got {:?}",
            spy.seen
        );

        // restoreSettings is the interpose's own negotiation, and only that one
        // carries C's 2 s (:836).
        let mut replies = Vec::new();
        replies.extend(vec![IAC, WILL, WD_TRANSMIT_BINARY]);
        replies.extend(vec![IAC, DO, WD_TRANSMIT_BINARY]);
        replies.extend(vec![IAC, DO, SB_COM_PORT_OPTION]);
        replies.extend(ack(CPO_SET_MODEMSTATE_MASK, &[0]));
        replies.extend(ack(CPO_SET_BAUDRATE, &9600i32.to_be_bytes()));
        replies.extend(ack(CPO_SET_DATASIZE, &[8]));
        replies.extend(ack(CPO_SET_PARITY, &[CPO_PARITY_NONE]));
        replies.extend(ack(CPO_SET_STOPSIZE, &[1]));
        replies.extend(ack(CPO_SET_CONTROL, &[CPO_CONTROL_NOFLOW]));
        replies.extend(ack(CPO_SET_CONTROL, &[CPO_CONTROL_NOFLOW]));
        let mut spy = TimeoutSpy {
            inner: FakeServer::new(&replies),
            seen: Vec::new(),
        };
        let mut com = ComPortOptions::new();
        com.restore_settings(&mut spy).unwrap();
        assert!(!spy.seen.is_empty());
        assert!(
            spy.seen.iter().all(|t| *t == INTERPOSE_USER_TIMEOUT),
            "restoreSettings runs on the interpose's own 2 s asynUser, got {:?}",
            spy.seen
        );
    }

    /// R9-57, first half. C `readIt` traces every read it unstuffed at
    /// `ASYN_TRACEIO_FILTER` (asynInterposeCom.c:237-239). The interpose has no
    /// handle on the port to trace through — in C it prints through the
    /// `asynUser` (`asynPrintIO(pasynUser, …)` → `findTracePvt`), and the port's
    /// `AsynUser` now carries the same port/trace linkage, stamped by the actor.
    #[test]
    fn an_unstuffed_read_is_traced_at_traceio_filter() {
        use crate::trace::{TraceFile, TraceInfoMask, TraceIoMask, TraceManager};
        use crate::user::UserTrace;
        use std::sync::{Arc, Mutex};

        let mgr = Arc::new(TraceManager::new());
        mgr.set_trace_mask(Some("comtrace"), TraceMask::IO_FILTER);
        mgr.set_trace_info_mask(Some("comtrace"), TraceInfoMask::PORT);
        mgr.set_trace_io_mask(Some("comtrace"), TraceIoMask::ESCAPE);
        let temp = std::env::temp_dir().join("asyn_com_filter_trace.txt");
        let file = std::fs::File::create(&temp).unwrap();
        mgr.set_trace_file(
            Some("comtrace"),
            TraceFile::File(Arc::new(Mutex::new(file))),
        );

        let user = AsynUser {
            trace: Some(UserTrace {
                manager: mgr.clone(),
                port: "comtrace".into(),
            }),
            ..AsynUser::default()
        };

        // "A<IAC><IAC>B" on the wire is "A<IAC>B" to the caller — one unstuffing.
        let mut server = FakeServer::new(&[b'A', IAC, IAC, b'B']);
        let mut com = ComInterpose::new();
        let mut buf = [0u8; 8];
        let r = com.read(&user, &mut buf, &mut server).unwrap();
        assert_eq!(&buf[..r.nbytes_transferred], &[b'A', IAC, b'B']);

        let contents = std::fs::read_to_string(&temp).unwrap();
        assert!(
            contents.contains("nRead 3 after IAC unstuffing"),
            "C :238 label, got {contents:?}"
        );
        assert!(contents.contains("IO_FILTER"), "at ASYN_TRACEIO_FILTER");
        let _ = std::fs::remove_file(&temp);

        // A read with nothing to unstuff prints nothing (C tests `unstuffed`).
        let temp2 = std::env::temp_dir().join("asyn_com_filter_trace_quiet.txt");
        let file = std::fs::File::create(&temp2).unwrap();
        mgr.set_trace_file(
            Some("comtrace"),
            TraceFile::File(Arc::new(Mutex::new(file))),
        );
        let mut server = FakeServer::new(b"AB");
        com.read(&user, &mut buf, &mut server).unwrap();
        assert_eq!(std::fs::read_to_string(&temp2).unwrap(), "");
        let _ = std::fs::remove_file(&temp2);
    }

    /// R9-57, second half. C's `setOption` leaves an advisory in
    /// `pasynUser->errorMessage` when the operator switches flow control over a
    /// live setting of the other kind, and does *not* fail the call (:571-573,
    /// :587-589). A `Result` cannot carry a message that does not fail, so the
    /// port dropped both; they now land in the user's message slot, which is
    /// where C's caller finds them.
    #[test]
    fn switching_flow_control_leaves_c_s_advisory_in_the_users_message() {
        let mut user = AsynUser::default();

        // XON/XOFF is live; the operator turns RTS/CTS on.
        let mut server = FakeServer::new(&ack(CPO_SET_CONTROL, &[CPO_CONTROL_HWFLOW]));
        let mut com = ComPortOptions::new();
        com.flow = CPO_CONTROL_IXON;
        com.set_option(&mut user, &mut server, "crtscts", "y")
            .expect("the advisory does not fail the call");
        assert_eq!(
            user.error_message,
            "XON/XOFF already set. Now using RTS/CTS."
        );
        assert_eq!(com.flow, CPO_CONTROL_HWFLOW);

        // Mirror image: RTS/CTS is live; the operator turns XON/XOFF on.
        let mut user = AsynUser::default();
        let mut server = FakeServer::new(&ack(CPO_SET_CONTROL, &[CPO_CONTROL_IXON]));
        let mut com = ComPortOptions::new();
        com.flow = CPO_CONTROL_HWFLOW;
        com.set_option(&mut user, &mut server, "ixon", "y")
            .expect("the advisory does not fail the call");
        assert_eq!(
            user.error_message,
            "RTS/CTS already set. Now using XON/XOFF."
        );
        assert_eq!(com.flow, CPO_CONTROL_IXON);

        // No live setting of the other kind: no advisory (C only writes it under
        // the flow test).
        let mut user = AsynUser::default();
        let mut server = FakeServer::new(&ack(CPO_SET_CONTROL, &[CPO_CONTROL_HWFLOW]));
        let mut com = ComPortOptions::new();
        com.set_option(&mut user, &mut server, "crtscts", "y")
            .unwrap();
        assert_eq!(user.error_message, "");
    }

    #[test]
    fn get_option_reports_the_defaults_asyn_interpose_com_installs() {
        let com = ComPortOptions::new();
        assert_eq!(com.get_option("baud").unwrap(), "9600");
        assert_eq!(com.get_option("bits").unwrap(), "8");
        assert_eq!(com.get_option("parity").unwrap(), "none");
        assert_eq!(com.get_option("stop").unwrap(), "1");
        assert_eq!(com.get_option("crtscts").unwrap(), "N");
        assert_eq!(com.get_option("ixon").unwrap(), "N");
        assert_eq!(com.get_option("break").unwrap(), "off");
    }

    #[test]
    fn bad_option_values_are_rejected_with_c_s_messages() {
        let mut server = FakeServer::new(&[]);
        let mut com = ComPortOptions::new();
        assert_eq!(
            com.set_option(&mut AsynUser::default(), &mut server, "baud", "fast")
                .unwrap_err()
                .message(),
            "Bad number"
        );
        assert_eq!(
            com.set_option(&mut AsynUser::default(), &mut server, "parity", "sideways")
                .unwrap_err()
                .message(),
            "Invalid parity selection"
        );
        // C's message really does have two spaces (:553).
        assert_eq!(
            com.set_option(&mut AsynUser::default(), &mut server, "stop", "1.5")
                .unwrap_err()
                .message(),
            "Bad  stop bit count"
        );
        assert_eq!(
            com.set_option(&mut AsynUser::default(), &mut server, "crtscts", "maybe")
                .unwrap_err()
                .message(),
            "Bad  value"
        );
        // Nothing reached the wire.
        assert!(server.written.is_empty());
    }

    /// C `sscanf("%d")` semantics: a trailing non-numeric tail is ignored, not an
    /// error.
    #[test]
    fn baud_accepts_a_trailing_tail_the_way_sscanf_does() {
        let mut server = FakeServer::new(&ack(CPO_SET_BAUDRATE, &[0x00, 0x00, 0x25, 0x80]));
        let mut com = ComPortOptions::new();
        com.set_option(&mut AsynUser::default(), &mut server, "baud", "9600 bps")
            .unwrap();
        assert_eq!(com.get_option("baud").unwrap(), "9600");
    }

    /// C `sscanf`'s leading skip is `isspace`, vertical tab included, so
    /// `asynSetOption(port, "baud", "\vBAUD")` sets the baud rate on a C IOC.
    #[test]
    fn a_leading_vertical_tab_is_whitespace_as_it_is_to_c() {
        assert_eq!(scan_int("\u{0b}9600"), Some(9600));
        assert_eq!(scan_uint("\u{0b}250ms"), Some(250));
    }

    #[test]
    fn scan_helpers_match_sscanf() {
        assert_eq!(scan_int("9600"), Some(9600));
        assert_eq!(scan_int("  -12xyz"), Some(-12));
        assert_eq!(scan_int("+7"), Some(7));
        assert_eq!(scan_int("abc"), None);
        assert_eq!(scan_int(""), None);
        assert_eq!(scan_uint("250ms"), Some(250));
        assert_eq!(scan_uint("x"), None);
        assert_eq!(scan_float("2"), Some(2.0));
        assert_eq!(scan_float("1.5abc"), Some(1.5));
        assert_eq!(scan_float("nope"), None);
    }

    #[test]
    fn hash_hex_matches_printf() {
        assert_eq!(hash_hex_lower(0), "0");
        assert_eq!(hash_hex_lower(44), "0x2c");
        assert_eq!(hash_hex_upper(0), "0");
        assert_eq!(hash_hex_upper(250), "0XFA");
        assert_eq!(hash_hex_upper(0x41), "0X41");
    }
}
