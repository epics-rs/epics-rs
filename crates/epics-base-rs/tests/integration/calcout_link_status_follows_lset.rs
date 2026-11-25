//! Base calcout classifies a link by its `lset` (`calcoutRecord.c:166-189`):
//! `dbLinkIsConstant` is `!plset || plset->isConstant` (`dbLink.c:220-225`),
//! so a hardware link, which `dbInitLink` gives no `lset`, is `Constant`.
//! So is an OUT naming a PV on a hardware DTYP: `dbCanSetLink` refuses it and
//! the link stays the empty one of the device's type (`dbStaticLib.c:2192`,
//! `:2222`). A `{state:…}` link's lset is neither constant nor volatile
//! (`lnkState.c:213-214`), so it is `Local PV`.

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::device_support::DeviceSupport;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, Record};

const LOC: i16 = 2;
const CON: i16 = 3;

/// A calcout device on an `INST_IO` link, as stream's is.
static HW: DevSup = DevSup {
    name: "devCalcoutHw",
    choice: "Hw",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(5, &[4], false),
};

struct Hw;

impl DeviceSupport for Hw {
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "Hw"
    }
}

const DB: &str = r#"
record(calcout, "K:HW") { field(DTYP, "Hw") field(OUT, "@teled_h.proto setDigital(6,A) TelH1_tty") field(CALC, "1") }
record(calcout, "K:ST") { field(INPA, {state:"kst"}) field(CALC, "A") }
record(calcout, "K:PV") { field(DTYP, "Hw") field(OUT, "K:TGT") field(CALC, "1") }
record(calcout, "K:EXT") { field(DTYP, "Hw") field(OUT, "NOWHERE:PV") field(CALC, "1") }
record(ao, "K:TGT") {}
"#;

async fn settled(db: &PvDatabase, pv: &str, want: i16) {
    let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    while std::time::Instant::now() < deadline {
        if db.get_pv(pv).ok().and_then(|v| v.to_f64()) == Some(f64::from(want)) {
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!("{pv} never reached {want}: {:?}", db.get_pv(pv));
}

#[epics_macros_rs::epics_test]
async fn a_calcout_reads_a_hardware_out_constant_and_a_state_link_local() {
    let ioc = IocBuilder::new()
        .register_device_support("calcout", &HW, || Box::new(Hw))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    settled(&db, "K:HW.OUTV", CON).await;
    settled(&db, "K:ST.INAV", LOC).await;
    settled(&db, "K:PV.OUTV", CON).await;
    settled(&db, "K:EXT.OUTV", CON).await;
}
