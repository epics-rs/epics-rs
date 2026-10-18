//! `info(asyn:INITIAL_READBACK)` is read only by devAsynOctet's output
//! initCommon (devAsynOctet.c:357). The scalar output dsets read the driver
//! at init whatever the tag says (initLo devAsynInt32.c:1080), and no input or
//! array dset reads at init. The tag used to switch the init read for every
//! dset and direction.
#![cfg(feature = "epics")]

use std::collections::HashMap;
use std::sync::Arc;

use asyn_rs::param::ParamType;
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

struct Drv(PortDriverBase);

impl PortDriver for Drv {
    fn base(&self) -> &PortDriverBase {
        &self.0
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.0
    }
}

const RECORDS: &str = r#"
record(longout, "LO_OFF") {
    field(DTYP, "asynInt32") field(OUT, "@asyn(INITRB,0,1)I32") info(asyn:INITIAL_READBACK, "0")
}
record(longin, "LI_ON") {
    field(DTYP, "asynInt32") field(INP, "@asyn(INITRB,0,1)I32") info(asyn:INITIAL_READBACK, "1")
}
record(ai, "AI_ON") {
    field(DTYP, "asynFloat64") field(INP, "@asyn(INITRB,0,1)F64") info(asyn:INITIAL_READBACK, "1")
}
record(stringin, "SI_ON") {
    field(DTYP, "asynOctetRead") field(INP, "@asyn(INITRB,0,1)MSG") info(asyn:INITIAL_READBACK, "1")
}
record(waveform, "WFI_ON") {
    field(DTYP, "asynInt32ArrayOut") field(INP, "@asyn(INITRB,0,1)ARR")
    field(FTVL, "LONG") field(NELM, "4") info(asyn:INITIAL_READBACK, "1")
}
record(stringout, "SO_ON") {
    field(DTYP, "asynOctetWrite") field(OUT, "@asyn(INITRB,0,1)MSG") info(asyn:INITIAL_READBACK, "1")
}
record(stringout, "SO") { field(DTYP, "asynOctetWrite") field(OUT, "@asyn(INITRB,0,1)MSG") field(VAL, "db") }
"#;

#[epics_base_rs::epics_test]
async fn only_an_octet_output_reads_the_initial_readback_tag() {
    let db = Arc::new(PvDatabase::new());
    let mut base = PortDriverBase::new(
        "INITRB",
        1,
        PortFlags::default(),
        InterfaceMask::UNMIGRATED_ALL,
        InterfaceMask::UNMIGRATED_ALL,
    );
    let i32 = base.create_param("I32", ParamType::Int32).unwrap();
    let f64 = base.create_param("F64", ParamType::Float64).unwrap();
    let msg = base.create_param("MSG", ParamType::Octet).unwrap();
    let arr = base.create_param("ARR", ParamType::Int32Array).unwrap();
    base.set_int32_param(i32, 0, 7).unwrap();
    base.set_float64_param(f64, 0, 2.5).unwrap();
    base.set_string_param(msg, 0, "hello").unwrap();
    base.params.set_int32_array(arr, 0, vec![1, 2, 3]).unwrap();
    let (runtime, _actor) =
        create_port_runtime_unclaimed(Drv(base), RuntimeConfig::default()).unwrap();
    asyn_rs::registry::register_port(
        "INITRB",
        runtime.port_handle().clone(),
        db.scan_control().interrupt_gate(),
    )
    .unwrap();
    let (db, _) = asyn_rs::adapter::register_asyn_device_support_for_builder(IocBuilder::new())
        .with_database(db)
        .db_string(RECORDS, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    let get = |name: &str| db.get_pv(name).unwrap();
    // initLo reads whatever the tag says.
    assert_eq!(get("LO_OFF"), EpicsValue::Long(7));
    // No input, and no array dset, reads at init.
    assert_eq!(get("LI_ON"), EpicsValue::Long(0));
    assert_eq!(get("AI_ON"), EpicsValue::Double(0.0));
    assert_eq!(get("SI_ON"), EpicsValue::String("".into()));
    assert_eq!(get("WFI_ON.NORD"), EpicsValue::ULong(0));
    // devAsynOctet's output reads on the tag, and only on it.
    assert_eq!(get("SO_ON"), EpicsValue::String("hello".into()));
    assert_eq!(get("SO"), EpicsValue::String("db".into()));
}
