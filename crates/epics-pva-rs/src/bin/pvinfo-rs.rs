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
    #[arg(required_unless_present_any = ["describe", "version"])]
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

    /// Verbose: print the effective PVA client configuration before
    /// the type output. pvxs `tools/info.cpp:76-79` implements `-v` as
    /// `Effective config` + the client context configuration.
    #[arg(short = 'v')]
    verbose: bool,

    /// Print the server's peer credentials (TLS X.509 identity, or the
    /// anonymous account on a plain TCP connection) before each PV's
    /// type. Rust-specific diagnostic — pvxs `pvxinfo` has no such
    /// flag; `-v` there is effective-config output, not credentials.
    #[arg(long = "show-credentials")]
    show_credentials: bool,
}

/// Print host network troubleshooting info and return, mirroring pvxs
/// `target_information` (`src/describe.cpp:27`) as invoked by `-D`
/// (`tools/info.cpp:62-64`). pvxs additionally dumps C++ toolchain/ABI
/// details that have no Rust analogue; the operationally meaningful
/// part for "why can't I reach my IOC" is the local interfaces,
/// broadcast targets, and effective client config, which is what we
/// reproduce here.
fn target_information() {
    let interfaces = config::list_intf_addresses();
    println!("Network targets");
    println!("  interfaces:");
    for addr in &interfaces {
        println!("    {addr}");
    }
    println!("  broadcast addresses (port {}):", config::broadcast_port());
    for addr in config::list_broadcast_addresses(config::broadcast_port()) {
        println!("    {addr}");
    }
    cli::print_effective_config();
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // pvxs `-V` prints version_information and exits before any client
    // setup (tools/info.cpp `case 'V'`).
    if args.version {
        print!("{}", cli::version_information());
        return;
    }

    // pvxs `-D` prints host troubleshooting info and exits before any
    // server contact (`tools/info.cpp:62-64`: `target_information();
    // return 0;`). No PV name is required (enforced by
    // `required_unless_present`).
    if args.describe {
        target_information();
        return;
    }

    // pvxs `pvxinfo -v` prints the effective client config once, before
    // the per-PV type output (`tools/info.cpp:76-79`), not per channel.
    if args.verbose {
        cli::print_effective_config();
    }

    let client = PvaClient::new().expect("failed to create PVA client");

    // Start a describe op for every PV before awaiting any, then await the
    // whole set under one command-level timeout (pvxs `tools/info.cpp:81-112`
    // exec()s all ops, hurryUp()s, and waits once). The serial await-per-PV
    // loop this replaces spent one timeout per PV and let a slow first PV
    // block later ones from even starting.
    let names: Vec<&str> = args.pv_names.iter().map(String::as_str).collect();
    let results = client.pvinfo_many_full_with_credentials(&names).await;

    let mut failed = false;
    for (i, (pv_name, result)) in args.pv_names.iter().zip(results).enumerate() {
        if i > 0 {
            println!();
        }
        match result {
            Ok((desc, server_addr, cred)) => {
                // Under `--show-credentials`, print the server's peer
                // identity line first. pvxs
                // `operator<<(PeerCredentials)` formats as
                // `TLS method:authority/account@peer`; the `TLS`
                // prefix and `authority` segment are omitted when not
                // applicable. (pvxs reserves `-v` for effective config,
                // so this Rust diagnostic gets its own flag.)
                if args.show_credentials {
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
                println!("{pv_name}");
                if server_addr.port() != 0 {
                    println!("Server: {server_addr}");
                }
                println!("Type:");
                print!("{}", format::format_info_indented(&desc, 1));
            }
            Err(e) => {
                eprintln!("{pv_name}: {e}");
                failed = true;
            }
        }
    }
    if failed {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `-v` is effective-config output (pvxs `pvxinfo -v`), and must
    /// NOT pull in the credential diagnostic — that lives behind
    /// `--show-credentials`. The two flags are independent.
    #[test]
    fn verbose_and_credentials_are_independent_flags() {
        let v = Args::parse_from(["pvinfo-rs", "-v", "PV"]);
        assert!(v.verbose, "-v sets verbose (effective config)");
        assert!(!v.show_credentials, "-v must not imply credential output");

        let c = Args::parse_from(["pvinfo-rs", "--show-credentials", "PV"]);
        assert!(c.show_credentials, "--show-credentials sets the flag");
        assert!(
            !c.verbose,
            "--show-credentials must not imply effective config"
        );

        let both = Args::parse_from(["pvinfo-rs", "-v", "--show-credentials", "PV"]);
        assert!(both.verbose && both.show_credentials);
    }

    /// `-D` is a self-contained host-info dump: pvxs exits before
    /// requiring a PV name (`tools/info.cpp:62-64`). So `pvinfo-rs -D`
    /// with no PV must parse, while a bare invocation with no PV and no
    /// `-D` must still error.
    #[test]
    fn describe_flag_requires_no_pv_name() {
        let d = Args::try_parse_from(["pvinfo-rs", "-D"]).expect("-D needs no PV name");
        assert!(d.describe);
        assert!(d.pv_names.is_empty());

        // Without -D, at least one PV name is still required.
        assert!(
            Args::try_parse_from(["pvinfo-rs"]).is_err(),
            "no PV and no -D must be a usage error"
        );
    }
}
