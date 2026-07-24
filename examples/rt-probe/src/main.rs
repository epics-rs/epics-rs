//! PREEMPT_RT measurement harness — see `probe.rs` for the subcommands and
//! methodology. Everything it measures (SCHED_FIFO banding, CPU affinity,
//! priority-inheritance mutexes) is Linux kernel surface, so the whole rig
//! is compiled for Linux only; on any other OS this binary says so and
//! exits rather than half-existing without its measurement primitives.

#[cfg(target_os = "linux")]
mod probe;

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    probe::run()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!(
        "rt-probe measures Linux RT scheduling (SCHED_FIFO, affinity, PI mutexes); \
         there is nothing for it to probe on this OS."
    );
    std::process::exit(2);
}
