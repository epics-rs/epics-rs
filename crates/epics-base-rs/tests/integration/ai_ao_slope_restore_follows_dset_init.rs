//! ai/ao `init_record` rebases EOFF to EGUL when ESLO/EOFF are 1/0, calls the
//! dset's `init_record`, and only then, and only if the dset has one, gives a
//! SLOPE record back the ESLO/EOFF it had (`aiRecord.c:115-126`,
//! `aoRecord.c:121-133`). softIoc 8f5015b66 reads these values for the soft
//! records; `slope` is a dset whose `init_record` writes ESLO/EOFF, as
//! `devAsynInt32.c`'s `initAi` does through `convertAi`.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::device_support::{
    DeviceInitOutcome, DeviceReadOutcome, DeviceSupport, DeviceUdf,
};
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, Record};
use epics_base_rs::types::EpicsValue;

struct WritesSlope;

impl DeviceSupport for WritesSlope {
    fn init(&mut self, record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
        record.put_field("ESLO", EpicsValue::Double(0.5))?;
        record.put_field("EOFF", EpicsValue::Double(9.0))?;
        Ok(DeviceInitOutcome::ok())
    }

    fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "slope"
    }
}

static ROWS: [DevSup; 1] = [DevSup {
    name: "devSlope",
    choice: "slope",
    link_type: DbLinkType::Constant,
    dset: Dset::new(6, &[2, 4], false),
}];

const DB: &str = r#"
record(ao, "AO:SOFT") { field(LINR, "SLOPE") field(EGUL, "5") field(EGUF, "10") }
record(ao, "AO:RAW") { field(DTYP, "Raw Soft Channel") field(LINR, "SLOPE") field(EGUL, "5") field(EGUF, "10") }
record(ao, "AO:CB") { field(DTYP, "Async Soft Channel") field(LINR, "SLOPE") field(EGUL, "5") field(EGUF, "10") }
record(ao, "AO:RAWLIN") { field(DTYP, "Raw Soft Channel") field(LINR, "LINEAR") field(EGUL, "5") field(EGUF, "10") }
record(ai, "AI:SOFT") { field(LINR, "SLOPE") field(EGUL, "5") field(EGUF, "10") }
record(ai, "AI:RAW") { field(DTYP, "Raw Soft Channel") field(LINR, "SLOPE") field(EGUL, "5") field(EGUF, "10") }
record(ai, "AI:LIN") { field(DTYP, "Raw Soft Channel") field(LINR, "LINEAR") field(EGUL, "5") field(EGUF, "10") }
record(ai, "AI:DSET:SLOPE") { field(DTYP, "slope") field(LINR, "SLOPE") field(ESLO, "2") field(EOFF, "3") }
record(ai, "AI:DSET:LIN") { field(DTYP, "slope") field(LINR, "LINEAR") field(ESLO, "2") field(EOFF, "3") }
record(ao, "AO:DSET:SLOPE") { field(DTYP, "slope") field(LINR, "SLOPE") field(ESLO, "2") field(EOFF, "3") }
"#;

async fn ioc() -> Arc<PvDatabase> {
    let devices = vec![("ai", &ROWS[..]), ("ao", &ROWS[..])];
    IocBuilder::new()
        .register_dynamic_device_support(&devices, |ctx: &DeviceSupportContext| {
            (ctx.dtyp == "slope").then(|| Box::new(WritesSlope) as Box<dyn DeviceSupport>)
        })
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn eslo_eoff(db: &PvDatabase, name: &str) -> (f64, f64) {
    let rec = db.get_record(name).unwrap();
    let inst = rec.read();
    let f = |field| {
        inst.record
            .get_field(field)
            .and_then(|v| v.to_f64())
            .unwrap()
    };
    (f("ESLO"), f("EOFF"))
}

#[epics_macros_rs::epics_test]
async fn slope_restore_needs_a_dset_init_record_and_follows_it() {
    let db = ioc().await;
    for (name, expected) in [
        ("AO:SOFT", (1.0, 0.0)),
        ("AO:RAW", (1.0, 5.0)),
        ("AO:CB", (1.0, 5.0)),
        ("AO:RAWLIN", (1.0, 5.0)),
        ("AI:SOFT", (1.0, 0.0)),
        ("AI:RAW", (1.0, 0.0)),
        ("AI:LIN", (1.0, 5.0)),
        ("AI:DSET:SLOPE", (2.0, 3.0)),
        ("AI:DSET:LIN", (0.5, 9.0)),
        ("AO:DSET:SLOPE", (2.0, 3.0)),
    ] {
        assert_eq!(eslo_eoff(&db, name), expected, "{name}");
    }
}
