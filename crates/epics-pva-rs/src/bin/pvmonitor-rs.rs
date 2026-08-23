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

    /// PV names to monitor. `pvmonitor.cpp` is `pvget.cpp` compiled with
    /// PVMONITOR, so it inherits the same absent check: no argument
    /// prints nothing and exits 0 (measured).
    pv_names: Vec<String>,

    /// Output mode: raw, nt, json. The full structure is shown with
    /// `-M raw` (pvxs reserves `-v` for effective-config diagnostics,
    /// not output formatting).
    #[arg(short = 'M', default_value = "nt")]
    mode: String,

    /// Output format mode (pvxs `-F`): `tree` or `delta`. When set, this
    /// selects the pvxs `Value::format()` formatter (monitor.cpp:78-84,
    /// 144-146) instead of `-M`. pvxs defaults this to `delta`; here it is
    /// opt-in so the established `-M nt` default is preserved. An unknown
    /// value warns and falls back to `delta`, like pvxs.
    #[arg(short = 'F', long = "format")]
    format: Option<String>,

    /// Array element limit for the `-F tree|delta` formatter (pvxs `-#`,
    /// monitor.cpp:75-76). `0` (the default) is unlimited. Only affects the
    /// `-F` formatter — the `-M` modes are unchanged.
    #[arg(short = '#', long = "array-limit", default_value = "0")]
    array_limit: usize,

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

    /// Raise the `epics_pva_rs` library log level to DEBUG. Mirrors pvxs
    /// `pvxmonitor -d` mapping to `logger_level_set("pvxs.*",
    /// Level::Debug)` (`tools/monitor.cpp:48-70`).
    #[arg(short = 'd')]
    debug: bool,
}

#[tokio::main]
async fn main() {
    let args: Args = cli::parse_or_exit();

    // pvxs `-V` prints version_information and exits before any client
    // setup (tools/monitor.cpp:63).
    if args.version {
        print!("{}", cli::version_information());
        return;
    }

    // Install the shared tracing subscriber (honours EPICS_PVA_LOG /
    // RUST_LOG) and apply `-d` as a DEBUG bump of the library namespace,
    // mirroring pvxs `logger_config_env()` + `-d` (tools/monitor.cpp:48-70).
    epics_pva_rs::log::install_cli_logging(args.debug);

    // Parse the custom pvRequest once, up front, so an invalid string
    // exits before any subscription task is spawned. `None` means use
    // the channel-default request. The strict pvxs grammar is used so
    // acceptance matches pvxs's own `pvmonitor`: `tools/monitor.cpp:110`
    // routes `-r` through `RequestBuilder::pvRequest()` (strict `PVRParser`,
    // `src/clientreq.cpp:137-283`), which rejects the lenient pvDataCPP
    // `createRequest` extensions the bare `parse` allows.
    let request: Option<PvRequestExpr> = {
        let trimmed = args.request.trim();
        if trimmed.is_empty() {
            None
        } else {
            match PvRequestExpr::parse_pvxs_compat(trimmed) {
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
        let format_mode = args.format.clone();
        let array_limit = args.array_limit;
        let request = request.clone();
        let client = client.clone();
        let verbose = args.verbose > 0;
        let handle = tokio::spawn(async move {
            // Format every Data update with the descriptor carried by the
            // monitor's own INIT response (`MonitorEvent::Data.intro`), not
            // a separate GET_FIELD, and surface the connect/disconnect/
            // finished lifecycle the way pvxs does. Data goes to stdout;
            // lifecycle goes to stderr (pvxs `tools/monitor.cpp:140-163`:
            // `std::cout` for the value, `std::cerr` for Connected /
            // Disconnected / Finished, the last only when verbose).
            let on_event = |event: MonitorEvent| match event {
                MonitorEvent::Data {
                    intro,
                    value,
                    marked,
                } => {
                    // `-q` is a deprecated no-op (see Args::quiet); monitor
                    // updates are always printed, matching pvAccessCPP.
                    let output = if let Some(vfmt) =
                        format::parse_value_format(format_mode.as_deref())
                    {
                        // pvxs `-F` path (monitor.cpp:142-146): the PV-name
                        // line, then `Value::format()` wrapped in one
                        // `Indented` level. `value` is the full reconstructed
                        // snapshot; `marked` carries this update's
                        // server-marked changed leaves so Delta prints only
                        // them (`Value::imarked()`, datafmt.cpp:112-120). It is
                        // `None` on the first update (full snapshot) so the
                        // first Delta shows every leaf, like pvxs.
                        let fmt = format::ValueFmt {
                            format: vfmt,
                            array_limit,
                            show_value: true,
                        };
                        format!(
                            "{pv_name}\n{}",
                            format::format_value(&intro, Some(&value), &fmt, marked.as_ref(), 1)
                        )
                    } else {
                        // pvget.cpp:239 `fmt.show(mon.changed)` — the raw
                        // and JSON printers restrict themselves to this
                        // update's changed set, so a monitor reprints only
                        // what moved. The NT branch never reads
                        // `Formatter::xshow` (printer.cpp:414-452), so it
                        // stays a full summary line.
                        match mode.as_str() {
                            "json" => format::format_json(&pv_name, &value, marked.as_ref()),
                            "raw" => format::format_raw(&pv_name, &intro, &value, marked.as_ref()),
                            _ => format::format_nt(&pv_name, &intro, &value),
                        }
                    };
                    print!("{output}");
                }
                MonitorEvent::Connected { peer } => {
                    eprintln!("{pv_name} Connected to {peer}");
                }
                MonitorEvent::Disconnected => {
                    eprintln!("{pv_name} Disconnected");
                }
                MonitorEvent::Finished => {
                    if verbose {
                        eprintln!("{pv_name} Finished");
                    }
                }
            };

            // pvxs monitors with maskConnected=false, maskDisconnected=false
            // so the CLI reports the subscription lifecycle
            // (tools/monitor.cpp:111-112).
            let mask = MonitorEventMask {
                mask_connected: false,
                mask_disconnected: false,
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

    /// `-d` parses into a real flag wired to `install_cli_logging`
    /// (pvxs `pvxmonitor -d`, monitor.cpp:48-70). Default off.
    #[test]
    fn debug_flag_parses() {
        assert!(Args::parse_from(["pvmonitor-rs", "-d", "PV"]).debug);
        assert!(!Args::parse_from(["pvmonitor-rs", "PV"]).debug);
    }

    /// `-F`/`--format` parses into `format` and routes through
    /// `parse_value_format` to select the pvxs `Value::format()` output
    /// (monitor.cpp:78-84). Absent by default so the established `-M nt`
    /// output is preserved; present it overrides `-M`.
    #[test]
    fn format_flag_parses_and_selects_value_formatter() {
        assert_eq!(Args::parse_from(["pvmonitor-rs", "PV"]).format, None);
        let tree = Args::parse_from(["pvmonitor-rs", "-F", "tree", "PV"]);
        assert_eq!(tree.format.as_deref(), Some("tree"));
        assert_eq!(
            format::parse_value_format(tree.format.as_deref()),
            Some(format::ValueFormat::Tree)
        );
        let delta = Args::parse_from(["pvmonitor-rs", "--format", "delta", "PV"]);
        assert_eq!(
            format::parse_value_format(delta.format.as_deref()),
            Some(format::ValueFormat::Delta)
        );
    }

    /// `-#`/`--array-limit` parses the pvxs per-array element cap
    /// (monitor.cpp:75-76). Default `0` = unlimited; it only affects the
    /// `-F` formatter, never the `-M` modes.
    #[test]
    fn array_limit_flag_parses_with_unlimited_default() {
        assert_eq!(Args::parse_from(["pvmonitor-rs", "PV"]).array_limit, 0);
        assert_eq!(
            Args::parse_from(["pvmonitor-rs", "-#", "5", "PV"]).array_limit,
            5
        );
        assert_eq!(
            Args::parse_from(["pvmonitor-rs", "--array-limit", "12", "PV"]).array_limit,
            12
        );
    }
}
