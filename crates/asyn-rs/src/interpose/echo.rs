//! Echo interpose — for half-duplex devices that echo each transmitted character.
//!
//! After sending each byte, waits for the echo to come back before sending the next.
//! Reports an error if the echo doesn't match. Matches C asyn's `asynInterposeEcho.c`.

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::interpose::{OctetInterpose, OctetNext, OctetReadResult};
use crate::user::AsynUser;

/// Format a byte as an escaped string for error messages.
fn escape_byte(b: u8) -> String {
    match b {
        b'\n' => "\\n".to_string(),
        b'\r' => "\\r".to_string(),
        b'\t' => "\\t".to_string(),
        b'\0' => "\\0".to_string(),
        0x20..=0x7e => String::from(b as char),
        _ => format!("\\x{:02x}", b),
    }
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
        let mut total = 0;
        for byte in data {
            // Write one byte
            let n = next.write(user, std::slice::from_ref(byte))?;
            if n != 1 {
                return Err(AsynError::Status {
                    status: AsynStatus::Error,
                    message: format!(
                        "echo: write (0x{:02X}) returned {} bytes, expected 1",
                        byte, n
                    ),
                });
            }
            total += n;

            // Read back echo
            let mut echo_buf = [0u8; 1];
            let echo_user = AsynUser::new(user.reason)
                .with_addr(user.addr)
                .with_timeout(user.timeout);
            let echo_result = match next.read(&echo_user, &mut echo_buf) {
                Ok(r) => r,
                // C parity: a timeout on the echo read gets the specific
                // "Loss of communication?" message (asynInterposeEcho.c).
                // Classify by the carried status, not the variant: the layer
                // below this one may be the EOS interpose, which returns C's
                // `asynTimeout` wrapped as `PartialRead` — a variant match
                // would drop such a timeout into the generic branch and
                // report the wrong diagnostic.
                Err(e) if e.status() == AsynStatus::Timeout => {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!(
                            "echo: write/read (0x{:02X}) -- no echo - Loss of communication?",
                            byte
                        ),
                    });
                }
                Err(e) => {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!("echo: write/read (0x{:02X}) -- read failed: {}", byte, e),
                    });
                }
            };

            if echo_result.nbytes_transferred != 1 {
                return Err(AsynError::Status {
                    status: AsynStatus::Error,
                    message: format!(
                        "echo: write/read (0x{:02X}) -- read count {}",
                        byte, echo_result.nbytes_transferred
                    ),
                });
            }
            if echo_buf[0] != *byte {
                return Err(AsynError::Status {
                    status: AsynStatus::Error,
                    message: format!(
                        "echo: expected '{}' got '{}'",
                        escape_byte(*byte),
                        escape_byte(echo_buf[0])
                    ),
                });
            }
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
        let mut stack = OctetInterposeStack::new();
        stack.push(Box::new(EchoInterpose::new()));

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

        let mut stack = OctetInterposeStack::new();
        stack.push(Box::new(EchoInterpose::new()));

        let mut base = BadEchoBase;
        let mut user = AsynUser::default();

        let err = stack
            .dispatch_write(&mut user, b"A", &mut base)
            .unwrap_err();
        match err {
            AsynError::Status { message, .. } => {
                assert!(message.contains("expected"));
                assert!(message.contains("got"));
            }
            other => panic!("expected echo mismatch error, got {other:?}"),
        }
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

        let mut stack = OctetInterposeStack::new();
        stack.push(Box::new(EchoInterpose::new()));

        let mut base = NoEchoBase;
        let mut user = AsynUser::default();

        let err = stack
            .dispatch_write(&mut user, b"A", &mut base)
            .unwrap_err();
        // C parity: timeout is converted to Error with "Loss of communication?" message
        match err {
            AsynError::Status { status, message } => {
                assert_eq!(status, AsynStatus::Error);
                assert!(message.contains("no echo"));
                assert!(message.contains("Loss of communication"));
            }
            other => panic!("expected loss-of-comm error, got {other:?}"),
        }
    }

    #[test]
    fn test_escape_byte_formatting() {
        assert_eq!(escape_byte(b'A'), "A");
        assert_eq!(escape_byte(b'\n'), "\\n");
        assert_eq!(escape_byte(b'\r'), "\\r");
        assert_eq!(escape_byte(0x01), "\\x01");
        assert_eq!(escape_byte(0xFF), "\\xff");
    }
}
