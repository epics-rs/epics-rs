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
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};

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

    /// PV name of the RPC endpoint
    #[arg(required_unless_present = "version")]
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
    Ok((k.to_string(), ScalarValue::String(v.to_string())))
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
/// `query.<key>`. pvxs's RPC server side accepts this shape — see
/// `pvxs/src/pvxs/nt.h` `NTURI`.
fn build_nturi(pv_name: &str, args: &[(String, ScalarValue)]) -> (FieldDesc, PvField) {
    let query_fields: Vec<(String, FieldDesc)> = args
        .iter()
        .map(|(k, v)| (k.clone(), FieldDesc::Scalar(v.scalar_type())))
        .collect();
    let desc = FieldDesc::Structure {
        struct_id: "epics:nt/NTURI:1.0".into(),
        fields: vec![
            ("scheme".into(), FieldDesc::Scalar(ScalarType::String)),
            ("path".into(), FieldDesc::Scalar(ScalarType::String)),
            (
                "query".into(),
                FieldDesc::Structure {
                    struct_id: String::new(),
                    fields: query_fields.clone(),
                },
            ),
        ],
    };
    let mut top = PvStructure::new("epics:nt/NTURI:1.0");
    top.fields.push((
        "scheme".into(),
        PvField::Scalar(ScalarValue::String("pva".into())),
    ));
    top.fields.push((
        "path".into(),
        PvField::Scalar(ScalarValue::String(pv_name.into())),
    ));
    let mut query = PvStructure::new("");
    for (k, v) in args {
        query.fields.push((k.clone(), PvField::Scalar(v.clone())));
    }
    top.fields.push(("query".into(), PvField::Structure(query)));
    (desc, PvField::Structure(top))
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // pvxs `-V` prints version_information and exits before any client
    // setup (tools/call.cpp:55).
    if args.version {
        print!("{}", epics_pva_rs::cli::version_information());
        return;
    }

    let pv_name = args
        .pv_name
        .expect("clap enforces required unless --version");

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

    let client = PvaClient::builder()
        .timeout(epics_pva_rs::cli::timeout_duration(args.timeout))
        .build();

    match client.pvrpc(&pv_name, &desc, &value).await {
        Ok((resp_desc, resp_value)) => match args.mode.as_str() {
            "json" => {
                let s = epics_pva_rs::format::format_json(&pv_name, &resp_value);
                println!("{s}");
            }
            "raw" => {
                let s = epics_pva_rs::format::format_raw(&pv_name, &resp_desc, &resp_value);
                println!("{s}");
            }
            _ => {
                let s = epics_pva_rs::format::format_nt(&pv_name, &resp_desc, &resp_value);
                println!("{s}");
            }
        },
        Err(e) => {
            eprintln!("pvcall-rs: RPC failed: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            assert_eq!(v, ScalarValue::String(want.to_string()), "for {raw:?}");
        }
    }

    /// The NTURI query descriptor declares each member as a string,
    /// regardless of how the value text looks.
    #[test]
    fn nturi_query_members_are_string_typed() {
        let args = [
            ("gain".to_string(), ScalarValue::String("2".to_string())),
            ("rate".to_string(), ScalarValue::String("2.5".to_string())),
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
}
