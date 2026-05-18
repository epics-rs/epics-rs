//! End-of-string (EOS) interpose layer.
//!
//! Corresponds to C asyn's `asynInterposeEos.c`. Supports up to 2-character
//! input/output EOS sequences. On read, buffers data and scans for the EOS
//! pattern using a character-by-character state machine with resynchronization.
//! On write, appends the output EOS to outgoing data.

use crate::error::AsynResult;
use crate::user::AsynUser;

use super::{EomReason, OctetInterpose, OctetNext, OctetReadResult};

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

/// EOS interpose layer with internal read buffer and character-by-character
/// state machine matching, including resynchronization on partial matches.
///
/// Matches the C implementation's behavior:
/// - Fixed-size internal buffer (2048 bytes)
/// - Character-by-character EOS matching with resynchronization
/// - Filters ASYN_EOM_CNT from lower layer reads
/// - Null-terminates output when there's room
pub struct EosInterpose {
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

impl EosInterpose {
    pub fn new(config: EosConfig) -> Self {
        Self {
            config,
            in_buf: vec![0u8; INPUT_BUFFER_SIZE],
            in_buf_head: 0,
            in_buf_tail: 0,
            eos_in_match: 0,
        }
    }

    pub fn set_input_eos(&mut self, eos: &[u8]) {
        self.config.input_eos = eos.to_vec();
        self.eos_in_match = 0;
    }

    pub fn set_output_eos(&mut self, eos: &[u8]) {
        self.config.output_eos = eos.to_vec();
    }

    pub fn get_input_eos(&self) -> &[u8] {
        &self.config.input_eos
    }

    pub fn get_output_eos(&self) -> &[u8] {
        &self.config.output_eos
    }
}

impl OctetInterpose for EosInterpose {
    fn read(
        &mut self,
        user: &AsynUser,
        buf: &mut [u8],
        next: &mut dyn OctetNext,
    ) -> AsynResult<OctetReadResult> {
        // If no input EOS configured, just pass through
        if self.config.input_eos.is_empty() {
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
        let mut n_read: usize = 0;
        let mut eom = EomReason::empty();

        loop {
            // Process buffered data character by character
            if self.in_buf_tail != self.in_buf_head {
                let c = self.in_buf[self.in_buf_tail];
                self.in_buf_tail += 1;
                buf[n_read] = c;
                n_read += 1;

                let eos = &self.config.input_eos;
                if c == eos[self.eos_in_match] {
                    self.eos_in_match += 1;
                    if self.eos_in_match == eos.len() {
                        // Full EOS match — remove the EOS bytes from the
                        // output count. Only the EOS bytes written into
                        // *this* buffer can be removed: when a 2-byte EOS
                        // straddles two read() calls, the leading byte was
                        // already returned to the previous caller, so
                        // `n_read` here may be smaller than `eos.len()`.
                        // An unguarded `n_read -= eos.len()` underflows.
                        self.eos_in_match = 0;
                        n_read -= eos.len().min(n_read);
                        eom |= EomReason::EOS;
                        break;
                    }
                } else {
                    // Resynchronize the search. Since asyn allows a maximum
                    // two-character EOS, we only need to check if the current
                    // character matches the first EOS character.
                    if c == eos[0] {
                        self.eos_in_match = 1;
                    } else {
                        self.eos_in_match = 0;
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
            // C parity (`asynInterposeEos.c::readIt`): the lower-layer
            // `status` is preserved across the whole loop. When the
            // lower read fails, C `break`s the loop and then executes
            // `return status` — the caller sees the error/timeout
            // regardless of how many bytes were already accumulated in
            // `nRead`. An earlier Rust version swallowed the error into
            // `Ok(...)` when `n_read > 0`, dropping the timeout/error
            // indication entirely. We surface the lower-layer error
            // even when partial data was buffered, matching C.
            let result = next.read(user, &mut self.in_buf[..])?;

            if result.nbytes_transferred == 0 {
                break;
            }

            self.in_buf_tail = 0;
            self.in_buf_head = result.nbytes_transferred;

            // Filter out CNT from lower layer — the lower read may have set CNT
            // because available data exceeded our buffer size. This is not a
            // reason for us to stop reading. (C parity: eom &= ~ASYN_EOM_CNT)
            eom = result.eom_reason & !EomReason::CNT;
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
        if self.config.output_eos.is_empty() {
            return next.write(user, data);
        }

        // Append output EOS to the data
        let mut buf = Vec::with_capacity(data.len() + self.config.output_eos.len());
        buf.extend_from_slice(data);
        buf.extend_from_slice(&self.config.output_eos);
        let actual = next.write(user, &buf)?;
        // Report only user data bytes, not EOS bytes (C parity)
        Ok(actual.min(data.len()))
    }

    fn flush(&mut self, user: &mut AsynUser, next: &mut dyn OctetNext) -> AsynResult<()> {
        self.in_buf_head = 0;
        self.in_buf_tail = 0;
        self.eos_in_match = 0;
        next.flush(user)
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
        assert_eq!(interpose.in_buf_head, 0);
        assert_eq!(interpose.in_buf_tail, 0);
        assert_eq!(interpose.eos_in_match, 0);
    }

    #[test]
    fn test_eos_config_getters_setters() {
        let mut interpose = EosInterpose::new(EosConfig::default());
        assert!(interpose.get_input_eos().is_empty());

        interpose.set_input_eos(b"\n");
        assert_eq!(interpose.get_input_eos(), b"\n");

        interpose.set_output_eos(b"\r\n");
        assert_eq!(interpose.get_output_eos(), b"\r\n");
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

    #[test]
    fn test_lower_layer_error_surfaces_with_partial_data() {
        // BUG 1 regression: C `asynInterposeEos.c::readIt` preserves the
        // lower-layer `status` and `return status` even when partial
        // data was already accumulated. An earlier Rust version
        // converted the timeout/error into `Ok(...)` whenever
        // `n_read > 0`, hiding the failure from the caller.
        //
        // This base feeds one chunk with no EOS, then a timeout. The
        // EOS layer has buffered "abc" (n_read > 0) and must still
        // propagate the timeout `Err`, not return `Ok`.
        struct PartialThenErrBase {
            served: bool,
        }
        impl OctetNext for PartialThenErrBase {
            fn read(&mut self, _user: &AsynUser, buf: &mut [u8]) -> AsynResult<OctetReadResult> {
                if !self.served {
                    self.served = true;
                    let data = b"abc";
                    buf[..data.len()].copy_from_slice(data);
                    Ok(OctetReadResult {
                        nbytes_transferred: data.len(),
                        // No CNT/EOS — short read, EOS layer keeps reading.
                        eom_reason: EomReason::empty(),
                    })
                } else {
                    Err(AsynError::Status {
                        status: AsynStatus::Timeout,
                        message: "read timeout".into(),
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
        let mut base = PartialThenErrBase { served: false };
        let user = AsynUser::default();
        let mut buf = [0u8; 64];

        let err = interpose
            .read(&user, &mut buf, &mut base)
            .expect_err("lower-layer timeout must surface even with partial data");
        match err {
            AsynError::Status {
                status: AsynStatus::Timeout,
                ..
            } => {}
            other => panic!("expected Timeout error, got {other:?}"),
        }
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
