//! The port's thread gets the stack C's `registerPort` asks for: `stackSize` 0
//! is `epicsThreadStackMedium` (asynManager.c:2078-2080), and
//! `epicsThreadCreate` reads 1/2 as the Medium/Big classes and anything else as
//! bytes (osdThread.c:602-603) — FTDI's 500000 (drvAsynFTDIPort.cpp:590).
#![cfg(all(target_os = "linux", target_env = "gnu"))]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use asyn_rs::error::AsynResult;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
use asyn_rs::user::AsynUser;
use epics_libcom_rs::runtime::task::StackSizeClass;

/// Records the stack of the thread its read runs on: the port's own.
struct Device {
    base: PortDriverBase,
    stack: Arc<AtomicUsize>,
}

impl PortDriver for Device {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
    fn read_int32(&mut self, _user: &AsynUser) -> AsynResult<i32> {
        self.stack.store(current_stack_bytes(), Ordering::SeqCst);
        Ok(0)
    }
}

fn current_stack_bytes() -> usize {
    // SAFETY: a zeroed attr filled by `pthread_getattr_np` for this thread,
    // destroyed before return.
    unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        assert_eq!(libc::pthread_getattr_np(libc::pthread_self(), &mut attr), 0);
        let mut addr = std::ptr::null_mut();
        let mut size = 0;
        assert_eq!(libc::pthread_attr_getstack(&attr, &mut addr, &mut size), 0);
        libc::pthread_attr_destroy(&mut attr);
        size
    }
}

#[test]
fn the_port_thread_takes_register_port_s_stack_size() {
    // Every port stays up until the end: glibc hands a new thread a cached
    // stack up to 4x the request, so a freed 1 MiB one would serve 500000.
    let mut live = Vec::new();
    for (raw, asked) in [
        (0, StackSizeClass::Medium.bytes()),
        (1, StackSizeClass::Medium.bytes()),
        (2, StackSizeClass::Big.bytes()),
        (500_000, 500_000),
    ] {
        let stack = Arc::new(AtomicUsize::new(0));
        let base = PortDriverBase::new(
            &format!("STACK{raw}"),
            1,
            PortFlags {
                can_block: true,
                ..PortFlags::default()
            },
        );
        let device = Device {
            base,
            stack: Arc::clone(&stack),
        };
        let config = RuntimeConfig {
            stack_size: raw,
            ..RuntimeConfig::default()
        };
        let (rt, _jh) = create_port_runtime_unclaimed(device, config).unwrap();
        rt.port_handle().read_int32_blocking(0, 0).unwrap();
        let got = stack.load(Ordering::SeqCst);
        // glibc aligns a byte count down, as for C's own pthread_attr_setstacksize.
        assert!(
            got + 64 > asked && got < asked + 64 * 1024,
            "stackSize {raw}: asked for {asked} bytes, the port thread has {got}"
        );
        live.push(rt);
    }
    for rt in live {
        rt.shutdown();
    }
}

/// A synchronous port has no C thread, so its `stackSize` is never read
/// (asynManager.c:2074); the port's own thread keeps Medium.
#[test]
fn a_synchronous_port_s_stack_size_is_not_read() {
    let stack = Arc::new(AtomicUsize::new(0));
    let device = Device {
        base: PortDriverBase::new("STACK_SYNC", 1, PortFlags::default()),
        stack: Arc::clone(&stack),
    };
    let config = RuntimeConfig {
        stack_size: 2,
        ..RuntimeConfig::default()
    };
    let (rt, _jh) = create_port_runtime_unclaimed(device, config).unwrap();
    rt.port_handle().read_int32_blocking(0, 0).unwrap();
    let got = stack.load(Ordering::SeqCst);
    let asked = StackSizeClass::Medium.bytes();
    assert!(
        got + 64 > asked && got < asked + 64 * 1024,
        "a synchronous port's thread has {got} bytes, want Medium's {asked}"
    );
    rt.shutdown();
}
