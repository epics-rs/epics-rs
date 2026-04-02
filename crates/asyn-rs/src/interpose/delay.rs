//! Delay interpose — inserts a delay between each character on write.

use std::time::Duration;

use crate::error::AsynResult;
use crate::interpose::{OctetInterpose, OctetNext, OctetReadResult};
use crate::user::AsynUser;

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
        self.delay = Duration::from_secs_f64(secs);
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
        if self.delay.is_zero() || data.len() <= 1 {
            return next.write(user, data);
        }
        let mut total = 0;
        for (i, byte) in data.iter().enumerate() {
            if i > 0 {
                std::thread::sleep(self.delay);
            }
            let n = next.write(user, std::slice::from_ref(byte))?;
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
        let mut stack = OctetInterposeStack::new();
        stack.push(Box::new(DelayInterpose::new(Duration::from_nanos(1))));

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
        let mut stack = OctetInterposeStack::new();
        stack.push(Box::new(DelayInterpose::new(Duration::ZERO)));

        let mut base = RecordingBase::new();
        let mut user = AsynUser::default();

        let n = stack.dispatch_write(&mut user, b"abc", &mut base).unwrap();
        assert_eq!(n, 3);
        // Zero delay: single write
        assert_eq!(base.written.len(), 1);
    }

    #[test]
    fn test_delay_set_delay() {
        let mut d = DelayInterpose::new(Duration::ZERO);
        d.set_delay("0.001").unwrap();
        assert_eq!(d.delay, Duration::from_millis(1));
        assert!(d.set_delay("invalid").is_err());
    }
}
