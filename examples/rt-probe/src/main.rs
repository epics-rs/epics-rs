//! PREEMPT_RT measurement harness — see `probe.rs` for the subcommands and
//! methodology. Everything it measures (SCHED_FIFO banding, CPU affinity,
//! priority-inheritance mutexes) is Linux kernel surface, so the whole rig
//! is compiled for Linux only; on any other OS this binary says so and
//! exits rather than half-existing without its measurement primitives.
//!
//! The rig also needs the tokio backend: every subcommand boots an in-process
//! `CaServer`/`PvaServer` and measures it, and neither type is compiled on the
//! reactor-free backend. The three `main` arms below partition exactly.
#[cfg(all(target_os = "linux", tokio_backend))]
mod probe;

#[cfg(all(target_os = "linux", tokio_backend))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    probe::run()
}

/// The `exec_backend` arm on Linux: the measurement targets are the async CA
/// and PVA servers, which this backend does not compile.
#[cfg(all(target_os = "linux", exec_backend))]
fn main() {
    eprintln!(
        "rt-probe measures the async CA/PVA servers; this build selects the \
         reactor-free backend (EPICS_RS_BUILD_EXEC_BACKEND=thread), which does not
         have them."
    );
    std::process::exit(2);
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!(
        "rt-probe measures Linux RT scheduling (SCHED_FIFO, affinity, PI mutexes); \
         there is nothing for it to probe on this OS."
    );
    std::process::exit(2);
}
