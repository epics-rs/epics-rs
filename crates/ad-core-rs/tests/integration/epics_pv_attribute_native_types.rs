//! A `DBR_NATIVE` `EPICS_PV` attribute carries the type C's `PVAttribute`
//! gives it, which is the datatype HDF5 and NeXus write for the attribute.
//!
//! C reads a native ENUM as `DBR_SHORT` and a native CHAR as `DBR_CHAR`
//! (PVAttribute.cpp:233-237) and stores them as `NDAttrInt16` and
//! `NDAttrInt8` (:261-271), so a CHAR byte above 127 reads back negative.
//!
//! Needs the `ioc` feature.
#![cfg(feature = "ioc")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use ad_core_rs::attributes::{NDAttrValue, NDAttributeSource};
use ad_core_rs::driver::ndarray_driver::NDArrayDriverBase;
use ad_core_rs::plugin::runtime::ASYN_CANBLOCK;
use asyn_rs::port::InterfaceMask;
use epics_base_rs::server::access_security::new_acf_cell;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::bo::BoRecord;
use epics_base_rs::server::records::waveform::WaveformRecord;
use epics_ca_rs::client::CaClient;
use epics_ca_rs::server::blocking_serving::BlockingCaServing;
use epics_ca_rs::server::stats::ServerStats;
use epics_ca_rs::{DbFieldType, EpicsValue};
use fact_budget::FACT_BUDGET;

const ENUM_PV: &str = "ADNATIVE:Enum";
const CHAR_PV: &str = "ADNATIVE:Char";

fn attribute_value(drv: &NDArrayDriverBase, name: &str) -> NDAttrValue {
    drv.attributes()
        .get(name)
        .expect("the attribute is loaded")
        .epics_pv_source()
        .expect("it is an EPICS_PV attribute")
        .evaluate()
}

async fn first_value(drv: &NDArrayDriverBase, name: &str) -> NDAttrValue {
    let deadline = Instant::now() + FACT_BUDGET;
    loop {
        match attribute_value(drv, name) {
            NDAttrValue::Undefined => {
                assert!(
                    Instant::now() < deadline,
                    "the {name} attribute was never fed"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            value => return value,
        }
    }
}

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — ran and passed on the
// exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p
// ad-core-rs --features ioc`); its CA server is the reactor-free RSRV, and
// its reactor is `#[tokio::test]`'s own, which the backend does not remove.
#[tokio::test]
async fn native_enum_and_char_pvs_publish_c_signed_attribute_types() {
    let mut chars = WaveformRecord::new(1, DbFieldType::Char);
    chars
        .put_field("VAL", EpicsValue::CharArray(vec![200].into()))
        .expect("seed the CHAR waveform");
    let (db, _) = IocBuilder::new()
        .record(ENUM_PV, BoRecord::new(1))
        .record(CHAR_PV, chars)
        .build_served()
        .await
        .expect("build the database");
    let server = BlockingCaServing::start(
        0,
        0,
        db,
        new_acf_cell(None),
        Arc::new(ServerStats::default()),
    )
    .expect("serve CA");
    let port = server.udp_port();

    // SAFETY: under nextest, the repo's runner, each test runs in its own
    // process, and this sets the environment before `CaClient::new`
    // snapshots its resolver config.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
    let client = Arc::new(CaClient::new().await.expect("CA client"));

    let mut drv = NDArrayDriverBase::new(
        "ADNATIVE",
        1,
        1 << 20,
        ASYN_CANBLOCK,
        InterfaceMask::NONE,
        InterfaceMask::NONE,
    )
    .expect("driver");
    drv.set_ca_client(client, tokio::runtime::Handle::current());

    // No `dbrtype`: C's default is DBR_NATIVE.
    let xml = format!(
        "<Attributes><Attribute name=\"Enum\" type=\"EPICS_PV\" source=\"{ENUM_PV}\"/>\
         <Attribute name=\"Char\" type=\"EPICS_PV\" source=\"{CHAR_PV}\"/></Attributes>"
    );
    drv.port_base
        .set_string_param(drv.params.attributes_file, 0, &xml)
        .expect("set NDAttributesFile");
    drv.read_nd_attributes_file()
        .expect("a fed EPICS_PV attribute loads clean");

    assert_eq!(first_value(&drv, "Enum").await, NDAttrValue::Int16(1));
    assert_eq!(first_value(&drv, "Char").await, NDAttrValue::Int8(-56));
}
