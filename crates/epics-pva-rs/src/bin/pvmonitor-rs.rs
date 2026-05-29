use clap::Parser;
use epics_pva_rs::client::PvaClient;
use epics_pva_rs::pv_request::PvRequestExpr;
use epics_pva_rs::{cli, format};

#[derive(Parser)]
#[command(
    name = "pvmonitor-rs",
    about = "Monitor EPICS PVs via pvAccess",
    disable_version_flag = true
)]
struct Args {
    /// Print version information (pvxs `version_information`, tools
    /// `case 'V'`) and exit. Routed through `cli::version_information`
    /// so every PVA CLI reports the same library + protocol stack
    /// instead of clap's crate-only `<binary> <version>`.
    #[arg(short = 'V', long = "version")]
    version: bool,

    /// PV names to monitor
    #[arg(required_unless_present = "version")]
    pv_names: Vec<String>,

    /// Output mode: raw, nt, json. The full structure is shown with
    /// `-M raw` (pvxs reserves `-v` for effective-config diagnostics,
    /// not output formatting).
    #[arg(short = 'M', default_value = "nt")]
    mode: String,

    /// Verbose ("make more noise"): print the effective PVA client
    /// configuration before the subscription. pvxs
    /// `tools/monitor.cpp:65-67,97-98` sets `verbose=true` and prints
    /// `Effective config` + the client context config; it does NOT
    /// change the value formatter (that is `-M`/`-F`).
    #[arg(short = 'v', action = clap::ArgAction::Count)]
    verbose: u8,

    /// Connect/operation timeout in seconds (bounds the initial
    /// connect, not the monitor duration). pvxs `tools/monitor.cpp:56`
    /// has no `-w`; a monitor runs until interrupted. Kept as a
    /// Rust-only connect-timeout control threaded into the client.
    #[arg(short = 'w', default_value = "5.0")]
    timeout: f64,

    /// Deprecated, ignored. pvAccessCPP routes `pvmonitor` through the
    /// same `pvget.cpp` that treats `-q` as a deprecated no-op
    /// (`pvtoolsSrc/pvget.cpp:332-338`, included via `pvmonitor.cpp`)
    /// and still prints monitor updates; pvxs `tools/monitor.cpp` has
    /// no such option. Accepted for legacy compatibility, no effect.
    #[arg(short = 'q')]
    quiet: bool,

    /// pvRequest string (`field(...)` / `record[k=v]` syntax). When
    /// non-empty it drives the monitor subscription — e.g. to enable
    /// server-side filters: `-r 'record[_filter="{\"dec\":{\"n\":3}}"]'`.
    #[arg(short = 'r', default_value = "")]
    request: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // pvxs `-V` prints version_information and exits before any client
    // setup (tools/monitor.cpp:63).
    if args.version {
        print!("{}", cli::version_information());
        return;
    }

    // Parse the custom pvRequest once, up front, so an invalid string
    // exits before any subscription task is spawned. `None` means use
    // the channel-default request.
    let request: Option<PvRequestExpr> = {
        let trimmed = args.request.trim();
        if trimmed.is_empty() {
            None
        } else {
            match PvRequestExpr::parse(trimmed) {
                Ok(req) => Some(req),
                Err(e) => {
                    eprintln!("error: invalid pvRequest {:?}: {e}", args.request);
                    std::process::exit(1);
                }
            }
        }
    };

    // pvxs `-v` prints the effective client config once, before any
    // subscription is started (tools/monitor.cpp:97-98). It is a
    // diagnostic, not an output-format switch — the value formatter
    // stays under `-M`.
    if args.verbose > 0 {
        cli::print_effective_config();
    }

    let mut handles = Vec::new();

    for pv_name in args.pv_names {
        let mode = args.mode.clone();
        let request = request.clone();
        let timeout = args.timeout;
        let handle = tokio::spawn(async move {
            // `-w` bounds the connect/operation timeout (pvxs monitor
            // has none; thread it into the client per finding 23's
            // connect-timeout option) rather than being silently dropped.
            let client = PvaClient::builder()
                .timeout(epics_pva_rs::cli::timeout_duration(timeout))
                .build();

            // Get introspection once for typed formatting
            let desc = client.pvinfo(&pv_name).await.ok();

            let render = |value: &epics_pva_rs::pvdata::PvField| {
                // `-q` is a deprecated no-op (see Args::quiet); monitor
                // updates are always printed, matching pvAccessCPP.
                let output = if let Some(ref d) = desc {
                    match mode.as_str() {
                        "json" => format::format_json(&pv_name, value),
                        "raw" => format::format_raw(&pv_name, d, value),
                        _ => format::format_nt(&pv_name, d, value),
                    }
                } else {
                    format!("{pv_name} {value}\n")
                };
                print!("{output}");
            };

            let result = match request {
                Some(req) => client.pvmonitor_with_request(&pv_name, &req, render).await,
                None => client.pvmonitor(&pv_name, render).await,
            };

            if let Err(e) = result {
                eprintln!("{pv_name}: {e}");
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        let _ = handle.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `-q` is accepted but a deprecated no-op: monitor updates must
    /// still print (pvAccessCPP pvget.cpp:332-338, shared by pvmonitor).
    /// This locks the flag; the print-always behavior is enforced
    /// structurally in the render closure.
    #[test]
    fn quiet_flag_is_accepted_as_deprecated_noop() {
        let args = Args::parse_from(["pvmonitor-rs", "-q", "PV"]);
        assert!(args.quiet);
    }

    /// `-w` parses to a real value that is threaded into the client
    /// connect timeout (finding 23 — it must not be silently dropped).
    #[test]
    fn wait_flag_parses_as_timeout() {
        let args = Args::parse_from(["pvmonitor-rs", "-w", "2.5", "PV"]);
        assert_eq!(args.timeout, 2.5);
        // Default mirrors the other PVA CLIs (5s).
        let dflt = Args::parse_from(["pvmonitor-rs", "PV"]);
        assert_eq!(dflt.timeout, 5.0);
    }
}
