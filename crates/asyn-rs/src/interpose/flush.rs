//! Flush-on-read interpose layer.
//!
//! Corresponds to C asyn's `asynInterposeFlush.c`. Before each read,
//! discards any data that arrives within `flush_timeout` to clear stale
//! responses from the input buffer.

use std::time::{Duration, Instant};

use crate::error::AsynResult;
use crate::user::AsynUser;

use super::{OctetInterpose, OctetNext, OctetReadResult};

/// Flush-before-read interpose layer.
///
/// On each `read`, first discards any data available within `flush_timeout`,
/// then performs the actual read. Write and flush are passed through.
pub struct FlushTimeoutInterpose {
    /// How long to wait for stale data before the real read.
    pub flush_timeout: Duration,
    /// Size of the discard buffer.
    pub flush_buffer_size: usize,
}

impl FlushTimeoutInterpose {
    pub fn new(flush_timeout: Duration) -> Self {
        Self {
            flush_timeout,
            flush_buffer_size: 512,
        }
    }
}

impl Default for FlushTimeoutInterpose {
    fn default() -> Self {
        Self::new(Duration::from_millis(50))
    }
}

impl OctetInterpose for FlushTimeoutInterpose {
    fn read(
        &mut self,
        user: &AsynUser,
        buf: &mut [u8],
        next: &mut dyn OctetNext,
    ) -> AsynResult<OctetReadResult> {
        // Discard stale data within the flush timeout window.
        // We create a short-timeout user to avoid blocking forever.
        let deadline = Instant::now() + self.flush_timeout;
        let mut discard = vec![0u8; self.flush_buffer_size];

        let flush_user = AsynUser::new(user.reason)
            .with_addr(user.addr)
            .with_timeout(self.flush_timeout);

        loop {
            if Instant::now() >= deadline {
                break;
            }
            match next.read(&flush_user, &mut discard) {
                Ok(result) if result.nbytes_transferred > 0 => {
                    // Discarded some data, keep going
                    continue;
                }
                _ => break,
            }
        }

        // Now do the real read
        next.read(user, buf)
    }

    fn write(
        &mut self,
        user: &mut AsynUser,
        data: &[u8],
        next: &mut dyn OctetNext,
    ) -> AsynResult<usize> {
        next.write(user, data)
    }

    fn flush(&mut self, user: &mut AsynUser, next: &mut dyn OctetNext) -> AsynResult<()> {
        next.flush(user)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::interpose::EomReason;

    struct CountingBase {
        read_count: Arc<AtomicUsize>,
        reads_returning_data: usize,
    }

    impl CountingBase {
        fn new(reads_returning_data: usize) -> Self {
            Self {
                read_count: Arc::new(AtomicUsize::new(0)),
                reads_returning_data,
            }
        }
    }

    impl OctetNext for CountingBase {
        fn read(&mut self, _user: &AsynUser, buf: &mut [u8]) -> AsynResult<OctetReadResult> {
            let n = self.read_count.fetch_add(1, Ordering::Relaxed);
            if n < self.reads_returning_data {
                // Return stale data
                let msg = b"stale";
                let len = msg.len().min(buf.len());
                buf[..len].copy_from_slice(&msg[..len]);
                Ok(OctetReadResult {
                    nbytes_transferred: len,
                    eom_reason: EomReason::CNT,
                })
            } else if n == self.reads_returning_data {
                // Return nothing to end flush phase
                Ok(OctetReadResult {
                    nbytes_transferred: 0,
                    eom_reason: EomReason::CNT,
                })
            } else {
                // Real read
                let msg = b"real";
                let len = msg.len().min(buf.len());
                buf[..len].copy_from_slice(&msg[..len]);
                Ok(OctetReadResult {
                    nbytes_transferred: len,
                    eom_reason: EomReason::CNT,
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

    #[test]
    fn test_flush_discards_stale_data() {
        let mut interpose = FlushTimeoutInterpose::new(Duration::from_millis(10));
        let mut base = CountingBase::new(2); // 2 reads return stale data
        let user = AsynUser::default();
        let mut buf = [0u8; 32];

        let result = interpose.read(&user, &mut buf, &mut base).unwrap();
        // Should have discarded stale reads, then gotten "real"
        assert_eq!(&buf[..result.nbytes_transferred], b"real");
        // Total reads: 2 stale + 1 empty (breaks flush loop) + 1 real = 4
        assert!(base.read_count.load(Ordering::Relaxed) >= 3);
    }

    #[test]
    fn test_write_passthrough() {
        let mut interpose = FlushTimeoutInterpose::default();
        let mut base = CountingBase::new(0);
        let mut user = AsynUser::default();

        let n = interpose.write(&mut user, b"hello", &mut base).unwrap();
        assert_eq!(n, 0); // base returns 0
    }
}
