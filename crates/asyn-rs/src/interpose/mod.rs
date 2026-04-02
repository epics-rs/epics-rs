#![allow(dead_code)]
#![allow(unused_imports)]
//! Interpose (middleware) framework for layered I/O processing.
//!
//! Currently implements octet-level interpose only. The pattern is designed
//! so that other interface types (e.g., `int32`) can follow the same structure.
//!
//! # Architecture
//!
//! An [`OctetInterposeStack`] holds a chain of [`OctetInterpose`] layers.
//! When I/O is dispatched, an `InterposeChain` cursor walks the stack
//! from outermost to innermost, finally reaching the base driver (which
//! implements [`OctetNext`]).

pub mod delay;
pub mod echo;
pub mod eos;
pub mod flush;

use bitflags::bitflags;

use crate::error::AsynResult;
use crate::user::AsynUser;

bitflags! {
    /// End-of-message reason flags (mirrors C asyn's asynEomReason).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct EomReason: u32 {
        /// Transfer completed because byte count was reached.
        const CNT = 0x01;
        /// Transfer completed because EOS character was detected.
        const EOS = 0x02;
        /// Transfer completed because END indicator (e.g. EOI) was asserted.
        const END = 0x04;
    }
}

/// Result of an octet read operation.
#[derive(Debug, Clone)]
pub struct OctetReadResult {
    /// Number of bytes actually transferred into the buffer.
    pub nbytes_transferred: usize,
    /// Reason(s) the read terminated.
    pub eom_reason: EomReason,
}

/// "Next layer" interface — implemented by both the base driver adapter
/// and by `InterposeChain` to allow recursive dispatch.
pub trait OctetNext: Send + Sync {
    fn read(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<OctetReadResult>;
    fn write(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize>;
    fn flush(&mut self, user: &mut AsynUser) -> AsynResult<()>;
}

/// Interpose layer for octet (byte-stream) I/O.
///
/// Each layer receives the `next` handle to delegate to the layer below.
pub trait OctetInterpose: Send + Sync {
    fn read(
        &mut self,
        user: &AsynUser,
        buf: &mut [u8],
        next: &mut dyn OctetNext,
    ) -> AsynResult<OctetReadResult>;

    fn write(
        &mut self,
        user: &mut AsynUser,
        data: &[u8],
        next: &mut dyn OctetNext,
    ) -> AsynResult<usize>;

    fn flush(&mut self, user: &mut AsynUser, next: &mut dyn OctetNext) -> AsynResult<()>;
}

/// A stack of octet interpose layers.
pub struct OctetInterposeStack {
    layers: Vec<Box<dyn OctetInterpose>>,
}

impl OctetInterposeStack {
    pub fn new() -> Self {
        Self { layers: Vec::new() }
    }

    /// Push a layer onto the top of the stack (outermost = called first).
    pub fn push(&mut self, layer: Box<dyn OctetInterpose>) {
        self.layers.push(layer);
    }

    /// Number of interpose layers.
    pub fn len(&self) -> usize {
        self.layers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    /// Dispatch a read through the interpose chain, ending at `base`.
    pub fn dispatch_read(
        &mut self,
        user: &AsynUser,
        buf: &mut [u8],
        base: &mut dyn OctetNext,
    ) -> AsynResult<OctetReadResult> {
        if self.layers.is_empty() {
            return base.read(user, buf);
        }
        let mut chain = InterposeChain {
            layers: &mut self.layers,
            base,
        };
        chain.read(user, buf)
    }

    /// Dispatch a write through the interpose chain, ending at `base`.
    pub fn dispatch_write(
        &mut self,
        user: &mut AsynUser,
        data: &[u8],
        base: &mut dyn OctetNext,
    ) -> AsynResult<usize> {
        if self.layers.is_empty() {
            return base.write(user, data);
        }
        let mut chain = InterposeChain {
            layers: &mut self.layers,
            base,
        };
        chain.write(user, data)
    }

    /// Dispatch a flush through the interpose chain, ending at `base`.
    pub fn dispatch_flush(
        &mut self,
        user: &mut AsynUser,
        base: &mut dyn OctetNext,
    ) -> AsynResult<()> {
        if self.layers.is_empty() {
            return base.flush(user);
        }
        let mut chain = InterposeChain {
            layers: &mut self.layers,
            base,
        };
        chain.flush(user)
    }
}

impl Default for OctetInterposeStack {
    fn default() -> Self {
        Self::new()
    }
}

/// Cursor that walks the interpose stack via recursive `split_first_mut`.
struct InterposeChain<'a> {
    layers: &'a mut [Box<dyn OctetInterpose>],
    base: &'a mut dyn OctetNext,
}

impl OctetNext for InterposeChain<'_> {
    fn read(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<OctetReadResult> {
        if let Some((first, rest)) = self.layers.split_first_mut() {
            let mut next = InterposeChain {
                layers: rest,
                base: self.base,
            };
            first.read(user, buf, &mut next)
        } else {
            self.base.read(user, buf)
        }
    }

    fn write(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        if let Some((first, rest)) = self.layers.split_first_mut() {
            let mut next = InterposeChain {
                layers: rest,
                base: self.base,
            };
            first.write(user, data, &mut next)
        } else {
            self.base.write(user, data)
        }
    }

    fn flush(&mut self, user: &mut AsynUser) -> AsynResult<()> {
        if let Some((first, rest)) = self.layers.split_first_mut() {
            let mut next = InterposeChain {
                layers: rest,
                base: self.base,
            };
            first.flush(user, &mut next)
        } else {
            self.base.flush(user)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user::AsynUser;

    /// A base driver that just records calls.
    struct MockBase {
        read_data: Vec<u8>,
        written: Vec<u8>,
        flushed: bool,
    }

    impl MockBase {
        fn new(data: &[u8]) -> Self {
            Self {
                read_data: data.to_vec(),
                written: Vec::new(),
                flushed: false,
            }
        }
    }

    impl OctetNext for MockBase {
        fn read(&mut self, _user: &AsynUser, buf: &mut [u8]) -> AsynResult<OctetReadResult> {
            let n = self.read_data.len().min(buf.len());
            buf[..n].copy_from_slice(&self.read_data[..n]);
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
            self.flushed = true;
            Ok(())
        }
    }

    /// A simple pass-through interpose layer.
    struct PassthroughInterpose;

    impl OctetInterpose for PassthroughInterpose {
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
            next.write(user, data)
        }
        fn flush(&mut self, user: &mut AsynUser, next: &mut dyn OctetNext) -> AsynResult<()> {
            next.flush(user)
        }
    }

    /// An interpose that uppercases data on write.
    struct UppercaseInterpose;

    impl OctetInterpose for UppercaseInterpose {
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
            let upper: Vec<u8> = data.iter().map(|b| b.to_ascii_uppercase()).collect();
            next.write(user, &upper)
        }
        fn flush(&mut self, user: &mut AsynUser, next: &mut dyn OctetNext) -> AsynResult<()> {
            next.flush(user)
        }
    }

    #[test]
    fn test_empty_stack_passthrough() {
        let mut stack = OctetInterposeStack::new();
        let mut base = MockBase::new(b"hello");
        let user = AsynUser::default();
        let mut buf = [0u8; 32];

        let result = stack.dispatch_read(&user, &mut buf, &mut base).unwrap();
        assert_eq!(result.nbytes_transferred, 5);
        assert_eq!(&buf[..5], b"hello");
    }

    #[test]
    fn test_single_passthrough_layer() {
        let mut stack = OctetInterposeStack::new();
        stack.push(Box::new(PassthroughInterpose));

        let mut base = MockBase::new(b"world");
        let user = AsynUser::default();
        let mut buf = [0u8; 32];

        let result = stack.dispatch_read(&user, &mut buf, &mut base).unwrap();
        assert_eq!(result.nbytes_transferred, 5);
        assert_eq!(&buf[..5], b"world");
    }

    #[test]
    fn test_uppercase_interpose_write() {
        let mut stack = OctetInterposeStack::new();
        stack.push(Box::new(UppercaseInterpose));

        let mut base = MockBase::new(b"");
        let mut user = AsynUser::default();

        let n = stack
            .dispatch_write(&mut user, b"hello", &mut base)
            .unwrap();
        assert_eq!(n, 5);
        assert_eq!(&base.written, b"HELLO");
    }

    #[test]
    fn test_multi_layer_chain() {
        let mut stack = OctetInterposeStack::new();
        stack.push(Box::new(PassthroughInterpose));
        stack.push(Box::new(UppercaseInterpose));
        assert_eq!(stack.len(), 2);

        let mut base = MockBase::new(b"");
        let mut user = AsynUser::default();

        // PassthroughInterpose -> UppercaseInterpose -> base
        stack.dispatch_write(&mut user, b"test", &mut base).unwrap();
        assert_eq!(&base.written, b"TEST");
    }

    #[test]
    fn test_flush_dispatch() {
        let mut stack = OctetInterposeStack::new();
        stack.push(Box::new(PassthroughInterpose));

        let mut base = MockBase::new(b"");
        let mut user = AsynUser::default();

        stack.dispatch_flush(&mut user, &mut base).unwrap();
        assert!(base.flushed);
    }
}
