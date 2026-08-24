//! Flush interpose layer.
//!
//! Corresponds to C asyn's `asynInterposeFlush.c`. On explicit flush, discards
//! any stale data by reading with a short timeout until nothing remains.
//! Read and write operations pass through unchanged.

use std::time::Duration;

use crate::error::AsynResult;
use crate::user::AsynUser;

use super::{OctetInterpose, OctetNext, OctetReadResult};

/// Flush interpose layer.
///
/// On `flush()`, temporarily sets a short timeout and reads in a loop to
/// discard stale data, matching C asyn's `asynInterposeFlush` semantics.
/// Read and write are pure pass-through.
pub struct FlushTimeoutInterpose {
    /// Timeout used during flush discard reads.
    pub flush_timeout: Duration,
}

impl FlushTimeoutInterpose {
    pub fn new(flush_timeout: Duration) -> Self {
        Self { flush_timeout }
    }
}

impl Default for FlushTimeoutInterpose {
    fn default() -> Self {
        // C default: 1 ms (minimum when timeout <= 0)
        Self::new(Duration::from_millis(1))
    }
}

impl OctetInterpose for FlushTimeoutInterpose {
    fn read(
        &mut self,
        user: &AsynUser,
        buf: &mut [u8],
        next: &mut dyn OctetNext,
    ) -> AsynResult<OctetReadResult> {
        // Pure pass-through (C parity: readIt just delegates to lower layer)
        next.read(user, buf)
    }

    fn write(
        &mut self,
        user: &mut AsynUser,
        data: &[u8],
        next: &mut dyn OctetNext,
    ) -> AsynResult<usize> {
        // Pure pass-through
        next.write(user, data)
    }

    fn flush(&mut self, user: &mut AsynUser, next: &mut dyn OctetNext) -> AsynResult<()> {
        // Save the user's original timeout and set our short flush timeout
        let save_timeout = user.timeout;
        user.timeout = self.flush_timeout;

        // Discard stale data by reading until nothing comes back.
        //
        // The byte count is the ONLY termination signal, which is C exactly:
        // `flushIt` (asynInterposeFlush.c:124-127) zeroes `nbytesTransferred`,
        // calls `pasynOctetDrv->read` with the returned `asynStatus`
        // **discarded entirely**, and breaks on `if(nbytesTransferred==0)`.
        // `asynOctet::read` writes that count out even when it fails
        // (asynInterposeEos.c:242-253), and this port carries the same count in
        // `PartialRead` — so a failing read that still delivered bytes has
        // drained them and the loop must go on. Deciding from the `Result`
        // variant instead left those bytes in the input path, where they became
        // the next transaction's reply and put every later one off by one.
        //
        // Recomputing `drained` inside the loop is C's per-iteration
        // `nbytesTransferred = 0`: no count survives an iteration to be read
        // again if the layer below leaves one behind.
        let mut buffer = [0u8; 100];
        loop {
            let drained = match next.read(user, &mut buffer) {
                Ok(result) => result.nbytes_transferred,
                Err(e) => e.partial_read().map_or(0, |p| p.nbytes_transferred()),
            };
            if drained == 0 {
                break;
            }
        }

        // Restore original timeout
        user.timeout = save_timeout;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::error::{AsynError, AsynStatus};
    use crate::interpose::{EomReason, PartialOctetRead};

    struct FlushableBase {
        read_count: Arc<AtomicUsize>,
        reads_with_data: usize,
    }

    impl FlushableBase {
        fn new(reads_with_data: usize) -> Self {
            Self {
                read_count: Arc::new(AtomicUsize::new(0)),
                reads_with_data,
            }
        }
    }

    impl OctetNext for FlushableBase {
        fn read(&mut self, _user: &AsynUser, buf: &mut [u8]) -> AsynResult<OctetReadResult> {
            let n = self.read_count.fetch_add(1, Ordering::Relaxed);
            if n < self.reads_with_data {
                let msg = b"stale";
                let len = msg.len().min(buf.len());
                buf[..len].copy_from_slice(&msg[..len]);
                Ok(OctetReadResult {
                    nbytes_transferred: len,
                    eom_reason: EomReason::CNT,
                })
            } else {
                Ok(OctetReadResult {
                    nbytes_transferred: 0,
                    eom_reason: EomReason::CNT,
                })
            }
        }

        fn write(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
            Ok(data.len())
        }

        fn flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
            Ok(())
        }
    }

    #[test]
    fn test_flush_discards_stale_data() {
        let mut interpose = FlushTimeoutInterpose::new(Duration::from_millis(10));
        let mut base = FlushableBase::new(2); // 2 reads return stale data
        let mut user = AsynUser::default();

        interpose.flush(&mut user, &mut base).unwrap();
        // Should have done 2 stale reads + 1 empty read (breaks loop) = 3
        assert!(base.read_count.load(Ordering::Relaxed) >= 3);
    }

    #[test]
    fn test_read_passthrough() {
        let mut interpose = FlushTimeoutInterpose::default();
        let mut base = FlushableBase::new(1); // 1 read returns data
        let user = AsynUser::default();
        let mut buf = [0u8; 32];

        let result = interpose.read(&user, &mut buf, &mut base).unwrap();
        assert_eq!(&buf[..result.nbytes_transferred], b"stale");
    }

    #[test]
    fn test_write_passthrough() {
        let mut interpose = FlushTimeoutInterpose::default();
        let mut base = FlushableBase::new(0);
        let mut user = AsynUser::default();

        let n = interpose.write(&mut user, b"hello", &mut base).unwrap();
        assert_eq!(n, 5);
    }

    #[test]
    fn test_flush_restores_timeout() {
        let mut interpose = FlushTimeoutInterpose::new(Duration::from_millis(10));
        let mut base = FlushableBase::new(0);
        let original_timeout = Duration::from_secs(5);
        let mut user = AsynUser {
            timeout: original_timeout,
            ..Default::default()
        };

        interpose.flush(&mut user, &mut base).unwrap();
        assert_eq!(user.timeout, original_timeout);
    }

    /// One lower-layer read outcome. These are the drain loop's boundaries:
    /// the count is zero or positive, and it arrives on the `Ok` arm or on the
    /// `Err` arm.
    #[derive(Clone, Copy)]
    enum Step {
        /// A successful read of `n` bytes.
        Ok(usize),
        /// A failing status that still transferred `n` bytes — what the EOS
        /// interpose returns for a timeout part-way through a line
        /// (`interpose/mod.rs` `PartialOctetRead`).
        ErrWith(usize),
        /// A failing status that transferred nothing.
        ErrEmpty,
    }

    /// Replays a fixed script of read outcomes and counts the calls. Once the
    /// script runs out it reports an empty read, so a loop that fails to
    /// terminate shows up as a wrong call count instead of hanging the suite.
    struct ScriptedBase {
        steps: Vec<Step>,
        reads: usize,
    }

    impl OctetNext for ScriptedBase {
        fn read(&mut self, _user: &AsynUser, buf: &mut [u8]) -> AsynResult<OctetReadResult> {
            let step = self.steps.get(self.reads).copied().unwrap_or(Step::Ok(0));
            self.reads += 1;
            let timeout = || AsynError::Status {
                status: AsynStatus::Timeout,
                message: "flush read timeout".into(),
            };
            let deliver = |buf: &mut [u8], n: usize| {
                let n = n.min(buf.len());
                buf[..n].fill(b'x');
                n
            };
            match step {
                Step::Ok(n) => Ok(OctetReadResult {
                    nbytes_transferred: deliver(buf, n),
                    eom_reason: EomReason::CNT,
                }),
                Step::ErrWith(n) => {
                    let n = deliver(buf, n);
                    Err(timeout().with_partial_read(PartialOctetRead {
                        data: vec![b'x'; n],
                        eom_reason: EomReason::empty(),
                    }))
                }
                Step::ErrEmpty => Err(timeout()),
            }
        }

        fn write(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
            Ok(data.len())
        }

        fn flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
            Ok(())
        }
    }

    /// Runs one flush against `script` and reports how many reads it took.
    fn drain_reads(script: &[Step]) -> usize {
        let mut interpose = FlushTimeoutInterpose::new(Duration::from_millis(10));
        let mut base = ScriptedBase {
            steps: script.to_vec(),
            reads: 0,
        };
        let mut user = AsynUser::default();
        interpose
            .flush(&mut user, &mut base)
            .expect("flush reports success whatever the drain saw, as C's flushIt does");
        base.reads
    }

    /// Boundary: count zero on the `Ok` arm ends the drain — C's
    /// `if(nbytesTransferred==0) break;`.
    #[test]
    fn zero_byte_ok_read_terminates_the_drain() {
        assert_eq!(drain_reads(&[Step::Ok(0)]), 1);
    }

    /// Boundary: count positive on the `Ok` arm keeps draining.
    #[test]
    fn positive_byte_ok_read_continues_the_drain() {
        assert_eq!(drain_reads(&[Step::Ok(5), Step::Ok(0)]), 2);
    }

    /// Boundary: count positive on the `Err` arm keeps draining. This is the
    /// regression — a failing read that delivered bytes has drained them, and
    /// stopping there left the rest of the stale input in the path, where it
    /// became the next transaction's reply.
    #[test]
    fn failing_read_with_partial_bytes_continues_the_drain() {
        assert_eq!(drain_reads(&[Step::ErrWith(5), Step::Ok(0)]), 2);
        assert_eq!(
            drain_reads(&[Step::ErrWith(5), Step::ErrWith(3), Step::Ok(0)]),
            3,
            "the count governs for as long as failing reads keep delivering bytes"
        );
    }

    /// Boundary: count zero on the `Err` arm ends the drain — the layer below
    /// has nothing left, which is the ordinary way a flush finishes on a port
    /// whose input has no trailing EOS.
    #[test]
    fn failing_read_with_no_bytes_terminates_the_drain() {
        assert_eq!(drain_reads(&[Step::ErrEmpty]), 1);
    }
}
