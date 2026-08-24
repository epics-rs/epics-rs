use clap::Parser;
use epics_pva_rs::client::PvaClient;
use epics_pva_rs::pv_request::PvRequestExpr;
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure};
use epics_pva_rs::{cli, format};

/// Mirror of legacy `pvput` flag set (pvAccessCPP `pvput.cpp`).
#[derive(Parser)]
#[command(
    name = "pvput-rs",
    about = "Write a value to an EPICS PV via pvAccess",
    disable_version_flag = true
)]
struct Args {
    /// Print version information (pvxs `version_information`, tools
    /// `case 'V'`) and exit. Routed through `cli::version_information`
    /// so every PVA CLI reports the same library + protocol stack
    /// instead of clap's crate-only `<binary> <version>`.
    #[arg(short = 'V', long = "version")]
    version: bool,

    /// pvRequest string (`field(...)` / `record[k=v]` syntax). When
    /// non-empty it drives the PUT operation — e.g. `-r 'record[process=true]'`
    /// requests a synchronous PROC after the write.
    #[arg(short = 'r', default_value = "")]
    request: String,

    /// CA-style operation timeout in seconds.
    #[arg(short = 'w', default_value = "5.0")]
    timeout: f64,

    /// Provider name. pvAccessCPP defaults this to `pva`
    /// (`pvtoolsSrc/pvutils.cpp:32`); only the native PVA provider is
    /// supported here, so a non-`pva` value is rejected rather than
    /// silently performing a PVA write.
    #[arg(short = 'p', long = "provider", default_value = "pva")]
    provider: String,

    /// Output mode: `nt` (default), `raw`, or `json`. Selects how the
    /// `--read-back` Old:/New: echo renders values, mirroring
    /// `pvget-rs`. The full structure is shown with `-M raw` (pvxs
    /// reserves `-v` for effective-config diagnostics, not formatting).
    #[arg(short = 'M', long = "mode", default_value = "nt")]
    mode: String,

    /// Verbose ("make more noise"): print the effective PVA client
    /// configuration before the PUT. pvxs `tools/put.cpp:56-58,109-110`
    /// sets `verbose=true` and prints `Effective config` + the client
    /// context config; it does NOT change the value formatter (that is
    /// `-M`). pvxs additionally prints a `Writing fields:` delta of the
    /// PUT prototype (put.cpp:129) — not reproduced here because the
    /// prototype is built inside the client, not surfaced to the CLI.
    #[arg(short = 'v', action = clap::ArgAction::Count)]
    verbose: u8,

    /// Quiet mode — print only error messages.
    #[arg(short = 'q')]
    quiet: bool,

    /// Read the value before and after the PUT and print `Old :` /
    /// `New :` echo lines. Rust-specific and off by default: pvxs
    /// `pvxput` issues only the PUT and never reads back
    /// (`tools/put.cpp:115-158`), so the default command does not GET a
    /// write-only or read-restricted PV.
    #[arg(long = "read-back")]
    read_back: bool,

    /// Raise the `epics_pva_rs` library log level to DEBUG. Mirrors pvxs
    /// `pvxput -d` mapping to `logger_level_set("pvxs.*", Level::Debug)`
    /// (`tools/put.cpp:41-64`).
    #[arg(short = 'd')]
    debug: bool,

    /// PV name to write to. Not clap-required: C makes the check itself
    /// and returns 1 with its own message (pvput.cpp:363-367), where a
    /// clap-enforced argument would exit 2.
    pv_name: Option<String>,

    /// Value(s) to write. Legacy pvput accepts:
    ///   `pvput <PV> <value>`
    ///   `pvput <PV> <size/ignored> <value> [<value> ...]`
    ///   `pvput <PV> <field>=<value> ...`
    ///   `pvput <PV> <json>`
    /// EVERY token is forwarded raw and classified against the server
    /// PUT prototype (pvAccessCPP `pvput.cpp:109-235`): a `field=value`
    /// token is a field assignment only if `field` exists in the
    /// prototype, otherwise it is a bare string value (when `.value` is
    /// a string) or warned-and-ignored. A scalar-array `.value` drops
    /// the leading `<size/ignored>` token (a lone `[...]` token is the
    /// JSON-array shortcut); a scalar `.value` takes exactly one token.
    /// The CLI makes no field-vs-bare guess before contacting the server.
    #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
    values: Vec<String>,
}

#[tokio::main]
async fn main() {
    let args: Args = cli::parse_or_exit();

    // pvxs `-V` prints version_information and exits before any client
    // setup (tools/put.cpp:55).
    if args.version {
        print!("{}", cli::version_information());
        return;
    }

    // Install the shared tracing subscriber (honours EPICS_PVA_LOG /
    // RUST_LOG) and apply `-d` as a DEBUG bump of the library namespace.
    // pvxs runs `logger_config_env()` + maps `-d` to a debug level set
    // before the PUT (tools/put.cpp:41-64). This replaces the prior
    // no-op `-d` that silently discarded the flag.
    epics_pva_rs::log::install_cli_logging(args.debug);

    // Only the native PVA provider is implemented. pvAccessCPP would
    // route `-p ca` through its CA provider (pvput.cpp:368-399); we
    // cannot, so reject it instead of silently writing over PVA.
    if let Err(e) = check_provider(&args.provider) {
        eprintln!("pvput-rs: {e}");
        std::process::exit(1);
    }

    // pvxs `-v` prints the effective client config once, before the PUT
    // (tools/put.cpp:109-110). It is a diagnostic, not an output-format
    // switch — the readback echo formatter stays under `-M`.
    if args.verbose > 0 {
        cli::print_effective_config();
    }

    // C's own two argument checks, in C's order and wording
    // (pvput.cpp:363-377). Both return 1; clap would have exited 2.
    let Some(pv_name) = args.pv_name else {
        eprintln!("No pv name specified. ('pvput -h' for help.)");
        std::process::exit(1);
    };

    if args.values.is_empty() {
        eprintln!("No value(s) specified. ('pvput -h' for help.)");
        std::process::exit(1);
    }

    // Build the client with the parsed `-w` as the operation timeout,
    // applied once at construction so the optional readback GET, the PUT
    // wait, and the post-PUT GET all use the same owner. pvxs `pvxput`
    // waits for completion with `done.wait(timeout)` (tools/put.cpp:64,153),
    // and `epicsEvent::wait(0)` is `tryWait()` — an immediate poll
    // (epicsEvent.h:101-107). Route `-w` through `wait_timeout_duration`
    // so `-w 0` maps to an immediate (zero) timeout rather than the generic
    // `timeout_duration` 5 s clamp.
    let client = PvaClient::builder()
        .timeout(epics_pva_rs::cli::wait_timeout_duration(args.timeout))
        .build();

    // Parse the optional pvRequest once (shared by both put forms) with
    // the strict pvxs grammar, so acceptance matches pvxs's own `pvput`:
    // `tools/put.cpp:115` routes `-r` through `RequestBuilder::pvRequest()`
    // (strict `PVRParser`, `src/clientreq.cpp:137-283`), which rejects the
    // lenient pvDataCPP `createRequest` extensions the bare `parse` allows.
    let trimmed = args.request.trim();
    let request = if trimmed.is_empty() {
        None
    } else {
        match PvRequestExpr::parse_pvxs_compat(trimmed) {
            Ok(req) => Some(req),
            Err(e) => {
                eprintln!("error: invalid pvRequest {:?}: {e}", args.request);
                std::process::exit(1);
            }
        }
    };

    // pvxs `pvxput` issues a single PUT and prints nothing on success
    // (tools/put.cpp:115-158): no pre- or post-PUT GET. The Old:/New:
    // readback below is a Rust-only convenience gated behind
    // `--read-back`, so the default command matches pvxs and never
    // GETs a write-only PV.
    //
    // 1. Optional pre-PUT read for the `Old :` echo line. Errors are
    //    tolerated — the echo prints an "Old : ***" line and continues.
    let old_get = if args.read_back {
        Some(client.pvget_full(&pv_name).await)
    } else {
        None
    };

    // 2. Do the put. ALL value tokens are forwarded raw; the client
    //    classifies them against the server PUT prototype (field=value
    //    vs bare string, unknown-field warn, array length-drop) so the
    //    CLI makes no guess before the structure is known. pvAccessCPP
    //    pvput.cpp:109-235.
    let put_result = client
        .pvput_args(&pv_name, &args.values, request.as_ref())
        .await;
    if let Err(e) = put_result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }

    // pvxs-faithful default: the PUT is the whole command. Without
    // `--read-back` there is no post-PUT GET and success is silent.
    if !args.read_back {
        return;
    }

    // 3. Post-PUT read for the `New :` echo line.
    let new_get = client.pvget_full(&pv_name).await;

    // The old and new readback values render through the same
    // formatter, selected by `-M` (mirrors `pvget-rs`). `-v` is the
    // effective-config diagnostic (pvxs put.cpp:109-110), not an
    // output-format switch, so the full structure is requested with
    // `-M raw`, not `-v`.
    let mode = args.mode.as_str();
    let render = |label: &str, value: &PvField, desc: &FieldDesc| -> String {
        match mode {
            "json" => format::format_json(label, value, None),
            "raw" => format::format_raw(label, desc, value, None),
            // Default `-M nt`: legacy compact `<label><ts>  <val>` shape.
            _ => match value {
                PvField::Structure(s) => format_old_new_line(label, s),
                _ => format!("{label}(non-NT value)\n"),
            },
        }
    };

    // pvAccessCPP `pvput -q` (pvput.cpp:403-428) suppresses the `Old :`
    // line and the `New :` label but still prints the post-PUT value.
    // So quiet drops the old echo entirely and prints the new value
    // with an empty label, while a non-quiet run keeps both labels.
    if !args.quiet
        && let Some(old_get) = &old_get
    {
        match old_get {
            Ok(r) => print!("{}", render("Old : ", &r.value, &r.introspection)),
            Err(e) => println!("Old : *** {e}"),
        }
    }
    let new_label = new_echo_label(args.quiet);
    match &new_get {
        Ok(r) => print!("{}", render(new_label, &r.value, &r.introspection)),
        // A readback GET failure is not the PUT result (already checked);
        // report it to stderr so quiet's stdout stays value-only.
        Err(e) if args.quiet => eprintln!("pvput-rs: readback failed: {e}"),
        Err(e) => println!("New : *** {e}"),
    }
}

/// Validate the requested provider. Only the native PVA provider is
/// supported; pvAccessCPP's `-p ca` CA-provider path (pvput.cpp:368-399)
/// is not implemented, so it is rejected rather than silently dropped.
fn check_provider(provider: &str) -> Result<(), String> {
    if provider == "pva" {
        Ok(())
    } else {
        Err(format!(
            "provider {provider:?} not supported (only \"pva\")"
        ))
    }
}

/// Label for the post-PUT `New :` echo line. pvAccessCPP `pvput -q`
/// (pvput.cpp:403-428) drops the `New :` label but still prints the
/// value, so quiet mode uses an empty label.
fn new_echo_label(quiet: bool) -> &'static str {
    if quiet { "" } else { "New : " }
}

/// Render an `Old :` / `New :` echo line in legacy `pvput.cpp` shape:
///   `<label><timestamp>  <value> \n`
/// where `<label>` already carries the `: ` and `<value>` is rendered
/// via the same NT-scalar formatter as `pvget -M nt`. Mirrors the
/// hexdump of `pvput`'s output (`Old : 2026-... 0 \n`).
fn format_old_new_line(label: &str, s: &PvStructure) -> String {
    format!("{label}{}", format::format_nt_old_new_payload(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `pvput-rs -w 0` is an immediate completion poll: pvxs `pvxput`
    /// waits with `done.wait(timeout)` and `epicsEvent::wait(0)` is
    /// `tryWait()` (tools/put.cpp:153, epicsEvent.h:101-107), so a
    /// non-positive `-w` maps to `Duration::ZERO`, NOT the prior 5 s clamp.
    #[test]
    fn w_zero_is_immediate_timeout() {
        let args = Args::parse_from(["pvput-rs", "-w", "0", "PV", "42"]);
        assert_eq!(
            epics_pva_rs::cli::wait_timeout_duration(args.timeout),
            std::time::Duration::ZERO
        );
    }

    /// A finite, strictly-positive `-w` is preserved as a bounded timeout.
    #[test]
    fn w_positive_is_finite_timeout() {
        let args = Args::parse_from(["pvput-rs", "-w", "2.5", "PV", "42"]);
        assert_eq!(
            epics_pva_rs::cli::wait_timeout_duration(args.timeout),
            std::time::Duration::from_secs_f64(2.5)
        );
    }

    // pvput token classification (field=value vs bare, unknown-field
    // warn, mix rejection) is no longer guessed at the CLI — it is
    // deferred to the server PUT prototype. Those contract tests now
    // live next to the classifier `build_put_from_args` in
    // `client_native::ops_v2` (pvAccessCPP pvput.cpp:109-235), where the
    // prototype is available; the CLI just forwards `args.values` raw.

    #[test]
    fn pva_provider_is_accepted() {
        assert!(check_provider("pva").is_ok());
    }

    #[test]
    fn non_pva_provider_is_rejected() {
        // pvxs has no provider switch; pvAccessCPP `-p ca` is unsupported.
        assert!(check_provider("ca").is_err());
        assert!(check_provider("").is_err());
    }

    /// pvxs `pvxput` issues only the PUT — no readback. The default
    /// command must not read back, so a write-only PV is never GET'd.
    #[test]
    fn readback_is_off_by_default() {
        let args = Args::parse_from(["pvput-rs", "PV", "42"]);
        assert!(!args.read_back);
    }

    /// `--read-back` opts in to the Rust-only Old:/New: echo.
    #[test]
    fn read_back_flag_enables_readback() {
        let args = Args::parse_from(["pvput-rs", "--read-back", "PV", "42"]);
        assert!(args.read_back);
    }

    /// pvAccessCPP `pvput -q` drops the `New :` label but still prints
    /// the value, so quiet uses an empty label and non-quiet keeps it.
    #[test]
    fn quiet_suppresses_new_label_only() {
        assert_eq!(new_echo_label(false), "New : ");
        assert_eq!(new_echo_label(true), "");
    }

    /// `-v` is the effective-config diagnostic, decoupled from output
    /// formatting: it must NOT force the readback echo into raw mode.
    /// The full structure is requested with `-M raw` instead (pvxs
    /// reserves `-v` for `Effective config`, put.cpp:109-110).
    #[test]
    fn verbose_does_not_change_output_mode() {
        let v = Args::parse_from(["pvput-rs", "-v", "PV", "42"]);
        assert!(v.verbose > 0, "-v sets the verbose flag");
        assert_eq!(v.mode, "nt", "-v must not change -M from its default");

        let raw = Args::parse_from(["pvput-rs", "-M", "raw", "PV", "42"]);
        assert_eq!(raw.mode, "raw", "-M raw selects the raw echo formatter");
        assert_eq!(raw.verbose, 0, "-M raw does not imply verbose");
    }

    /// `-d` is no longer a no-op: it parses into a real flag wired to
    /// `install_cli_logging` (pvxs `pvxput -d`, put.cpp:41-64). Default
    /// off so ordinary puts stay quiet.
    #[test]
    fn debug_flag_parses() {
        assert!(Args::parse_from(["pvput-rs", "-d", "PV", "42"]).debug);
        assert!(!Args::parse_from(["pvput-rs", "PV", "42"]).debug);
    }
}
