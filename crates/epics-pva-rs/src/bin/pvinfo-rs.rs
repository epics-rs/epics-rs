use clap::Parser;
use epics_pva_rs::client::PvaClient;
use epics_pva_rs::{config, format};

#[derive(Parser)]
#[command(
    name = "pvinfo-rs",
    version,
    about = "Show EPICS PV type info via pvAccess"
)]
struct Args {
    /// PV names to query
    #[arg(required = true)]
    pv_names: Vec<String>,

    /// Wait time in seconds
    #[arg(short = 'w', default_value = "5.0")]
    timeout: f64,

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

/// Print the effective PVA client configuration, mirroring pvxs
/// `pvxinfo -v` (`tools/info.cpp:76-79`, which prints `Effective config`
/// followed by the client context config). The values are read through
/// the resolved `config` getters so the output reflects environment +
/// defaults exactly as the client will use them.
fn print_effective_config() {
    let addr_list = std::env::var("EPICS_PVA_ADDR_LIST").unwrap_or_default();
    let name_servers = config::name_servers()
        .iter()
        .map(|a| a.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let interfaces = config::list_intf_addresses()
        .iter()
        .map(|a| a.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    println!("Effective config");
    println!("  EPICS_PVA_ADDR_LIST={addr_list}");
    println!(
        "  EPICS_PVA_AUTO_ADDR_LIST={}",
        if config::auto_addr_list_enabled() {
            "YES"
        } else {
            "NO"
        }
    );
    println!("  EPICS_PVA_SERVER_PORT={}", config::server_port());
    println!("  EPICS_PVA_BROADCAST_PORT={}", config::broadcast_port());
    println!("  EPICS_PVA_NAME_SERVERS={name_servers}");
    println!("  interfaces={interfaces}");
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // pvxs `pvxinfo -v` prints the effective client config once, before
    // the per-PV type output (`tools/info.cpp:76-79`), not per channel.
    if args.verbose {
        print_effective_config();
    }

    let client = PvaClient::new().expect("failed to create PVA client");

    let mut failed = false;
    for (i, pv_name) in args.pv_names.iter().enumerate() {
        if i > 0 {
            println!();
        }
        match client.pvinfo_full_with_credentials(pv_name).await {
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
}
