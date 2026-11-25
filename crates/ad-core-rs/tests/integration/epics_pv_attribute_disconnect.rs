//! An `EPICS_PV` attribute reads `Undefined` from the moment its PV
//! disconnects until the first value after the reconnect, so no frame taken
//! during the outage carries a value no IOC vouches for. C keeps the value
//! from before the outage.
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
use epics_ca_rs::EpicsValue;
use epics_ca_rs::client::CaClient;
use epics_ca_rs::server::blocking_serving::BlockingCaServing;
use epics_ca_rs::server::stats::ServerStats;
use fact_budget::FACT_BUDGET;

const PV: &str = "ADDISC:Value";

fn attribute_value(drv: &NDArrayDriverBase) -> NDAttrValue {
    drv.attributes()
        .get("Value")
        .expect("the attribute is loaded")
        .epics_pv_source()
        .expect("it is an EPICS_PV attribute")
        .evaluate()
}

/// The attribute's value once it is no longer `old`. The budget covers the
/// client's re-search after an outage, which alone takes about 10 s.
async fn value_other_than(drv: &NDArrayDriverBase, old: &NDAttrValue) -> NDAttrValue {
    let deadline = Instant::now() + FACT_BUDGET;
    loop {
        let value = attribute_value(drv);
        if value != *old {
            return value;
        }
        assert!(Instant::now() < deadline, "the attribute stayed {old:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// RTEMS-EXEC-MODEL-ALLOW(1): its CA server is the reactor-free RSRV, and its
// reactor is `#[tokio::test]`'s own, which the exec backend does not remove.
#[tokio::test]
async fn an_epics_pv_attribute_is_undefined_from_disconnect_to_the_next_value() {
    let (sdb, _) = IocBuilder::new()
        .pv(PV, EpicsValue::Double(1.5))
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
        "ADDISC",
        1,
        1 << 20,
        ASYN_CANBLOCK,
        InterfaceMask::NONE,
        InterfaceMask::NONE,
    )
    .expect("driver");
    drv.set_ca_client(client, tokio::runtime::Handle::current());
    let xml = format!(
        "<Attributes><Attribute name=\"Value\" type=\"EPICS_PV\" source=\"{PV}\"/></Attributes>"
    );
    drv.port_base
        .set_string_param(drv.params.attributes_file, 0, &xml)
        .expect("set NDAttributesFile");
    drv.read_nd_attributes_file()
        .expect("a fed EPICS_PV attribute loads clean");

    let connected = value_other_than(&drv, &NDAttrValue::Undefined).await;
    assert_eq!(connected, NDAttrValue::Float64(1.5));

    sdb.remove_simple_pv(PV).await.expect("the PV was served");
    assert_eq!(
        value_other_than(&drv, &connected).await,
        NDAttrValue::Undefined,
        "the disconnect leaves no value behind"
    );

    sdb.add_pv(PV, EpicsValue::Double(2.5))
        .await
        .expect("serve the PV again");
    assert_eq!(
        value_other_than(&drv, &NDAttrValue::Undefined).await,
        NDAttrValue::Float64(2.5),
        "the first value after the reconnect ends the Undefined"
    );
}
