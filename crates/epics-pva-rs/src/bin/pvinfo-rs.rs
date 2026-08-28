use clap::Parser;
use epics_pva_rs::client::PvaClient;
use epics_pva_rs::{cli, config, format};

#[derive(Parser)]
#[command(
    name = "pvinfo-rs",
    about = "Show EPICS PV type info via pvAccess",
    disable_version_flag = true
)]
struct Args {
    /// Print version information (pvxs `version_information`, tools
    /// `case 'V'`) and exit. Routed through `cli::version_information`
    /// so every PVA CLI reports the same library + protocol stack
    /// instead of clap's crate-only `<binary> <version>`.
    #[arg(short = 'V', long = "version")]
    version: bool,

    /// PV names to query
    /// Not clap-required: C makes the check itself and returns 1 with
    /// its own message (pvinfo.cpp:166-170), where a clap-enforced
    /// argument would exit 2. `-D` keeps its own name-less form.
    pv_names: Vec<String>,

    /// Wait time in seconds
    #[arg(short = 'w', default_value = "5.0")]
    timeout: f64,

    /// Print host network troubleshooting info (local interfaces,
    /// broadcast targets, effective config) and exit, without
    /// contacting any server. pvxs `tools/info.cpp:62-64` implements
    /// `-D` as `target_information(std::cout)` followed by an immediate
    /// `return 0` — so no PV name is required.
    #[arg(short = 'D')]
    describe: bool,

    /// Verbose: print the effective PVA client configuration before the
    /// type output, then the server's peer credentials before each PV.
    /// pvxs `tools/info.cpp:78-79` prints the `Effective config` under
    /// `-v`; the `# <PeerCredentials>` line under that same `-v` exists only
    /// on the unmerged `tls` branch (`info.cpp:88-90` at `b3a10bf0`), so
    /// verbose couples both here.
    #[arg(short = 'v')]
    verbose: bool,

    /// Print the server's peer credentials (TLS X.509 identity, or the
    /// anonymous account on a plain TCP connection) before each PV's type,
    /// without `-v`'s effective-config dump. pvxs-TLS couples credentials
    /// to `-v` (`info.cpp:88-90`); this Rust-only flag is the creds-only
    /// opt-in, and `-v` is a superset of it.
    #[arg(long = "show-credentials")]
    show_credentials: bool,

    /// Raise the `epics_pva_rs` library log level to DEBUG. Mirrors pvxs
    /// `pvxinfo -d` mapping to `logger_level_set("pvxs.*",
    /// Level::Debug)` (`tools/info.cpp:47-66`).
    #[arg(short = 'd')]
    debug: bool,
}

/// Whether to print the server's peer-credentials (`# <cred>`) line.
/// pvxs-TLS gates it on verbose `-v` (`tools/info.cpp:88-90`); the Rust
/// port additionally honours the creds-only `--show-credentials` opt-in,
/// so `-v` is a superset.
fn show_credentials_for(verbose: bool, show_credentials: bool) -> bool {
    verbose || show_credentials
}

/// Build the host-troubleshooting report `-D` prints, the Rust analogue
/// of pvxs `target_information` (`src/describe.cpp:27-133`) invoked by
/// `tools/info.cpp:62-64`. Returned as a string so the section layout is
/// unit-testable without capturing process stdout.
///
/// pvxs's report is Host/Target → Toolchain → Versions → Runtime →
/// Effective Client config → Effective Server config. The C++
/// toolchain/ABI block (`describe.cpp:36-75`: `__cplusplus`, libstdc++
/// ABI, compiler build) has no Rust counterpart and is omitted; every
/// other section has a Rust-side equivalent and is reproduced:
///   - Host: the single-binary arch/OS (no host≠target cross split as in
///     the C build system, so the `EPICS_HOST_ARCH`/`T_A` pair collapses),
///   - Versions: the shared `version_information` stack (pvxs/EPICS
///     Base/protocol — `describe.cpp:77-83`),
///   - Runtime: CPU count (`epicsThreadGetCPUs`), the bound interface
///     addresses (`osiLocalAddr`), and the per-NIC broadcast targets
///     (`osiSockDiscoverBroadcastAddresses` — `describe.cpp:85-113`),
///   - the effective client AND server config blocks
///     (`describe.cpp:115-129`); the server block is the `EPICS_PVAS_*`
///     set an operator needs to debug why a server does or does not
///     advertise, and it never appears in the client-only `-v` output.
fn target_information_string() -> String {
    use std::fmt::Write;
    let mut s = String::new();

    // Host arch/OS — pvxs `Host:`/`Target:` (describe.cpp:29-34). One
    // Rust binary has no host≠target split, so it is a single line.
    let _ = writeln!(
        s,
        "Host: {}-{}",
        std::env::consts::ARCH,
        std::env::consts::OS
    );

    // Versions — pvxs prints pvxs/EPICS Base/libevent (describe.cpp:77-83);
    // the Rust analogue is the same `-V` stack (lib/EPICS Base/protocol).
    let _ = writeln!(s, "Versions");
    for line in cli::version_information().lines() {
        let _ = writeln!(s, "  {line}");
    }

    // Runtime — pvxs prints epicsThreadGetCPUs()/osiLocalAddr()/
    // osiSockDiscoverBroadcastAddresses() (describe.cpp:85-113). The Rust
    // analogues: available parallelism, the bound interface addresses, and
    // the per-NIC broadcast targets. (pvxs's `uname()` line has no
    // allocation-free std analogue without libc FFI; the Host line already
    // carries arch/OS.)
    let _ = writeln!(s, "Runtime");
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let _ = writeln!(s, "  available_parallelism() -> {cpus}");
    let _ = writeln!(s, "  local interfaces:");
    for addr in config::list_intf_addresses() {
        let _ = writeln!(s, "    {addr}");
    }
    let bport = config::broadcast_port();
    let _ = writeln!(s, "  broadcast addresses (port {bport}):");
    for addr in config::list_broadcast_addresses(bport) {
        let _ = writeln!(s, "    {addr}");
    }

    // Effective client + server config — pvxs prints BOTH under -D
    // (describe.cpp:115-129). The client block is the same one `-v` shows;
    // the server (`EPICS_PVAS_*`) block is `-D`-only.
    let _ = write!(s, "{}", cli::effective_config_string());
    let _ = write!(s, "{}", cli::effective_server_config_string());
    s
}

/// Print the `-D` host-troubleshooting report and return (`tools/info.cpp:
/// 62-64`: `target_information(std::cout); return 0;`).
fn target_information() {
    print!("{}", target_information_string());
}

#[tokio::main]
async fn main() {
    let args: Args = cli::parse_or_exit();

    // pvxs `-V` prints version_information and exits before any client
    // setup (tools/info.cpp `case 'V'`).
    if args.version {
        print!("{}", cli::version_information());
        return;
    }

    // Install the shared tracing subscriber (honours EPICS_PVA_LOG /
    // RUST_LOG) and apply `-d` as a DEBUG bump of the library namespace.
    // pvxs runs `logger_config_env()` + maps `-d` before any option
    // action, including `-D` (tools/info.cpp:47-66).
    epics_pva_rs::log::install_cli_logging(args.debug);

    // pvxs `-D` prints host troubleshooting info and exits before any
    // server contact (`tools/info.cpp:62-64`: `target_information();
    // return 0;`), so it is the one form that needs no PV name and it is
    // tested before the name check below.
    if args.describe {
        target_information();
        return;
    }

    // C's own check, in C's wording (pvinfo.cpp:166-170) — note the
    // `(s)`, which pvput and pvcall do not have. It returns 1; a
    // clap-required argument would have exited 2.
    if args.pv_names.is_empty() {
        eprintln!("No pv name(s) specified. ('pvinfo -h' for help.)");
        std::process::exit(1);
    }

    // pvxs `pvxinfo -v` prints the effective client config once, before
    // the per-PV type output (`tools/info.cpp:78-79`), not per channel.
    if args.verbose {
        cli::print_effective_config();
    }

    // Build the client with the parsed `-w` as the operation timeout,
    // applied once at construction so every describe op uses the same
    // owner. pvxs `pvxinfo` waits for completion with `done.wait(timeout)`
    // (tools/info.cpp:66,112), and `epicsEvent::wait(0)` is `tryWait()` —
    // an immediate poll (epicsEvent.h:101-107). Route `-w` through
    // `wait_timeout_duration` so `-w 0` maps to an immediate (zero) timeout
    // rather than the generic `timeout_duration` 5 s clamp.
    let client = PvaClient::builder()
        .timeout(epics_pva_rs::cli::wait_timeout_duration(args.timeout))
        .build();

    // Start a describe op for every PV before awaiting any, then print
    // each PV's result the instant its task completes — completion order,
    // not input order — so a fast PV is visible before a slow or missing
    // sibling's timeout expires. pvxs `pvxinfo` (tools/info.cpp:78-112)
    // exec()s all ops, installs a per-op `.result()` callback that prints
    // the PV when its op finishes, hurryUp()s, and waits once. The serial
    // await-per-PV loop this replaces spent one timeout per PV; the prior
    // collect-then-zip loop buffered every completed PV behind the slowest
    // sibling. Each
    // output block leads with the PV name, so completion-order output
    // stays self-identifying.
    let names: Vec<&str> = args.pv_names.iter().map(String::as_str).collect();
    let mut failed = false;
    client
        .pvinfo_many_full_streaming(&names, |idx, result| {
            let pv_name = &args.pv_names[idx];
            match result {
                Ok((desc, server_addr, cred)) => {
                    // Under `-v` or `--show-credentials`, print the server's
                    // peer identity line first. pvxs-TLS prints this under
                    // verbose `-v` (info.cpp:88-90) via
                    // `operator<<(PeerCredentials)`, formatted as
                    // `TLS method:authority/account@peer`; the `TLS` prefix
                    // and `authority` segment are omitted when not applicable.
                    if show_credentials_for(args.verbose, args.show_credentials) {
                        match &cred {
                            Some(c) => {
                                let authority = if c.authority.is_empty() {
                                    String::new()
                                } else {
                                    format!(":{}", c.authority)
                                };
                                println!("# TLS x509{authority}/{}@{server_addr}", c.account);
                            }
                            None => {
                                // Plain `pva://` TCP — no peer certificate,
                                // so no X.509 identity to report.
                                println!("# anonymous/anonymous@{server_addr}");
                            }
                        }
                    }
                    // pvxs `pvinfo` prints "<pv> from <peer>" then the type tree
                    // with values hidden (info.cpp:90-94). This replaces the
                    // Rust-specific `<pv>` / `Server:` / `Type:` label block; the
                    // `--show-credentials` `#` line above is a Rust-only
                    // diagnostic kept ahead of the pvxs-format block.
                    print!("{}", format::format_info_value(pv_name, server_addr, &desc));
                }
                Err(e) => {
                    eprintln!("{pv_name}: {e}");
                    failed = true;
                }
            }
        })
        .await;
    if failed {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `pvinfo-rs -w 0` is an immediate completion poll: pvxs `pvxinfo`
    /// waits with `done.wait(timeout)` and `epicsEvent::wait(0)` is
    /// `tryWait()` (tools/info.cpp:112, epicsEvent.h:101-107), so a
    /// non-positive `-w` maps to `Duration::ZERO`, NOT the prior 5 s clamp.
    #[test]
    fn w_zero_is_immediate_timeout() {
        let args = Args::parse_from(["pvinfo-rs", "-w", "0", "PV"]);
        assert_eq!(
            epics_pva_rs::cli::wait_timeout_duration(args.timeout),
            std::time::Duration::ZERO
        );
    }

    /// A finite, strictly-positive `-w` is preserved as a bounded timeout.
    #[test]
    fn w_positive_is_finite_timeout() {
        let args = Args::parse_from(["pvinfo-rs", "-w", "2.5", "PV"]);
        assert_eq!(
            epics_pva_rs::cli::wait_timeout_duration(args.timeout),
            std::time::Duration::from_secs_f64(2.5)
        );
    }

    /// pvxs-TLS prints the peer-credentials line under verbose `-v`
    /// (`tools/info.cpp:88-90`); the Rust port keeps a creds-only
    /// `--show-credentials` opt-in too. The flags parse independently
    /// (neither sets the other's field), and either one drives the
    /// credential line via `show_credentials_for` — with `-v` a superset.
    #[test]
    fn verbose_drives_credentials_and_flags_parse_independently() {
        let v = Args::parse_from(["pvinfo-rs", "-v", "PV"]);
        assert!(v.verbose, "-v sets verbose (effective config)");
        assert!(
            !v.show_credentials,
            "-v does not set the show_credentials field"
        );
        assert!(
            show_credentials_for(v.verbose, v.show_credentials),
            "-v drives the credential line (pvxs-TLS info.cpp:88-90)"
        );

        let c = Args::parse_from(["pvinfo-rs", "--show-credentials", "PV"]);
        assert!(c.show_credentials, "--show-credentials sets the flag");
        assert!(
            !c.verbose,
            "--show-credentials does not set verbose (no effective-config dump)"
        );
        assert!(show_credentials_for(c.verbose, c.show_credentials));

        let neither = Args::parse_from(["pvinfo-rs", "PV"]);
        assert!(
            !show_credentials_for(neither.verbose, neither.show_credentials),
            "no flag -> no credential line"
        );

        let both = Args::parse_from(["pvinfo-rs", "-v", "--show-credentials", "PV"]);
        assert!(both.verbose && both.show_credentials);
    }

    /// `-D` is a self-contained host-info dump: pvxs exits before
    /// requiring a PV name (`tools/info.cpp:62-64`), so `pvinfo-rs -D`
    /// with no PV must parse.
    ///
    /// A bare invocation must parse too, and that is the change: C
    /// checks the missing name itself and returns 1 with its own message
    /// (pvinfo.cpp:166-170), so leaving it to clap exited 2. This test
    /// used to assert the clap rejection. The diagnostic and the exit
    /// code are pinned in `tests/cli_missing_pv_name_exit.rs`, which is
    /// where the check now lives.
    #[test]
    fn a_pv_name_is_not_a_clap_requirement() {
        let d = Args::try_parse_from(["pvinfo-rs", "-D"]).expect("-D needs no PV name");
        assert!(d.describe);
        assert!(d.pv_names.is_empty());

        let bare = Args::try_parse_from(["pvinfo-rs"]).expect("main owns the missing-name check");
        assert!(!bare.describe);
        assert!(bare.pv_names.is_empty());
    }

    /// `-d` parses into a real flag wired to `install_cli_logging`
    /// (pvxs `pvxinfo -d`, info.cpp:47-66). Default off.
    #[test]
    fn debug_flag_parses() {
        assert!(Args::parse_from(["pvinfo-rs", "-d", "PV"]).debug);
        assert!(!Args::parse_from(["pvinfo-rs", "PV"]).debug);
    }

    /// `-D` is the Rust analogue of pvxs `target_information`
    /// (`src/describe.cpp:27-133`): it must carry every section that has a
    /// Rust equivalent — Host, the version stack, Runtime readback, and
    /// BOTH the effective client and the effective server config blocks.
    /// The server (`EPICS_PVAS_*`) block is the piece the narrow pre-fix
    /// network print dropped, and it is what an operator uses to debug why
    /// a server does or does not advertise.
    #[test]
    fn target_information_includes_all_sections() {
        let s = target_information_string();
        for needle in [
            "Host: ",
            "Versions",
            // version stack (pvxs/EPICS Base/protocol analogue)
            "epics-pva-rs ",
            "EPICS Base ",
            "PVA protocol version ",
            "Runtime",
            "available_parallelism() ->",
            "local interfaces:",
            "broadcast addresses (port ",
            // both config sections present
            "Effective config",
            "Effective server config",
            // a representative key from each config block
            "EPICS_PVA_BROADCAST_PORT=",
            "EPICS_PVAS_SERVER_PORT=",
        ] {
            assert!(s.contains(needle), "missing {needle:?} in:\n{s}");
        }
    }

    /// `-D` shows the effective *server* config from `EPICS_PVAS_*`, while
    /// the client-only `-v` block does not — the parity contract the
    /// pre-fix print broke by omitting the server section. Serialised on
    /// `epics_env` because the variables are process-global.
    #[test]
    #[serial_test::serial(epics_env)]
    fn target_information_reflects_pvas_env_but_v_stays_client_only() {
        // SAFETY: std::env mutation is unsafe in edition 2024; the
        // `epics_env` serial guard makes it race-free.
        unsafe {
            std::env::set_var("EPICS_PVAS_SERVER_PORT", "5099");
            std::env::set_var("EPICS_PVAS_INTF_ADDR_LIST", "10.1.2.3");
        }

        let d = target_information_string();
        assert!(
            d.contains("EPICS_PVAS_SERVER_PORT=5099"),
            "-D must reflect EPICS_PVAS_SERVER_PORT: {d}"
        );
        assert!(
            d.contains("EPICS_PVAS_INTF_ADDR_LIST=10.1.2.3"),
            "-D must reflect EPICS_PVAS_INTF_ADDR_LIST: {d}"
        );

        // The shared `-v` client block must stay client-only — no server
        // keys leak into it.
        let v = cli::effective_config_string();
        assert!(
            !v.contains("EPICS_PVAS_"),
            "the `-v` client block must not carry server keys: {v}"
        );

        unsafe {
            std::env::remove_var("EPICS_PVAS_SERVER_PORT");
            std::env::remove_var("EPICS_PVAS_INTF_ADDR_LIST");
        }
    }
}
