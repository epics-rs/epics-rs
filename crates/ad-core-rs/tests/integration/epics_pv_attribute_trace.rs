//! An `EPICS_PV` attribute traces its connect, its monitor events and its
//! disconnect on the global asyn trace, line for line as C's `PVAttribute`
//! (PVAttribute.cpp:122-129, :220-222, :288-290, :304-306).
//!
//! Needs the `ioc` feature.
#![cfg(feature = "ioc")]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ad_core_rs::attributes::{NDAttrValue, NDAttributeSource};
use ad_core_rs::driver::ndarray_driver::NDArrayDriverBase;
use ad_core_rs::plugin::runtime::ASYN_CANBLOCK;
use asyn_rs::port::InterfaceMask;
use asyn_rs::services::PortServices;
use asyn_rs::trace::{TraceFile, TraceMask};
use epics_base_rs::server::access_security::{new_acf_cell, parse_acf};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_ca_rs::EpicsValue;
use epics_ca_rs::client::CaClient;
use epics_ca_rs::protocol::ECA_NORDACCESS;
use epics_ca_rs::server::blocking_serving::BlockingCaServing;
use epics_ca_rs::server::stats::ServerStats;
use fact_budget::FACT_BUDGET;

const READABLE: &str = "ADTRACE:Value";
const DENIED: &str = "ADTRACE:Denied";
const DENY_READ: &str =
    "UAG(nobody) { nosuchuser }\nASG(DEFAULT) {\n RULE(1, READ) { UAG(nobody) }\n}\n";

fn attribute_value(drv: &NDArrayDriverBase, name: &str) -> NDAttrValue {
    drv.attributes()
        .get(name)
        .expect("the attribute is loaded")
        .epics_pv_source()
        .expect("it is an EPICS_PV attribute")
        .evaluate()
}

/// The trace lines so far, each from its `PVAttribute:` text on, past the
/// timestamp prefix.
fn traced(log: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .filter_map(|l| l.find("PVAttribute:").map(|i| l[i..].to_owned()))
        .collect()
}

/// Wait until a traced line satisfies `want`, and return it.
async fn line(log: &std::path::Path, want: impl Fn(&str) -> bool) -> String {
    let deadline = epics_libcom_rs::runtime::time::deadline_from_now(FACT_BUDGET);
    loop {
        if let Some(l) = traced(log).into_iter().find(|l| want(l)) {
            return l;
        }
        assert!(
            Instant::now() < deadline,
            "never traced; got {:#?}",
            traced(log)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn chan_id(line: &str) -> &str {
    let at = line.find("chanId=").expect("the line names the channel") + "chanId=".len();
    line[at..].split([',', ' ']).next().unwrap()
}

// RTEMS-EXEC-MODEL-ALLOW(1): its CA servers are the reactor-free RSRV, and
// its reactor is `#[tokio::test]`'s own, which the exec backend does not remove.
#[tokio::test]
async fn an_epics_pv_attribute_traces_connect_monitor_and_disconnect_on_the_global_trace() {
    let log = std::env::temp_dir().join(format!("pv-attribute-trace-{}.log", std::process::id()));
    let services = PortServices::global();
    let trace = services.trace();
    trace.set_trace_file(
        None,
        TraceFile::File(Arc::new(Mutex::new(std::fs::File::create(&log).unwrap()))),
    );
    trace.set_trace_mask(None, TraceMask::ERROR | TraceMask::FLOW);

    let (readable_db, _) = IocBuilder::new()
        .pv(READABLE, EpicsValue::Double(1.5))
        .build_served()
        .await
        .expect("build the readable database");
    let (denied_db, _) = IocBuilder::new()
        .pv(DENIED, EpicsValue::Double(2.5))
        .build_served()
        .await
        .expect("build the read-denied database");
    let readable = BlockingCaServing::start(
        0,
        0,
        readable_db.clone(),
        new_acf_cell(None),
        Arc::new(ServerStats::default()),
    )
    .expect("serve the readable PV");
    let denied = BlockingCaServing::start(
        0,
        0,
        denied_db,
        new_acf_cell(Some(parse_acf(DENY_READ).unwrap())),
        Arc::new(ServerStats::default()),
    )
    .expect("serve the read-denied PV");

    // SAFETY: under nextest, the repo's runner, each test runs in its own
    // process, and this sets the environment before `CaClient::new`
    // snapshots its resolver config.
    unsafe {
        std::env::set_var(
            "EPICS_CA_ADDR_LIST",
            format!(
                "127.0.0.1:{} 127.0.0.1:{}",
                readable.udp_port(),
                denied.udp_port()
            ),
        );
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
    }
    let client = Arc::new(CaClient::new().await.expect("CA client"));

    let mut drv = NDArrayDriverBase::new(
        "ADTRACE",
        1,
        1 << 20,
        ASYN_CANBLOCK,
        InterfaceMask::NONE,
        InterfaceMask::NONE,
    )
    .expect("driver");
    drv.set_ca_client(client, tokio::runtime::Handle::current());
    let xml = format!(
        "<Attributes>\
         <Attribute name=\"Value\" type=\"EPICS_PV\" source=\"{READABLE}\"/>\
         <Attribute name=\"Denied\" type=\"EPICS_PV\" source=\"{DENIED}\" dbrtype=\"DBR_LONG\"/>\
         </Attributes>"
    );
    drv.port_base
        .set_string_param(drv.params.attributes_file, 0, &xml)
        .expect("set NDAttributesFile");
    drv.read_nd_attributes_file()
        .expect("fed EPICS_PV attributes load clean");

    // A DBR_DOUBLE field read natively (-1) publishes an NDAttrFloat64 (9).
    let connect = line(&log, |l| {
        l.starts_with(&format!(
            "PVAttribute:connectCallback: Connect event, PV={READABLE}, chanId="
        )) && l.ends_with(", dbfType=6, elementCount=1, dbrType=-1")
    })
    .await;
    let typed = line(&log, |l| {
        l.starts_with(&format!(
            "PVAttribute:connectCallback: Connect event, PV={READABLE}, chanId="
        )) && l.ends_with(", type=9")
    })
    .await;
    assert_eq!(chan_id(&connect), chan_id(&typed));
    line(&log, |l| {
        l == format!("PVAttribute:monitorCallback: PV={READABLE}")
    })
    .await;

    // DBR_LONG (5) into an NDAttrInt32 (4); the IOC refuses the read.
    line(&log, |l| {
        l.starts_with(&format!(
            "PVAttribute:connectCallback: Connect event, PV={DENIED}, chanId="
        )) && l.ends_with(", dbfType=6, elementCount=1, dbrType=5")
    })
    .await;
    line(&log, |l| {
        l.starts_with(&format!(
            "PVAttribute:connectCallback: Connect event, PV={DENIED}, chanId="
        )) && l.ends_with(", type=4")
    })
    .await;
    line(&log, |l| {
        l == format!("PVAttribute:monitorCallback: PV={DENIED}")
    })
    .await;
    line(&log, |l| {
        l == format!("PVAttribute:monitorCallback: CA returns eha.status={ECA_NORDACCESS}")
    })
    .await;

    readable_db
        .remove_simple_pv(READABLE)
        .await
        .expect("the PV was served");
    let disconnect = line(&log, |l| {
        l.starts_with(&format!(
            "PVAttribute:connectCallback: Disconnect event, PV={READABLE}, chanId="
        ))
    })
    .await;
    assert_eq!(chan_id(&disconnect), chan_id(&connect));
    let deadline = epics_libcom_rs::runtime::time::deadline_from_now(FACT_BUDGET);
    while attribute_value(&drv, "Value") != NDAttrValue::Undefined {
        assert!(
            Instant::now() < deadline,
            "the disconnect never reached the value"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // libca hands a disconnect to the connection callback only, so the
    // monitor callback traces neither the event nor its status.
    let lines = traced(&log);
    assert_eq!(
        lines
            .iter()
            .filter(|l| **l == format!("PVAttribute:monitorCallback: PV={READABLE}"))
            .count(),
        1,
        "{lines:#?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("eha.status=192")),
        "{lines:#?}"
    );
    let _ = std::fs::remove_file(&log);
}
