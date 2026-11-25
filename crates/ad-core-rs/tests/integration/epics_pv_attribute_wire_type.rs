//! An `EPICS_PV` attribute reads its PV in the DBR type C's `PVAttribute`
//! requests on the wire, chosen once at the first connect
//! (PVAttribute.cpp:217-294), so the IOC does the conversion:
//!
//! - a CHAR waveform asked for as `DBR_STRING` is read as its char array and
//!   published as the text up to its first NUL;
//! - `DBR_SHORT` on a CHAR field converts the byte with the field's sign;
//! - a reconnect to a PV of another type keeps the first connect's request.
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
use epics_base_rs::server::records::waveform::WaveformRecord;
use epics_ca_rs::client::CaClient;
use epics_ca_rs::server::blocking_serving::BlockingCaServing;
use epics_ca_rs::server::stats::ServerStats;
use epics_ca_rs::{DbFieldType, EpicsValue};
use fact_budget::FACT_BUDGET;

const TEXT_PV: &str = "ADWIRE:Text";
const SCHAR_PV: &str = "ADWIRE:SChar";
const UCHAR_PV: &str = "ADWIRE:UChar";
const RETYPE_PV: &str = "ADWIRE:Retype";

fn attribute_value(drv: &NDArrayDriverBase, name: &str) -> NDAttrValue {
    drv.attributes()
        .get(name)
        .expect("the attribute is loaded")
        .epics_pv_source()
        .expect("it is an EPICS_PV attribute")
        .evaluate()
}

/// The attribute's first value that is neither `old` nor the `Undefined` an
/// outage leaves. The budget is the workspace's `FACT_BUDGET`: the client's
/// re-search after an outage alone takes about 10 s.
async fn value_other_than(drv: &NDArrayDriverBase, name: &str, old: &NDAttrValue) -> NDAttrValue {
    let deadline = Instant::now() + FACT_BUDGET;
    loop {
        let value = attribute_value(drv, name);
        if value != *old && value != NDAttrValue::Undefined {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "the {name} attribute stayed {old:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn waveform(nelm: i32, ftvl: DbFieldType, val: EpicsValue) -> WaveformRecord {
    let mut rec = WaveformRecord::new(nelm, ftvl);
    rec.put_field("VAL", val).expect("seed the waveform");
    rec
}

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — ran and passed on the
// exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p
// ad-core-rs --features ioc`); its CA server is the reactor-free RSRV, and
// its reactor is `#[tokio::test]`'s own, which the backend does not remove.
#[tokio::test]
async fn epics_pv_attributes_request_c_dbr_type_fixed_at_first_connect() {
    let (sdb, _) = IocBuilder::new()
        .record(
            TEXT_PV,
            waveform(
                8,
                DbFieldType::Char,
                EpicsValue::CharArray(b"ABC".to_vec().into()),
            ),
        )
        .record(
            SCHAR_PV,
            waveform(
                1,
                DbFieldType::Char,
                EpicsValue::CharArray(vec![200].into()),
            ),
        )
        .record(
            UCHAR_PV,
            waveform(
                1,
                DbFieldType::UChar,
                EpicsValue::UCharArray(vec![200].into()),
            ),
        )
        .pv(RETYPE_PV, EpicsValue::Long(7))
        .build_served()
        .await
        .expect("build the database");
    let server = BlockingCaServing::start(
        0,
        0,
        sdb.clone(),
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
        "ADWIRE",
        1,
        1 << 20,
        ASYN_CANBLOCK,
        InterfaceMask::NONE,
        InterfaceMask::NONE,
    )
    .expect("driver");
    drv.set_ca_client(client.clone(), tokio::runtime::Handle::current());

    let xml = format!(
        "<Attributes>\
         <Attribute name=\"Text\" type=\"EPICS_PV\" source=\"{TEXT_PV}\" dbrtype=\"DBR_STRING\"/>\
         <Attribute name=\"SChar\" type=\"EPICS_PV\" source=\"{SCHAR_PV}\" dbrtype=\"DBR_SHORT\"/>\
         <Attribute name=\"UChar\" type=\"EPICS_PV\" source=\"{UCHAR_PV}\" dbrtype=\"DBR_SHORT\"/>\
         <Attribute name=\"Retype\" type=\"EPICS_PV\" source=\"{RETYPE_PV}\"/>\
         </Attributes>"
    );
    drv.port_base
        .set_string_param(drv.params.attributes_file, 0, &xml)
        .expect("set NDAttributesFile");
    drv.read_nd_attributes_file()
        .expect("a fed EPICS_PV attribute loads clean");

    let unset = NDAttrValue::Undefined;
    assert_eq!(
        value_other_than(&drv, "Text", &unset).await,
        NDAttrValue::String("ABC".into())
    );
    assert_eq!(
        value_other_than(&drv, "SChar", &unset).await,
        NDAttrValue::Int16(-56)
    );
    assert_eq!(
        value_other_than(&drv, "UChar", &unset).await,
        NDAttrValue::Int16(200)
    );
    let first = value_other_than(&drv, "Retype", &unset).await;
    assert_eq!(first, NDAttrValue::Int32(7));

    // Serve the PV again as a DOUBLE: the attribute stays the DBR_LONG its
    // first connect asked for, and the IOC converts the new value to it.
    let probe = client.create_channel(RETYPE_PV);
    probe
        .wait_connected(FACT_BUDGET)
        .await
        .expect("the probe connects");
    sdb.remove_simple_pv(RETYPE_PV)
        .await
        .expect("the PV was served");
    let deadline = Instant::now() + FACT_BUDGET;
    while probe.info().await.is_ok() {
        assert!(Instant::now() < deadline, "the client sees the outage");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    sdb.add_pv(RETYPE_PV, EpicsValue::Double(2.0))
        .await
        .expect("serve the PV again");
    assert_eq!(
        value_other_than(&drv, "Retype", &first).await,
        NDAttrValue::Int32(2)
    );
}
