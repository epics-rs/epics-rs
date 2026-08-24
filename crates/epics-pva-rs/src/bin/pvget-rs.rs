use clap::Parser;
use epics_pva_rs::client::PvaClient;
use epics_pva_rs::pv_request::PvRequestExpr;
use epics_pva_rs::{cli, format};

#[derive(Parser)]
#[command(
    name = "pvget-rs",
    about = "Read EPICS PV values via pvAccess",
    disable_version_flag = true
)]
struct Args {
    /// Print version information (pvxs `version_information`, tools
    /// `case 'V'`) and exit. Routed through `cli::version_information`
    /// so every PVA CLI reports the same library + protocol stack
    /// instead of clap's crate-only `<binary> <version>`.
    #[arg(short = 'V', long = "version")]
    version: bool,

    /// PV names to read. C `pvget` has no missing-name check: the
    /// connect loop runs from `optind` to `argc` (pvget.cpp:400) and an
    /// empty range leaves `haderror` clear, so `pvget` with no argument
    /// prints nothing and exits 0 (measured).
    pv_names: Vec<String>,

    /// Request, specifies what fields to return and options
    #[arg(short = 'r', default_value = "")]
    request: String,

    /// Output mode: raw, nt, json. The full structure is shown with
    /// `-M raw` (pvxs reserves `-v` for effective-config diagnostics,
    /// not output formatting).
    #[arg(short = 'M', default_value = "nt")]
    mode: String,

    /// Output format mode (pvxs `-F`): `tree` or `delta`. When set, this
    /// selects the pvxs `Value::format()` formatter (get.cpp:80-86,114-117)
    /// instead of `-M`. pvxs defaults this to `delta`; here it is opt-in so
    /// the established `-M nt` default is preserved. An unknown value warns
    /// and falls back to `delta`, like pvxs.
    #[arg(short = 'F', long = "format")]
    format: Option<String>,

    /// Array element limit for the `-F tree|delta` formatter (pvxs `-#`,
    /// get.cpp:77-78). `0` (the default) is unlimited. Only affects the
    /// `-F` formatter — the `-M` modes are unchanged.
    #[arg(short = '#', long = "array-limit", default_value = "0")]
    array_limit: usize,

    /// Verbose ("make more noise"): print the effective PVA client
    /// configuration before the GET. pvxs `tools/get.cpp:65-67,99-100`
    /// sets `verbose=true` and prints `Effective config` + the client
    /// context config; it does NOT change the value formatter (that is
    /// `-M`/`-F`).
    #[arg(short = 'v', action = clap::ArgAction::Count)]
    verbose: u8,

    /// Wait time in seconds
    #[arg(short = 'w', default_value = "5.0")]
    timeout: f64,

    /// Deprecated, ignored. pvAccessCPP `pvget` treats `-q` as a
    /// deprecated no-op (`pvtoolsSrc/pvget.cpp:332-338`) and still
    /// prints successful values; pvxs `tools/get.cpp` has no such
    /// option. Accepted for legacy CLI compatibility, but has no effect.
    #[arg(short = 'q')]
    quiet: bool,

    /// Raise the `epics_pva_rs` library log level to DEBUG. Mirrors pvxs
    /// `pvxget -d` mapping to `logger_level_set("pvxs.*", Level::Debug)`
    /// (`tools/get.cpp:47-70`). Combine with `EPICS_PVA_LOG`/`RUST_LOG`
    /// for finer control; this just raises the library namespace.
    #[arg(short = 'd')]
    debug: bool,
}

#[tokio::main]
async fn main() {
    let args: Args = cli::parse_or_exit();

    // pvxs `-V` prints version_information and exits before any client
    // setup (tools/get.cpp:62-64).
    if args.version {
        print!("{}", cli::version_information());
        return;
    }

    // Install the shared tracing subscriber (honours EPICS_PVA_LOG /
    // RUST_LOG) and apply `-d` as a DEBUG bump of the library namespace.
    // pvxs runs `logger_config_env()` + maps `-d` to a debug level set
    // here, before any operation (tools/get.cpp:47-70).
    epics_pva_rs::log::install_cli_logging(args.debug);

    // pvxs `-v` prints the effective client config once, before any
    // operation (tools/get.cpp:99-100). It is a diagnostic, not an
    // output-format switch — the value formatter stays under `-M`.
    if args.verbose > 0 {
        cli::print_effective_config();
    }

    // Parse the pvRequest once with the strict pvxs grammar, so the
    // request strings this CLI accepts are exactly those pvxs's own
    // `pvget` accepts: `field(...)` selections and `record[k=v]` options
    // pass, while the lenient pvDataCPP `createRequest` extensions (brace
    // member groups, per-field option brackets, quoted / `:`-bearing
    // option values) are rejected. pvxs `tools/get.cpp:110` hands `-r` to
    // `RequestBuilder::pvRequest()`, whose strict `PVRParser`
    // (`src/clientreq.cpp:137-283`) is what `parse_pvxs_compat` mirrors —
    // the lenient `parse` would let this CLI accept requests pvxs rejects.
    // Empty `-r` means the default all-fields GET; an invalid request
    // exits before any GET.
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

    // Build the client with the parsed `-w` as the operation timeout,
    // applied once at construction so every op path uses the same owner.
    // pvxs `pvxget` waits for completion with `done.wait(timeout)`
    // (tools/get.cpp:72,132), and `epicsEvent::wait(0)` is `tryWait()` —
    // an immediate poll (epicsEvent.h:101-107). Route `-w` through
    // `wait_timeout_duration` so `-w 0` maps to an immediate (zero)
    // timeout rather than the generic `timeout_duration` 5 s clamp.
    let client = PvaClient::builder()
        .timeout(epics_pva_rs::cli::wait_timeout_duration(args.timeout))
        .build();
    let mode = args.mode.as_str();

    // Start a GET for every PV before awaiting any, then print each PV's
    // result the instant its task completes — completion order, not input
    // order — so a fast PV is visible before a slow or missing sibling's
    // timeout expires. pvxs `pvxget` (tools/get.cpp:102-133) exec()s all
    // ops, installs a per-op `.result()` callback that prints the PV when
    // its op finishes, hurryUp()s, and waits once. The serial await-per-PV
    // loop this replaces spent one timeout per PV; the prior
    // collect-then-zip loop buffered every completed PV behind the slowest
    // sibling. Each
    // output block leads with the PV name, so completion-order output
    // stays self-identifying — exactly as pvxs prints `argv[n]` first.
    let names: Vec<&str> = args.pv_names.iter().map(String::as_str).collect();
    let mut failed = false;
    client
        .pvget_many_full_streaming(&names, request.as_ref(), |idx, result| {
            let pv_name = &args.pv_names[idx];
            match result {
                Ok(result) => {
                    // `-q` is a deprecated no-op (see Args::quiet); successful
                    // values are always printed, matching pvAccessCPP pvget.
                    let output = if let Some(vfmt) =
                        format::parse_value_format(args.format.as_deref())
                    {
                        // pvxs `-F` path (get.cpp:112-117): print the PV-name
                        // line, then the `Value::format()` output wrapped in one
                        // `Indented` level (base_depth=1). A GET marks every
                        // field it returns, so Delta shows them all (marked=None).
                        let fmt = format::ValueFmt {
                            format: vfmt,
                            array_limit: args.array_limit,
                            show_value: true,
                        };
                        format!(
                            "{pv_name}\n{}",
                            format::format_value(
                                &result.introspection,
                                Some(&result.value),
                                &fmt,
                                None,
                                1,
                            )
                        )
                    } else {
                        match mode {
                            "json" => format::format_json(pv_name, &result.value, None),
                            "raw" => format::format_raw(
                                pv_name,
                                &result.introspection,
                                &result.value,
                                None,
                            ),
                            _ => format::format_nt(pv_name, &result.introspection, &result.value),
                        }
                    };
                    print!("{output}");
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

    /// `pvget-rs -w 0` is an immediate completion poll: pvxs `pvxget`
    /// waits with `done.wait(timeout)` and `epicsEvent::wait(0)` is
    /// `tryWait()` (tools/get.cpp:132, epicsEvent.h:101-107), so a
    /// non-positive `-w` maps to `Duration::ZERO`, NOT the prior 5 s clamp.
    #[test]
    fn w_zero_is_immediate_timeout() {
        let args = Args::parse_from(["pvget-rs", "-w", "0", "PV"]);
        assert_eq!(
            epics_pva_rs::cli::wait_timeout_duration(args.timeout),
            std::time::Duration::ZERO
        );
    }

    /// A finite, strictly-positive `-w` is preserved as a bounded timeout.
    #[test]
    fn w_positive_is_finite_timeout() {
        let args = Args::parse_from(["pvget-rs", "-w", "2.5", "PV"]);
        assert_eq!(
            epics_pva_rs::cli::wait_timeout_duration(args.timeout),
            std::time::Duration::from_secs_f64(2.5)
        );
    }

    /// `-q` is accepted for legacy compatibility but is a deprecated
    /// no-op: it must not gate successful output (pvAccessCPP
    /// pvget.cpp:332-338). This locks the flag's presence; the
    /// print-always behavior is enforced structurally in `main`.
    #[test]
    fn quiet_flag_is_accepted_as_deprecated_noop() {
        let args = Args::parse_from(["pvget-rs", "-q", "PV"]);
        assert!(args.quiet);
    }

    /// `-r` is parsed with the strict pvxs grammar (`parse_pvxs_compat`,
    /// the parser `main` uses), so `record[...]` options survive instead
    /// of being dropped by a field-list split, while acceptance matches
    /// pvxs's own `pvget` (`tools/get.cpp:110` → `RequestBuilder::pvRequest`,
    /// `src/clientreq.cpp:137-283`). `record[process=true]field(value)` is
    /// strict-valid, so it must round-trip the record option and field.
    #[test]
    fn request_record_options_parse_via_strict_pvxs_grammar() {
        let args = Args::parse_from(["pvget-rs", "-r", "record[process=true]field(value)", "PV"]);
        let req = PvRequestExpr::parse_pvxs_compat(args.request.trim()).expect("valid pvRequest");
        assert_eq!(
            req.record_options,
            vec![(
                "process".to_string(),
                epics_pva_rs::pvdata::ScalarValue::String("true".into())
            )],
            "record[...] options must be preserved as the parsed-text string form"
        );
        assert_eq!(req.fields, vec!["value".to_string()]);
    }

    /// Empty `-r` (the default) selects the all-fields GET — no request
    /// is built.
    #[test]
    fn empty_request_is_default_get() {
        let args = Args::parse_from(["pvget-rs", "PV"]);
        assert!(args.request.trim().is_empty());
    }

    /// `-d` parses into a real flag wired to `install_cli_logging`
    /// (pvxs `pvxget -d` raises the library log level, get.cpp:47-70).
    /// It must default off so ordinary runs stay quiet.
    #[test]
    fn debug_flag_parses() {
        assert!(Args::parse_from(["pvget-rs", "-d", "PV"]).debug);
        assert!(!Args::parse_from(["pvget-rs", "PV"]).debug);
    }

    /// `-F`/`--format` parses into `format` and routes through
    /// `parse_value_format` to select the pvxs `Value::format()` output
    /// (get.cpp:80-86). Absent by default so the established `-M nt` output
    /// is preserved; present it overrides `-M`.
    #[test]
    fn format_flag_parses_and_selects_value_formatter() {
        assert_eq!(Args::parse_from(["pvget-rs", "PV"]).format, None);
        let tree = Args::parse_from(["pvget-rs", "-F", "tree", "PV"]);
        assert_eq!(tree.format.as_deref(), Some("tree"));
        assert_eq!(
            format::parse_value_format(tree.format.as_deref()),
            Some(format::ValueFormat::Tree)
        );
        let delta = Args::parse_from(["pvget-rs", "--format", "delta", "PV"]);
        assert_eq!(
            format::parse_value_format(delta.format.as_deref()),
            Some(format::ValueFormat::Delta)
        );
    }

    /// `-#`/`--array-limit` parses the pvxs per-array element cap
    /// (get.cpp:77-78). Default `0` = unlimited; it only affects the `-F`
    /// formatter, never the `-M` modes.
    #[test]
    fn array_limit_flag_parses_with_unlimited_default() {
        assert_eq!(Args::parse_from(["pvget-rs", "PV"]).array_limit, 0);
        assert_eq!(
            Args::parse_from(["pvget-rs", "-#", "5", "PV"]).array_limit,
            5
        );
        assert_eq!(
            Args::parse_from(["pvget-rs", "--array-limit", "12", "PV"]).array_limit,
            12
        );
    }
}
