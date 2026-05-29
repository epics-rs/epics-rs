use clap::Parser;
use epics_pva_rs::client::PvaClient;
use epics_pva_rs::format;

#[derive(Parser)]
#[command(
    name = "pvget-rs",
    version,
    about = "Read EPICS PV values via pvAccess"
)]
struct Args {
    /// PV names to read
    #[arg(required = true)]
    pv_names: Vec<String>,

    /// Request, specifies what fields to return and options
    #[arg(short = 'r', default_value = "")]
    request: String,

    /// Output mode: raw, nt, json
    #[arg(short = 'M', default_value = "nt")]
    mode: String,

    /// Show entire structure (implies raw mode)
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
}

/// Parse a pvRequest string like "field(value,alarm,timeStamp)" into field names.
fn parse_pv_request(request: &str) -> Vec<&str> {
    let trimmed = request.trim();
    if trimmed.is_empty() {
        return vec![];
    }
    // Strip "field(...)" wrapper if present
    let inner = if let Some(rest) = trimmed.strip_prefix("field(") {
        rest.strip_suffix(')').unwrap_or(rest)
    } else {
        trimmed
    };
    inner
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect()
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let client = PvaClient::new().expect("failed to create PVA client");
    let mode = if args.verbose > 0 {
        "raw"
    } else {
        args.mode.as_str()
    };
    let fields = parse_pv_request(&args.request);

    let mut failed = false;
    for pv_name in &args.pv_names {
        let result = if fields.is_empty() {
            client.pvget_full(pv_name).await
        } else {
            client.pvget_fields(pv_name, &fields).await
        };
        match result {
            Ok(result) => {
                // `-q` is a deprecated no-op (see Args::quiet); successful
                // values are always printed, matching pvAccessCPP pvget.
                let output = match mode {
                    "json" => format::format_json(pv_name, &result.value),
                    "raw" => format::format_raw(pv_name, &result.introspection, &result.value),
                    _ => format::format_nt(pv_name, &result.introspection, &result.value),
                };
                print!("{output}");
            }
            Err(e) => {
                eprintln!("{pv_name}: {e}");
                failed = true;
            }
        }
    }
    if failed {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `-q` is accepted for legacy compatibility but is a deprecated
    /// no-op: it must not gate successful output (pvAccessCPP
    /// pvget.cpp:332-338). This locks the flag's presence; the
    /// print-always behavior is enforced structurally in `main`.
    #[test]
    fn quiet_flag_is_accepted_as_deprecated_noop() {
        let args = Args::parse_from(["pvget-rs", "-q", "PV"]);
        assert!(args.quiet);
    }
}
