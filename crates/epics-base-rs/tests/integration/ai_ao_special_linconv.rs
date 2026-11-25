//! ai/ao `special(SPC_LINCONV)` calls the dset's `special_linconv` on both
//! passes of a LINR/EGUF/EGUL put while LINR is LINEAR (`aiRecord.c:187-198`,
//! `aoRecord.c:254-265`), after rebasing EOFF to EGUL.
//!
//! `lcAsyn` is `devAsynInt32.c`'s `convertAi`/`convertAo` over the
//! `asynPortDriver` default bounds 0..65535, called from init as `initAi` does.
//! The expected ESLO/EOFF are oracleMotorIoc (8f5015b66) with asynInt32 ai/ao
//! records on a motorSim port, the same fields, the same puts. `lcNoop` fills
//! the slot with a routine that does nothing, as `devGpib.h`'s
//! `dummySpecialLinconvAIRAW` does.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::device_support::{
    DeviceInitOutcome, DeviceReadOutcome, DeviceSupport, DeviceUdf,
};
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, Record, ScanType};
use epics_base_rs::types::{DbFieldType, EpicsValue};

const HIGH: f64 = 65535.0;

static PASSES: Mutex<Vec<(String, bool)>> = Mutex::new(Vec::new());

struct Linconv {
    name: String,
    asyn: bool,
}

fn f(record: &dyn Record, field: &str) -> f64 {
    record.get_field(field).and_then(|v| v.to_f64()).unwrap()
}

/// `convertAi(prec, 1)` with deviceLow 0, deviceHigh 65535.
fn convert(record: &mut dyn Record) -> CaResult<()> {
    let (eguf, egul) = (f(record, "EGUF"), f(record, "EGUL"));
    record.put_field("ESLO", EpicsValue::Double((eguf - egul) / HIGH))?;
    record.put_field("EOFF", EpicsValue::Double(HIGH * egul / HIGH))
}

impl DeviceSupport for Linconv {
    fn init(&mut self, record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
        if self.asyn {
            convert(record)?;
        }
        Ok(DeviceInitOutcome::ok())
    }

    fn has_special_linconv(&self) -> bool {
        true
    }

    fn special_linconv(&mut self, record: &mut dyn Record, after: bool) -> CaResult<()> {
        PASSES.lock().unwrap().push((self.name.clone(), after));
        if self.asyn && after {
            convert(record)?;
        }
        Ok(())
    }

    fn set_record_info(&mut self, name: &str, _scan: ScanType) {
        self.name = name.to_owned();
    }

    fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        if self.asyn { "lcAsyn" } else { "lcNoop" }
    }
}

static ROWS: [DevSup; 2] = [row("lcAsyn"), row("lcNoop")];

const fn row(choice: &'static str) -> DevSup {
    DevSup {
        name: "devLinconv",
        choice,
        link_type: DbLinkType::Constant,
        dset: Dset::new(6, &[2, 4, 5], false),
    }
}

const FIELDS: &str = r#"field(EGUL, "5") field(EGUF, "10") field(ESLO, "2") field(EOFF, "3")"#;

async fn ioc() -> Arc<PvDatabase> {
    let mut db = String::new();
    for (name, rtype, dtyp, linr) in [
        ("LC:AI:LIN", "ai", "lcAsyn", "LINEAR"),
        ("LC:AI:NOC", "ai", "lcAsyn", "NO CONVERSION"),
        ("LC:AI:SLOPE", "ai", "lcAsyn", "SLOPE"),
        ("LC:AO:LIN", "ao", "lcAsyn", "LINEAR"),
        ("LC:AO:NOC", "ao", "lcAsyn", "NO CONVERSION"),
        ("LC:AI:NOOP", "ai", "lcNoop", "LINEAR"),
        ("LC:AO:NOOP", "ao", "lcNoop", "LINEAR"),
    ] {
        db += &format!(
            "record({rtype}, \"{name}\") {{ field(DTYP, \"{dtyp}\") field(LINR, \"{linr}\") {FIELDS} }}\n"
        );
    }
    let devices = vec![("ai", &ROWS[..]), ("ao", &ROWS[..])];
    IocBuilder::new()
        .register_dynamic_device_support(&devices, |ctx: &DeviceSupportContext| {
            let asyn = match ctx.dtyp {
                "lcAsyn" => true,
                "lcNoop" => false,
                _ => return None,
            };
            Some(Box::new(Linconv {
                name: String::new(),
                asyn,
            }) as Box<dyn DeviceSupport>)
        })
        .db_string(&db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn eslo_eoff(db: &PvDatabase, name: &str) -> (f64, f64) {
    let rec = db.get_record(name).unwrap();
    let inst = rec.read();
    (f(&*inst.record, "ESLO"), f(&*inst.record, "EOFF"))
}

async fn put(db: &PvDatabase, name: &str, field: &str, value: EpicsValue) {
    db.put_record_field_from_ca(name, field, value)
        .await
        .unwrap();
}

fn passes(name: &str) -> Vec<bool> {
    PASSES
        .lock()
        .unwrap()
        .iter()
        .filter(|(n, _)| n == name)
        .map(|(_, after)| *after)
        .collect()
}

#[epics_macros_rs::epics_test]
async fn linconv_follows_linr_eguf_egul_puts_as_c_does() {
    let db = ioc().await;
    let init = (5.0 / HIGH, 5.0);
    let eguf20 = (15.0 / HIGH, 5.0);
    let egul1 = (19.0 / HIGH, 1.0);

    for name in ["LC:AI:LIN", "LC:AI:NOC", "LC:AO:LIN", "LC:AO:NOC"] {
        assert_eq!(eslo_eoff(&db, name), init, "{name} after init");
    }
    // `init_record` restores ESLO/EOFF for SLOPE after the dset's init.
    assert_eq!(eslo_eoff(&db, "LC:AI:SLOPE"), (2.0, 3.0));

    let mut eslo_rx = db
        .get_record("LC:AI:LIN")
        .unwrap()
        .write()
        .add_subscriber(
            "ESLO",
            1,
            DbFieldType::Double,
            (EventMask::VALUE | EventMask::LOG).bits(),
        )
        .unwrap();
    while eslo_rx.try_recv().is_ok() {}

    for name in [
        "LC:AI:LIN",
        "LC:AI:NOC",
        "LC:AI:SLOPE",
        "LC:AO:LIN",
        "LC:AO:NOC",
    ] {
        put(&db, name, "EGUF", EpicsValue::Double(20.0)).await;
    }
    assert!(eslo_rx.try_recv().is_ok(), "special() posts the moved ESLO");
    for name in ["LC:AI:LIN", "LC:AO:LIN"] {
        assert_eq!(eslo_eoff(&db, name), eguf20, "{name} after EGUF 20");
        assert_eq!(passes(name), [false, true], "{name}: both special passes");
    }
    for name in ["LC:AI:NOC", "LC:AO:NOC"] {
        assert_eq!(eslo_eoff(&db, name), init, "{name}: not LINEAR, no call");
        assert!(passes(name).is_empty());
    }
    assert_eq!(eslo_eoff(&db, "LC:AI:SLOPE"), (2.0, 3.0));
    assert!(passes("LC:AI:SLOPE").is_empty());

    for name in ["LC:AI:LIN", "LC:AO:LIN"] {
        put(&db, name, "EGUL", EpicsValue::Double(1.0)).await;
        assert_eq!(eslo_eoff(&db, name), egul1, "{name} after EGUL 1");
    }

    // LINR NO CONVERSION -> LINEAR: pass 0 still sees the old LINR.
    for name in ["LC:AI:NOC", "LC:AO:NOC"] {
        put(&db, name, "LINR", EpicsValue::Short(2)).await;
        assert_eq!(eslo_eoff(&db, name), eguf20, "{name} after LINR LINEAR");
        assert_eq!(passes(name), [true], "{name}: only the after pass");
    }
}

/// A filled slot whose routine does nothing still rebases EOFF to EGUL.
#[epics_macros_rs::epics_test]
async fn a_filled_slot_rebases_eoff_to_egul() {
    let db = ioc().await;
    for name in ["LC:AI:NOOP", "LC:AO:NOOP"] {
        assert_eq!(eslo_eoff(&db, name), (2.0, 3.0), "{name} after init");
        put(&db, name, "EGUL", EpicsValue::Double(7.0)).await;
        assert_eq!(eslo_eoff(&db, name), (2.0, 7.0), "{name} after EGUL 7");
    }
}
