use clap::Parser;
use epics_base_rs::error::CaResult;
use epics_base_rs::server::records::{
    ai::AiRecord, ao::AoRecord, bi::BiRecord, bo::BoRecord, longin::LonginRecord,
    longout::LongoutRecord, mbbi::MbbiRecord, mbbo::MbboRecord, stringin::StringinRecord,
    stringout::StringoutRecord,
};
use epics_base_rs::types::{DbFieldType, EpicsValue};
use epics_ca_rs::server::CaServer;
use std::collections::HashMap;

/// A simple soft IOC that hosts PVs over Channel Access.
///
/// Example: rsoftioc --pv TEMP:double:25.0 --record ai:TEMP_REC:25.0 --db test.db
#[derive(Parser)]
#[command(name = "rsoftioc")]
struct Args {
    /// PV definitions in the format NAME:TYPE:VALUE
    /// Supported types: string, short, float, enum, char, long, double
    #[arg(long = "pv")]
    pvs: Vec<String>,

    /// Record definitions in the format RECORD_TYPE:NAME:VALUE
    /// Supported record types: ai, ao, bi, bo, stringin, stringout, longin, longout, mbbi, mbbo
    #[arg(long = "record")]
    records: Vec<String>,

    /// DB file paths to load
    #[arg(long = "db")]
    db_files: Vec<String>,

    /// Macro definitions for DB files in KEY=VALUE format
    #[arg(long = "macro", short = 'm')]
    macros: Vec<String>,

    /// Port to listen on (default: 5064)
    #[arg(long, default_value_t = 5064)]
    port: u16,

    /// Start interactive iocsh shell
    #[arg(long, short = 'i')]
    shell: bool,
}

fn is_type_keyword(s: &str) -> bool {
    matches!(
        s,
        "string"
            | "str"
            | "short"
            | "int16"
            | "float"
            | "f32"
            | "enum"
            | "u16"
            | "char"
            | "u8"
            | "long"
            | "int32"
            | "double"
            | "f64"
    )
}

fn parse_pv_def(def: &str) -> CaResult<(String, EpicsValue)> {
    // Format is NAME:TYPE:VALUE, but NAME may contain colons (e.g. "SEQ:counter").
    // Find the type keyword by scanning the colon-separated segments from the right.
    let segments: Vec<&str> = def.split(':').collect();

    // We need at least 3 segments (name, type, value), with the type being a known keyword.
    // Scan from the end to find the type keyword — the segment after it is the value,
    // and everything before it is the name.
    let type_idx = segments
        .iter()
        .rposition(|s| is_type_keyword(&s.to_lowercase()));

    let type_idx = match type_idx {
        Some(idx) if idx > 0 && idx + 1 < segments.len() => idx,
        _ => {
            return Err(epics_base_rs::error::CaError::InvalidValue(format!(
                "expected NAME:TYPE:VALUE, got '{def}'"
            )));
        }
    };

    let name = segments[..type_idx].join(":");
    let type_str = segments[type_idx].to_lowercase();
    let value_str = segments[type_idx + 1..].join(":");

    let dbr_type = match type_str.as_str() {
        "string" | "str" => DbFieldType::String,
        "short" | "int16" => DbFieldType::Short,
        "float" | "f32" => DbFieldType::Float,
        "enum" | "u16" => DbFieldType::Enum,
        "char" | "u8" => DbFieldType::Char,
        "long" | "int32" => DbFieldType::Long,
        "double" | "f64" => DbFieldType::Double,
        _ => unreachable!(),
    };

    let value = EpicsValue::parse(dbr_type, &value_str)?;
    Ok((name, value))
}

fn parse_record_def(
    def: &str,
) -> CaResult<(String, Box<dyn epics_base_rs::server::record::Record>)> {
    // Split on first ':' to get record type; the remainder is NAME or NAME:...:VALUE.
    // PV names often contain colons (e.g. "SEQ:counter"), so we try to parse the
    // last ':'-separated segment as a value — if that fails, the whole remainder is the name.
    let (rec_type_str, remainder) = def.split_once(':').ok_or_else(|| {
        epics_base_rs::error::CaError::InvalidValue(format!(
            "expected RECORD_TYPE:NAME[:VALUE], got '{def}'"
        ))
    })?;

    let rec_type = rec_type_str.to_lowercase();

    // Try splitting off the last ':' segment as a candidate value.
    let (name, value_str) = if let Some((prefix, suffix)) = remainder.rsplit_once(':') {
        (prefix, suffix)
    } else {
        (remainder, "")
    };

    // Helper: attempt to parse the candidate value; if it fails, treat the whole
    // remainder as the name and use the default value.
    macro_rules! parse_or_default {
        ($type:ty, $default:expr) => {{
            if value_str.is_empty() {
                (remainder, $default)
            } else if let Ok(v) = value_str.parse::<$type>() {
                (name, v)
            } else {
                (remainder, $default)
            }
        }};
    }

    let record: Box<dyn epics_base_rs::server::record::Record> = match rec_type.as_str() {
        "ai" => {
            let (n, val) = parse_or_default!(f64, 0.0);
            return Ok((n.to_string(), Box::new(AiRecord::new(val))));
        }
        "ao" => {
            let (n, val) = parse_or_default!(f64, 0.0);
            return Ok((n.to_string(), Box::new(AoRecord::new(val))));
        }
        "bi" => {
            let (n, val) = parse_or_default!(u16, 0);
            return Ok((n.to_string(), Box::new(BiRecord::new(val))));
        }
        "bo" => {
            let (n, val) = parse_or_default!(u16, 0);
            return Ok((n.to_string(), Box::new(BoRecord::new(val))));
        }
        "longin" => {
            let (n, val) = parse_or_default!(i32, 0);
            return Ok((n.to_string(), Box::new(LonginRecord::new(val))));
        }
        "longout" => {
            let (n, val) = parse_or_default!(i32, 0);
            return Ok((n.to_string(), Box::new(LongoutRecord::new(val))));
        }
        "mbbi" => {
            let (n, val) = parse_or_default!(u16, 0);
            return Ok((n.to_string(), Box::new(MbbiRecord::new(val))));
        }
        "mbbo" => {
            let (n, val) = parse_or_default!(u16, 0);
            return Ok((n.to_string(), Box::new(MbboRecord::new(val))));
        }
        "stringin" => Box::new(StringinRecord::new(remainder)),
        "stringout" => Box::new(StringoutRecord::new(remainder)),
        _ => {
            return Err(epics_base_rs::error::CaError::InvalidValue(format!(
                "unknown record type '{rec_type}'"
            )));
        }
    };

    Ok((remainder.to_string(), record))
}

fn parse_macros(macro_strs: &[String]) -> HashMap<String, String> {
    let mut macros = HashMap::new();
    for m in macro_strs {
        if let Some((k, v)) = m.split_once('=') {
            macros.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    macros
}

#[tokio::main]
async fn main() -> CaResult<()> {
    let args = Args::parse();

    if args.pvs.is_empty() && args.records.is_empty() && args.db_files.is_empty() {
        eprintln!("Error: at least one --pv, --record, or --db is required");
        std::process::exit(1);
    }

    let mut builder = CaServer::builder().port(args.port);

    for pv_def in &args.pvs {
        let (name, value) = parse_pv_def(pv_def)?;
        eprintln!("  PV: {name} = {value} ({})", value.dbr_type() as u16);
        builder = builder.pv(&name, value);
    }

    for rec_def in &args.records {
        let (name, record) = parse_record_def(rec_def)?;
        eprintln!("  Record: {name} ({})", record.record_type());
        builder = builder.record_boxed(&name, record);
    }

    let macros = parse_macros(&args.macros);
    for db_file in &args.db_files {
        eprintln!("  Loading DB: {db_file}");
        builder = builder.db_file(db_file, &macros)?;
    }

    let server = builder.build().await?;

    if args.shell {
        server.run_with_shell(|_shell| {}).await
    } else {
        server.run().await
    }
}
