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
//! Numeric values may be passed bare (`gain=2.5`) — the CLI parses
//! best-effort as `i64` first, then `f64`, then falls back to
//! `String`.

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
    /// Values are parsed as i64 → f64 → String (first match wins).
    args: Vec<String>,

    /// Wait time in seconds for the RPC to complete
    #[arg(short = 'w', default_value = "5.0")]
    timeout: f64,

    /// Output mode: nt (NTURI-aware), raw, json
    #[arg(short = 'M', default_value = "nt")]
    mode: String,
}

/// Parse a `key=value` pair into a typed [`ScalarValue`]. Best-effort
/// — i64 first, then f64, then String. Mirrors pvxs's `call.cpp`
/// argument parser at the type-coercion level (pvxs goes further by
/// asking the server for the request schema and coercing
/// per-field; we keep it simple).
fn parse_arg(arg: &str) -> Result<(String, ScalarValue), String> {
    let (k, v) = arg
        .split_once('=')
        .ok_or_else(|| format!("expected key=value, got {arg:?}"))?;
    let value = if let Ok(n) = v.parse::<i64>() {
        ScalarValue::Long(n)
    } else if let Ok(n) = v.parse::<f64>() {
        ScalarValue::Double(n)
    } else {
        ScalarValue::String(v.to_string())
    };
    Ok((k.to_string(), value))
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
