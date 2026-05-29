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
    version,
    about = "Call an EPICS pvAccess RPC method"
)]
struct Args {
    /// PV name of the RPC endpoint
    #[arg(required = true)]
    pv_name: String,

    /// `field=value` arguments. Repeat for multiple args.
    /// Values are sent verbatim as PVA strings (pvxs CLI semantics).
    args: Vec<String>,

    /// Wait time in seconds for the RPC to complete
    #[arg(short = 'w', default_value = "5.0")]
    timeout: f64,

    /// Output mode: nt (NTURI-aware), raw, json
    #[arg(short = 'M', default_value = "nt")]
    mode: String,
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
    let parsed_args: Vec<(String, ScalarValue)> = match args
        .args
        .iter()
        .map(|s| parse_arg(s))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("pvcall-rs: {e}");
            std::process::exit(2);
        }
    };

    let (desc, value) = build_nturi(&args.pv_name, &parsed_args);

    let client = PvaClient::builder()
        .timeout(epics_pva_rs::cli::timeout_duration(args.timeout))
        .build();

    match client.pvrpc(&args.pv_name, &desc, &value).await {
        Ok((resp_desc, resp_value)) => match args.mode.as_str() {
            "json" => {
                let s = epics_pva_rs::format::format_json(&args.pv_name, &resp_value);
                println!("{s}");
            }
            "raw" => {
                let s = epics_pva_rs::format::format_raw(&args.pv_name, &resp_desc, &resp_value);
                println!("{s}");
            }
            _ => {
                let s = epics_pva_rs::format::format_nt(&args.pv_name, &resp_desc, &resp_value);
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
}
