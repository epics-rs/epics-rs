//! A tool whose operations do not finish within `-w` prints pvxs's one
//! timeout line and exits 1. pvxs 1.5.3, whole stderr:
//!
//! ```text
//! $ pvxget -w 1 nopv nopv2    -> Timeout with 2 outstanding   (get.cpp:137)
//! $ pvxinfo -w 1 nopv nopv2   -> Timeout                      (info.cpp:114)
//! $ pvxput -w 1 nopv x=1      -> Timeout                      (put.cpp:168)
//! $ pvxcall -w 1 nopv         -> Timeout                      (call.cpp:152)
//! ```

// Host-only: drives the built CLI binaries out of process, and those are
// the tokio-backend builds.
#![cfg(tokio_backend)]

use std::process::Command;

fn run(exe: &str, args: &[&str]) -> (i32, String) {
    // A number nobody answers on, held so no stranger binds it while the
    // tool runs, so every search goes unanswered.
    let closed = named_port::HeldPort::closed_udp();
    let out = Command::new(exe)
        .args(args)
        .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_PVA_ADDR_LIST", format!("127.0.0.1:{}", closed.port))
        .env_remove("EPICS_PVA_NAME_SERVERS")
        .output()
        .expect("run CLI");
    (
        out.status.code().expect("exited normally"),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn pvget_counts_the_outstanding_operations() {
    let (code, stderr) = run(
        env!("CARGO_BIN_EXE_pvget-rs"),
        &["-w", "0.3", "TST:NOPV1", "TST:NOPV2"],
    );
    assert_eq!((code, stderr.as_str()), (1, "Timeout with 2 outstanding\n"));
}

#[test]
fn pvinfo_pvput_and_pvcall_print_timeout() {
    for (exe, args) in [
        (
            env!("CARGO_BIN_EXE_pvinfo-rs"),
            &["-w", "0.3", "TST:NOPV1", "TST:NOPV2"][..],
        ),
        (
            env!("CARGO_BIN_EXE_pvput-rs"),
            &["-w", "0.3", "TST:NOPV1", "1"],
        ),
        (env!("CARGO_BIN_EXE_pvcall-rs"), &["-w", "0.3", "TST:NOPV1"]),
    ] {
        let (code, stderr) = run(exe, args);
        assert_eq!((code, stderr.as_str()), (1, "Timeout\n"), "{exe}");
    }
}
