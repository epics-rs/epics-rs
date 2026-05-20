use clap::Parser;
use epics_pva_rs::client::PvaClient;
use epics_pva_rs::format;
use epics_pva_rs::pv_request::PvRequestExpr;
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

    /// pvRequest string (`field(...)` / `record[k=v]` syntax). When
    /// non-empty it drives the PUT operation — e.g. `-r 'record[process=true]'`
    /// requests a synchronous PROC after the write.
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

    // pvxs `pvxput` (tools/put.cpp:83-104): a single bare token is
    // shorthand for `value=<token>`; otherwise every token must be
    // `<field>=<value>`. Mixed bare/field input is rejected.
    let input = match parse_put_args(&args.values) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    let client = PvaClient::new().expect("failed to create PVA client");

    // Parse the optional pvRequest once (shared by both put forms).
    let trimmed = args.request.trim();
    let request = if trimmed.is_empty() {
        None
    } else {
        match PvRequestExpr::parse(trimmed) {
            Ok(req) => Some(req),
            Err(e) => {
                eprintln!("error: invalid pvRequest {:?}: {e}", args.request);
                std::process::exit(1);
            }
        }
    };

    // 1. Read old NTScalar (with timestamp) for the `Old :` echo line.
    //    Errors are tolerated — legacy pvput emits an "Error:" line in
    //    the gap and continues to the put.
    let old_get = client.pvget_full(&pv_name).await;

    // 2. Do the put. Field assignments build one prototype-based delta;
    //    a bare value targets `.value` via the existing helpers.
    let put_result = match &input {
        PutInput::Fields(assignments) => {
            client
                .pvput_fields(&pv_name, assignments, request.as_ref())
                .await
        }
        PutInput::Bare(value_str) => match &request {
            None => client.pvput(&pv_name, value_str).await,
            Some(req) => client.pvput_with_request(&pv_name, req, value_str).await,
        },
    };
    if let Err(e) = put_result {
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
    let _ = (args.provider, args.mode, args.verbose, args.debug);
}

/// Parsed CLI value tokens. pvxs `pvxput` accepts either one bare
/// value (→ `value=<token>`) or one-or-more `<field>=<value>` pairs;
/// the two forms must not be mixed.
#[derive(Debug)]
enum PutInput {
    /// Single bare value (possibly space-joined array tokens), written
    /// to `.value`.
    Bare(String),
    /// One or more `field=value` assignments, written as a delta.
    Fields(Vec<(String, String)>),
}

/// Classify the value tokens. A token containing `=` switches the whole
/// invocation into field-assignment mode (mirrors pvxs put.cpp:83-104):
/// then *every* token must be `<field>=<value>`, else it is an error.
fn parse_put_args(values: &[String]) -> Result<PutInput, String> {
    if !values.iter().any(|v| v.contains('=')) {
        // No assignment syntax — legacy bare/array value joined by space.
        return Ok(PutInput::Bare(values.join(" ")));
    }
    let mut assignments = Vec::with_capacity(values.len());
    for tok in values {
        match tok.split_once('=') {
            Some((field, value)) if !field.is_empty() => {
                assignments.push((field.to_string(), value.to_string()));
            }
            _ => {
                return Err(format!(
                    "expected <field>=<value>, got {tok:?} (do not mix bare and field assignments)"
                ));
            }
        }
    }
    Ok(PutInput::Fields(assignments))
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

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn bare_single_value_maps_to_value() {
        match parse_put_args(&v(&["42"])).unwrap() {
            PutInput::Bare(s) => assert_eq!(s, "42"),
            PutInput::Fields(_) => panic!("single bare token must be Bare"),
        }
    }

    #[test]
    fn bare_array_tokens_join_with_space() {
        match parse_put_args(&v(&["1", "2", "3"])).unwrap() {
            PutInput::Bare(s) => assert_eq!(s, "1 2 3"),
            PutInput::Fields(_) => panic!("no '=' tokens must be Bare"),
        }
    }

    #[test]
    fn field_assignments_parse_each_pair() {
        match parse_put_args(&v(&["alarm.severity=2", "timeStamp.nanoseconds=5"])).unwrap() {
            PutInput::Fields(f) => {
                assert_eq!(
                    f,
                    vec![
                        ("alarm.severity".to_string(), "2".to_string()),
                        ("timeStamp.nanoseconds".to_string(), "5".to_string()),
                    ]
                );
            }
            PutInput::Bare(_) => panic!("'=' tokens must be Fields"),
        }
    }

    #[test]
    fn mixed_bare_and_field_is_rejected() {
        let err = parse_put_args(&v(&["42", "alarm.severity=2"])).unwrap_err();
        assert!(err.contains("expected <field>=<value>"), "got: {err}");
    }
}
