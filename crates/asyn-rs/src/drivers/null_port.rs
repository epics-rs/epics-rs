//! Null octet port driver — a registered-but-disconnected asyn port.
//!
//! # Why this exists
//!
//! The differential oracle boots an asyn record on both the C and the Rust
//! side and compares its fields. C's `asynRecord` refuses to `init_record`
//! against an empty PORT (`connectDevice` finds no port and the record errors
//! out), so BOTH sides need a *named* asyn port to attach to before `iocInit`.
//! The reproducer names that port `"ORACLEASYN"`; this driver is the Rust half
//! — a port that exists in the registry so the record can attach, but that
//! never opens a transport, so its connection state is deterministically
//! *disconnected*.
//!
//! # The model: registered, `noAutoConnect`, never connected
//!
//! * **Registered** — publishing it under a name (`ORACLEASYN`) is what lets
//!   the record's `connectDevice` resolve a [`PortHandle`](crate::port_handle::PortHandle)
//!   and latch PCNCT=1 (attached). That attachment is independent of the link
//!   being open.
//! * **`noAutoConnect`** (`base.auto_connect = false`) — the port actor never
//!   fires the auto-reconnect cycle, so nothing drives the link toward
//!   connected behind the record's back.
//! * **Never connected** (`base.init_connected(false)`, and a [`connect`] that
//!   is a no-op) — the link stays open-less. There is no socket, no host, no
//!   thread doing I/O. CNCT stays `0` (Disconnect) and AUCT stays `0`
//!   (noAutoConnect), matching the asyn record's honest portless defaults.
//!
//! This is deterministic by construction: no sockets, no timers, no host to be
//! reachable or not. Every boot yields the same field values.
//!
//! [`connect`]: NullOctetPort::connect

use crate::error::AsynResult;
use crate::port::{PortDriver, PortDriverBase, PortFlags};
use crate::user::AsynUser;

/// A null octet port: registered so an asyn record can attach, but permanently
/// disconnected. See the module docs.
pub struct NullOctetPort {
    base: PortDriverBase,
}

impl NullOctetPort {
    /// Create a null octet port published under `port_name`.
    ///
    /// Starts disconnected with `auto_connect = false` — the record attaches
    /// (PCNCT=1) but the link never opens (CNCT=0).
    pub fn new(port_name: &str) -> Self {
        // Single-device, blocking (ASYN_CANBLOCK, mirroring a real transport
        // port like drvAsynIPPort), destructible.
        let mut base = PortDriverBase::new(
            port_name,
            1,
            PortFlags {
                multi_device: false,
                can_block: true,
                destructible: true,
            },
        );
        // Disconnected link, noAutoConnect — the two properties that make CNCT
        // and AUCT deterministically 0.
        base.init_connected(false);
        base.auto_connect = false;
        Self { base }
    }
}

impl PortDriver for NullOctetPort {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    // CAPS PINNED BY MAIN AFTER C OBSERVATION — provisional
    //
    // The interface set a port advertises decides asyn record readback fields
    // (OCTETIV/OPTIONIV/GPIBIV/I32IV/UI32IV/F64IV from `findInterface`, and the
    // "No asyn<X> interface" diagnostics). It MUST mirror exactly what the C
    // `ORACLEASYN` port serves. Main has not yet booted the C side to observe
    // that set, so this is a placeholder octet-transport guess (the same set
    // drvAsynIPPort registers — asynCommon + asynOption + asynOctet), NOT a
    // confirmed value. Main replaces this after cageting the C readbacks.
    fn capabilities(&self) -> Vec<crate::interfaces::Capability> {
        crate::interfaces::octet_transport_capabilities()
    }

    /// A null port has no transport to open, so a connect request leaves the
    /// link disconnected rather than latching CNCT=1. Combined with
    /// `auto_connect = false`, this keeps the connection state deterministically
    /// `0` regardless of who asks. (Final connect semantics are pinned by main
    /// after C observation — see the `capabilities` note.)
    fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
        Ok(())
    }

    /// No backing transport: a read yields no bytes.
    fn read_octet(&mut self, _user: &AsynUser, _buf: &mut [u8]) -> AsynResult<usize> {
        Ok(0)
    }

    /// No backing transport: a write is accepted and discarded.
    fn write_octet(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        Ok(data.len())
    }
}

#[cfg(test)]
mod tests {
    use super::NullOctetPort;
    use crate::interfaces::Capability;
    use crate::port::PortDriver;
    use crate::user::AsynUser;

    /// The port is born disconnected with `noAutoConnect` — the two properties
    /// that make the asyn record's CNCT and AUCT deterministically 0.
    #[test]
    fn starts_disconnected_and_no_auto_connect() {
        let drv = NullOctetPort::new("ORACLEASYN");
        assert_eq!(drv.base().port_name, "ORACLEASYN");
        assert!(
            !drv.base().is_connected(),
            "null port must start disconnected"
        );
        assert!(
            !drv.base().is_auto_connect(),
            "null port must be noAutoConnect"
        );
    }

    /// A connect request does NOT open the link — the port stays disconnected,
    /// so CNCT cannot drift to 1 behind the record's back.
    #[test]
    fn connect_leaves_port_disconnected() {
        let mut drv = NullOctetPort::new("ORACLEASYN");
        let user = AsynUser::new(0);
        drv.connect(&user).expect("null connect is a no-op Ok");
        assert!(
            !drv.base().is_connected(),
            "null port stays disconnected after a connect request"
        );
    }

    /// Provisional (main pins after C observation): the octet-transport set —
    /// asynCommon + asynOption + asynOctet, the drvAsynIPPort mirror.
    #[test]
    fn capabilities_are_provisional_octet_transport() {
        let drv = NullOctetPort::new("ORACLEASYN");
        let caps = drv.capabilities();
        assert!(caps.contains(&Capability::OctetRead));
        assert!(caps.contains(&Capability::OctetWrite));
        assert!(caps.contains(&Capability::Option));
        // Not a kitchen-sink port: no scalar-numeric interfaces.
        assert!(!caps.contains(&Capability::Int32Read));
        assert!(!caps.contains(&Capability::Float64Read));
    }
}
