//! Echo interpose — for half-duplex devices that echo each transmitted character.
//!
//! After sending each byte, waits for the echo to come back before sending the next.
//! Reports an error if the echo doesn't match.

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::interpose::{OctetInterpose, OctetNext, OctetReadResult};
use crate::user::AsynUser;

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
            let n = next.write(user, std::slice::from_ref(byte))?;
            total += n;

            // Read back echo
            let mut echo_buf = [0u8; 1];
            let echo_result = next.read(
                &AsynUser::new(user.reason)
                    .with_addr(user.addr)
                    .with_timeout(user.timeout),
                &mut echo_buf,
            )?;
            if echo_result.nbytes_transferred != 1 {
                return Err(AsynError::Status {
                    status: AsynStatus::Error,
                    message: "echo: no echo received".into(),
                });
            }
            if echo_buf[0] != *byte {
                return Err(AsynError::Status {
                    status: AsynStatus::Error,
                    message: format!(
                        "echo mismatch: sent 0x{:02X}, received 0x{:02X}",
                        byte, echo_buf[0]
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
                assert!(message.contains("echo mismatch"));
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
        assert!(matches!(
            err,
            AsynError::Status {
                status: AsynStatus::Timeout,
                ..
            }
        ));
    }
}
