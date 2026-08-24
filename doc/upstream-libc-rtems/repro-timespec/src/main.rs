//! TASK 1 probe: does std compile an 8-byte or a 16-byte libc::timespec on
//! armv7-rtems-eabihf, and what happens when the 8-byte one meets RTEMS?
use std::mem::{align_of, size_of};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Byte-for-byte what upstream libc 0.2.188 generates for
/// `struct timespec` on newlib when `time_t = i32`:
/// `{ tv_sec: time_t, tv_nsec: c_long }` = { i32, i32 }.
#[repr(C)]
#[derive(Copy, Clone)]
struct StockTimespec {
    tv_sec: i32,
    tv_nsec: i32,
}

/// A deterministic stack frame: canary, the slot the kernel is handed, canary.
/// `repr(C)` so the compiler may not reorder the fields.
#[repr(C)]
struct CanaryFrame {
    before: u64,
    slot: StockTimespec,
    after: u64,
    tail: u64,
}

const CANARY: u64 = 0xDEAD_BEEF_CAFE_F00D;

fn main() {
    println!("tsprobe: START");

    // ---- 1. what did *this* build compile? -----------------------------
    println!(
        "tsprobe: size_of::<libc::timespec>()={} align={}",
        size_of::<libc::timespec>(),
        align_of::<libc::timespec>()
    );
    println!(
        "tsprobe: size_of::<libc::time_t>()={} size_of::<libc::timeval>()={} size_of::<libc::c_long>()={}",
        size_of::<libc::time_t>(),
        size_of::<libc::timeval>(),
        size_of::<libc::c_long>()
    );
    println!(
        "tsprobe: size_of::<libc::off_t>()={} dev_t={} ino_t={}",
        size_of::<libc::off_t>(),
        size_of::<libc::dev_t>(),
        size_of::<libc::ino_t>()
    );

    // ---- 2. what does STD actually observe? ----------------------------
    // If std compiled the 8-byte struct, it reads tv_nsec out of bytes 4..8,
    // which on little-endian ARM is the HIGH word of the kernel's 64-bit
    // tv_sec -- always 0 for any positive time. So a permanently-zero
    // subsec_nanos is std's own fingerprint of the wrong layout.
    for i in 0..5 {
        let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        println!(
            "tsprobe: std SystemTime[{}] secs={} subsec_nanos={}",
            i,
            d.as_secs(),
            d.subsec_nanos()
        );
        spin(200_000);
    }
    let t0 = Instant::now();
    spin(2_000_000);
    let dt = t0.elapsed();
    println!(
        "tsprobe: std Instant elapsed secs={} subsec_nanos={} (nanos={})",
        dt.as_secs(),
        dt.subsec_nanos(),
        dt.as_nanos()
    );

    // ---- 3. the overwrite, demonstrated ---------------------------------
    // Hand RTEMS a pointer to an 8-byte slot -- exactly what std does when
    // libc::timespec is 8 bytes -- and check the canary that follows it.
    let mut f = CanaryFrame {
        before: CANARY,
        slot: StockTimespec { tv_sec: 0, tv_nsec: 0 },
        after: CANARY,
        tail: CANARY,
    };
    println!(
        "tsprobe: frame layout size={} off(before)=0 off(slot)={} off(after)={} slot_size={}",
        size_of::<CanaryFrame>(),
        core::mem::offset_of!(CanaryFrame, slot),
        core::mem::offset_of!(CanaryFrame, after),
        size_of::<StockTimespec>()
    );
    let rc = unsafe {
        libc::clock_gettime(
            libc::CLOCK_REALTIME,
            (&raw mut f.slot).cast::<libc::timespec>(),
        )
    };
    println!(
        "tsprobe: clock_gettime rc={} before=0x{:016x} {} after=0x{:016x} {} tail=0x{:016x} {}",
        rc,
        f.before,
        if f.before == CANARY { "INTACT" } else { "CLOBBERED" },
        f.after,
        if f.after == CANARY { "INTACT" } else { "CLOBBERED" },
        f.tail,
        if f.tail == CANARY { "INTACT" } else { "CLOBBERED" }
    );
    println!(
        "tsprobe: slot read back as 8-byte struct: tv_sec={} tv_nsec={}",
        f.slot.tv_sec, f.slot.tv_nsec
    );

    println!("tsprobe: DONE");
}

fn spin(n: u64) {
    let mut acc: u64 = 0;
    for i in 0..n {
        acc = acc.wrapping_add(i ^ 0x9e37_79b9);
    }
    if acc == u64::MAX {
        println!("unreachable {acc}");
    }
}
