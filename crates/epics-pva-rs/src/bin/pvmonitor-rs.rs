use clap::Parser;
use epics_pva_rs::client::PvaClient;
use epics_pva_rs::client_native::ops_v2::{MonitorEvent, MonitorEventMask};
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

    // One client for the whole command, shared by every monitor task —
    // not one PvaClient per PV. pvxs builds a single client::Context and
    // starts every subscription from it (tools/monitor.cpp:95-122), so PVs
    // on the same IOC share one channel cache, connection pool, and search
    // engine, and one coherent hurryUp() can follow the install loop. `-w`
    // bounds the connect/operation timeout (pvxs monitor has none; threaded
    // per finding 23) rather than being silently dropped.
    let client = PvaClient::builder()
        .timeout(epics_pva_rs::cli::timeout_duration(args.timeout))
        .build();

    let mut handles = Vec::new();

    for pv_name in args.pv_names {
        let mode = args.mode.clone();
        let request = request.clone();
        let client = client.clone();
        let handle = tokio::spawn(async move {
            // Format every update with the descriptor carried by the
            // monitor's own INIT response (`MonitorEvent::Data.intro`),
            // not a separate GET_FIELD. A projected request
            // (`-r 'field(alarm)'`) then formats against the projected
            // monitor shape — and no extra wire op is issued that a server
            // or gateway might fail or authorize differently from MONITOR.
            // pvxs formats the Value popped from the subscription itself
            // (tools/monitor.cpp:133-146).
            let on_event = |event: MonitorEvent| {
                if let MonitorEvent::Data { intro, value } = event {
                    // `-q` is a deprecated no-op (see Args::quiet); monitor
                    // updates are always printed, matching pvAccessCPP.
                    let output = match mode.as_str() {
                        "json" => format::format_json(&pv_name, &value),
                        "raw" => format::format_raw(&pv_name, &intro, &value),
                        _ => format::format_nt(&pv_name, &intro, &value),
                    };
                    print!("{output}");
                }
            };

            // Value-only output: lifecycle events stay suppressed at this
            // step so the descriptor-source change is isolated.
            let mask = MonitorEventMask {
                mask_connected: true,
                mask_disconnected: true,
            };
            let result = client
                .pvmonitor_events(&pv_name, request.as_ref(), mask, on_event)
                .await;

            if let Err(e) = result {
                eprintln!("{pv_name}: {e}");
            }
        });
        handles.push(handle);
    }

    // All subscriptions registered — hurry discovery once from the shared
    // client, matching pvxs `ctxt.hurryUp()` after the install loop
    // (tools/monitor.cpp:120-122).
    client.hurry_up().await;

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
