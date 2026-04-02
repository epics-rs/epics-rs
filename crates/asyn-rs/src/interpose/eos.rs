//! End-of-string (EOS) interpose layer.
//!
//! Corresponds to C asyn's `asynInterposeEos.c`. Supports up to 2-character
//! input/output EOS sequences. On read, buffers data and scans for the EOS
//! pattern. On write, appends the output EOS to outgoing data.

use crate::error::AsynResult;
use crate::user::AsynUser;

use super::{EomReason, OctetInterpose, OctetNext, OctetReadResult};

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

/// EOS interpose layer with internal read buffer and state machine.
pub struct EosInterpose {
    config: EosConfig,
    read_buffer: Vec<u8>,
    read_pos: usize,
}

impl EosInterpose {
    pub fn new(config: EosConfig) -> Self {
        Self {
            config,
            read_buffer: Vec::with_capacity(256),
            read_pos: 0,
        }
    }

    pub fn set_input_eos(&mut self, eos: &[u8]) {
        self.config.input_eos = eos.to_vec();
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

    /// Scan the buffer [read_pos..] for the input EOS sequence.
    /// Returns Some(end_index) where end_index is the position right after
    /// the last EOS byte, or None if not found.
    fn find_eos(&self) -> Option<usize> {
        let eos = &self.config.input_eos;
        if eos.is_empty() {
            return None;
        }
        let data = &self.read_buffer[self.read_pos..];
        if eos.len() == 1 {
            data.iter()
                .position(|b| *b == eos[0])
                .map(|pos| self.read_pos + pos + 1)
        } else {
            // 2-byte EOS
            data.windows(2)
                .position(|w| w[0] == eos[0] && w[1] == eos[1])
                .map(|pos| self.read_pos + pos + 2)
        }
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

        loop {
            // Check if we already have EOS in the buffer
            if let Some(eos_end) = self.find_eos() {
                // Copy up to (but not including) the EOS chars
                let eos_len = self.config.input_eos.len();
                let data_end = eos_end - eos_len;
                let avail = data_end - self.read_pos;
                let n = avail.min(buf.len());
                buf[..n].copy_from_slice(&self.read_buffer[self.read_pos..self.read_pos + n]);
                // Advance past the EOS
                self.read_pos = eos_end;

                // If we've consumed the entire buffer, reset
                if self.read_pos >= self.read_buffer.len() {
                    self.read_buffer.clear();
                    self.read_pos = 0;
                }

                return Ok(OctetReadResult {
                    nbytes_transferred: n,
                    eom_reason: EomReason::EOS,
                });
            }

            // Check if there's buffered data that fills the user buffer
            let buffered = self.read_buffer.len() - self.read_pos;
            if buffered >= buf.len() {
                // Return what we have (no EOS found yet), buffer is full
                let n = buf.len();
                buf.copy_from_slice(&self.read_buffer[self.read_pos..self.read_pos + n]);
                self.read_pos += n;
                if self.read_pos >= self.read_buffer.len() {
                    self.read_buffer.clear();
                    self.read_pos = 0;
                }
                return Ok(OctetReadResult {
                    nbytes_transferred: n,
                    eom_reason: EomReason::CNT,
                });
            }

            // Read more data from next layer
            let mut tmp = vec![0u8; buf.len().max(256)];
            let result = next.read(user, &mut tmp)?;
            if result.nbytes_transferred == 0 {
                // EOF from lower layer — return whatever we have
                let avail = self.read_buffer.len() - self.read_pos;
                let n = avail.min(buf.len());
                if n > 0 {
                    buf[..n].copy_from_slice(&self.read_buffer[self.read_pos..self.read_pos + n]);
                    self.read_pos += n;
                }
                if self.read_pos >= self.read_buffer.len() {
                    self.read_buffer.clear();
                    self.read_pos = 0;
                }
                return Ok(OctetReadResult {
                    nbytes_transferred: n,
                    eom_reason: result.eom_reason,
                });
            }

            self.read_buffer
                .extend_from_slice(&tmp[..result.nbytes_transferred]);

            // If the lower layer indicated END, check for EOS one more time,
            // then return what we have
            if result.eom_reason.contains(EomReason::END) {
                if let Some(eos_end) = self.find_eos() {
                    let eos_len = self.config.input_eos.len();
                    let data_end = eos_end - eos_len;
                    let avail = data_end - self.read_pos;
                    let n = avail.min(buf.len());
                    buf[..n].copy_from_slice(&self.read_buffer[self.read_pos..self.read_pos + n]);
                    self.read_pos = eos_end;
                    if self.read_pos >= self.read_buffer.len() {
                        self.read_buffer.clear();
                        self.read_pos = 0;
                    }
                    return Ok(OctetReadResult {
                        nbytes_transferred: n,
                        eom_reason: EomReason::EOS | EomReason::END,
                    });
                }
                // No EOS found, return buffered data with END
                let avail = self.read_buffer.len() - self.read_pos;
                let n = avail.min(buf.len());
                buf[..n].copy_from_slice(&self.read_buffer[self.read_pos..self.read_pos + n]);
                self.read_pos += n;
                if self.read_pos >= self.read_buffer.len() {
                    self.read_buffer.clear();
                    self.read_pos = 0;
                }
                return Ok(OctetReadResult {
                    nbytes_transferred: n,
                    eom_reason: EomReason::END,
                });
            }
        }
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
        next.write(user, &buf)
    }

    fn flush(&mut self, user: &mut AsynUser, next: &mut dyn OctetNext) -> AsynResult<()> {
        self.read_buffer.clear();
        self.read_pos = 0;
        next.flush(user)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                eom_reason: if self.pos >= self.data.len() {
                    EomReason::CNT
                } else {
                    EomReason::CNT
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
    fn test_output_eos_append() {
        let mut interpose = EosInterpose::new(EosConfig {
            input_eos: vec![],
            output_eos: vec![b'\r', b'\n'],
        });
        let mut base = MockOctetBase::new(b"");
        let mut user = AsynUser::default();

        interpose.write(&mut user, b"hello", &mut base).unwrap();
        assert_eq!(&base.written, b"hello\r\n");
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
        // After flush, the internal buffer should be empty
        assert_eq!(interpose.read_buffer.len(), 0);
        assert_eq!(interpose.read_pos, 0);
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
}
