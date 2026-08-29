#![allow(dead_code)]
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

pub mod com;
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

/// The transfer an octet read completed *before* it failed — C's
/// `*nbytesTransfered` / `*eomReason`, which `asynOctet::read` writes out
/// even when it returns a failing `asynStatus`
/// (`asynInterposeEos.c:242-253`).
///
/// This owns the bytes rather than pointing at the caller's buffer: it rides
/// inside [`crate::error::AsynError::PartialRead`], so the data and the
/// status are one value and a `?` on the read cannot deliver the failure
/// while dropping the bytes. Every dispatch hop between the interpose and a
/// record (`port_actor` → `PortHandle` → device support / `SyncIO`) hands the
/// error on by value, so the transfer arrives with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartialOctetRead {
    /// The bytes transferred into the caller's buffer before the failure.
    pub data: Vec<u8>,
    /// The end-of-message reason accumulated up to the failure.
    pub eom_reason: EomReason,
}

impl PartialOctetRead {
    /// C's `*nbytesTransfered` for this failed read.
    pub fn nbytes_transferred(&self) -> usize {
        self.data.len()
    }
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

    /// Told the port's device model when the layer is installed. Default no-op;
    /// only a layer that holds per-device state needs it.
    ///
    /// C creates one interpose instance per (port, addr) — the addr is an
    /// argument of `asynInterposeEosConfig` (asynInterposeEos.c:84-110) — so a
    /// layer's state is per device by construction. A Rust stack is per port, so
    /// a layer with device state keys it by the `asynUser`'s addr instead, and
    /// [`crate::port::eos_device_key`] needs this flag to know whether the port
    /// has devices to key by at all.
    fn attach_port(&mut self, _multi_device: bool) {}

    /// Notify the layer of an input end-of-string change *for the device the
    /// `asynUser` addressed*. Default no-op; only EOS-aware layers
    /// (`eos::EosInterpose`) act on it. C asyn routes `setInputEos` through
    /// every interpose via `pasynOctet->setInputEos`, `asynUser` and all
    /// (asynInterposeEos.c:288); this is the Rust equivalent so a runtime IEOS
    /// change reaches the installed EOS interpose on the right device.
    ///
    /// Returns whether **this layer** took the terminator. C's interpose does
    /// not answer with a flag — it either stores the value and returns
    /// `asynSuccess`, or delegates *downwards* to the next `setInputEos`
    /// (:293-295) and ultimately to the driver's, which `initOverride` has
    /// replaced with `setInputEosFail` when the driver has none
    /// (asynOctetBase.c:115, :401-403). The bool is that delegation made
    /// explicit, so [`crate::port::PortDriver::set_input_eos`]'s default can
    /// be the Fail stub without having to guess whether a layer above it
    /// answered first.
    fn set_input_eos(&mut self, _addr: i32, _eos: &[u8]) -> bool {
        false
    }

    /// Notify the layer of an output end-of-string change (see
    /// [`Self::set_input_eos`]). Default: not taken.
    fn set_output_eos(&mut self, _addr: i32, _eos: &[u8]) -> bool {
        false
    }

    /// Drop every piece of state scoped to the *current* link, because the
    /// port's connection state just changed (connected → disconnected or
    /// back). Default no-op: a layer whose state is pure configuration
    /// (`delay`, `flush`, `echo`) survives a reconnect unchanged.
    ///
    /// C parity: `asynInterposeEos.c:110` registers `eosInExceptionHandler`
    /// via `exceptionCallbackAdd`, and `:142-151` clears `inBufHead`,
    /// `inBufTail` and `eosInMatch` on `asynExceptionConnect` — which
    /// `exceptionConnect` (asynManager.c:2158) *and* `exceptionDisconnect`
    /// (asynManager.c:2185) both fire, so the reset happens on either edge.
    /// [`crate::port::PortDriverBase::set_connected`] is the Rust owner of
    /// that transition and drives this hook for the whole stack.
    fn connection_changed(&mut self) {}
}

/// The key of the port's own interpose chain — C's `pport->dpc`, the
/// `dpCommon` every addr that names no device resolves to
/// (`findDpCommon`/`findInterface`, asynManager.c:536-551, 1493-1501).
pub const PORT_CHAIN: i32 = -1;

/// The octet interpose layers of one port — C's `dpCommon.interposeInterfaceList`,
/// which exists once per *device* as well as once per port.
///
/// `interposeInterface` takes an `addr` (asynManager.c:2190-2220): `addr >= 0` on
/// a multi-device port puts the layer on that DEVICE's list (:2202-2206), and
/// `findInterface` resolves a request device-first, port-second (:1493-1501). So
/// `asynInterposeDelay("gpib", 4, 0.01)` slows device 4 and nothing else — a
/// single port-wide chain slowed every device on the bus (R15-48).
///
/// A device's chain **shadows** the port's rather than extending it, because
/// that is what C builds: a layer installed on a device whose list was empty
/// takes its `pPrev` from the *driver's* `interfaceList` (:2211-2215), not from
/// the port's interposes, so it delegates straight down to the driver.
pub struct OctetInterposeStack {
    /// [`PORT_CHAIN`] plus one entry per device that has an interpose of its own.
    chains: std::collections::BTreeMap<i32, Vec<Box<dyn OctetInterpose>>>,
    /// The port's device model, handed to every layer at install time
    /// ([`OctetInterpose::attach_port`]) and what decides whether an addr can
    /// name a device at all — C `locateDevice` returns none for a port that is
    /// not `ASYN_MULTIDEVICE` (asynManager.c:574).
    multi_device: bool,
}

impl OctetInterposeStack {
    pub fn new(multi_device: bool) -> Self {
        Self {
            chains: std::collections::BTreeMap::new(),
            multi_device,
        }
    }

    /// Install an interpose layer on the device `addr` names — C
    /// `interposeInterface` (asynManager.c:2190-2220). `addr < 0`, or any addr on
    /// a port that is not multi-device, installs on the port itself
    /// ([`crate::port::eos_device_key`], C's `locateDevice` at :574).
    ///
    /// The new layer *becomes* that `dpCommon`'s octet interface (C overwrites the
    /// interpose node's `pasynInterface` with it, :2217) and the interface it
    /// displaced — the previously installed interpose at the same level, or the
    /// driver's own interface when there was none — becomes the one it delegates
    /// down to (C hands it back as `pPrev`, :2209-2215).
    ///
    /// So the **last layer installed is the outermost**: a caller enters it first,
    /// and it calls down through the earlier layers to the driver. An
    /// `asynInterposeEcho` installed from iocsh after the driver's configure-time
    /// EOS layer therefore sits *above* EOS, exactly as in C. Dispatch walks index
    /// 0 first, so the new layer goes to the front.
    pub fn install(&mut self, addr: i32, mut layer: Box<dyn OctetInterpose>) {
        layer.attach_port(self.multi_device);
        self.chains
            .entry(crate::port::eos_device_key(self.multi_device, addr))
            .or_default()
            .insert(0, layer);
    }

    /// The chain a request on `addr` runs through — C `findInterface`
    /// (asynManager.c:1493-1501): the device's own interposes if it has any,
    /// otherwise the port's.
    fn chain_key(&self, addr: i32) -> i32 {
        let key = crate::port::eos_device_key(self.multi_device, addr);
        if key != PORT_CHAIN && self.chains.get(&key).is_some_and(|c| !c.is_empty()) {
            key
        } else {
            PORT_CHAIN
        }
    }

    fn chain_mut(&mut self, addr: i32) -> Option<&mut Vec<Box<dyn OctetInterpose>>> {
        let key = self.chain_key(addr);
        self.chains.get_mut(&key).filter(|c| !c.is_empty())
    }

    /// Total number of interpose layers on the port, across every device's chain
    /// — what `asynReport` counts (asynManager.c:993-1005 walks each `dpCommon`'s
    /// list).
    pub fn len(&self) -> usize {
        self.chains.values().map(Vec::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of layers on the chain the given addr resolves to.
    pub fn len_for(&self, addr: i32) -> usize {
        self.chains
            .get(&self.chain_key(addr))
            .map_or(0, |c| c.len())
    }

    /// Forward an input EOS change to the chain the addressed device resolves to.
    /// EOS-aware layers update that device's terminator; others ignore it (trait
    /// default). C routes `setInputEos` through the `asynOctet` interface
    /// `findInterface` returned for that `asynUser`, so it reaches exactly the
    /// layers that will serve the device's reads.
    /// Returns whether any layer took it — C's chain either terminates in a
    /// layer that stores the value or falls through to the driver's method.
    pub fn set_input_eos(&mut self, addr: i32, eos: &[u8]) -> bool {
        let mut taken = false;
        if let Some(chain) = self.chain_mut(addr) {
            for layer in chain {
                taken |= layer.set_input_eos(addr, eos);
            }
        }
        taken
    }

    /// Forward an output EOS change (see [`Self::set_input_eos`]).
    pub fn set_output_eos(&mut self, addr: i32, eos: &[u8]) -> bool {
        let mut taken = false;
        if let Some(chain) = self.chain_mut(addr) {
            for layer in chain {
                taken |= layer.set_output_eos(addr, eos);
            }
        }
        taken
    }

    /// Tell every layer, on every device's chain, that the port's connection
    /// state changed, so any link-scoped state (read-ahead buffers, partial
    /// terminator match position) is dropped before the new link delivers its
    /// first byte. Mirrors C's per-interpose `asynExceptionConnect` handlers
    /// (`asynInterposeEos.c:142-151`); the only caller is the transition owner
    /// [`crate::port::PortDriverBase::set_connected`], and the link it moved is
    /// the one under *all* of them.
    pub fn connection_changed(&mut self) {
        for chain in self.chains.values_mut() {
            for layer in chain {
                layer.connection_changed();
            }
        }
    }

    /// Dispatch a read through the addressed device's interpose chain, ending at
    /// `base`.
    pub fn dispatch_read(
        &mut self,
        user: &AsynUser,
        buf: &mut [u8],
        base: &mut dyn OctetNext,
    ) -> AsynResult<OctetReadResult> {
        let addr = user.addr;
        let Some(layers) = self.chain_mut(addr) else {
            return base.read(user, buf);
        };
        InterposeChain {
            layers: layers.as_mut_slice(),
            base,
        }
        .read(user, buf)
    }

    /// Dispatch a write through the addressed device's interpose chain.
    pub fn dispatch_write(
        &mut self,
        user: &mut AsynUser,
        data: &[u8],
        base: &mut dyn OctetNext,
    ) -> AsynResult<usize> {
        let addr = user.addr;
        let Some(layers) = self.chain_mut(addr) else {
            return base.write(user, data);
        };
        InterposeChain {
            layers: layers.as_mut_slice(),
            base,
        }
        .write(user, data)
    }

    /// Dispatch a flush through the addressed device's interpose chain.
    pub fn dispatch_flush(
        &mut self,
        user: &mut AsynUser,
        base: &mut dyn OctetNext,
    ) -> AsynResult<()> {
        let addr = user.addr;
        let Some(layers) = self.chain_mut(addr) else {
            return base.flush(user);
        };
        InterposeChain {
            layers: layers.as_mut_slice(),
            base,
        }
        .flush(user)
    }
}

impl Default for OctetInterposeStack {
    /// A stack on a single-device port — the common case, and the one where
    /// every addr collapses onto one EOS entry.
    fn default() -> Self {
        Self::new(false)
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
        let mut stack = OctetInterposeStack::new(false);
        let mut base = MockBase::new(b"hello");
        let user = AsynUser::default();
        let mut buf = [0u8; 32];

        let result = stack.dispatch_read(&user, &mut buf, &mut base).unwrap();
        assert_eq!(result.nbytes_transferred, 5);
        assert_eq!(&buf[..5], b"hello");
    }

    #[test]
    fn test_single_passthrough_layer() {
        let mut stack = OctetInterposeStack::new(false);
        stack.install(-1, Box::new(PassthroughInterpose));

        let mut base = MockBase::new(b"world");
        let user = AsynUser::default();
        let mut buf = [0u8; 32];

        let result = stack.dispatch_read(&user, &mut buf, &mut base).unwrap();
        assert_eq!(result.nbytes_transferred, 5);
        assert_eq!(&buf[..5], b"world");
    }

    #[test]
    fn test_uppercase_interpose_write() {
        let mut stack = OctetInterposeStack::new(false);
        stack.install(-1, Box::new(UppercaseInterpose));

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
        let mut stack = OctetInterposeStack::new(false);
        stack.install(-1, Box::new(PassthroughInterpose));
        stack.install(-1, Box::new(UppercaseInterpose));
        assert_eq!(stack.len(), 2);

        let mut base = MockBase::new(b"");
        let mut user = AsynUser::default();

        // UppercaseInterpose (installed last, so outermost) -> Passthrough -> base
        stack.dispatch_write(&mut user, b"test", &mut base).unwrap();
        assert_eq!(&base.written, b"TEST");
    }

    #[test]
    fn test_flush_dispatch() {
        let mut stack = OctetInterposeStack::new(false);
        stack.install(-1, Box::new(PassthroughInterpose));

        let mut base = MockBase::new(b"");
        let mut user = AsynUser::default();

        stack.dispatch_flush(&mut user, &mut base).unwrap();
        assert!(base.flushed);
    }

    /// R9-56. C `interposeInterface` (asynManager.c:2209-2217) makes each newly
    /// installed layer the port's octet interface and hands it the one it
    /// displaced to call down into, so the *last* install is the *outermost*.
    /// This stack appended and dispatched from index 0, so a later install landed
    /// *innermost* — the exact inverse. Under the inverted rule an
    /// `asynInterposeEcho`/`asynInterposeDelay` installed from iocsh sank *below*
    /// the EOS layer the driver installs at configure time, when C puts it above.
    #[test]
    fn the_last_layer_installed_is_the_outermost() {
        /// Marks the payload with its own tag on the way down, so the base's
        /// buffer records the order the layers ran in.
        struct Tag(u8);
        impl OctetInterpose for Tag {
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
                let mut tagged = vec![self.0];
                tagged.extend_from_slice(data);
                next.write(user, &tagged)
            }
            fn flush(&mut self, user: &mut AsynUser, next: &mut dyn OctetNext) -> AsynResult<()> {
                next.flush(user)
            }
        }

        let mut stack = OctetInterposeStack::new(false);
        stack.install(-1, Box::new(Tag(b'A')));
        stack.install(-1, Box::new(Tag(b'B')));
        stack.install(-1, Box::new(Tag(b'C')));

        let mut base = MockBase::new(b"");
        let mut user = AsynUser::default();
        stack.dispatch_write(&mut user, b"x", &mut base).unwrap();

        // C's chain is C -> B -> A -> driver: the caller enters the last-installed
        // layer first, and each calls down into the one it displaced, so the
        // driver sees the tags in install order. The inverted stack ran A first
        // and delivered b"CBAx".
        assert_eq!(&base.written, b"ABCx");
    }

    /// R15-48: an interpose installed with an `addr` serves that DEVICE, not the
    /// whole port.
    ///
    /// C `interposeInterface(portName, addr, ...)` puts the layer on the device's
    /// `dpCommon.interposeInterfaceList` when `addr >= 0` (asynManager.c:2202-2206),
    /// and `findInterface` resolves a request device-first, port-second
    /// (:1493-1501). Both iocsh interposes pass their addr
    /// (asynInterposeEcho.c:176, asynInterposeDelay.c:187) — so
    /// `asynInterposeDelay("gpib",4,0.01)` slows device 4 and nothing else. The
    /// stack was one port-wide chain, so it slowed every device on the bus.
    ///
    /// One case per boundary: the addressed device, an unaddressed sibling, the
    /// port-level chain as fallback, and the single-device port that collapses
    /// every addr onto one chain.
    #[test]
    fn a_device_addressed_interpose_serves_only_that_device() {
        struct Tag(u8);
        impl OctetInterpose for Tag {
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
                let mut tagged = vec![self.0];
                tagged.extend_from_slice(data);
                next.write(user, &tagged)
            }
            fn flush(&mut self, user: &mut AsynUser, next: &mut dyn OctetNext) -> AsynResult<()> {
                next.flush(user)
            }
        }

        let write_from = |stack: &mut OctetInterposeStack, addr: i32| {
            let mut base = MockBase::new(b"");
            let mut user = AsynUser::new(0).with_addr(addr);
            stack.dispatch_write(&mut user, b"x", &mut base).unwrap();
            base.written
        };

        // A multi-device port: a layer on device 4, another on device 2.
        let mut stack = OctetInterposeStack::new(true);
        stack.install(4, Box::new(Tag(b'D')));
        stack.install(2, Box::new(Tag(b'E')));

        assert_eq!(
            write_from(&mut stack, 4),
            b"Dx",
            "device 4 runs its own layer"
        );
        assert_eq!(
            write_from(&mut stack, 2),
            b"Ex",
            "device 2 runs its own layer"
        );
        assert_eq!(
            write_from(&mut stack, 7),
            b"x",
            "a device with no interpose of its own runs none — C's findInterface \
             falls back to the port's list, which is empty here"
        );
        assert_eq!(stack.len(), 2, "two layers on the port, one per device");
        assert_eq!(stack.len_for(4), 1);
        assert_eq!(stack.len_for(7), 0);

        // The port's own chain (addr < 0) is what every unaddressed device falls
        // back to — C `findInterface`'s second lookup (:1499-1501).
        stack.install(PORT_CHAIN, Box::new(Tag(b'P')));
        assert_eq!(
            write_from(&mut stack, 7),
            b"Px",
            "a device with no chain of its own falls back to the port's"
        );
        assert_eq!(
            write_from(&mut stack, 4),
            b"Dx",
            "...and a device WITH one keeps running only its own: C gives it the \
             driver's interface as pPrev, not the port's interposes (:2211-2215)"
        );

        // A port that never declared ASYN_MULTIDEVICE has no devices to key by:
        // C's `locateDevice` returns none for it (:574), so every addr — the
        // iocsh default 0 included — lands on the port itself.
        let mut single = OctetInterposeStack::new(false);
        single.install(0, Box::new(Tag(b'S')));
        assert_eq!(write_from(&mut single, 0), b"Sx");
        assert_eq!(write_from(&mut single, 4), b"Sx");
        assert_eq!(write_from(&mut single, -1), b"Sx");
    }
}
