//! C's `dbRecordField` (`dbLexRoutines.c:1390-1415`) refuses a `.db`
//! `field()` only for its NAME and for a `dbPutString` failure; there is no
//! `SPC_NOMOD` gate at load, so a `.db` may set any declared field. For every
//! record type the workspace generates a table for, each declared field but
//! `NAME` and the `DBF_NOACCESS` ones is loaded with a value of its type, and
//! the load must succeed and the field read back what was loaded.

use std::collections::HashMap;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::RecordFactory;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{
    FieldDesc, Record, Special, dbd_generated as base, shared_menu_choices,
};
use epics_base_rs::server::records::{
    acalcout::AcalcoutRecord, busy::BusyRecord, scalcout::ScalcoutRecord, sseq::SseqRecord,
    swait::SwaitRecord, transform::TransformRecord,
};
use epics_base_rs::types::{DbfCode, EpicsValue};

type Fields = fn(&str) -> Option<&'static [FieldDesc]>;

/// Every generated table: its record types and its field lookup.
fn tables() -> Vec<(&'static [&'static str], Fields)> {
    vec![
        (base::RECORD_TYPES, base::record_fields),
        (
            std_rs::records::dbd_generated::RECORD_TYPES,
            std_rs::records::dbd_generated::record_fields,
        ),
        (
            mca_rs::record::dbd_generated::RECORD_TYPES,
            mca_rs::record::dbd_generated::record_fields,
        ),
        (
            motor_rs::record::dbd_generated::RECORD_TYPES,
            motor_rs::record::dbd_generated::record_fields,
        ),
        (
            optics_rs::records::dbd_generated::RECORD_TYPES,
            optics_rs::records::dbd_generated::record_fields,
        ),
        (
            scaler_rs::records::dbd_generated::RECORD_TYPES,
            scaler_rs::records::dbd_generated::record_fields,
        ),
        (
            sscan_rs::records::dbd_generated::RECORD_TYPES,
            sscan_rs::records::dbd_generated::record_fields,
        ),
        (
            asyn_rs::dbd_generated::RECORD_TYPES,
            asyn_rs::dbd_generated::record_fields,
        ),
    ]
}

fn factories() -> Vec<(&'static str, RecordFactory)> {
    let mut f = std_rs::std_record_factories();
    f.extend([
        asyn_rs::asyn_record::asyn_record_factory(),
        mca_rs::mca_record_factory(),
        motor_rs::motor_record_factory(),
        optics_rs::table_record_factory(),
        scaler_rs::scaler_record_factory(),
        sscan_rs::sscan_record_factory(),
        sscan_rs::scanparm_record_factory(),
        // calc's and busy's record types, which `create_record` does not make
        // without a registration.
        ("acalcout", factory(|| Box::new(AcalcoutRecord::default()))),
        ("busy", factory(|| Box::new(BusyRecord::default()))),
        ("scalcout", factory(|| Box::new(ScalcoutRecord::default()))),
        ("sseq", factory(|| Box::new(SseqRecord::default()))),
        ("swait", factory(|| Box::new(SwaitRecord::default()))),
        (
            "transform",
            factory(|| Box::new(TransformRecord::default())),
        ),
    ]);
    f
}

fn factory(f: fn() -> Box<dyn Record>) -> RecordFactory {
    Box::new(f)
}

fn device_menu(record_type: &str) -> Option<&'static [&'static str]> {
    base::device_menu(record_type)
        .or_else(|| std_rs::records::dbd_generated::device_menu(record_type))
        .or_else(|| asyn_rs::dbd_generated::device_menu(record_type))
}

/// A `DBF_MENU` field's choices; `SCAN` and `SSCN` take `menuScan` by name.
fn menu(f: &FieldDesc) -> &'static [&'static str] {
    f.menu
        .or_else(|| shared_menu_choices(f.name))
        .unwrap_or_else(|| panic!("{}: a menu field with no menu", f.name))
}

/// The value the `.db` sets: one of the field's type that differs from what a
/// record starts with, so a store that does not land reads back as a miss.
/// `None` for a `DTYP` whose record type has no device to name.
fn load_value(record_type: &str, f: &FieldDesc) -> Option<String> {
    Some(match f.declared_dbf {
        DbfCode::Menu => menu(f).last()?.to_string(),
        DbfCode::Device => device_menu(record_type)?.first()?.to_string(),
        DbfCode::String if f.declared_special == Special::Calc => "3".to_string(),
        DbfCode::String => "x".to_string(),
        DbfCode::Inlink | DbfCode::Outlink | DbfCode::Fwdlink => String::new(),
        DbfCode::Char | DbfCode::UChar => "1".to_string(),
        _ => "3".to_string(),
    })
}

/// `SPC_DBADDR` scalars whose own byte nothing reads after `iocInit`:
/// asyn's `cvt_dbaddr` points BOUT and BINP at the output and input buffers
/// (`asynRecord.dbd:224`, `:281`; `asynRecord.c:944-952`), so the record
/// serves the buffer, not the loaded byte.
const UNREAD_DBADDR_SCALARS: [(&str, &str); 2] = [("asyn", "BOUT"), ("asyn", "BINP")];

fn quoted(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"")
}

/// What the field reads back as, when it is not what was loaded.
fn read_back_miss(f: &FieldDesc, value: &str, got: &EpicsValue) -> Option<String> {
    let matches = match f.declared_dbf {
        // The device menu is the registry's merge; its index is not ours.
        DbfCode::Device => true,
        DbfCode::Menu => {
            let index = menu(f).iter().position(|c| *c == value);
            match got {
                EpicsValue::String(s) => s.as_str_lossy() == value,
                other => other.to_f64() == index.map(|i| i as f64),
            }
        }
        DbfCode::String | DbfCode::Inlink | DbfCode::Outlink | DbfCode::Fwdlink => match got {
            EpicsValue::String(s) => s.as_str_lossy().trim() == value,
            EpicsValue::CharArray(b) | EpicsValue::UCharArray(b) => {
                b.iter()
                    .take_while(|c| **c != 0)
                    .copied()
                    .collect::<Vec<u8>>()
                    == value.as_bytes()
            }
            _ => false,
        },
        _ => got.to_f64() == value.parse::<f64>().ok(),
    };
    (!matches).then(|| format!("{got:?}"))
}

/// Load `db` alone, as `dbLoadRecords` with no `iocInit`; `Err` when the load
/// failed. On success, what `census.<field>` reads back.
async fn load(db: &str, field: &str) -> Result<CaResult<EpicsValue>, String> {
    let mut builder = IocBuilder::new();
    for (name, factory) in factories() {
        builder = builder.register_record_type(name, factory);
    }
    let loaded = match builder.db_string(db, &HashMap::new()) {
        Ok(b) => b.load().await,
        Err(e) => Err(e),
    };
    loaded
        .map(|db| db.get_pv(format!("census.{field}")))
        .map_err(|e| e.to_string().replace('\n', " | "))
}

/// Both arms of `dbRecordField`: a field set on the record the block creates,
/// and the same field set by a second block that merges into it. Each must
/// load, and the field must then read back what was loaded — before `iocInit`,
/// whose `init_record` passes may legitimately rewrite it.
#[tokio::test(flavor = "multi_thread")]
async fn every_declared_field_loads_and_reads_back() {
    let mut failures = Vec::new();
    let mut skipped = Vec::new();
    let mut cases = 0usize;
    for (record_types, fields) in tables() {
        for &record_type in record_types {
            let own = fields(record_type).unwrap_or_else(|| panic!("{record_type}: no table"));
            for f in base::DB_COMMON_FIELDS.iter().chain(own) {
                if f.name == "NAME"
                    || f.declared_dbf == DbfCode::NoAccess
                    || UNREAD_DBADDR_SCALARS.contains(&(record_type, f.name))
                {
                    continue;
                }
                let Some(value) = load_value(record_type, f) else {
                    skipped.push(format!("{record_type}.{}", f.name));
                    continue;
                };
                cases += 1;
                let body = format!(
                    "record({record_type}, \"census\") {{\n  field({}, \"{}\")\n}}\n",
                    f.name,
                    quoted(&value)
                );
                let merged = format!("record({record_type}, \"census\") {{\n}}\n{body}");
                for (arm, db) in [("new", &body), ("merge", &merged)] {
                    eprintln!("@@case {arm} {record_type}.{}", f.name);
                    let case = format!("{arm} {record_type}.{} = \"{value}\"", f.name);
                    match load(db, f.name).await {
                        Err(e) => failures.push(format!("{case}: load failed: {e}")),
                        Ok(Err(e)) => failures.push(format!("{case}: read back: {e}")),
                        Ok(Ok(got)) => {
                            if let Some(miss) = read_back_miss(f, &value, &got) {
                                failures.push(format!("{case}: read back {miss}"));
                            }
                        }
                    }
                }
            }
        }
    }
    eprintln!("{cases} fields, skipped (no device to name) {skipped:?}");
    assert!(
        failures.is_empty(),
        "{} loads of a declared field failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
