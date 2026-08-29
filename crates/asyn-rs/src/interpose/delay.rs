//! Delay interpose — inserts a delay after each character on write.

use std::time::Duration;

use crate::error::AsynResult;
use crate::interpose::{OctetInterpose, OctetNext, OctetReadResult};
use crate::user::AsynUser;

/// The single owner of every `f64 seconds` → per-character delay conversion.
///
/// C stores the operator's `double` verbatim (`pvt->delay = delay`,
/// asynInterposeDelay.c:210) and hands it to `epicsThreadSleep`, which returns
/// immediately for any non-positive argument. `Duration::from_secs_f64` instead
/// *panics* on a negative or non-finite value, so every f64 the operator can
/// reach — iocsh argument, protocol wire field, `set_delay` string — is
/// converted here: non-positive and non-finite collapse to `Duration::ZERO`,
/// C's "no delay".
pub fn delay_from_secs(secs: f64) -> Duration {
    Duration::try_from_secs_f64(secs).unwrap_or(Duration::ZERO)
}

/// Interpose layer that introduces a per-character write delay.
pub struct DelayInterpose {
    delay: Duration,
}

impl DelayInterpose {
    pub fn new(delay: Duration) -> Self {
        Self { delay }
    }

    /// Set delay from a string value (seconds, e.g. "0.001").
    pub fn set_delay(&mut self, secs_str: &str) -> AsynResult<()> {
        let secs: f64 = secs_str
            .parse()
            .map_err(|_| crate::error::AsynError::Status {
                status: crate::error::AsynStatus::Error,
                message: format!("invalid delay value: '{secs_str}'"),
            })?;
        self.delay = delay_from_secs(secs);
        Ok(())
    }
}

impl OctetInterpose for DelayInterpose {
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
        if self.delay.is_zero() {
            return next.write(user, data);
        }
        // C asynInterposeDelay.c:41-50 writeIt: write one char, then
        // epicsThreadSleep(delay) — AFTER every char, including the last
        // and including a single-char write. On a write error it breaks
        // before sleeping and publishes what it managed to send
        // (`*nbytesTransfered = transfered`, :52), so the count rides out on
        // the error instead of being dropped by `?`.
        let mut total = 0;
        for byte in data.iter() {
            match next.write(user, std::slice::from_ref(byte)) {
                Ok(n) => total += n,
                Err(e) => return Err(e.with_partial_write(total)),
            }
            std::thread::sleep(self.delay);
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

    struct RecordingBase {
        written: Vec<Vec<u8>>,
    }

    impl RecordingBase {
        fn new() -> Self {
            Self {
                written: Vec::new(),
            }
        }
    }

    impl OctetNext for RecordingBase {
        fn read(&mut self, _user: &AsynUser, _buf: &mut [u8]) -> AsynResult<OctetReadResult> {
            Ok(OctetReadResult {
                nbytes_transferred: 0,
                eom_reason: EomReason::CNT,
            })
        }
        fn write(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
            self.written.push(data.to_vec());
            Ok(data.len())
        }
        fn flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
            Ok(())
        }
    }

    #[test]
    fn test_delay_writes_per_char() {
        let mut stack = OctetInterposeStack::new(false);
        stack.install(-1, Box::new(DelayInterpose::new(Duration::from_nanos(1))));

        let mut base = RecordingBase::new();
        let mut user = AsynUser::default();

        let n = stack.dispatch_write(&mut user, b"abc", &mut base).unwrap();
        assert_eq!(n, 3);
        // Each character should be a separate write call
        assert_eq!(base.written.len(), 3);
        assert_eq!(base.written[0], b"a");
        assert_eq!(base.written[1], b"b");
        assert_eq!(base.written[2], b"c");
    }

    #[test]
    fn test_delay_zero_passthrough() {
        let mut stack = OctetInterposeStack::new(false);
        stack.install(-1, Box::new(DelayInterpose::new(Duration::ZERO)));

        let mut base = RecordingBase::new();
        let mut user = AsynUser::default();

        let n = stack.dispatch_write(&mut user, b"abc", &mut base).unwrap();
        assert_eq!(n, 3);
        // Zero delay: single write
        assert_eq!(base.written.len(), 1);
    }

    #[test]
    fn test_single_char_incurs_delay() {
        // C writeIt sleeps after every char, including a lone one; the
        // old `data.len() <= 1` short-circuit skipped the delay entirely.
        let mut stack = OctetInterposeStack::new(false);
        let delay = Duration::from_millis(5);
        stack.install(-1, Box::new(DelayInterpose::new(delay)));

        let mut base = RecordingBase::new();
        let mut user = AsynUser::default();

        let start = std::time::Instant::now();
        let n = stack.dispatch_write(&mut user, b"x", &mut base).unwrap();
        let elapsed = start.elapsed();

        assert_eq!(n, 1);
        assert_eq!(base.written.len(), 1);
        assert!(
            elapsed >= delay,
            "single-char write must incur one delay (>= {delay:?}), got {elapsed:?}"
        );
    }

    #[test]
    fn test_trailing_delay_after_last_char() {
        // N chars => N delays (incl. the trailing one after the last
        // char). The old `if i > 0` guard produced only N-1 delays.
        let mut stack = OctetInterposeStack::new(false);
        let delay = Duration::from_millis(5);
        stack.install(-1, Box::new(DelayInterpose::new(delay)));

        let mut base = RecordingBase::new();
        let mut user = AsynUser::default();

        let start = std::time::Instant::now();
        stack.dispatch_write(&mut user, b"abc", &mut base).unwrap();
        let elapsed = start.elapsed();

        assert!(
            elapsed >= 3 * delay,
            "3 chars must incur 3 delays incl. trailing (>= {:?}), got {elapsed:?}",
            3 * delay
        );
    }

    #[test]
    fn test_delay_set_delay() {
        let mut d = DelayInterpose::new(Duration::ZERO);
        d.set_delay("0.001").unwrap();
        assert_eq!(d.delay, Duration::from_millis(1));
        assert!(d.set_delay("invalid").is_err());
    }

    /// R9-54: the same `Duration::from_secs_f64`-on-parsed-input panic lived on
    /// the delay path. C hands the double to `epicsThreadSleep`, which returns
    /// immediately for a non-positive argument.
    #[test]
    fn test_set_delay_non_positive_is_zero_not_a_panic() {
        let mut d = DelayInterpose::new(Duration::from_millis(5));
        for s in ["-1", "-0.001", "NaN", "inf"] {
            d.set_delay(s).unwrap_or_else(|e| panic!("{s}: {e}"));
            assert_eq!(d.delay, Duration::ZERO, "{s}");
        }
        assert_eq!(delay_from_secs(-1.0), Duration::ZERO);
        assert_eq!(delay_from_secs(0.002), Duration::from_millis(2));
    }
}
