use clap::Parser;
use epics_pva_rs::client::PvaClient;
use epics_pva_rs::format;
use epics_pva_rs::pvdata::{PvField, PvStructure};

const VERSION_INFO: &str = concat!("pvput-rs ", env!("CARGO_PKG_VERSION"));

/// Mirror of legacy `pvput` flag set (pvAccessCPP `pvput.cpp`).
#[derive(Parser)]
#[command(
    name = "pvput-rs",
    about = "Write a value to an EPICS PV via pvAccess",
    disable_version_flag = true
)]
struct Args {
    #[arg(short = 'V', long, hide = true)]
    version: bool,

    /// pvRequest string (`field(...)` syntax). Currently accepted for
    /// parity; the request is delegated to the channel default.
    #[arg(short = 'r', default_value = "")]
    request: String,

    /// CA-style operation timeout in seconds.
    #[arg(short = 'w', default_value = "5.0")]
    timeout: f64,

    /// Default provider name (parity-only; we always speak PVA).
    #[arg(short = 'p', long = "provider", default_value = "pva")]
    provider: String,

    /// Output mode: `nt` (default), `raw`, or `json`. Currently the
    /// post-put echo always uses the legacy NT shape.
    #[arg(short = 'M', long = "mode", default_value = "nt")]
    mode: String,

    /// Show entire structure in raw mode (legacy `-v` shorthand).
    #[arg(short = 'v', action = clap::ArgAction::Count)]
    verbose: u8,

    /// Quiet mode — print only error messages.
    #[arg(short = 'q')]
    quiet: bool,

    /// Enable debug log output (currently a no-op; kept for parity).
    #[arg(short = 'd')]
    debug: bool,

    /// PV name to write to.
    #[arg(required_unless_present_any = ["version"])]
    pv_name: Option<String>,

    /// Value(s) to write. Legacy pvput accepts:
    ///   `pvput <PV> <value>`
    ///   `pvput <PV> <size/ignored> <value> [<value> ...]`
    ///   `pvput <PV> <field>=<value> ...`
    ///   `pvput <PV> <json>`
    /// Today the Rust client supports the first and third forms via
    /// `pvput`/`pvput_with_field_request`.
    #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
    values: Vec<String>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    if args.version {
        println!("{VERSION_INFO}");
        return;
    }

    let pv_name = args.pv_name.expect("clap enforces required");

    if args.values.is_empty() {
        eprintln!("pvput-rs: missing value");
        std::process::exit(1);
    }

    // Legacy pvput joins multiple value tokens with single spaces when
    // they aren't `field=value` pairs. We follow the same convention.
    let value_str = args.values.join(" ");

    let client = PvaClient::new().expect("failed to create PVA client");

    // 1. Read old NTScalar (with timestamp) for the `Old :` echo line.
    //    Errors are tolerated — legacy pvput emits an "Error:" line in
    //    the gap and continues to the put.
    let old_get = client.pvget_full(&pv_name).await;

    // 2. Do the put.
    if let Err(e) = client.pvput(&pv_name, &value_str).await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }

    // 3. Read new NTScalar (with timestamp) for the `New :` echo line.
    let new_get = client.pvget_full(&pv_name).await;

    if args.quiet {
        return;
    }

    let echo_line = |label: &str, value: &PvField| -> Option<String> {
        let s = match value {
            PvField::Structure(s) => s,
            _ => return None,
        };
        Some(format_old_new_line(label, s))
    };

    match &old_get {
        Ok(r) => match echo_line("Old : ", &r.value) {
            Some(line) => print!("{line}"),
            None => println!("Old : (non-NT value)"),
        },
        Err(e) => println!("Old : *** {e}"),
    }
    match &new_get {
        Ok(r) => match echo_line("New : ", &r.value) {
            Some(line) => print!("{line}"),
            None => println!("New : (non-NT value)"),
        },
        Err(e) => println!("New : *** {e}"),
    }

    // Suppress unused-warning for parity-only flags.
    let _ = (
        args.request,
        args.provider,
        args.mode,
        args.verbose,
        args.debug,
    );
}

/// Render an `Old :` / `New :` echo line in legacy `pvput.cpp` shape:
///   `<label><timestamp>  <value> \n`
/// where `<label>` already carries the `: ` and `<value>` is rendered
/// via the same NT-scalar formatter as `pvget -M nt`. Mirrors the
/// hexdump of `pvput`'s output (`Old : 2026-... 0 \n`).
fn format_old_new_line(label: &str, s: &PvStructure) -> String {
    format!("{label}{}", format::format_nt_old_new_payload(s))
}
