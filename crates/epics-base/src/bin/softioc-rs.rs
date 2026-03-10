use std::collections::HashMap;
use clap::Parser;
use epics_base_rs::error::CaResult;
use epics_base_rs::server::CaServer;
use epics_base_rs::server::records::{
    ai::AiRecord, ao::AoRecord, bi::BiRecord, bo::BoRecord,
    stringin::StringinRecord, stringout::StringoutRecord,
    longin::LonginRecord, longout::LongoutRecord,
    mbbi::MbbiRecord, mbbo::MbboRecord,
};
use epics_base_rs::types::{DbFieldType, EpicsValue};

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

fn parse_pv_def(def: &str) -> CaResult<(String, EpicsValue)> {
    let parts: Vec<&str> = def.splitn(3, ':').collect();
    if parts.len() != 3 {
        return Err(epics_base_rs::error::CaError::InvalidValue(format!(
            "expected NAME:TYPE:VALUE, got '{def}'"
        )));
    }

    let name = parts[0];
    let type_str = parts[1].to_lowercase();
    let value_str = parts[2];

    let dbr_type = match type_str.as_str() {
        "string" | "str" => DbFieldType::String,
        "short" | "int16" => DbFieldType::Short,
        "float" | "f32" => DbFieldType::Float,
        "enum" | "u16" => DbFieldType::Enum,
        "char" | "u8" => DbFieldType::Char,
        "long" | "int32" => DbFieldType::Long,
        "double" | "f64" => DbFieldType::Double,
        _ => {
            return Err(epics_base_rs::error::CaError::InvalidValue(format!(
                "unknown type '{type_str}'"
            )));
        }
    };

    let value = EpicsValue::parse(dbr_type, value_str)?;
    Ok((name.to_string(), value))
}

fn parse_record_def(def: &str) -> CaResult<(String, Box<dyn epics_base_rs::server::record::Record>)> {
    let parts: Vec<&str> = def.splitn(3, ':').collect();
    if parts.len() < 2 {
        return Err(epics_base_rs::error::CaError::InvalidValue(format!(
            "expected RECORD_TYPE:NAME[:VALUE], got '{def}'"
        )));
    }

    let rec_type = parts[0].to_lowercase();
    let name = parts[1];
    let value_str = if parts.len() > 2 { parts[2] } else { "" };

    let record: Box<dyn epics_base_rs::server::record::Record> = match rec_type.as_str() {
        "ai" => {
            let val: f64 = if value_str.is_empty() { 0.0 } else {
                value_str.parse().map_err(|e: std::num::ParseFloatError| epics_base_rs::error::CaError::InvalidValue(e.to_string()))?
            };
            Box::new(AiRecord::new(val))
        }
        "ao" => {
            let val: f64 = if value_str.is_empty() { 0.0 } else {
                value_str.parse().map_err(|e: std::num::ParseFloatError| epics_base_rs::error::CaError::InvalidValue(e.to_string()))?
            };
            Box::new(AoRecord::new(val))
        }
        "bi" => {
            let val: u16 = if value_str.is_empty() { 0 } else {
                value_str.parse().map_err(|e: std::num::ParseIntError| epics_base_rs::error::CaError::InvalidValue(e.to_string()))?
            };
            Box::new(BiRecord::new(val))
        }
        "bo" => {
            let val: u16 = if value_str.is_empty() { 0 } else {
                value_str.parse().map_err(|e: std::num::ParseIntError| epics_base_rs::error::CaError::InvalidValue(e.to_string()))?
            };
            Box::new(BoRecord::new(val))
        }
        "longin" => {
            let val: i32 = if value_str.is_empty() { 0 } else {
                value_str.parse().map_err(|e: std::num::ParseIntError| epics_base_rs::error::CaError::InvalidValue(e.to_string()))?
            };
            Box::new(LonginRecord::new(val))
        }
        "longout" => {
            let val: i32 = if value_str.is_empty() { 0 } else {
                value_str.parse().map_err(|e: std::num::ParseIntError| epics_base_rs::error::CaError::InvalidValue(e.to_string()))?
            };
            Box::new(LongoutRecord::new(val))
        }
        "mbbi" => {
            let val: u16 = if value_str.is_empty() { 0 } else {
                value_str.parse().map_err(|e: std::num::ParseIntError| epics_base_rs::error::CaError::InvalidValue(e.to_string()))?
            };
            Box::new(MbbiRecord::new(val))
        }
        "mbbo" => {
            let val: u16 = if value_str.is_empty() { 0 } else {
                value_str.parse().map_err(|e: std::num::ParseIntError| epics_base_rs::error::CaError::InvalidValue(e.to_string()))?
            };
            Box::new(MbboRecord::new(val))
        }
        "stringin" => Box::new(StringinRecord::new(value_str)),
        "stringout" => Box::new(StringoutRecord::new(value_str)),
        _ => {
            return Err(epics_base_rs::error::CaError::InvalidValue(format!(
                "unknown record type '{rec_type}'"
            )));
        }
    };

    Ok((name.to_string(), record))
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
