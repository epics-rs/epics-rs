//! R18-60: the octet interpose chain belongs to the *port*, not to the driver.
//!
//! C `interposeInterface` (asynManager.c:2190-2220) swaps the octet interface
//! `findInterface` hands out and keeps the displaced one as the new layer's
//! `pPrev`. The driver underneath is never consulted: it does not dispatch the
//! chain, it *is* the bottom of it. So `asynInterposeEos` / `asynInterposeEcho`
//! / `asynInterposeDelay` serve every port, whichever driver is below.
//!
//! Before the fix each driver ran its own chain inside `io_read_octet_eom` /
//! `io_write_octet`, which made the chain per-driver opt-in — and the drivers
//! that did not opt in silently had none. The sharpest case is
//! `drvAsynFTDIPort`, whose configure installs an EOS interpose
//! (drvAsynFTDIPort.cpp:622-623, `ftdi.rs`) that nothing ever dispatched: an
//! FTDI port with `IEOS="\n"` handed the record the terminator and everything
//! behind it.
//!
//! The driver below stands in for FTDI (which needs the hardware to construct):
//! its `io_*_octet` are the raw device transfer and nothing else — which is what
//! every asyn driver is.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::error::AsynResult;
use asyn_rs::interpose::EomReason;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::request::RequestOp;
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime};
use asyn_rs::user::AsynUser;

/// Reads bytes off a canned device stream and records every write as the driver
/// saw it — one entry per `io_write_octet` call, so a layer that splits a write
/// is visible. It installs the EOS interpose at construction exactly as
/// `DrvAsynFTDIPort::new` does, and — like every asyn driver — never dispatches
/// an interpose chain itself.
struct RawDeviceDriver {
    base: PortDriverBase,
    inbound: Vec<u8>,
    pos: usize,
    writes: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl RawDeviceDriver {
    fn new(port: &str, inbound: &[u8], writes: Arc<Mutex<Vec<Vec<u8>>>>) -> Self {
        let mut base = PortDriverBase::new(port, 1, PortFlags::default());
        base.install_octet_interpose(Box::new(asyn_rs::interpose::eos::EosInterpose::default()));
        Self {
            base,
            inbound: inbound.to_vec(),
            pos: 0,
            writes,
        }
    }
}

impl PortDriver for RawDeviceDriver {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    fn io_read_octet_eom(
        &mut self,
        _user: &AsynUser,
        buf: &mut [u8],
    ) -> AsynResult<(usize, EomReason)> {
        let n = (self.inbound.len() - self.pos).min(buf.len());
        buf[..n].copy_from_slice(&self.inbound[self.pos..self.pos + n]);
        self.pos += n;
        let eom = if n == buf.len() {
            EomReason::CNT
        } else {
            EomReason::empty()
        };
        Ok((n, eom))
    }

    fn io_write_octet(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        self.writes.lock().unwrap().push(data.to_vec());
        Ok(data.len())
    }
}

/// The EOS interpose the port's configure installed must serve the port's
/// reads: the read stops at the terminator, strips it, reports `asynEomEos`, and
/// leaves the bytes behind it for the next read.
#[test]
fn an_installed_eos_interpose_serves_a_driver_that_never_dispatches_it() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let driver = RawDeviceDriver::new("R1860R", b"ab\ncd\n", writes);
    let (rt, _jh) = create_port_runtime(driver, RuntimeConfig::default())
        .expect("the port runtime thread must start");
    let handle = rt.port_handle().clone();

    handle
        .set_input_eos_blocking(AsynUser::new(0), b"\n")
        .expect("set IEOS failed");

    let r = handle
        .submit_blocking(RequestOp::OctetRead { buf_size: 32 }, AsynUser::new(0))
        .expect("read failed");
    assert_eq!(
        r.data.as_deref(),
        Some(&b"ab"[..]),
        "the EOS interpose the port installed must run: the read stops at the \
         terminator and strips it (C asynInterposeEos.c). A chain dispatched by \
         the driver never ran on a driver that did not dispatch it."
    );
    assert!(
        EomReason::from_bits_truncate(r.eom_reason).contains(EomReason::EOS),
        "an EOS-terminated read reports asynEomEos"
    );

    let r = handle
        .submit_blocking(RequestOp::OctetRead { buf_size: 32 }, AsynUser::new(0))
        .expect("second read failed");
    assert_eq!(
        r.data.as_deref(),
        Some(&b"cd"[..]),
        "the next message comes out of the same chain"
    );
}

/// The write side of the same chain: `asynInterposeDelay`, pushed from iocsh
/// after configure (asynInterposeDelay.c:187,215-237), writes one character at a
/// time on *any* port. The driver below sees three one-byte writes; before the
/// fix the layer was never entered and it saw one three-byte write.
#[test]
fn an_interpose_pushed_after_configure_serves_the_write_path() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let driver = RawDeviceDriver::new("R1860W", b"", writes.clone());
    let (rt, _jh) = create_port_runtime(driver, RuntimeConfig::default())
        .expect("the port runtime thread must start");
    let handle = rt.port_handle().clone();

    handle
        .push_delay_interpose_blocking(0, Duration::from_micros(1))
        .expect("asynInterposeDelay install failed");

    let r = handle
        .submit_blocking(
            RequestOp::OctetWrite {
                data: b"CMD".to_vec(),
            },
            AsynUser::new(0),
        )
        .expect("write failed");
    assert_eq!(r.nbytes, 3);

    let got = writes.lock().unwrap().clone();
    assert_eq!(
        got,
        vec![b"C".to_vec(), b"M".to_vec(), b"D".to_vec()],
        "the delay interpose must sit above the driver and hand it one character \
         per write (C asynInterposeDelay.c:33-53)"
    );
}
