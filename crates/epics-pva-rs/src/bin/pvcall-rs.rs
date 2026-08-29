//! `pvcall-rs` — RPC client CLI mirroring pvxs `pvcall`
//! (`tools/call.cpp`).
//!
//! ```text
//! pvcall-rs <pvname> [field=value]...
//! ```
//!
//! Builds an NTURI-shaped RPC request whose `query` substructure
//! carries the supplied `field=value` pairs (typed as strings, since
//! the CLI doesn't know the server's schema), submits it via
//! `PvaClient::pvrpc`, and prints the response.
//!
//! Every bare `field=value` token is sent as a PVA string, matching
//! pvxs `tools/call.cpp:104-118` (each query member is declared
//! `String(key)` and assigned the raw token text). The CLI does not
//! infer numeric types: `gain=2` and `gain=2.5` are both sent as the
//! strings `"2"` / `"2.5"`, leaving the receiving RPC service to
//! coerce per its own schema.

use clap::Parser;
use epics_pva_rs::client::PvaClient;
use epics_pva_rs::pvdata::{FieldDesc, PvField, ScalarValue};

#[derive(Parser)]
#[command(
    name = "pvcall-rs",
    about = "Call an EPICS pvAccess RPC method",
    disable_version_flag = true
)]
struct Args {
    /// Print version information (pvxs `version_information`, tools
    /// `case 'V'`) and exit. Routed through `cli::version_information`
    /// so every PVA CLI reports the same library + protocol stack
    /// instead of clap's crate-only `<binary> <version>`.
    #[arg(short = 'V', long = "version")]
    version: bool,

    /// PV name of the RPC endpoint. Not clap-required: C makes the check
    /// itself and returns 1 (pvcall.cpp:172-174), where a clap-enforced
    /// argument would exit 2.
    pv_name: Option<String>,

    /// `field=value` arguments. Repeat for multiple args.
    /// Values are sent verbatim as PVA strings (pvxs CLI semantics).
    args: Vec<String>,

    /// Wait time in seconds for the RPC to complete
    #[arg(short = 'w', default_value = "5.0")]
    timeout: f64,

    /// Output mode: nt (NTURI-aware), raw, json
    #[arg(short = 'M', default_value = "nt")]
    mode: String,

    /// Verbose ("make more noise"): print the effective PVA client
    /// configuration before issuing the RPC. pvxs
    /// `tools/call.cpp:56-58,122-123` sets `verbose=true` and prints
    /// `Effective config` + the client context config. pvxs accepts
    /// `-v`; the prior Rust CLI rejected it outright.
    #[arg(short = 'v', action = clap::ArgAction::Count)]
    verbose: u8,

    /// Raise the `epics_pva_rs` library log level to DEBUG. Mirrors pvxs
    /// `pvxcall -d` mapping to `logger_level_set("pvxs.*",
    /// Level::Debug)` (`tools/call.cpp:47-64`).
    #[arg(short = 'd')]
    debug: bool,
}

/// Parse a `key=value` pair into a string-valued [`ScalarValue`]. pvxs
/// `tools/call.cpp:86-118` splits on the first `=`, stores the value
/// as a `std::string`, and declares each query member as `String(key)`
/// — bare numeric-looking tokens are sent as strings, never coerced to
/// PVA integers/doubles. The receiving RPC service coerces per its own
/// schema; the CLI must not guess the type before that schema is known.
fn parse_arg(arg: &str) -> Result<(String, ScalarValue), String> {
    let (k, v) = arg
        .split_once('=')
        .ok_or_else(|| format!("expected key=value, got {arg:?}"))?;
    Ok((k.to_string(), ScalarValue::String(v.to_string().into())))
}

/// Parse all `field=value` tokens, rejecting a duplicate field name the
/// way pvxs does: `tools/call.cpp:93-101` stores arguments in a
/// `std::map` and errors out on a repeated key before building the
/// NTURI `query`, so a duplicate has no stable meaning and is refused
/// locally rather than sent. Insertion order is preserved for the
/// `query` members (matching pvxs's `keys` list).
fn collect_args(tokens: &[String]) -> Result<Vec<(String, ScalarValue)>, String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(tokens.len());
    for tok in tokens {
        let (k, v) = parse_arg(tok)?;
        if !seen.insert(k.clone()) {
            return Err(format!("duplicate argument name {k:?}"));
        }
        out.push((k, v));
    }
    Ok(out)
}

/// Build an NTURI-shaped pvRequest carrying the parsed args under
/// `query.<key>`, with `scheme="pva"` and `path=<pv_name>`.
///
/// Delegates to the shared [`epics_pva_rs::nt::NTURI::request`] builder
/// so the request carries **all four** normative members — `scheme`,
/// `authority`, `path`, `query` — that pvxs `NTURI::NTURI()` defines
/// (`src/nt.cpp:253-263`). The previous hand-rolled descriptor omitted
/// `authority`, so a strict NTURI receiver keying off
/// `struct_id="epics:nt/NTURI:1.0"` saw a non-pvxs shape.
fn build_nturi(pv_name: &str, args: &[(String, ScalarValue)]) -> (FieldDesc, PvField) {
    epics_pva_rs::nt::NTURI::request("pva", pv_name, args)
}

#[tokio::main]
async fn main() {
    let args: Args = epics_pva_rs::cli::parse_or_exit();

    // pvxs `-V` prints version_information and exits before any client
    // setup (tools/call.cpp:55).
    if args.version {
        print!("{}", epics_pva_rs::cli::version_information());
        return;
    }

    // Install the shared tracing subscriber (honours EPICS_PVA_LOG /
    // RUST_LOG) and apply `-d` as a DEBUG bump of the library namespace,
    // mirroring pvxs `logger_config_env()` + `-d` (tools/call.cpp:47-64).
    epics_pva_rs::log::install_cli_logging(args.debug);

    // pvcall.cpp:172-174 prints the pvput wording, `pvput -h` and all;
    // this is a transcription, not a copy-paste slip.
    let Some(pv_name) = args.pv_name else {
        eprintln!("No pv name specified. ('pvput -h' for help.)");
        std::process::exit(1);
    };

    let parsed_args: Vec<(String, ScalarValue)> = match collect_args(&args.args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("pvcall-rs: {e}");
            std::process::exit(2);
        }
    };

    // pvxs `-v` prints the effective client config once, before the
    // RPC is issued (tools/call.cpp:122-123).
    if args.verbose > 0 {
        epics_pva_rs::cli::print_effective_config();
    }

    let (desc, value) = build_nturi(&pv_name, &parsed_args);

    // pvxs `pvxcall -w 0` is an immediate completion poll (epicsEvent
    // `tryWait`, `epicsEvent.h:101-107`), not a 5 s wait. Route `-w`
    // through `wait_timeout_duration` so a non-positive value maps to an
    // immediate (zero) operation timeout rather than the generic
    // `timeout_duration` 5 s clamp (tools/call.cpp:44-65,150-155).
    let client = PvaClient::builder()
        .timeout(epics_pva_rs::cli::wait_timeout_duration(args.timeout))
        .build();

    match client.pvrpc(&pv_name, &desc, &value).await {
        // pvxs `pvxcall` prints the reply only when it carries a Value
        // (`if(val) std::cout<<val;`, tools/call.cpp:132-133); a no-value
        // reply prints nothing and still exits 0.
        Ok(reply) => {
            if let Some((resp_desc, resp_value)) = reply.into_value() {
                match args.mode.as_str() {
                    "json" => {
                        let s = epics_pva_rs::format::format_json(&pv_name, &resp_value, None);
                        println!("{s}");
                    }
                    "raw" => {
                        let s = epics_pva_rs::format::format_raw(
                            &pv_name,
                            &resp_desc,
                            &resp_value,
                            None,
                        );
                        println!("{s}");
                    }
                    _ => {
                        let s = epics_pva_rs::format::format_nt(&pv_name, &resp_desc, &resp_value);
                        println!("{s}");
                    }
                }
            }
        }
        Err(e) => {
            eprintln!("pvcall-rs: RPC failed: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epics_pva_rs::pvdata::ScalarType;

    /// pvxs `tools/call.cpp:104-118` sends every bare token as a PVA
    /// string; integer- and float-looking values must not be coerced.
    #[test]
    fn bare_values_are_strings() {
        for (raw, key, want) in [
            ("gain=2", "gain", "2"),
            ("gain=2.5", "gain", "2.5"),
            ("name=hello", "name", "hello"),
            ("flag=true", "flag", "true"),
            ("neg=-7", "neg", "-7"),
        ] {
            let (k, v) = parse_arg(raw).expect("valid key=value");
            assert_eq!(k, key);
            assert_eq!(
                v,
                ScalarValue::String(want.to_string().into()),
                "for {raw:?}"
            );
        }
    }

    /// The built NTURI advertises all four normative members, including
    /// `authority` (pvxs `NTURI::NTURI()`, `src/nt.cpp:253-263`). The
    /// pre-fix hand-rolled descriptor omitted `authority`.
    #[test]
    fn nturi_descriptor_includes_authority() {
        let args = [(
            "op".to_string(),
            ScalarValue::String("x".to_string().into()),
        )];
        let (desc, value) = build_nturi("svc", &args);
        let FieldDesc::Structure { struct_id, fields } = &desc else {
            panic!("expected NTURI structure descriptor");
        };
        assert_eq!(struct_id, "epics:nt/NTURI:1.0");
        let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["scheme", "authority", "path", "query"]);
        let PvField::Structure(root) = &value else {
            panic!("expected NTURI structure value");
        };
        assert!(
            root.get_field("authority").is_some(),
            "value must carry the authority member"
        );
    }

    /// The NTURI query descriptor declares each member as a string,
    /// regardless of how the value text looks.
    #[test]
    fn nturi_query_members_are_string_typed() {
        let args = [
            (
                "gain".to_string(),
                ScalarValue::String("2".to_string().into()),
            ),
            (
                "rate".to_string(),
                ScalarValue::String("2.5".to_string().into()),
            ),
        ];
        let (desc, _value) = build_nturi("svc", &args);
        let FieldDesc::Structure { fields, .. } = desc else {
            panic!("expected NTURI structure descriptor");
        };
        let query = fields
            .iter()
            .find(|(n, _)| n == "query")
            .map(|(_, d)| d)
            .expect("query substructure");
        let FieldDesc::Structure { fields: q, .. } = query else {
            panic!("expected query substructure");
        };
        for (name, fd) in q {
            assert_eq!(
                fd,
                &FieldDesc::Scalar(ScalarType::String),
                "query member {name:?} must be string-typed"
            );
        }
    }

    /// A token without `=` is a usage error, mirroring pvxs
    /// `tools/call.cpp:88-91`.
    #[test]
    fn missing_equals_is_error() {
        assert!(parse_arg("novalue").is_err());
    }

    /// pvxs `tools/call.cpp:93-101` rejects a repeated field name; so
    /// must we, before building an ambiguous duplicate `query` member.
    #[test]
    fn duplicate_field_name_is_rejected() {
        let toks = vec!["a=1".to_string(), "a=2".to_string()];
        let err = collect_args(&toks).unwrap_err();
        assert!(err.contains("duplicate argument name"), "got: {err}");
    }

    /// Distinct field names are accepted in order.
    #[test]
    fn distinct_field_names_preserved_in_order() {
        let toks = vec!["a=1".to_string(), "b=2".to_string()];
        let got = collect_args(&toks).unwrap();
        assert_eq!(got[0].0, "a");
        assert_eq!(got[1].0, "b");
    }

    /// `pvcall-rs -w 0` is an immediate completion poll (pvxs `pvxcall`
    /// inherits epicsEvent `tryWait`, `tools/call.cpp:150-155`,
    /// `epicsEvent.h:101-107`): the parsed `-w 0` maps to a zero
    /// operation timeout, NOT the prior 5 s clamp.
    #[test]
    fn w_zero_is_immediate_timeout() {
        let args = Args::parse_from(["pvcall-rs", "-w", "0", "svc"]);
        assert_eq!(
            epics_pva_rs::cli::wait_timeout_duration(args.timeout),
            std::time::Duration::ZERO
        );
    }

    /// A positive `-w` is preserved as a bounded RPC timeout.
    #[test]
    fn w_positive_is_finite_timeout() {
        let args = Args::parse_from(["pvcall-rs", "-w", "2.5", "svc"]);
        assert_eq!(
            epics_pva_rs::cli::wait_timeout_duration(args.timeout),
            std::time::Duration::from_secs_f64(2.5)
        );
    }

    /// `-d` parses into a real flag wired to `install_cli_logging`
    /// (pvxs `pvxcall -d`, call.cpp:47-64). Default off.
    #[test]
    fn debug_flag_parses() {
        assert!(Args::parse_from(["pvcall-rs", "-d", "svc"]).debug);
        assert!(!Args::parse_from(["pvcall-rs", "svc"]).debug);
    }
}
