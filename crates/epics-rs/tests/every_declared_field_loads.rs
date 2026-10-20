//! C's `dbRecordField` (`dbLexRoutines.c:1390-1415`) refuses a `.db`
//! `field()` only for its NAME and for a `dbPutString` failure; there is no
//! `SPC_NOMOD` gate at load, so a `.db` may set any declared field. For every
//! record type the workspace generates a table for, each declared field but
//! `NAME` and the `DBF_NOACCESS` ones is loaded with a value of its type, and
//! the load must succeed and the field read back what was loaded.
#![allow(clippy::disallowed_macros, reason = "tests may keep eprintln!")]

use std::collections::HashMap;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::RecordFactory;
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport, DeviceUdf};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{
    DbLinkType, DevSup, Dset, FieldDesc, Record, Special, dbd_generated as base,
    shared_menu_choices,
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

/// `SPC_NOMOD` has one owner, the database's gate: C refuses only in
/// `dbPutSpecial` pass 0 (`dbAccess.c:122-127`). It refuses a client put to
/// every declared NOMOD value field of every record type.
#[tokio::test(flavor = "multi_thread")]
async fn every_nomod_field_is_refused_by_the_gate() {
    let mut failures = Vec::new();
    let mut cases = 0usize;
    for (record_types, fields) in tables() {
        for &record_type in record_types {
            let nomod: Vec<&str> = fields(record_type)
                .unwrap_or_else(|| panic!("{record_type}: no table"))
                .iter()
                .filter(|f| {
                    f.read_only
                        && !matches!(
                            f.declared_dbf,
                            DbfCode::NoAccess
                                | DbfCode::Inlink
                                | DbfCode::Outlink
                                | DbfCode::Fwdlink
                        )
                })
                .map(|f| f.name)
                .collect();
            let mut builder = IocBuilder::new();
            for (name, factory) in factories() {
                builder = builder.register_record_type(name, factory);
            }
            let db = builder
                .db_string(
                    &format!("record({record_type}, \"nomod\") {{\n}}\n"),
                    &HashMap::new(),
                )
                .unwrap()
                .load()
                .await
                .unwrap();
            for field in nomod {
                cases += 1;
                match db.check_external_put_preconditions("nomod", field).await {
                    Err(CaError::ReadOnlyField(_)) => {}
                    other => failures.push(format!("{record_type}.{field}: {other:?}")),
                }
            }
        }
    }
    let links = nomod_link_failures().await;
    cases += links.0;
    failures.extend(links.1);
    eprintln!("{cases} NOMOD fields");
    assert!(
        failures.is_empty(),
        "{} NOMOD fields the gate let through:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// A device row with a dsxt, so a device-link put passes `dbPutFieldLink`'s
/// `S_db_noSupport` and `del_record` gates and reaches `SPC_NOMOD`.
static CENSUS_HW: DevSup = DevSup {
    name: "devCensusHw",
    choice: "Census Hw",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(6, &[], true),
};

struct CensusDevice;

impl DeviceSupport for CensusDevice {
    fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "Census Hw"
    }
}

/// A link field's `SPC_NOMOD` waits for the put's hold (`dbPutFieldLink`
/// removes the old link first, `dbAccess.c:1167-1176`), so each NOMOD link
/// field takes a CA put: refused, the field reads what was loaded. INP/OUT
/// once as a device link and once holding a hardware link on a type with no
/// device row, which C's `dbLinkIsDefined` refuses before the gate.
async fn nomod_link_failures() -> (usize, Vec<String>) {
    let mut failures = Vec::new();
    let mut cases = 0usize;
    for (record_types, fields) in tables() {
        for &record_type in record_types {
            let own = fields(record_type).unwrap_or_else(|| panic!("{record_type}: no table"));
            for f in base::DB_COMMON_FIELDS.iter().chain(own) {
                if !f.read_only
                    || !matches!(
                        f.declared_dbf,
                        DbfCode::Inlink | DbfCode::Outlink | DbfCode::Fwdlink
                    )
                {
                    continue;
                }
                let device = matches!(f.name, "INP" | "OUT");
                let arms: &[(bool, &str, &str)] = if device {
                    &[(false, "@census", "@other"), (true, "@census", "@other")]
                } else {
                    &[(false, "census", "other")]
                };
                for &(with_device, loaded, put) in arms {
                    cases += 1;
                    let case = format!("{record_type}.{} (device row {with_device})", f.name);
                    let mut builder = IocBuilder::new();
                    for (name, factory) in factories() {
                        builder = builder.register_record_type(name, factory);
                    }
                    let dtyp = if with_device {
                        builder = builder.register_device_support(record_type, &CENSUS_HW, || {
                            Box::new(CensusDevice)
                        });
                        "field(DTYP, \"Census Hw\")"
                    } else {
                        ""
                    };
                    let db = builder
                        .db_string(
                            &format!(
                                "record({record_type}, \"nomod\") {{ {dtyp} field({}, \"{loaded}\") }}\n",
                                f.name
                            ),
                            &HashMap::new(),
                        )
                        .unwrap()
                        .load()
                        .await
                        .unwrap();
                    let refused = db
                        .put_record_field_from_ca("nomod", f.name, EpicsValue::String(put.into()))
                        .await;
                    let gate = with_device || !device;
                    match &refused {
                        Err(CaError::ReadOnlyField(_)) => {}
                        Err(_) if !gate => {}
                        other => failures.push(format!("{case}: put {other:?}")),
                    }
                    // A removed PV link keeps its name; C prints the
                    // `PV_LINK` `dbCaRemoveLink` leaves as "name NPP NMS"
                    // (`dbStaticLib.c:1947-1951`). A hardware link has no
                    // lset to remove and keeps its text.
                    let got = db.get_pv(format!("nomod.{}", f.name));
                    let kept = match &got {
                        Ok(EpicsValue::String(s)) if loaded.starts_with('@') => {
                            s.as_str_lossy() == loaded
                        }
                        Ok(EpicsValue::String(s)) => {
                            s.as_str_lossy().split_whitespace().next() == Some(loaded)
                        }
                        _ => false,
                    };
                    if !kept {
                        failures.push(format!("{case}: reads {got:?} after the refused put"));
                    }
                }
            }
        }
    }
    (cases, failures)
}
