//! A record's own constant seeds land before its dset's `init_record`.
//!
//! C seeds inside `init_record` ahead of `pdset->init_record`
//! (`lsoRecord.c:82` before `:85`, `stringoutRecord.c:113` before `:117`,
//! `longoutRecord.c:113` before `:117`, `int64outRecord.c:110` before `:114`,
//! `aoRecord.c:112` before `:127`), so a dset that reads the device back at
//! init sees the DOL value and its readback wins. C StreamDevice with
//! `@init { out "Q%s"; in "%s"; }` and `DOL={const:'hello'}` / `DOL=5` sends
//! `QShello` / `QN5` / `QN5.0` and leaves VAL "init" / 9. The port seeded after
//! the dset init, so the DOL overwrote the readback.

use std::collections::HashMap;
use std::sync::Mutex;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{
    DeviceInitOutcome, DeviceReadOutcome, DeviceSupport, DeviceUdf,
};
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, Record};
use epics_base_rs::types::EpicsValue;

/// What each record's dset found in VAL at init.
static SEEN: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// A device that reads back at init: it notes VAL, then writes its own.
struct Readback {
    name: String,
}

fn text(v: Option<EpicsValue>) -> String {
    match v {
        Some(EpicsValue::CharArray(v)) => {
            String::from_utf8_lossy(&v[..v.iter().position(|&b| b == 0).unwrap_or(v.len())])
                .into_owned()
        }
        Some(EpicsValue::String(s)) => s.to_string(),
        Some(v) => v.to_f64().unwrap().to_string(),
        None => panic!("no VAL"),
    }
}

impl DeviceSupport for Readback {
    fn init(&mut self, record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
        SEEN.lock()
            .unwrap()
            .push((self.name.clone(), text(record.val())));
        let readback = match record.record_type() {
            "lso" | "stringout" => EpicsValue::String("init".into()),
            "longout" => EpicsValue::Long(9),
            "int64out" => EpicsValue::Int64(9),
            _ => EpicsValue::Double(9.0),
        };
        record.put_field("VAL", readback)?;
        Ok(DeviceInitOutcome::DoNotConvert(DeviceUdf::Untouched))
    }

    fn set_record_info(&mut self, name: &str, _: epics_base_rs::server::record::ScanType) {
        self.name = name.to_owned();
    }

    fn read(&mut self, _: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        Ok(DeviceReadOutcome::computed(DeviceUdf::Untouched))
    }

    fn write(&mut self, _: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "readback"
    }
}

static ROWS: [DevSup; 1] = [DevSup {
    name: "devReadback",
    choice: "readback",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(5, &[2, 4], false),
}];

const DB: &str = r#"
record(lso, "lso") { field(DTYP, "readback") field(OUT, "@x") field(DOL, {const:'hello'}) }
record(stringout, "so") { field(DTYP, "readback") field(OUT, "@x") field(DOL, {const:'hello'}) }
record(longout, "lo") { field(DTYP, "readback") field(OUT, "@x") field(DOL, "5") }
record(int64out, "i64") { field(DTYP, "readback") field(OUT, "@x") field(DOL, "5") }
record(ao, "ao") { field(DTYP, "readback") field(OUT, "@x") field(DOL, "5") }
"#;

#[epics_macros_rs::epics_test]
async fn the_dset_init_sees_the_dol_and_its_readback_stays() {
    let devices: Vec<(&str, &[DevSup])> = ["lso", "stringout", "longout", "int64out", "ao"]
        .into_iter()
        .map(|t| (t, &ROWS[..]))
        .collect();
    let db = IocBuilder::new()
        .register_dynamic_device_support(&devices, |ctx: &DeviceSupportContext| {
            (ctx.dtyp == "readback").then(|| {
                Box::new(Readback {
                    name: String::new(),
                }) as Box<dyn DeviceSupport>
            })
        })
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    let mut seen = SEEN.lock().unwrap().clone();
    seen.sort();
    let want = [
        ("ao", "5"),
        ("i64", "5"),
        ("lo", "5"),
        ("lso", "hello"),
        ("so", "hello"),
    ];
    assert_eq!(
        seen,
        want.map(|(r, v)| (r.to_string(), v.to_string())),
        "C StreamDevice's @init sent QN5.0, QN5, QN5, QShello, QShello"
    );
    for (pv, want) in [
        ("lso", "init"),
        ("so", "init"),
        ("so.OVAL", "init"),
        ("lo", "9"),
        ("i64", "9"),
        ("ao", "9"),
        ("ao.OVAL", "9"),
    ] {
        assert_eq!(text(db.get_pv(pv).ok()), want, "{pv}, as C's dbgf");
    }
}
