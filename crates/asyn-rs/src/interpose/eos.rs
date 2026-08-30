//! End-of-string (EOS) interpose layer.
//!
//! Corresponds to C asyn's `asynInterposeEos.c`. Supports up to 2-character
//! input/output EOS sequences. On read, buffers data and scans for the EOS
//! pattern using a character-by-character state machine with resynchronization.
//! On write, appends the output EOS to outgoing data.

use std::collections::HashMap;

use crate::error::AsynResult;
use crate::port::eos_device_key;
use crate::user::AsynUser;

use super::{
    EomReason, EosSet, MAX_EOS_LEN, OctetInterpose, OctetNext, OctetReadResult, PartialOctetRead,
};

/// Fixed internal buffer size matching C asyn's INPUT_SIZE.
const INPUT_BUFFER_SIZE: usize = 2048;

/// EOS configuration — input and output terminator sequences.
#[derive(Debug, Clone)]
pub struct EosConfig {
    /// Input EOS sequence (max 2 bytes). Empty = no input EOS detection.
    pub input_eos: Vec<u8>,
    /// Output EOS sequence (max 2 bytes). Empty = no output EOS append.
    pub output_eos: Vec<u8>,
}

impl Default for EosConfig {
    fn default() -> Self {
        Self {
            input_eos: Vec::new(),
            output_eos: Vec::new(),
        }
    }
}

/// One device's EOS state — C's `eosPvt` (asynInterposeEos.c:36-55), of which
/// there is exactly one per (port, addr) because `asynInterposeEosConfig` takes
/// the addr and creates the instance for it (:84-140).
struct EosDevice {
    config: EosConfig,
    /// Fixed-size internal read buffer.
    in_buf: Vec<u8>,
    /// How far the internal buffer has been filled by the lower layer.
    in_buf_head: usize,
    /// How far the internal buffer has been consumed.
    in_buf_tail: usize,
    /// Current EOS match position for the resynchronization state machine.
    eos_in_match: usize,
}

impl EosDevice {
    fn new(config: EosConfig) -> Self {
        Self {
            config,
            in_buf: vec![0u8; INPUT_BUFFER_SIZE],
            in_buf_head: 0,
            in_buf_tail: 0,
            eos_in_match: 0,
        }
    }

    /// Single owner of the link-scoped input state: the read-ahead buffer
    /// (`in_buf_head` / `in_buf_tail`) and the partial-EOS match position
    /// (`eos_in_match`). Every site that must forget bytes belonging to a
    /// past link or a past terminator routes through here — `flush`
    /// (C `flushIt`, asynInterposeEos.c:264-266) and `connection_changed`
    /// (C `eosInExceptionHandler`, asynInterposeEos.c:146-150) clear all
    /// three; `set_input_eos` clears only the match position, because C
    /// leaves `inBuf` alone on a terminator change.
    fn reset_link_state(&mut self) {
        self.in_buf_head = 0;
        self.in_buf_tail = 0;
        self.eos_in_match = 0;
    }
}

/// EOS interpose layer with internal read buffer and character-by-character
/// state machine matching, including resynchronization on partial matches.
///
/// Matches the C implementation's behavior:
/// - One `EosDevice` per addressed device (C's per-(port, addr) `eosPvt`), so
///   two devices on a multi-device port hold two terminators *and* two
///   read-ahead buffers — neither device's bytes can be served to the other
/// - Fixed-size internal buffer (2048 bytes) per device
/// - Character-by-character EOS matching with resynchronization
/// - Filters ASYN_EOM_CNT from lower layer reads
/// - Null-terminates output when there's room
pub struct EosInterpose {
    /// The terminators a device starts with — the config this layer was
    /// installed with, C's `asynInterposeEosConfig` arguments.
    initial: EosConfig,
    /// C `eosPvt::processEosIn` (asynInterposeEos.c:42, set from the
    /// `asynInterposeEosConfig` argument at :128). When false the layer is not
    /// in the input path at all: `readIt` delegates straight to the driver
    /// (:191-193) and the terminator accessors are the driver's (:293, :318).
    process_in: bool,
    /// C `eosPvt::processEosOut` (:50, :134) — same, for the write path (:161).
    process_out: bool,
    /// The port's device model, set at install ([`OctetInterpose::attach_port`]).
    multi_device: bool,
    devices: HashMap<i32, EosDevice>,
}

impl EosInterpose {
    /// The `drvAsynIPPortConfigure` install: C passes `processEosIn = 1,
    /// processEosOut = 1` (drvAsynIPPort.c:1065-1066), so both halves run.
    pub fn new(config: EosConfig) -> Self {
        Self {
            initial: config,
            process_in: true,
            process_out: true,
            multi_device: false,
            devices: HashMap::new(),
        }
    }

    /// The `asynInterposeEosConfig portName addr processIn processOut` install:
    /// the shell chooses which half of the layer is live
    /// (asynInterposeEos.c:84-140).
    pub fn with_processing(config: EosConfig, process_in: bool, process_out: bool) -> Self {
        Self {
            process_in,
            process_out,
            ..Self::new(config)
        }
    }

    /// The state for the device this `asynUser` addresses, created on first
    /// touch with the layer's configured terminators.
    fn device(&mut self, addr: i32) -> &mut EosDevice {
        let key = eos_device_key(self.multi_device, addr);
        let initial = self.initial.clone();
        self.devices
            .entry(key)
            .or_insert_with(|| EosDevice::new(initial))
    }

    pub fn get_input_eos(&self, addr: i32) -> &[u8] {
        self.devices
            .get(&eos_device_key(self.multi_device, addr))
            .map_or(&self.initial.input_eos, |d| &d.config.input_eos)
    }

    pub fn get_output_eos(&self, addr: i32) -> &[u8] {
        self.devices
            .get(&eos_device_key(self.multi_device, addr))
            .map_or(&self.initial.output_eos, |d| &d.config.output_eos)
    }
}

#[cfg(test)]
impl EosInterpose {
    /// Test-only view of one device's `eosPvt` — the read-ahead buffer bounds
    /// and the partial-match position the boundary tests below assert on.
    fn peek(&self, addr: i32) -> &EosDevice {
        self.devices
            .get(&eos_device_key(self.multi_device, addr))
            .expect("device state is created on first touch")
    }
}

impl Default for EosInterpose {
    /// An EOS interpose with no terminator — a pass-through until
    /// `set_input_eos`/`set_output_eos` configure one. This is the
    /// auto-install form (C `asynInterposeEosConfig` installs the layer
    /// with an empty EOS; the terminator arrives later via `setInputEos`).
    fn default() -> Self {
        Self::new(EosConfig::default())
    }
}

impl OctetInterpose for EosInterpose {
    fn read(
        &mut self,
        user: &AsynUser,
        buf: &mut [u8],
        next: &mut dyn OctetNext,
    ) -> AsynResult<OctetReadResult> {
        // C parity (`asynInterposeEos.c::readIt:191`): an installed EOS
        // interpose is always `processEosIn==1`, so the read ALWAYS runs the
        // buffering loop below — even with no terminator set. The "no EOS"
        // case is handled by gating only the *match* on a non-empty
        // terminator (mirroring C's `if (eosInLen > 0)` at readIt:199), NOT
        // by short-circuiting to `next.read`. Short-circuiting would skip
        // `in_buf`, stranding read-ahead bytes left by a prior EOS read when
        // the terminator is later cleared (binary I/O or a runtime IEOS
        // clear) — bytes C delivers from `inBuf` first.
        // C `readIt` (:191-193): a layer installed with `processEosIn == 0` is
        // not in the input path — the read is the driver's, unbuffered and
        // unterminated. `in_buf` is never allocated in that case (:129-132), so
        // there is nothing here that could strand bytes either.
        if !self.process_in {
            return next.read(user, buf);
        }
        let maxchars = buf.len();
        if maxchars == 0 {
            // A zero-length destination buffer can store nothing — return
            // here so the scan loop never indexes `buf[0]` and panics.
            return Ok(OctetReadResult {
                nbytes_transferred: 0,
                eom_reason: EomReason::CNT,
            });
        }
        // C's `eosPvt` for the device this read addresses: its terminator, its
        // read-ahead buffer, its match position. Nothing below can reach
        // another device's bytes.
        let dev = self.device(user.addr);
        let mut n_read: usize = 0;
        let mut eom = EomReason::empty();

        loop {
            // Process buffered data character by character
            if dev.in_buf_tail != dev.in_buf_head {
                let c = dev.in_buf[dev.in_buf_tail];
                dev.in_buf_tail += 1;
                buf[n_read] = c;
                n_read += 1;

                // EOS matching only when a terminator is configured
                // (C `asynInterposeEos.c::readIt:199` `if (eosInLen > 0)`).
                // With an empty terminator we still deliver the buffered
                // byte above, we just never match/strip — so cleared-EOS
                // reads drain `in_buf` instead of dropping it.
                let eos_len = dev.config.input_eos.len();
                if eos_len > 0 {
                    let expected = dev.config.input_eos[dev.eos_in_match];
                    let first = dev.config.input_eos[0];
                    if c == expected {
                        dev.eos_in_match += 1;
                        if dev.eos_in_match == eos_len {
                            // Full EOS match — remove the EOS bytes from the
                            // output count. Only the EOS bytes written into
                            // *this* buffer can be removed: when a 2-byte EOS
                            // straddles two read() calls, the leading byte was
                            // already returned to the previous caller, so
                            // `n_read` here may be smaller than `eos_len`.
                            // An unguarded `n_read -= eos_len` underflows.
                            dev.eos_in_match = 0;
                            n_read -= eos_len.min(n_read);
                            eom |= EomReason::EOS;
                            break;
                        }
                    } else {
                        // Resynchronize the search. Since asyn allows a maximum
                        // two-character EOS, we only need to check if the current
                        // character matches the first EOS character.
                        dev.eos_in_match = usize::from(c == first);
                    }
                }

                if n_read >= maxchars {
                    eom = EomReason::CNT;
                    break;
                }
                continue;
            }

            // If we have end-of-message flags from a previous lower read, stop
            if !eom.is_empty() {
                break;
            }

            // Read more data from the lower layer into our internal buffer.
            //
            // C parity (`asynInterposeEos.c::readIt:242-253`): a failing
            // lower-layer read `break`s the loop and falls through to the
            // SAME tail as a successful one — null-terminate, `*eomReason =
            // eom`, `*nbytesTransfered = nRead` — and only then `return
            // status`. So the caller gets the error AND everything already
            // transferred: the classic partial-line timeout reaches
            // asynRecord as `asynTimeout` with `AINP="abc"`, `NORD=3`
            // (asynRecord.c:1591,1627 assign `eomr`/`nord` regardless of
            // status).
            //
            // `?` here would return the bare error and drop both the count
            // and the eom reason. The bytes are not recoverable afterwards —
            // `in_buf_tail` has already advanced past them at :114-118 — so
            // dropping the count loses the data outright. Run C's tail, then
            // hand the error back with the partial attached.
            //
            // The partial carries a *copy* of the bytes, not just the count:
            // `buf` here is the caller's buffer, but every dispatch hop above
            // (`port_actor`'s scratch `Vec`, `SyncIO`'s) owns a different one
            // and drops it on `?`. Only bytes that travel inside the error
            // reach the record.
            let result = match next.read(user, &mut dev.in_buf[..]) {
                Ok(r) => r,
                Err(e) => {
                    if n_read < maxchars {
                        buf[n_read] = 0;
                    }
                    return Err(e.with_partial_read(PartialOctetRead {
                        data: buf[..n_read].to_vec(),
                        eom_reason: eom,
                    }));
                }
            };

            // Filter out CNT from lower layer — the lower read may have set CNT
            // because available data exceeded our buffer size. This is not a
            // reason for us to stop reading. (C parity: eom &= ~ASYN_EOM_CNT)
            //
            // C parity (`asynInterposeEos.c:232,241,246,251`): the lower
            // read sets `eom` even on a zero-byte read (e.g. ASYN_EOM_END on
            // a TCP EOF), and `*eomReason = eom` propagates it after the
            // loop. Capture the reason BEFORE the zero-byte break so END
            // survives the interpose instead of being dropped.
            eom = result.eom_reason & !EomReason::CNT;

            if result.nbytes_transferred == 0 {
                break;
            }

            dev.in_buf_tail = 0;
            dev.in_buf_head = result.nbytes_transferred;
        }

        // Null terminate if there's room (C parity)
        if n_read < maxchars {
            buf[n_read] = 0;
        }

        Ok(OctetReadResult {
            nbytes_transferred: n_read,
            eom_reason: eom,
        })
    }

    fn write(
        &mut self,
        user: &mut AsynUser,
        data: &[u8],
        next: &mut dyn OctetNext,
    ) -> AsynResult<usize> {
        // C `writeIt` (:161): no terminator is appended when the layer was
        // installed with `processEosOut == 0`, or when this device has no
        // output terminator.
        if !self.process_out {
            return next.write(user, data);
        }
        let out_eos = self.device(user.addr).config.output_eos.clone();
        if out_eos.is_empty() {
            return next.write(user, data);
        }

        // Append this device's output EOS to the data
        let mut buf = Vec::with_capacity(data.len() + out_eos.len());
        buf.extend_from_slice(data);
        buf.extend_from_slice(&out_eos);
        // C `asynInterposeEos.c::writeIt` (:189-197) runs one common tail
        // regardless of status: `*nbytesTransfered = min(nbytesActual,
        // numchars); return status`. So the clamp — never report the appended
        // terminator bytes as caller payload — applies to the failure path too,
        // and a lower layer that stalled part-way through the *user* bytes has
        // that count published beside its error.
        match next.write(user, &buf) {
            Ok(actual) => Ok(actual.min(data.len())),
            Err(e) => {
                let transferred = e.partial_write().unwrap_or(0).min(data.len());
                Err(e.with_partial_write(transferred))
            }
        }
    }

    fn attach_port(&mut self, multi_device: bool) {
        self.multi_device = multi_device;
    }

    fn flush(&mut self, user: &mut AsynUser, next: &mut dyn OctetNext) -> AsynResult<()> {
        // C `flushIt` runs on the addressed device's `eosPvt`
        // (asynInterposeEos.c:258-266): flushing one device does not throw away
        // another's read-ahead.
        self.device(user.addr).reset_link_state();
        next.flush(user)
    }

    fn set_input_eos(&mut self, addr: i32, eos: &[u8]) -> EosSet {
        // C :293-295 — with `processEosIn == 0` the terminator belongs to the
        // driver, not to this layer, and the call is *delegated downwards*;
        // taking it here would make `read` (which delegates) and the stored
        // terminator disagree. C tests the flag before the length, so a
        // delegated call is never refused here for its length either.
        if !self.process_in {
            return EosSet::NotTaken;
        }
        if eos.len() > MAX_EOS_LEN {
            return EosSet::IllegalLength;
        }
        let dev = self.device(addr);
        dev.config.input_eos = eos.to_vec();
        // Reset the resync state machine — a mid-stream terminator change
        // must not carry a partial match from the old terminator.
        dev.eos_in_match = 0;
        EosSet::Stored
    }

    fn set_output_eos(&mut self, addr: i32, eos: &[u8]) -> EosSet {
        // C `setOutputEos` (asynInterposeEos.c:344-363) carries no
        // `processEosOut` test at all: it validates the length, stores
        // `eosOut`/`eosOutLen` and answers asynSuccess whatever the flag,
        // and `getOutputEos` (:365-390) reads it back the same way. The flag
        // gates `writeIt` alone (:161-163), so with `processOut = 0` the
        // terminator is held and simply never appended — a set-then-read-back
        // from a startup script succeeds, which is what refusing it broke.
        if eos.len() > MAX_EOS_LEN {
            return EosSet::IllegalLength;
        }
        self.device(addr).config.output_eos = eos.to_vec();
        EosSet::Stored
    }

    /// C `eosInExceptionHandler` (asynInterposeEos.c:142-151): on
    /// `asynExceptionConnect` the interpose drops its read-ahead buffer and
    /// its partial-EOS match position. Without it the first read on a
    /// re-established link is served from up to `INPUT_BUFFER_SIZE` bytes of
    /// the *previous* connection's traffic, and an `eos_in_match == 1` left
    /// over from a 2-byte terminator that straddled the drop makes the first
    /// byte of the new session complete a spurious EOS match.
    fn connection_changed(&mut self) {
        // The connect exception is port-level: C fires it at every `eosPvt`
        // registered on the port — `exceptionOccurred` (asynManager.c:638)
        // hands off to `announceExceptionOccurred` (:611-637), which walks the
        // dpCommon's `exceptionUserList` (:621-625) — so every device drops its
        // read-ahead.
        for dev in self.devices.values_mut() {
            dev.reset_link_state();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{AsynError, AsynStatus};

    struct MockOctetBase {
        data: Vec<u8>,
        pos: usize,
        written: Vec<u8>,
    }

    impl MockOctetBase {
        fn new(data: &[u8]) -> Self {
            Self {
                data: data.to_vec(),
                pos: 0,
                written: Vec::new(),
            }
        }
    }

    impl OctetNext for MockOctetBase {
        fn read(&mut self, _user: &AsynUser, buf: &mut [u8]) -> AsynResult<OctetReadResult> {
            let avail = self.data.len() - self.pos;
            let n = avail.min(buf.len());
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(OctetReadResult {
                nbytes_transferred: n,
                eom_reason: EomReason::CNT,
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

    /// C `setOutputEos` (asynInterposeEos.c:344-363) carries no
    /// `processEosOut` test: it validates the length, stores
    /// `eosOut`/`eosOutLen` and answers asynSuccess whatever the flag, which
    /// gates `writeIt` alone (:161-163). So `processOut = 0` means "held but
    /// never appended", never "refused" — a startup script's set-then-read-back
    /// succeeds. And the length switch refuses before assigning, so a refusal
    /// leaves the stored terminator standing.
    #[test]
    fn output_eos_stores_whatever_process_out_says() {
        let mut eos = EosInterpose::with_processing(EosConfig::default(), true, false);
        assert_eq!(eos.set_output_eos(0, b"\r\n"), EosSet::Stored);
        assert_eq!(eos.get_output_eos(0), b"\r\n");

        assert_eq!(eos.set_output_eos(0, b"\r\n\0"), EosSet::IllegalLength);
        assert_eq!(
            eos.get_output_eos(0),
            b"\r\n",
            "a refused length must store nothing"
        );

        // The flag still gates the write: the stored terminator is not
        // appended (C `writeIt` :161-163).
        let mut base = MockOctetBase::new(b"");
        let mut user = AsynUser::default();
        eos.write(&mut user, b"CMD", &mut base).unwrap();
        assert_eq!(base.written, b"CMD".to_vec());

        // With the flag on, the same stored terminator is appended.
        let mut on = EosInterpose::with_processing(EosConfig::default(), true, true);
        assert_eq!(on.set_output_eos(0, b"\r\n"), EosSet::Stored);
        let mut base = MockOctetBase::new(b"");
        on.write(&mut user, b"CMD", &mut base).unwrap();
        assert_eq!(base.written, b"CMD\r\n".to_vec());
    }

    /// The input half is *not* the same shape: C's `setInputEos` tests
    /// `processEosIn` first and delegates downwards when it is 0 (:293-295),
    /// as `getInputEos` (:318-320) and `readIt` (:191-193) do.
    #[test]
    fn input_eos_delegates_when_process_in_is_off() {
        let mut eos = EosInterpose::with_processing(EosConfig::default(), false, true);
        assert_eq!(eos.set_input_eos(0, b"\n"), EosSet::NotTaken);
        // Refused for the flag, not for the length — C never reaches the
        // switch on that path.
        assert_eq!(eos.set_input_eos(0, b"abc"), EosSet::NotTaken);
    }

    #[test]
    fn test_single_char_eos() {
        let mut interpose = EosInterpose::new(EosConfig {
            input_eos: vec![b'\n'],
            output_eos: vec![],
        });
        let mut base = MockOctetBase::new(b"hello\nworld\n");
        let user = AsynUser::default();
        let mut buf = [0u8; 64];

        let r = interpose.read(&user, &mut buf, &mut base).unwrap();
        assert_eq!(&buf[..r.nbytes_transferred], b"hello");
        assert!(r.eom_reason.contains(EomReason::EOS));

        let r = interpose.read(&user, &mut buf, &mut base).unwrap();
        assert_eq!(&buf[..r.nbytes_transferred], b"world");
        assert!(r.eom_reason.contains(EomReason::EOS));
    }

    #[test]
    fn test_two_char_eos() {
        let mut interpose = EosInterpose::new(EosConfig {
            input_eos: vec![b'\r', b'\n'],
            output_eos: vec![],
        });
        let mut base = MockOctetBase::new(b"cmd1\r\ncmd2\r\n");
        let user = AsynUser::default();
        let mut buf = [0u8; 64];

        let r = interpose.read(&user, &mut buf, &mut base).unwrap();
        assert_eq!(&buf[..r.nbytes_transferred], b"cmd1");
        assert!(r.eom_reason.contains(EomReason::EOS));

        let r = interpose.read(&user, &mut buf, &mut base).unwrap();
        assert_eq!(&buf[..r.nbytes_transferred], b"cmd2");
        assert!(r.eom_reason.contains(EomReason::EOS));
    }

    #[test]
    fn test_two_char_eos_straddling_reads() {
        // A 2-byte EOS split across two read() calls: the first call
        // fills the user buffer ending on the EOS's leading byte, the
        // second completes the match. `n_read -= eos.len()` would
        // underflow (panic in debug) without the saturating guard.
        let mut interpose = EosInterpose::new(EosConfig {
            input_eos: vec![b'\r', b'\n'],
            output_eos: vec![],
        });
        let mut base = MockOctetBase::new(b"AB\r\n");
        let user = AsynUser::default();
        let mut buf = [0u8; 3];

        // First read fills the 3-byte buffer with "AB\r" (partial match).
        let r = interpose.read(&user, &mut buf, &mut base).unwrap();
        assert_eq!(&buf[..r.nbytes_transferred], b"AB\r");

        // Second read consumes the trailing "\n", completing the EOS.
        let r = interpose.read(&user, &mut buf, &mut base).unwrap();
        assert_eq!(r.nbytes_transferred, 0);
        assert!(r.eom_reason.contains(EomReason::EOS));
    }

    #[test]
    fn test_output_eos_append() {
        let mut interpose = EosInterpose::new(EosConfig {
            input_eos: vec![],
            output_eos: vec![b'\r', b'\n'],
        });
        let mut base = MockOctetBase::new(b"");
        let mut user = AsynUser::default();

        let n = interpose.write(&mut user, b"hello", &mut base).unwrap();
        assert_eq!(&base.written, b"hello\r\n");
        // Return value should be user data length, not including EOS
        assert_eq!(n, 5);
    }

    #[test]
    fn test_no_eos_passthrough() {
        let mut interpose = EosInterpose::new(EosConfig::default());
        let mut base = MockOctetBase::new(b"data");
        let user = AsynUser::default();
        let mut buf = [0u8; 64];

        let r = interpose.read(&user, &mut buf, &mut base).unwrap();
        assert_eq!(&buf[..r.nbytes_transferred], b"data");
    }

    #[test]
    fn test_flush_clears_buffer() {
        let mut interpose = EosInterpose::new(EosConfig {
            input_eos: vec![b'\n'],
            output_eos: vec![],
        });
        let mut base = MockOctetBase::new(b"partial");
        let user = AsynUser::default();
        let mut buf = [0u8; 4]; // small buffer to force buffering

        // Read some data into internal buffer
        let _ = interpose.read(&user, &mut buf, &mut base);

        // Flush should clear internal state
        let mut user2 = AsynUser::default();
        interpose.flush(&mut user2, &mut base).unwrap();
        assert_eq!(interpose.peek(0).in_buf_head, 0);
        assert_eq!(interpose.peek(0).in_buf_tail, 0);
        assert_eq!(interpose.peek(0).eos_in_match, 0);
    }

    /// C parity (`asynInterposeEos.c::readIt:191,199`): clearing the input
    /// terminator on an installed interpose must NOT strand bytes already
    /// read ahead into `in_buf` by a prior EOS read — the cleared-EOS read
    /// still drains `in_buf` first (processEosIn stays on; only matching is
    /// gated on a non-empty terminator). Reachable via `OctetReadBinary`,
    /// which clears IEOS before the raw read (port_actor.rs).
    #[test]
    fn cleared_input_eos_still_drains_buffered_readahead() {
        let mut interpose = EosInterpose::new(EosConfig {
            input_eos: vec![b'\n'],
            output_eos: vec![],
        });
        // One lower read returns the whole buffer; the EOS read returns "AB"
        // and leaves "CD\n" stranded in in_buf.
        let mut base = MockOctetBase::new(b"AB\nCD\n");
        let user = AsynUser::default();

        let mut buf = [0u8; 16];
        let first = interpose.read(&user, &mut buf, &mut base).unwrap();
        assert_eq!(&buf[..first.nbytes_transferred], b"AB");
        assert!(first.eom_reason.contains(EomReason::EOS));
        assert_ne!(
            interpose.peek(0).in_buf_tail,
            interpose.peek(0).in_buf_head,
            "read-ahead must leave CD\\n buffered"
        );

        // Clear IEOS (the binary-suppress path). The next read must deliver
        // the buffered "CD\n", not skip to the (now empty) lower layer.
        interpose.set_input_eos(0, b"");
        let mut buf2 = [0u8; 16];
        let second = interpose.read(&user, &mut buf2, &mut base).unwrap();
        assert_eq!(
            &buf2[..second.nbytes_transferred],
            b"CD\n",
            "cleared EOS must still drain buffered read-ahead bytes"
        );
    }

    /// C `eosInExceptionHandler` (asynInterposeEos.c:142-151) drops the
    /// read-ahead buffer on `asynExceptionConnect`. Boundary: bytes of the
    /// *old* link are still buffered when the link changes — the first read
    /// on the new link must come from the new link, not from `in_buf`.
    #[test]
    fn connection_change_drops_stale_read_ahead() {
        let mut interpose = EosInterpose::new(EosConfig {
            input_eos: vec![b'\n'],
            output_eos: vec![],
        });
        // One lower read grabs "OLD1\nOLD2\n"; the first read returns "OLD1"
        // and leaves "OLD2\n" stranded in in_buf.
        let mut old_link = MockOctetBase::new(b"OLD1\nOLD2\n");
        let user = AsynUser::default();
        let mut buf = [0u8; 32];

        let r = interpose.read(&user, &mut buf, &mut old_link).unwrap();
        assert_eq!(&buf[..r.nbytes_transferred], b"OLD1");
        assert_ne!(
            interpose.peek(0).in_buf_tail,
            interpose.peek(0).in_buf_head,
            "precondition: OLD2\\n is buffered read-ahead"
        );

        // The link drops and comes back (either edge fires C's
        // asynExceptionConnect).
        interpose.connection_changed();
        assert_eq!(interpose.peek(0).in_buf_head, 0);
        assert_eq!(interpose.peek(0).in_buf_tail, 0);

        let mut new_link = MockOctetBase::new(b"NEW1\n");
        let r = interpose.read(&user, &mut buf, &mut new_link).unwrap();
        assert_eq!(
            &buf[..r.nbytes_transferred],
            b"NEW1",
            "first read after reconnect must not serve the previous link's bytes"
        );
    }

    /// The other half of C's reset: `eosInMatch`. Boundary: a 2-byte
    /// terminator straddles the drop, leaving `eos_in_match == 1`. Without
    /// the reset the first byte of the new session that happens to equal the
    /// terminator's *second* byte completes a spurious EOS match, truncating
    /// the first response and reporting EOS one byte early.
    #[test]
    fn connection_change_clears_partial_eos_match() {
        let mut interpose = EosInterpose::new(EosConfig {
            input_eos: vec![b'\r', b'\n'],
            output_eos: vec![],
        });
        let user = AsynUser::default();

        // Old link ends mid-terminator: "AB\r" leaves eos_in_match == 1.
        let mut old_link = MockOctetBase::new(b"AB\r");
        let mut buf = [0u8; 32];
        let r = interpose.read(&user, &mut buf, &mut old_link).unwrap();
        assert_eq!(&buf[..r.nbytes_transferred], b"AB\r");
        assert_eq!(
            interpose.peek(0).eos_in_match,
            1,
            "precondition: the trailing \\r left a partial match"
        );

        interpose.connection_changed();
        assert_eq!(interpose.peek(0).eos_in_match, 0);

        // New session's first byte is '\n' — the terminator's second byte.
        // With a stale match it would complete the EOS and return 0 bytes;
        // after the reset it is ordinary data.
        let mut new_link = MockOctetBase::new(b"\nHELLO\r\n");
        let mut buf2 = [0u8; 32];
        let r = interpose.read(&user, &mut buf2, &mut new_link).unwrap();
        assert_eq!(
            &buf2[..r.nbytes_transferred],
            b"\nHELLO",
            "a stale partial match must not eat the new session's first byte"
        );
        assert!(r.eom_reason.contains(EomReason::EOS));
    }

    /// R14-49: on a multi-device port each device gets its own `eosPvt` — its
    /// own terminator *and* its own read-ahead buffer (C creates one per
    /// (port, addr), asynInterposeEos.c:84-120). A shared buffer would serve one
    /// device's bytes to another.
    #[test]
    fn each_device_keeps_its_own_terminator_and_read_ahead() {
        let mut interpose = EosInterpose::new(EosConfig::default());
        interpose.attach_port(true);
        interpose.set_input_eos(1, b"\n");
        interpose.set_input_eos(2, b";");
        assert_eq!(interpose.get_input_eos(1), b"\n");
        assert_eq!(interpose.get_input_eos(2), b";");

        let dev1 = AsynUser::default().with_addr(1);
        let dev2 = AsynUser::default().with_addr(2);
        // One lower read drains the whole stream into *device 1's* buffer: it
        // returns "AB" on its own terminator and holds "CD;" as read-ahead.
        let mut base = MockOctetBase::new(b"AB\nCD;");
        let mut buf = [0u8; 16];
        let r = interpose.read(&dev1, &mut buf, &mut base).unwrap();
        assert_eq!(&buf[..r.nbytes_transferred], b"AB");
        assert!(r.eom_reason.contains(EomReason::EOS));

        // Device 2 reads with the stream exhausted. With a port-wide buffer it
        // would be handed device 1's "CD" and stop on device 2's ';'.
        let r = interpose.read(&dev2, &mut buf, &mut base).unwrap();
        assert_eq!(
            r.nbytes_transferred, 0,
            "a device must never be served another device's read-ahead"
        );

        // Device 1's own next read still gets its buffered remainder, on its
        // own terminator ('\n' — not device 2's ';').
        let r = interpose.read(&dev1, &mut buf, &mut base).unwrap();
        assert_eq!(&buf[..r.nbytes_transferred], b"CD;");
        assert!(!r.eom_reason.contains(EomReason::EOS));
    }

    /// The single-device boundary: no `ASYN_MULTIDEVICE`, so every addr resolves
    /// to the port itself and must share one terminator (see
    /// [`crate::port::eos_device_key`]).
    #[test]
    fn a_single_device_port_collapses_every_addr_onto_one_terminator() {
        let mut interpose = EosInterpose::new(EosConfig::default());
        interpose.attach_port(false);
        interpose.set_input_eos(-1, b"\n");
        assert_eq!(interpose.get_input_eos(0), b"\n");
        assert_eq!(interpose.get_input_eos(7), b"\n");
    }

    #[test]
    fn test_eos_config_getters_setters() {
        let mut interpose = EosInterpose::new(EosConfig::default());
        assert!(interpose.get_input_eos(0).is_empty());

        interpose.set_input_eos(0, b"\n");
        assert_eq!(interpose.get_input_eos(0), b"\n");

        interpose.set_output_eos(0, b"\r\n");
        assert_eq!(interpose.get_output_eos(0), b"\r\n");
    }

    #[test]
    fn test_null_termination() {
        let mut interpose = EosInterpose::new(EosConfig {
            input_eos: vec![b'\n'],
            output_eos: vec![],
        });
        let mut base = MockOctetBase::new(b"hi\n");
        let user = AsynUser::default();
        let mut buf = [0xFFu8; 64];

        let r = interpose.read(&user, &mut buf, &mut base).unwrap();
        assert_eq!(r.nbytes_transferred, 2);
        assert_eq!(&buf[..2], b"hi");
        // Null terminated after data
        assert_eq!(buf[2], 0);
    }

    #[test]
    fn test_eos_resynchronization() {
        // Test resync: EOS is "\r\n", input has a lone \r followed by \r\n
        let mut interpose = EosInterpose::new(EosConfig {
            input_eos: vec![b'\r', b'\n'],
            output_eos: vec![],
        });
        let mut base = MockOctetBase::new(b"a\rb\r\n");
        let user = AsynUser::default();
        let mut buf = [0u8; 64];

        let r = interpose.read(&user, &mut buf, &mut base).unwrap();
        // Should get "a\rb" — the lone \r doesn't match \r\n, resync finds real \r\n
        assert_eq!(&buf[..r.nbytes_transferred], b"a\rb");
        assert!(r.eom_reason.contains(EomReason::EOS));
    }

    #[test]
    fn test_cnt_filtering_from_lower_layer() {
        // If lower layer sets CNT (buffer full), EOS layer should ignore it
        // and keep reading for EOS
        struct CntBase {
            chunks: Vec<Vec<u8>>,
            idx: usize,
        }
        impl OctetNext for CntBase {
            fn read(&mut self, _user: &AsynUser, buf: &mut [u8]) -> AsynResult<OctetReadResult> {
                if self.idx < self.chunks.len() {
                    let chunk = &self.chunks[self.idx];
                    self.idx += 1;
                    let n = chunk.len().min(buf.len());
                    buf[..n].copy_from_slice(&chunk[..n]);
                    Ok(OctetReadResult {
                        nbytes_transferred: n,
                        // Lower layer reports CNT (its buffer was full)
                        eom_reason: EomReason::CNT,
                    })
                } else {
                    Ok(OctetReadResult {
                        nbytes_transferred: 0,
                        eom_reason: EomReason::empty(),
                    })
                }
            }
            fn write(&mut self, _user: &mut AsynUser, _data: &[u8]) -> AsynResult<usize> {
                Ok(0)
            }
            fn flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
                Ok(())
            }
        }

        let mut interpose = EosInterpose::new(EosConfig {
            input_eos: vec![b'\n'],
            output_eos: vec![],
        });
        // Data split across two lower reads, both with CNT
        let mut base = CntBase {
            chunks: vec![b"hel".to_vec(), b"lo\n".to_vec()],
            idx: 0,
        };
        let user = AsynUser::default();
        let mut buf = [0u8; 64];

        let r = interpose.read(&user, &mut buf, &mut base).unwrap();
        assert_eq!(&buf[..r.nbytes_transferred], b"hello");
        assert!(r.eom_reason.contains(EomReason::EOS));
        // CNT from lower layer should NOT be in the result
        assert!(!r.eom_reason.contains(EomReason::CNT));
    }

    /// A base that serves one EOS-less chunk and then fails — the classic
    /// "device emitted a partial line and went quiet" timeout.
    struct PartialThenErrBase {
        chunk: Vec<u8>,
        err: Option<AsynError>,
        served: bool,
    }
    impl OctetNext for PartialThenErrBase {
        fn read(&mut self, _user: &AsynUser, buf: &mut [u8]) -> AsynResult<OctetReadResult> {
            if !self.served {
                self.served = true;
                buf[..self.chunk.len()].copy_from_slice(&self.chunk);
                return Ok(OctetReadResult {
                    nbytes_transferred: self.chunk.len(),
                    // No CNT/EOS — short read, EOS layer keeps reading.
                    eom_reason: EomReason::empty(),
                });
            }
            Err(self.err.take().expect("base read called twice after error"))
        }
        fn write(&mut self, _user: &mut AsynUser, _data: &[u8]) -> AsynResult<usize> {
            Ok(0)
        }
        fn flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
            Ok(())
        }
    }

    /// R6-48: C `asynInterposeEos.c::readIt:242-253` runs the SAME tail on
    /// the error path as on the success path — null-terminate, publish the
    /// eom reason, publish `nRead` — and only then `return status`. So the
    /// caller gets the timeout AND the three bytes the device did send. The
    /// bytes land in the caller's buffer; the count and eom ride on the
    /// error.
    ///
    /// The previous version of this test asserted only `is_err()`, cementing
    /// the byte loss it was meant to guard.
    #[test]
    fn test_lower_layer_error_surfaces_with_partial_data() {
        let mut interpose = EosInterpose::new(EosConfig {
            input_eos: vec![b'\n'],
            output_eos: vec![],
        });
        let mut base = PartialThenErrBase {
            chunk: b"abc".to_vec(),
            err: Some(AsynError::Status {
                status: AsynStatus::Timeout,
                message: "read timeout".into(),
            }),
            served: false,
        };
        let user = AsynUser::default();
        let mut buf = [0xFFu8; 64];

        let err = interpose
            .read(&user, &mut buf, &mut base)
            .expect_err("lower-layer timeout must surface even with partial data");

        // The status is preserved (this much the old test checked)...
        assert_eq!(err.status(), AsynStatus::Timeout);
        // ...and so is everything C hands back alongside it.
        let partial = err
            .partial_read()
            .expect("a read that transferred bytes before failing must report them");
        assert_eq!(
            partial.nbytes_transferred(),
            3,
            "C publishes *nbytesTransfered = nRead on the error path"
        );
        assert_eq!(
            partial.data, b"abc",
            "the bytes travel with the error, not only in the caller's buffer — \
             every dispatch hop above this one owns a different buffer"
        );
        assert_eq!(
            partial.eom_reason,
            EomReason::empty(),
            "no EOS was matched and the buffer never filled"
        );
        assert_eq!(
            &buf[..3],
            b"abc",
            "the partial line is in the caller's buffer"
        );
        assert_eq!(buf[3], 0, "C null-terminates on the error path too");
    }

    /// Boundary: the error arrives with *nothing* accumulated. C still runs
    /// the tail, so `nRead` is 0 — the partial must be reported as zero-length,
    /// not omitted, and the status must survive unchanged.
    #[test]
    fn lower_layer_error_with_no_partial_reports_zero() {
        let mut interpose = EosInterpose::new(EosConfig {
            input_eos: vec![b'\n'],
            output_eos: vec![],
        });
        let mut base = PartialThenErrBase {
            chunk: Vec::new(),
            err: Some(AsynError::Status {
                status: AsynStatus::Disconnected,
                message: "peer closed".into(),
            }),
            served: false,
        };
        let user = AsynUser::default();
        let mut buf = [0xFFu8; 16];

        // A zero-byte Ok read breaks the loop, so drive the error directly:
        // mark the chunk as served so the first call returns the error.
        base.served = true;
        let err = interpose.read(&user, &mut buf, &mut base).unwrap_err();
        assert_eq!(err.status(), AsynStatus::Disconnected);
        assert_eq!(err.partial_read().map(|p| p.nbytes_transferred()), Some(0));
        assert_eq!(
            err.partial_read().map(|p| p.data.as_slice()),
            Some(&[][..]),
            "a zero transfer is reported as an empty partial, not omitted"
        );
        assert_eq!(buf[0], 0, "null-terminated with zero bytes read");
    }

    /// Boundary: a non-status error (`Io`) picked up mid-accumulation folds
    /// into C's generic `asynError` and still carries the partial. Without
    /// the single `AsynError::status()` owner this would have been
    /// misclassified by every consumer that matched on the variant.
    #[test]
    fn io_error_mid_accumulation_carries_partial_as_generic_error() {
        let mut interpose = EosInterpose::new(EosConfig {
            input_eos: vec![b'\r', b'\n'],
            output_eos: vec![],
        });
        let mut base = PartialThenErrBase {
            chunk: b"XY".to_vec(),
            err: Some(AsynError::Io(std::io::Error::other("cable yanked"))),
            served: false,
        };
        let user = AsynUser::default();
        let mut buf = [0u8; 32];

        let err = interpose.read(&user, &mut buf, &mut base).unwrap_err();
        assert_eq!(err.status(), AsynStatus::Error);
        assert_eq!(err.partial_read().map(|p| p.nbytes_transferred()), Some(2));
        assert_eq!(
            err.partial_read().map(|p| p.data.as_slice()),
            Some(&b"XY"[..])
        );
        assert_eq!(&buf[..2], b"XY");
        assert!(
            err.to_string().contains("cable yanked"),
            "the underlying cause must survive the fold, got {err}"
        );
        // R8-48: the carrier must not hide *which kind* of failure it wraps.
        // The drivers tear the link down on a real errno and leave it up on a
        // timeout; a hangup that arrives mid-line — wrapped — is still a
        // hangup, and asking through `is_fatal_transport` is what keeps it one.
        assert!(
            err.is_fatal_transport(),
            "an errno behind the partial carrier must still disconnect, got {err:?}"
        );
    }

    /// R8-48: C `asynInterposeEos.c::writeIt` (:189-197) runs one tail on every
    /// path — `*nbytesTransfered = min(nbytesActual, numchars); return status` —
    /// so a lower layer that stalled part-way reports its count *through* the
    /// interpose, clamped to the caller's payload. The appended terminator must
    /// never be counted as user bytes, on the error path either.
    #[test]
    fn partial_write_through_the_eos_interpose_clamps_to_the_caller_bytes() {
        /// Takes `accept` bytes of the (payload + EOS) buffer, then times out.
        struct StallingBase {
            accept: usize,
        }
        impl OctetNext for StallingBase {
            fn read(&mut self, _user: &AsynUser, _buf: &mut [u8]) -> AsynResult<OctetReadResult> {
                unreachable!("write-only test")
            }
            fn write(&mut self, _user: &mut AsynUser, _data: &[u8]) -> AsynResult<usize> {
                Err(AsynError::Status {
                    status: AsynStatus::Timeout,
                    message: "serial write timeout".into(),
                }
                .with_partial_write(self.accept))
            }
            fn flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
                Ok(())
            }
        }

        let mut interpose = EosInterpose::new(EosConfig {
            input_eos: vec![],
            output_eos: vec![b'\r', b'\n'],
        });
        let mut user = AsynUser::default();

        // Stalled inside the payload: the caller learns 3 of its 5 bytes went.
        let mut base = StallingBase { accept: 3 };
        let err = interpose.write(&mut user, b"hello", &mut base).unwrap_err();
        assert_eq!(err.status(), AsynStatus::Timeout, "status is untouched");
        assert_eq!(err.partial_write(), Some(3));

        // Stalled inside the *terminator*: every payload byte reached the
        // device, so the clamp reports 5 — not 6 (C: `nbytesActual > numchars`).
        let mut base = StallingBase { accept: 6 };
        let err = interpose.write(&mut user, b"hello", &mut base).unwrap_err();
        assert_eq!(
            err.partial_write(),
            Some(5),
            "the appended EOS is never counted as caller payload"
        );
    }

    #[test]
    fn test_buffer_full_returns_cnt() {
        let mut interpose = EosInterpose::new(EosConfig {
            input_eos: vec![b'\n'],
            output_eos: vec![],
        });
        let mut base = MockOctetBase::new(b"abcdefgh\n");
        let user = AsynUser::default();
        let mut buf = [0u8; 4]; // small buffer

        // First read fills user buffer → CNT
        let r = interpose.read(&user, &mut buf, &mut base).unwrap();
        assert_eq!(r.nbytes_transferred, 4);
        assert_eq!(&buf[..4], b"abcd");
        assert!(r.eom_reason.contains(EomReason::CNT));

        // Second read gets rest up to EOS (need larger buffer to fit remaining data)
        let mut buf2 = [0u8; 64];
        let r = interpose.read(&user, &mut buf2, &mut base).unwrap();
        assert_eq!(&buf2[..r.nbytes_transferred], b"efgh");
        assert!(r.eom_reason.contains(EomReason::EOS));
    }
}
