// This daemon is `tokio_backend`-only, because `epics_ca_rs::repeater` is:
// both of its loops await `recv_from_with_drop_count_socket` on a
// `tokio::net::UdpSocket`, which needs a reactor, and `exec_backend` — the
// reactor-free backend, selected on a *host* build by
// `EPICS_RS_BUILD_EXEC_BACKEND=thread` — has none. Cargo's
// `required-features` cannot name a build-script cfg, so the gate is in this
// file. The binary is still built and linted in that configuration and says
// why it will not serve, the same shape `realtime-ca-ioc` uses for its own
// wrong-backend arm.
#[cfg(tokio_backend)]
use clap::Parser;
#[cfg(tokio_backend)]
use epics_ca_rs::repeater::run_repeater_with_debug;

/// CA Repeater. Forwards UDP repeater traffic between local CA clients
/// so multiple processes on the same host can share the single UDP
/// receive port. Mirrors C `caRepeater`.
///
/// Detaches stdin/stdout/stderr to `/dev/null` by default (epics-base
/// 6dba2ec) so the daemon does not inherit the parent terminal — this
/// matches the C behaviour when launched implicitly by `libca`. Pass
/// `-v` to keep the original stdio for debugging. Pass `-d` (or `-dd`)
/// to enable debug-level client lifecycle / forwarding messages
/// (epics-base PR #831).
#[cfg(tokio_backend)]
#[derive(Parser)]
#[command(name = "ca-repeater-rs", about = "EPICS CA Repeater")]
struct Args {
    /// Keep stdin/stdout/stderr inherited from the parent. Without
    /// this flag they are redirected to `/dev/null` on Unix.
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,

    /// Increase debug verbosity. `-d` shows client lifecycle (new /
    /// refused / deleted / verified count). `-dd` adds per-message
    /// "Sent to port N" and "Client on port N is alive". Mirrors
    /// the C caRepeater `-d`/`-dd` option (PR #831).
    #[arg(short = 'd', long = "debug", action = clap::ArgAction::Count)]
    debug: u8,
}

#[cfg(all(unix, tokio_backend))]
fn detach_stdio() {
    // Open /dev/null O_RDWR, then `dup2` it onto fds 0/1/2. The
    // pattern matches the C `caRepeater.cpp` block: any open failure
    // is fatal-but-silent (we can't print to stderr either way), and
    // we close the temporary fd if it wasn't one of 0/1/2.
    let path = b"/dev/null\0";
    // SAFETY: path is NUL-terminated; libc::open is a thin syscall wrapper.
    let dn = unsafe { libc::open(path.as_ptr() as *const _, libc::O_RDWR) };
    if dn < 0 {
        return;
    }
    // SAFETY: dn is a valid fd just returned from `open`.
    unsafe {
        libc::dup2(dn, 0);
        libc::dup2(dn, 1);
        libc::dup2(dn, 2);
        if dn > 2 {
            libc::close(dn);
        }
    }
}

#[cfg(all(not(unix), tokio_backend))]
fn detach_stdio() {
    // C `caRepeater` skips detach on Windows / RTEMS / VxWorks via
    // `CAN_DETACH_STDINOUT`. Match that — leave stdio inherited.
}

#[cfg(tokio_backend)]
fn main() {
    let args = Args::parse();
    // `-d` implies keeping stderr open so the operator actually sees
    // the messages. Without `-v`, stderr is otherwise dup2'd to
    // /dev/null and `-d` would be silent.
    if !args.verbose && args.debug == 0 {
        detach_stdio();
    }
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let debug = args.debug;
    rt.block_on(async move {
        if let Err(e) = run_repeater_with_debug(debug).await {
            // A socket the daemon could not make or bind never reaches here:
            // C `ca_repeater` names both outcomes at the bind and returns
            // from its void function either way (`repeater.cpp:513-531`), and
            // `run_repeater_with_debug` does the same, printing "CA Repeater:
            // Exiting, a repeater is already running" or the fatal stderr
            // line and answering `Ok(())`. Anything left is a failure C's
            // `ca_repeater` cannot produce, so it is not one this binary can
            // classify — say it and exit.
            //
            // After detach, stderr goes to /dev/null. The eprintln! is
            // still useful when `-v` or `-d` was passed.
            eprintln!("ca-repeater: {e}");
            std::process::exit(1);
        }
    });
}

/// The `exec_backend` arm. It refuses instead of starting, and it refuses
/// before detaching stdio so the reason reaches the operator's terminal
/// rather than `/dev/null`.
#[cfg(exec_backend)]
fn main() -> std::process::ExitCode {
    eprintln!(
        "ca-repeater-rs: this build selects the reactor-free execution backend \
         (EPICS_RS_BUILD_EXEC_BACKEND=thread), and the repeater's UDP loops need \
         a tokio reactor. Unset that variable and rebuild to run the repeater."
    );
    std::process::ExitCode::FAILURE
}
