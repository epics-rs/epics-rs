use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket as StdUdpSocket};

use tokio::net::UdpSocket;

use crate::protocol::*;

use crate::repeater_clients::*;

/// Run the CA repeater daemon. Equivalent to
/// `run_repeater_with_debug(0)`.
pub async fn run_repeater() -> io::Result<()> {
    run_repeater_with_debug(0).await
}

/// Run the CA repeater daemon with an explicit debug level.
///
/// Mirrors epics-base PR #831 (commit `e2717521` "Added -d option
/// to caRepeater, sets debug level"), i.e. C `ca_repeater(setDebug)`
/// assigning the file-static `debug` (`repeater.cpp:493-502`):
/// - level 0: the four `debugPrintf` sites C leaves unguarded still
///   print — `CA Repeater: Attached and initialized` among them.
/// - level 1: also "New client", "Verified N active clients",
///   "Client on port N refused message", "Deleted client on port N" —
///   high-level client lifecycle.
/// - level 2: also per-beacon "Sent to port N" and per-client
///   "Client on port N is alive" verification.
///
/// Which stream a line takes is `Diag`'s business, not this level's:
/// `debugPrintf` is stdout, `fprintf(stderr, …)` is stderr, and
/// `errlogPrintf` is the errlog queue. `ca-repeater-rs -v` keeps all
/// three connected; without `-v` fds 0/1/2 are `dup2`'d to `/dev/null`
/// and the stream ones are discarded, as C `caRepeater` does.
///
/// Binds to UDP 5065, accepts client registrations, and fans out beacons.
///
/// `Ok(())` is C `ca_repeater`'s `return` from a `void` function: the
/// daemon stopped after saying why. A socket it could not create or bind
/// is reported here, not handed to the caller to re-interpret.
pub async fn run_repeater_with_debug(debug: u8) -> io::Result<()> {
    use socket2::{Domain, Protocol, Socket, Type};
    let diag = Diag::new(debug);
    // C folds socket creation and bind into one `makeSocket` errno
    // (`repeater.cpp:94-129`), so a failure at either step reaches the same
    // pair of diagnostics at `:513-531`.
    let sock = match Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)) {
        Ok(s) => s,
        Err(e) => {
            diag.stderr(format_args!(
                "{C_FILE}: Unable to create repeater socket because \"{}\" - fatal",
                sock_err_string(&e)
            ));
            return Ok(());
        }
    };
    // libcom commit 51191e6: Linux defaults IP_MULTICAST_ALL=1, which would
    // give the repeater multicast traffic for groups it never joined.
    // No-op on non-Linux.
    #[cfg(target_os = "linux")]
    {
        let _ = sock.set_multicast_all_v4(false);
    }
    sock.set_nonblocking(true)?;
    // libca `repeater.cpp:499` resolves the bind port through
    // `envGetInetPortConfigParam(&EPICS_CA_REPEATER_PORT, …)`. Mirror
    // that so sites that override the port via env (e.g. to coexist
    // with a parallel C caRepeater on the default 5065) reach our
    // daemon. The default remains 5065 when the env var is unset.
    let port = repeater_port();
    let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);
    // Singleton-per-host bind. libca `repeater.cpp` makeSocket() binds
    // with NO reuse option set, so a second repeater process gets
    // EADDRINUSE and exits (`repeater.cpp:505-510`); ca-repeater-rs
    // treats that error as "another repeater is already running". The
    // bind must therefore be exclusive — do not enable SO_REUSEPORT
    // here: `epicsSocketEnableAddressUseForDatagramFanout` (the
    // SO_REUSEPORT fanout helper) is never called for the repeater, and
    // enabling it would let two repeaters join the kernel UDP fanout
    // group and split client registration / beacon delivery between
    // them. CA server UDP sockets keep fanout (server/udp.rs) so
    // multiple IOCs share the CA port; the repeater daemon port does not.
    //
    // C `ca_repeater` decides what a bind failure MEANS right here and
    // returns from its void function either way (`repeater.cpp:513-531`):
    // EADDRINUSE is the ordinary "someone else got there first" and prints
    // on stdout, anything else is fatal and prints on stderr. Handing the
    // bare `io::Error` up made the caller re-derive that — and `caget-rs`,
    // whose in-process fallback discards the result entirely, derived
    // nothing. Reporting here and returning `Ok(())` is C's `return`.
    let bind_sa: socket2::SockAddr = bind_addr.into();
    if let Err(e) = sock.bind(&bind_sa) {
        if e.kind() == io::ErrorKind::AddrInUse {
            diag.printf(
                0,
                format_args!("CA Repeater: Exiting, a repeater is already running"),
            );
        } else {
            diag.stderr(format_args!(
                "{C_FILE}: Unable to create repeater socket because \"{}\" - fatal",
                sock_err_string(&e)
            ));
        }
        return Ok(());
    }
    // Only after a successful exclusive bind does C enable SO_REUSEADDR
    // (`epicsSocketEnableAddressReuseDuringTimeWaitState`) so THIS daemon
    // can rebind across a restart. POSIX-only — WINSOCK SO_REUSEADDR has
    // different (port-hijack) semantics, so the C helper is a no-op on
    // Windows.
    #[cfg(not(windows))]
    let _ = sock.set_reuse_address(true);

    // ca commit 97bf917: join every multicast (224.0.0.0/4) beacon address
    // from EPICS_CAS_BEACON_ADDR_LIST (or EPICS_CA_ADDR_LIST as fallback) so
    // multicast-configured sites actually receive the beacons they fan out.
    // Errors are reported but non-fatal — broadcast/unicast beacons still work.
    join_beacon_multicast_groups(&sock, port, diag);

    let std_sock: StdUdpSocket = sock.into();
    let socket = UdpSocket::from_std(std_sock)?;
    // pvxs `udp_collector.cpp` parity: opt the kernel into
    // SO_RXQ_OVFL so a sustained beacon-fanout backlog surfaces as
    // a debug log instead of silent loss. No-op on non-Linux.
    if let Err(e) = epics_base_rs::net::enable_so_rxq_ovfl_for_socket(&socket) {
        tracing::trace!(
            target: "epics_ca_rs::repeater",
            error = %e,
            "SO_RXQ_OVFL enable failed (non-fatal)"
        );
    }

    // C `repeater.cpp:583`. `debugPrintf` is `::printf` here (`:73`
    // `#define DEBUG`), so this is stdout and UNGUARDED — a stock C
    // `caget` whose `caStartRepeaterIfNotInstalled` fell back to the
    // in-process `caRepeaterThread` prints it on its own stdout. The port
    // gated it behind `debug > 0` and sent it to stderr, which is the
    // whole of the observed client-side A/B difference.
    diag.printf(0, format_args!("CA Repeater: Attached and initialized"));

    let mut clients: HashMap<u16, RepeaterClient> = HashMap::new();
    let mut buf = [0u8; 4096];
    let mut prev_drops: u32 = 0;

    loop {
        // C `repeater.cpp:577-593`: a recv error never exits the repeater —
        // `ECONNREFUSED` (Linux ICMP bug) and `ECONNRESET` (Windows KB263823)
        // are silently skipped, anything else is logged, and the loop always
        // continues. A repeater client that vanished between our fan-out send
        // and this recv otherwise killed the whole repeater.
        let (len, src, drops) =
            match epics_base_rs::net::recv_from_with_drop_count_socket(&socket, &mut buf).await {
                Ok(v) => v,
                Err(e) => {
                    if !matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset
                    ) {
                        // C `repeater.cpp:602` is `fprintf(stderr, …)`, and
                        // stderr is where `caRepeater -v` leaves it. A
                        // `tracing::warn!` reached nobody in a daemon that
                        // installs no subscriber.
                        diag.stderr(format_args!(
                            "CA Repeater: unexpected UDP recv err: {}",
                            sock_err_string(&e)
                        ));
                    }
                    continue;
                }
            };
        if drops != 0 && drops != prev_drops {
            tracing::debug!(
                target: "epics_ca_rs::repeater",
                prev = prev_drops,
                drops,
                "CA repeater UDP socket buffer overflow"
            );
        }
        prev_drops = drops;

        // C CA clients send a zero-length UDP packet for repeater
        // registration (backward compat with pre-3.12 repeaters).
        //
        // C `register_new_client` (`repeater.cpp:364-366` the
        // non-AF_INET reject, `:371-414` the non-loopback bind test)
        // applies the same locality gate to BOTH the zero-length
        // legacy form and `CA_PROTO_REPEATER_REGISTER`. Pre-fix
        // Rust registered any zero-length datagram regardless of
        // source.
        //
        // C accepts loopback OR any source IP that belongs
        // to a local interface (the bind-test compatibility quirk
        // for clients alternating between loopback and the first
        // non-loopback interface). Use the same `is_local_source`
        // helper as `CA_PROTO_REPEATER_REGISTER` so a site-local
        // legacy client registering from e.g. `192.168.x.y` is
        // accepted, matching C.
        if len == 0 {
            if !is_local_source(src, diag) {
                tracing::warn!(
                    src = %src,
                    "caRepeater: zero-length registration from non-local source rejected"
                );
                metrics::counter!("ca_repeater_register_non_loopback_rejects_total").increment(1);
                continue;
            }
            register_client(&mut clients, src, diag);
            continue;
        }

        // Intentional divergence from C: `repeater.cpp:601-625` only
        // special-cases `size >= sizeof(caHdr)` (16) and `size == 0`, so
        // a 1–15-byte sub-header datagram falls through to `fanOut` and
        // is forwarded verbatim. We drop it instead — a runt datagram
        // carries no decodable CA header, every registered (loopback)
        // client would discard it on receipt anyway, and no legitimate
        // sender emits one; dropping here avoids waking every client
        // with an undecodable packet.
        if len < CaHeader::SIZE {
            continue;
        }

        let Ok(hdr) = CaHeader::from_bytes(&buf[..len]) else {
            continue;
        };

        let action = decode_datagram(&buf[..len], &hdr, src);
        if action.register {
            // C `register_new_client` rejects REPEATER_REGISTER from
            // non-AF_INET peers (repeater.cpp:364-366) and, for
            // non-loopback sources, requires `bind()` to the source
            // address to succeed (repeater.cpp:371-414, proving the
            // IP belongs to a local interface). The intent: the
            // repeater and its clients must be on the same host —
            // beacon fan-out from a remote peer would silently
            // expose PV existence to unauthorised observers via the
            // registered-clients list.
            //
            // We accept loopback OR any source IP that belongs to a
            // local interface (C bind-test compatibility,
            // `repeater.cpp::register_new_client` accepts a
            // non-loopback source if `bind()` to that address
            // succeeds locally). C still needs that second arm: at
            // R7.0.10 `caRepeaterRegistrationMessage` alternates the
            // registration destination by attempt number — loopback
            // on even attempts, `osiLocalAddr` (the first
            // non-loopback local address) on odd ones
            // (`udpiiu.cpp:494-515`) — so a registration legitimately
            // arrives from a non-loopback local address.
            if !is_local_source(src, diag) {
                tracing::warn!(
                    src = %src,
                    "caRepeater: REPEATER_REGISTER from non-local source rejected"
                );
                metrics::counter!("ca_repeater_register_non_loopback_rejects_total").increment(1);
                continue;
            }
            register_client(&mut clients, src, diag);
        }
        if let Some(data) = action.fanout {
            fan_out(&mut clients, src, &data);
        }
    }
}

/// Try to register with an existing repeater. If none is running, spawn one
/// as a background process using the current executable's `ca-repeater` binary,
/// then register again.
/// `repeater_port` is the client's single resolution of
/// `EPICS_CA_REPEATER_PORT` (C `udpiiu::repeaterPort`, `udpiiu.cpp:168`) —
/// both the pre-spawn and the post-spawn attempt reuse it.
pub async fn ensure_repeater(repeater_port: u16) {
    if try_register(repeater_port).await.is_ok() {
        return;
    }

    // No repeater running — spawn one
    spawn_repeater();

    // Give it a moment to start, then register
    epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(50)).await;
    let _ = try_register(repeater_port).await;
}

/// Send a REPEATER_REGISTER to localhost:5065 and wait for CONFIRM.
async fn try_register(repeater_port: u16) -> Result<(), ()> {
    let socket = UdpSocket::bind("0.0.0.0:0").await.map_err(|_| ())?;
    // SO_RXQ_OVFL opt-in for diagnostic parity with the long-running
    // repeater; the brief CONFIRM wait below ignores the counter
    // (just one packet expected) but enables it so any future reuse
    // of this socket inherits the same diagnostic surface. No-op
    // on non-Linux.
    let _ = epics_base_rs::net::enable_so_rxq_ovfl_for_socket(&socket);

    let local_ip = match socket.local_addr().ok() {
        Some(SocketAddr::V4(v4)) => *v4.ip(),
        _ => Ipv4Addr::LOCALHOST,
    };

    let mut hdr = CaHeader::new(CA_PROTO_REPEATER_REGISTER);
    hdr.available = u32::from_be_bytes(local_ip.octets());

    // Client REGISTER target: the port the caller resolved once (C
    // `udpiiu::repeaterPort`, `udpiiu.cpp:168`), so the register attempt
    // and the daemon bind agree and a misconfigured value is diagnosed
    // once per process rather than once per attempt.
    let repeater_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, repeater_port);
    socket
        .send_to(&hdr.to_bytes(), repeater_addr)
        .await
        .map_err(|_| ())?;

    // Wait for confirm with short timeout
    let mut buf = [0u8; 64];
    let result = tokio::time::timeout(std::time::Duration::from_millis(200), async {
        loop {
            let (len, _, _drops) =
                epics_base_rs::net::recv_from_with_drop_count_socket(&socket, &mut buf)
                    .await
                    .map_err(|_| ())?;
            if len >= CaHeader::SIZE {
                if let Ok(resp) = CaHeader::from_bytes(&buf[..len]) {
                    if resp.cmmd == CA_PROTO_REPEATER_CONFIRM {
                        return Ok::<(), ()>(());
                    }
                }
            }
        }
    })
    .await;

    match result {
        Ok(Ok(())) => Ok(()),
        _ => Err(()),
    }
}

/// Spawn the repeater as a detached background process.
/// Falls back to an in-process repeater thread if the binary is not found.
fn spawn_repeater() {
    let exe = std::env::current_exe().unwrap_or_default();
    // The sibling binary carries the platform executable suffix: on Windows
    // it is `ca-repeater-rs.exe`. Joining the bare stem made `bin.exists()`
    // always false there, so every client fell through to the in-process
    // repeater fallback — which re-resolves `EPICS_CA_REPEATER_PORT` (and
    // re-prints its diagnostics into the client's own stderr) instead of
    // spawning the shared daemon C `caStartRepeaterIfNotInstalled` starts.
    let repeater_bin = exe
        .parent()
        .map(|p| p.join(format!("ca-repeater-rs{}", std::env::consts::EXE_SUFFIX)));

    // Try external binary first
    if let Some(ref bin) = repeater_bin {
        if bin.exists() {
            use std::process::{Command, Stdio};
            // Windows has no CLOEXEC: `CreateProcess` inherits every
            // inheritable handle we hold, and the stdout/stderr pipe a
            // capturing parent (`Command::output`, a shell `$(caput …)`,
            // nextest) handed us stays inheritable. The repeater is a
            // detached daemon that outlives us, so if it inherits that
            // pipe the capturing parent never sees EOF and blocks until
            // the daemon exits — i.e. forever. Unix avoids this because
            // `Stdio::null()` redirects the child's fds 0/1/2 and the pipe
            // only ever lived on fd 1. Clear the inherit flag on our own
            // standard handles first so the daemon cannot hold them — the
            // Windows analogue of the CLOEXEC unix already gets.
            #[cfg(windows)]
            unsafe {
                use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
                use windows_sys::Win32::System::Console::{
                    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
                };
                for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
                    let h = GetStdHandle(id);
                    // A closed/redirected slot is null; an unset one is
                    // INVALID_HANDLE_VALUE. Skip both — only real handles
                    // carry the inheritable pipe we must detach.
                    if !h.is_null() && h != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
                        SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
                    }
                }
            }
            if Command::new(bin)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .is_ok()
            {
                return;
            }
        }
    }

    // Fallback: run repeater in-process on a background thread.
    // This ensures beacon reception works even without the external binary.
    std::thread::spawn(|| {
        // C's in-process fallback is `caRepeaterThread` (`repeater.cpp:632`),
        // which registers with the watchdog before entering `ca_repeater()`.
        // Unbounded: the loop parks on `recv_from`, and a host with no CA
        // traffic is quiet rather than wedged.
        let _watched = epics_base_rs::runtime::taskwd::taskwd_insert(
            "CAC-repeater",
            epics_base_rs::runtime::taskwd::CheckIn::Unbounded,
            None,
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("repeater runtime");
        let _ = rt.block_on(run_repeater());
    });
}

/// C reports a bind-test socket it cannot create exactly once
/// (`repeater.cpp:382-398`, the `static bool init`), so an exhausted fd
/// table gives one line rather than one per registration datagram.
static BIND_TEST_SOCKET_REPORTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// C `repeater.cpp::register_new_client` accepts a registration
/// when the source IP is loopback OR when `bind()` to that address
/// succeeds locally (the 3.13-era compatibility quirk that allows
/// clients which alternate between loopback and the first non-
/// loopback interface). Pre-fix Rust simplified to loopback-only.
///
/// C splits the probe into two steps that mean different things: making
/// the test socket (`makeSocket(PORT_ANY, true, …)` — a failure is
/// reported on stderr, `:388-393`) and binding it to the source address
/// (a failure is the answer "not a local address", and is silent,
/// `:416-419`). The port had one `StdUdpSocket::bind(…).is_ok()`, so a
/// host out of file descriptors rejected every registration and said
/// nothing. Keep C's two steps — but a socket per probe, not C's single
/// re-bound `static testSock`, which cannot take a second distinct
/// non-loopback source.
pub(crate) fn is_local_source(src: SocketAddr, diag: Diag) -> bool {
    use std::sync::atomic::Ordering;

    if src.ip().is_loopback() {
        return true;
    }
    // C `register_new_client` rejects non-AF_INET (IPv6) explicitly.
    let SocketAddr::V4(v4) = src else {
        return false;
    };

    // `makeSocket(PORT_ANY, …)` returns before its `reuseAddr` block, so
    // C sets no reuse option on this socket either.
    let sock = match socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    ) {
        Ok(sock) => sock,
        Err(e) => {
            if !BIND_TEST_SOCKET_REPORTED.swap(true, Ordering::Relaxed) {
                diag.stderr(format_args!(
                    "{C_FILE}: Unable to create repeater bind test socket because \"{}\"",
                    sock_err_string(&e)
                ));
            }
            return false;
        }
    };
    let probe: socket2::SockAddr = SocketAddrV4::new(*v4.ip(), 0).into();
    sock.bind(&probe).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    /// fd 1 / fd 2 capture for the two daemon tests below.
    ///
    /// It sits beside its callers.  It lived in `repeater_clients` while both
    /// modules carried the same target gate; once `repeater` narrowed to
    /// `tokio_backend` the borrow ran the wrong way — the module that compiles
    /// away first is this one, so a helper left over there is stranded with no
    /// caller at all.
    ///
    /// Gated once, here. `capture_streams` dups and swaps this process's fds 1
    /// and 2, so it is unix-only; carrying that `#[cfg]` on the function
    /// instead left the module inhabited on Windows and an ungated `use` of a
    /// name that was configured out, which is E0432 rather than a quiet skip.
    #[cfg(unix)]
    mod stream_capture {
        /// Serialises the fd-swapping capture below: fds 1 and 2 are
        /// process-global, so two of these running at once would cross-read.
        static STREAM_CAPTURE: std::sync::Mutex<()> = std::sync::Mutex::new(());

        /// Run `f` with fd 1 and fd 2 redirected to pipes, and return what each
        /// received.
        ///
        /// WHICH stream a `repeater.cpp` diagnostic takes is half of what this
        /// module got wrong: `debugPrintf` is `::printf` (stdout) because
        /// `repeater.cpp:73` `#define DEBUG`s before including `iocinf.h`, while
        /// the port emitted every line on stderr. Asserting the bytes alone
        /// would not have caught that, so the tests assert the stream too.
        pub(crate) fn capture_streams<F: FnOnce() -> R, R>(f: F) -> (R, String, String) {
            use std::io::{Read, Write};
            use std::os::fd::FromRawFd;

            let _serial = STREAM_CAPTURE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);

            // SAFETY: plain fd bookkeeping on this process's own fds 1 and 2.
            // The lock above makes this the only thread swapping them.
            let (saved_out, saved_err, mut out_r, mut err_r) = unsafe {
                let saved_out = libc::dup(1);
                let saved_err = libc::dup(2);
                let mut op = [0i32; 2];
                let mut ep = [0i32; 2];
                assert_eq!(libc::pipe(op.as_mut_ptr()), 0, "stdout pipe");
                assert_eq!(libc::pipe(ep.as_mut_ptr()), 0, "stderr pipe");
                libc::dup2(op[1], 1);
                libc::dup2(ep[1], 2);
                libc::close(op[1]);
                libc::close(ep[1]);
                (
                    saved_out,
                    saved_err,
                    std::fs::File::from_raw_fd(op[0]),
                    std::fs::File::from_raw_fd(ep[0]),
                )
            };

            let value = f();
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();

            // SAFETY: restores the two fds saved above and releases the
            // duplicates; dropping the pipes' write ends is what ends the reads.
            unsafe {
                libc::dup2(saved_out, 1);
                libc::dup2(saved_err, 2);
                libc::close(saved_out);
                libc::close(saved_err);
            }

            let mut out = String::new();
            let mut err = String::new();
            out_r.read_to_string(&mut out).expect("captured stdout");
            err_r.read_to_string(&mut err).expect("captured stderr");
            (value, out, err)
        }
    }

    /// C `repeater.cpp:515-521`: a repeater that loses the race for the
    /// port says so with a `debugPrintf` — stdout, and inside no
    /// `if (debug)`, so it prints at the stock level 0 — and then returns.
    /// The port propagated a bare `AddrInUse` instead, which `caget-rs`'s
    /// in-process fallback discards with `let _ =`.
    // RTEMS-EXEC-MODEL-ALLOW(1): builds and enters its own current-thread
    // runtime rather than taking an ambient one; green on the exec
    // backend.
    #[cfg(unix)]
    #[test]
    fn second_repeater_says_one_is_already_running_on_stdout() {
        // Hold the port so the daemon's exclusive bind must fail.
        let held = StdUdpSocket::bind("0.0.0.0:0").expect("hold bind");
        let port = held.local_addr().expect("hold addr").port();
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("EPICS_CA_REPEATER_PORT", port.to_string()) };

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");

        let (result, out, err) =
            stream_capture::capture_streams(|| rt.block_on(run_repeater_with_debug(0)));

        assert!(
            result.is_ok(),
            "C returns from its void `ca_repeater` after reporting; the port \
             must not hand the bind error up instead: {result:?}"
        );
        let line = "CA Repeater: Exiting, a repeater is already running";
        assert!(
            out.lines().any(|l| l == line),
            "stdout must carry {line:?} verbatim; got {out:?}"
        );
        assert!(
            !err.contains(line),
            "the line is a `debugPrintf`, i.e. C `::printf`; got stderr {err:?}"
        );
        drop(held);
    }

    /// The bytes and the stream of three `repeater.cpp` diagnostics, taken
    /// off a live `run_repeater_with_debug`.
    ///
    /// * `CA Repeater: Attached and initialized` — `repeater.cpp:583`, a
    ///   `debugPrintf` inside NO `if (debug)`, so it prints at the stock
    ///   level 0. Captured from the C build for comparison: with the
    ///   `caRepeater` executable off `PATH` (so
    ///   `caStartRepeaterIfNotInstalled` falls back to the in-process
    ///   `caRepeaterThread`), `caget NOSUCH:PV` writes exactly this line to
    ///   its own **stdout** while `Channel connect timed out` goes to
    ///   stderr. The port gated it behind `debug > 0` and put it on stderr.
    /// * `New client on port %u` — `repeater.cpp:136`, the `repeaterClient`
    ///   constructor, inside `if (debug)`.
    /// * `Verified %u active clients` — `repeater.cpp:332`, and this is the
    ///   only function in the file that prints it; the port also printed it
    ///   from `fanOut` and from a registration wrapper.
    // RTEMS-EXEC-MODEL-ALLOW(1): builds and enters its own multi-thread
    // runtime rather than taking an ambient one; green on the exec
    // backend.
    #[cfg(unix)]
    #[test]
    fn repeater_diagnostics_carry_c_bytes_on_c_streams() {
        use std::time::{Duration, Instant};

        // A port nothing holds: bind, read it back, drop.
        let free_port = StdUdpSocket::bind("127.0.0.1:0")
            .expect("probe bind")
            .local_addr()
            .expect("probe addr")
            .port();
        // `run_repeater_with_debug` binds `repeater_port()`, C
        // `envGetInetPortConfigParam(&EPICS_CA_REPEATER_PORT, …)`, so this is
        // the only way to keep the test off a host repeater on 5065.
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("EPICS_CA_REPEATER_PORT", free_port.to_string()) };

        // `verifyClients` bind-tests the client's own address, and C's
        // order is confirm-then-sweep (`repeater.cpp:453-486`) — so the
        // socket has to stay open past the CONFIRM or the sweep correctly
        // reaps it. Share it rather than moving it into the blocking task,
        // whose return would close it.
        let client = std::sync::Arc::new(StdUdpSocket::bind("127.0.0.1:0").expect("client bind"));
        client
            .set_read_timeout(Some(Duration::from_millis(200)))
            .expect("client read timeout");
        let client_port = client.local_addr().expect("client addr").port();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test runtime");

        let (confirmed, out, err) = stream_capture::capture_streams(|| {
            let held = std::sync::Arc::clone(&client);
            rt.block_on(async move {
                let repeater = tokio::spawn(run_repeater_with_debug(1));
                // Re-send until the CONFIRM arrives: the first REGISTER can
                // predate the daemon's bind, and a lost datagram must not
                // turn into a flake.
                let confirmed = tokio::task::spawn_blocking(move || {
                    let register = CaHeader::new(CA_PROTO_REPEATER_REGISTER).to_bytes();
                    let deadline = Instant::now() + Duration::from_secs(5);
                    let mut buf = [0u8; 64];
                    while Instant::now() < deadline {
                        let _ = client.send_to(&register, ("127.0.0.1", free_port));
                        if let Ok((n, _)) = client.recv_from(&mut buf) {
                            if n >= CaHeader::SIZE {
                                if let Ok(h) = CaHeader::from_bytes(&buf[..n]) {
                                    if h.cmmd == CA_PROTO_REPEATER_CONFIRM {
                                        return true;
                                    }
                                }
                            }
                        }
                    }
                    false
                })
                .await
                .unwrap_or(false);
                // The sweep runs after the CONFIRM; give it the socket it
                // bind-tests, and a moment to print its line.
                tokio::time::sleep(Duration::from_millis(250)).await;
                repeater.abort();
                drop(held);
                confirmed
            })
        });

        assert!(
            confirmed,
            "the repeater never confirmed the registration; captured stdout {out:?} stderr {err:?}"
        );

        let banner = "CA Repeater: Attached and initialized";
        let new_client = format!("New client on port {client_port}");
        let verified = "Verified 1 active clients";

        for line in [banner, new_client.as_str(), verified] {
            assert!(
                out.lines().any(|l| l == line),
                "stdout must carry {line:?} verbatim; got {out:?}"
            );
            assert!(
                !err.contains(line),
                "{line:?} is a `debugPrintf`, i.e. C `::printf` — it must not \
                 reach stderr; got {err:?}"
            );
        }
    }
}
