//! Echo interpose — for half-duplex devices that echo each transmitted character.
//!
//! After sending each byte, waits for the echo to come back before sending the next.
//! Reports an error if the echo doesn't match. Matches C asyn's `asynInterposeEcho.c`.

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::interpose::{OctetInterpose, OctetNext, OctetReadResult};
use crate::user::AsynUser;

/// The `char outstr[16]` / `char echostr[16]` C escapes the two sides of the
/// mismatch into (asynInterposeEcho.c:71-74). Both sides are a single byte, so
/// the longest escaped form (`\xNN`, 4 chars) is nowhere near the bound — but
/// the bound is C's and it is stated here rather than assumed away.
const ECHO_STR_SIZE: usize = 16;

/// Escape bytes for the diagnostics, C `epicsStrnEscapedFromRaw`
/// (asynInterposeEcho.c:73-74) — the same libCom table the record's TINP goes
/// through, with this call site's own destination bound.
fn escaped(bytes: &[u8]) -> String {
    crate::escape::escaped_from_raw(bytes, ECHO_STR_SIZE)
}

/// Interpose layer for echo-mode serial communication.
pub struct EchoInterpose;

impl EchoInterpose {
    pub fn new() -> Self {
        Self
    }
}

impl Default for EchoInterpose {
    fn default() -> Self {
        Self::new()
    }
}

impl OctetInterpose for EchoInterpose {
    fn read(
        &mut self,
        user: &AsynUser,
        buf: &mut [u8],
        next: &mut dyn OctetNext,
    ) -> AsynResult<OctetReadResult> {
        next.read(user, buf)
    }

    fn write(
        &mut self,
        user: &mut AsynUser,
        data: &[u8],
        next: &mut dyn OctetNext,
    ) -> AsynResult<usize> {
        // C `asynInterposeEcho.c::writeIt` (:40-85) counts a byte as transferred
        // only once its echo has come back and matched — `transfered++` sits at
        // the bottom of the loop, after the echo check — and publishes that
        // count on *every* break (`*nbytesTransfered = transfered`, :83). So a
        // half-echoed command reports the bytes the device confirmed, and each
        // failure exit here carries `total` out with it.
        let mut total = 0;
        for byte in data {
            // Write one byte. A failing lower-layer write propagates its status
            // and message untouched — C breaks on `status != asynSuccess`
            // without rewriting either (:54).
            let n = match next.write(user, std::slice::from_ref(byte)) {
                Ok(n) => n,
                Err(e) => return Err(e.with_partial_write(total)),
            };
            if n != 1 {
                return Err(AsynError::Status {
                    status: AsynStatus::Error,
                    message: format!("wrote {n} chars instead of 1"),
                }
                .with_partial_write(total));
            }

            // Read back the echo.
            let mut echo_buf = [0u8; 1];
            let echo_user = AsynUser::new(user.reason)
                .with_addr(user.addr)
                .with_timeout_opt(user.timeout);
            let echo_result = match next.read(&echo_user, &mut echo_buf) {
                Ok(r) => r,
                // C :64-68 — a timed-out echo read keeps its `asynTimeout`
                // status and only *replaces the message*; it does not become
                // `asynError`. The status is the contract: `asynRecord` maps it
                // to a different alarm than a generic error, and a device
                // support layer that retries on timeout must still see one.
                // The count in the message is C's `transfered` — the 0-based
                // index of the char whose echo never came back.
                //
                // Classify by the carried status, not the variant: the layer
                // below may be the EOS interpose, which returns C's
                // `asynTimeout` wrapped in `PartialRead`, and a variant match
                // would drop it into the generic branch below.
                Err(e) if e.status() == AsynStatus::Timeout => {
                    return Err(AsynError::Status {
                        status: AsynStatus::Timeout,
                        message: format!("timeout reading back char number {total}"),
                    }
                    .with_partial_write(total));
                }
                // C :69 — any other failing status breaks with the lower
                // layer's status and message intact.
                Err(e) => return Err(e.with_partial_write(total)),
            };

            // C :70-79 — a short echo read and a mismatched echo are ONE
            // branch, reporting what came back against what went out.
            let echo = &echo_buf[..echo_result.nbytes_transferred.min(1)];
            if echo_result.nbytes_transferred != 1 || echo[0] != *byte {
                return Err(AsynError::Status {
                    status: AsynStatus::Error,
                    message: format!(
                        "got back '{}' instead of '{}'",
                        escaped(echo),
                        escaped(std::slice::from_ref(byte))
                    ),
                }
                .with_partial_write(total));
            }
            // C `transfered++` (:80) — after the echo matched, not after the
            // write.
            total += n;
        }
        Ok(total)
    }

    fn flush(&mut self, user: &mut AsynUser, next: &mut dyn OctetNext) -> AsynResult<()> {
        next.flush(user)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interpose::{EomReason, OctetInterposeStack, OctetNext, OctetReadResult};
    use crate::user::AsynUser;
    use std::collections::VecDeque;

    /// Mock base that echoes each written byte on the next read.
    struct EchoBase {
        echo_queue: VecDeque<u8>,
        written: Vec<u8>,
    }

    impl EchoBase {
        fn new() -> Self {
            Self {
                echo_queue: VecDeque::new(),
                written: Vec::new(),
            }
        }
    }

    impl OctetNext for EchoBase {
        fn read(&mut self, _user: &AsynUser, buf: &mut [u8]) -> AsynResult<OctetReadResult> {
            if let Some(b) = self.echo_queue.pop_front() {
                buf[0] = b;
                Ok(OctetReadResult {
                    nbytes_transferred: 1,
                    eom_reason: EomReason::CNT,
                })
            } else {
                Err(AsynError::Status {
                    status: AsynStatus::Timeout,
                    message: "no echo data".into(),
                })
            }
        }

        fn write(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
            for &b in data {
                self.written.push(b);
                self.echo_queue.push_back(b); // Echo it back
            }
            Ok(data.len())
        }

        fn flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
            Ok(())
        }
    }

    #[test]
    fn test_echo_success() {
        let mut stack = OctetInterposeStack::new(false);
        stack.install(-1, Box::new(EchoInterpose::new()));

        let mut base = EchoBase::new();
        let mut user = AsynUser::default();

        let n = stack.dispatch_write(&mut user, b"OK", &mut base).unwrap();
        assert_eq!(n, 2);
        assert_eq!(&base.written, b"OK");
    }

    #[test]
    fn test_echo_mismatch() {
        struct BadEchoBase;
        impl OctetNext for BadEchoBase {
            fn read(&mut self, _user: &AsynUser, buf: &mut [u8]) -> AsynResult<OctetReadResult> {
                buf[0] = b'X'; // Always echoes wrong char
                Ok(OctetReadResult {
                    nbytes_transferred: 1,
                    eom_reason: EomReason::CNT,
                })
            }
            fn write(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
                Ok(data.len())
            }
            fn flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
                Ok(())
            }
        }

        let mut stack = OctetInterposeStack::new(false);
        stack.install(-1, Box::new(EchoInterpose::new()));

        let mut base = BadEchoBase;
        let mut user = AsynUser::default();

        let err = stack
            .dispatch_write(&mut user, b"A", &mut base)
            .unwrap_err();
        // R8-49: C's exact diagnostic (asynInterposeEcho.c:75-76) — what came
        // back against what went out, both escaped.
        assert_eq!(err.status(), AsynStatus::Error);
        assert_eq!(err.message(), "got back 'X' instead of 'A'");
        // C `asynInterposeEcho.c:83` publishes `*nbytesTransfered = transfered`
        // on the mismatch break — the byte was sent but never confirmed, so it
        // does not count as transferred.
        assert_eq!(err.partial_write(), Some(0));
    }

    #[test]
    fn test_echo_no_response() {
        struct NoEchoBase;
        impl OctetNext for NoEchoBase {
            fn read(&mut self, _user: &AsynUser, _buf: &mut [u8]) -> AsynResult<OctetReadResult> {
                Err(AsynError::Status {
                    status: AsynStatus::Timeout,
                    message: "timeout".into(),
                })
            }
            fn write(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
                Ok(data.len())
            }
            fn flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
                Ok(())
            }
        }

        let mut stack = OctetInterposeStack::new(false);
        stack.install(-1, Box::new(EchoInterpose::new()));

        let mut base = NoEchoBase;
        let mut user = AsynUser::default();

        let err = stack
            .dispatch_write(&mut user, b"A", &mut base)
            .unwrap_err();
        // R8-49: C (asynInterposeEcho.c:64-68) keeps `asynTimeout` on a
        // timed-out echo read and only replaces the message. It does NOT
        // become `asynError` — asynRecord raises a different alarm for a
        // timeout, and retry-on-timeout device support must still see one.
        assert_eq!(err.status(), AsynStatus::Timeout);
        assert_eq!(err.message(), "timeout reading back char number 0");
        assert_eq!(err.partial_write(), Some(0));
    }

    /// R8-49: the timeout message counts C's `transfered` — the 0-based index
    /// of the char whose echo never came back, not the char position in the
    /// buffer minus one, and not the byte count written.
    #[test]
    fn timeout_message_names_the_char_whose_echo_was_lost() {
        struct EchoesOnlyTheFirst {
            written: usize,
        }
        impl OctetNext for EchoesOnlyTheFirst {
            fn read(&mut self, _user: &AsynUser, buf: &mut [u8]) -> AsynResult<OctetReadResult> {
                if self.written == 1 {
                    buf[0] = b'A';
                    return Ok(OctetReadResult {
                        nbytes_transferred: 1,
                        eom_reason: EomReason::CNT,
                    });
                }
                Err(AsynError::Status {
                    status: AsynStatus::Timeout,
                    message: "timeout".into(),
                })
            }
            fn write(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
                self.written += data.len();
                Ok(data.len())
            }
            fn flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
                Ok(())
            }
        }

        let mut stack = OctetInterposeStack::new(false);
        stack.install(-1, Box::new(EchoInterpose::new()));

        let mut base = EchoesOnlyTheFirst { written: 0 };
        let mut user = AsynUser::default();

        let err = stack
            .dispatch_write(&mut user, b"AB", &mut base)
            .unwrap_err();
        assert_eq!(err.status(), AsynStatus::Timeout);
        assert_eq!(err.message(), "timeout reading back char number 1");
        // The first char's echo matched, so C counts it as transferred.
        assert_eq!(err.partial_write(), Some(1));
    }

    /// R8-49: a non-timeout failure from the layer below breaks with its own
    /// status and message intact (C :73 — `break` without rewriting either).
    /// Only the timeout arm rewrites the message.
    #[test]
    fn non_timeout_read_error_propagates_untouched() {
        struct BrokenReadBase;
        impl OctetNext for BrokenReadBase {
            fn read(&mut self, _user: &AsynUser, _buf: &mut [u8]) -> AsynResult<OctetReadResult> {
                Err(AsynError::Status {
                    status: AsynStatus::Disconnected,
                    message: "port disconnected".into(),
                })
            }
            fn write(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
                Ok(data.len())
            }
            fn flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
                Ok(())
            }
        }

        let mut stack = OctetInterposeStack::new(false);
        stack.install(-1, Box::new(EchoInterpose::new()));

        let mut base = BrokenReadBase;
        let mut user = AsynUser::default();

        let err = stack
            .dispatch_write(&mut user, b"A", &mut base)
            .unwrap_err();
        assert_eq!(err.status(), AsynStatus::Disconnected);
        assert_eq!(err.message(), "port disconnected");
        assert_eq!(err.partial_write(), Some(0));
    }

    /// R8-49: C (:70-79) treats a short echo read and a wrong echo as the same
    /// break, and escapes both sides of the comparison
    /// (`epicsStrnEscapedFromRaw`, :73-74).
    #[test]
    fn short_echo_and_control_chars_use_the_escaped_mismatch_message() {
        struct ShortEchoBase;
        impl OctetNext for ShortEchoBase {
            fn read(&mut self, _user: &AsynUser, _buf: &mut [u8]) -> AsynResult<OctetReadResult> {
                Ok(OctetReadResult {
                    nbytes_transferred: 0,
                    eom_reason: EomReason::CNT,
                })
            }
            fn write(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
                Ok(data.len())
            }
            fn flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
                Ok(())
            }
        }

        let mut stack = OctetInterposeStack::new(false);
        stack.install(-1, Box::new(EchoInterpose::new()));
        let mut base = ShortEchoBase;
        let mut user = AsynUser::default();
        let err = stack
            .dispatch_write(&mut user, b"\n", &mut base)
            .unwrap_err();
        assert_eq!(err.status(), AsynStatus::Error);
        assert_eq!(err.message(), "got back '' instead of '\\n'");
    }

    /// R8-49: a short write from the layer below is C's `nbytesTransfered != 1`
    /// break (:55-60), reported as its own diagnostic rather than folded into
    /// the echo mismatch.
    #[test]
    fn short_write_reports_the_char_count_c_reports() {
        struct ShortWriteBase;
        impl OctetNext for ShortWriteBase {
            fn read(&mut self, _user: &AsynUser, _buf: &mut [u8]) -> AsynResult<OctetReadResult> {
                unreachable!("write never succeeds, so no echo is read")
            }
            fn write(&mut self, _user: &mut AsynUser, _data: &[u8]) -> AsynResult<usize> {
                Ok(0)
            }
            fn flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
                Ok(())
            }
        }

        let mut stack = OctetInterposeStack::new(false);
        stack.install(-1, Box::new(EchoInterpose::new()));
        let mut base = ShortWriteBase;
        let mut user = AsynUser::default();
        let err = stack
            .dispatch_write(&mut user, b"A", &mut base)
            .unwrap_err();
        assert_eq!(err.status(), AsynStatus::Error);
        assert_eq!(err.message(), "wrote 0 chars instead of 1");
        assert_eq!(err.partial_write(), Some(0));
    }
}
