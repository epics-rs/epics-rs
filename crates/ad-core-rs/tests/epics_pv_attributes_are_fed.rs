//! `EPICS_PV` attributes reach a written NDArray.
//!
//! `EpicsPvAttributeSource` evaluates whatever a CA monitor last wrote into
//! its `LiveValueCell`, and `spawn_ca_monitor` is that writer — but nothing
//! called it, so an `EPICS_PV` attribute evaluated `Undefined` for the life of
//! the IOC and every file writer dropped it. C++ `PVAttribute` has no
//! equivalent gap because it subscribes through the CA context the IOC already
//! owns; here the client is a value someone has to hand over, so the port
//! takes one and feeds every `EPICS_PV` attribute from it.
//!
//! Needs the `ioc` feature — without it there is no CA client to build a
//! feeder from, and the file is compiled away.
#![cfg(feature = "ioc")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use ad_core_rs::attributes::{NDAttrValue, NDAttributeSource};
use ad_core_rs::driver::ndarray_driver::{
    ATTR_STATUS_OK, ATTR_STATUS_XML_SYNTAX_ERROR, NDArrayDriverBase,
};
use epics_base_rs::server::records::ai::AiRecord;
use epics_ca_rs::client::CaClient;
use epics_ca_rs::server::CaServer;

const PV: &str = "ADATTR:Temp";

fn attributes_xml() -> String {
    format!(
        "<Attributes><Attribute name=\"Temp\" type=\"EPICS_PV\" source=\"{PV}\" \
         dbrtype=\"DBR_DOUBLE\"/></Attributes>"
    )
}

fn load_attributes(drv: &mut NDArrayDriverBase, xml: &str) -> Result<(), ()> {
    drv.port_base
        .set_string_param(drv.params.attributes_file, 0, xml.to_string())
        .expect("set NDAttributesFile");
    drv.read_nd_attributes_file().map_err(|_| ())
}

fn attribute_value(drv: &NDArrayDriverBase, name: &str) -> NDAttrValue {
    drv.attributes
        .get(name)
        .expect("the attribute is loaded")
        .epics_pv_source()
        .expect("it is an EPICS_PV attribute")
        .evaluate()
}

fn status(drv: &NDArrayDriverBase) -> i32 {
    drv.port_base
        .get_int32_param(drv.params.attributes_status, 0)
        .expect("NDAttributesStatus")
}

/// Without a client there is no feeder, and the port must keep saying so
/// rather than reporting the file as loaded.
#[test]
fn an_epics_pv_attribute_with_no_ca_client_stays_undefined_and_says_so() {
    let mut drv = NDArrayDriverBase::new("ADATTR_NOCLIENT", 1 << 20).expect("driver");

    assert!(
        load_attributes(&mut drv, &attributes_xml()).is_err(),
        "an unfed EPICS_PV attribute is not a successful load"
    );
    assert_eq!(status(&drv), ATTR_STATUS_XML_SYNTAX_ERROR);
    assert_eq!(attribute_value(&drv, "Temp"), NDAttrValue::Undefined);
}

/// The dispatched defect: with a client installed, the attribute tracks its PV.
#[tokio::test]
async fn an_epics_pv_attribute_is_fed_from_the_installed_ca_client() {
    let server = CaServer::builder()
        .port(0)
        .record(PV, AiRecord::new(42.5))
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    tokio::spawn(async move { server.run().await });

    // SAFETY: this is the only test in this file that touches the environment,
    // and it does so before `CaClient::new` snapshots its resolver config.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
    let client = Arc::new(CaClient::new().await.expect("CA client"));

    let mut drv = NDArrayDriverBase::new("ADATTR_FED", 1 << 20).expect("driver");
    drv.set_ca_client(client, tokio::runtime::Handle::current());

    load_attributes(&mut drv, &attributes_xml()).expect("a fed EPICS_PV attribute loads clean");
    assert_eq!(status(&drv), ATTR_STATUS_OK);

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match attribute_value(&drv, "Temp") {
            NDAttrValue::Float64(v) => {
                assert!((v - 42.5).abs() < 1e-9, "monitored value was {v}");
                break;
            }
            other => {
                assert!(
                    Instant::now() < deadline,
                    "the EPICS_PV attribute never left {other:?}"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
}
