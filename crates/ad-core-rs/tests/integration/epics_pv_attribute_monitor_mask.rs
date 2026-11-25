//! An `EPICS_PV` attribute follows only `DBE_VALUE` posts of its PV, as C's
//! `PVAttribute` subscribes with `DBE_VALUE` alone (PVAttribute.cpp:294-301).
//!
//! A change inside MDEL that crosses ADEL posts `DBE_LOG` only, and a change
//! inside both that moves the alarm posts `DBE_ALARM` only; both carry the
//! new value, and C's attribute keeps the last MDEL-crossing value instead.
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
use epics_base_rs::server::access_security::new_acf_cell;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::longin::LonginRecord;
use epics_ca_rs::EpicsValue;
use epics_ca_rs::client::CaClient;
use epics_ca_rs::server::blocking_serving::BlockingCaServing;
use epics_ca_rs::server::stats::ServerStats;
use fact_budget::FACT_BUDGET;

const LOG_PV: &str = "ADMASK:Log";
const ALARM_PV: &str = "ADMASK:Alarm";

fn attribute_value(drv: &NDArrayDriverBase, name: &str) -> NDAttrValue {
    drv.attributes()
        .get(name)
        .expect("the attribute is loaded")
        .epics_pv_source()
        .expect("it is an EPICS_PV attribute")
        .evaluate()
}

async fn wait_for(drv: &NDArrayDriverBase, name: &str, want: NDAttrValue) {
    let deadline = Instant::now() + FACT_BUDGET;
    loop {
        let got = attribute_value(drv, name);
        if got == want {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the {name} attribute stayed {got:?}, expected {want:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The monitor events the attribute on `pv` has traced so far.
fn monitor_events(log: &std::path::Path, pv: &str) -> usize {
    let line = format!("PVAttribute:monitorCallback: PV={pv}");
    std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .filter(|l| l.ends_with(&line))
        .count()
}

fn deadbanded(mdel: i32, adel: i32) -> LonginRecord {
    let mut rec = LonginRecord::new(0);
    rec.mdel = mdel;
    rec.adel = adel;
    rec
}

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — ran and passed on the
// exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p
// ad-core-rs --features ioc`); its CA server is the reactor-free RSRV, and
// its reactor is `#[tokio::test]`'s own, which the backend does not remove.
#[tokio::test]
async fn log_only_and_alarm_only_posts_leave_the_attribute_alone() {
    let log_file = std::env::temp_dir().join(format!(
        "pv-attribute-monitor-mask-{}.log",
        std::process::id()
    ));
    let services = PortServices::global();
    let trace = services.trace();
    trace.set_trace_file(
        None,
        TraceFile::File(Arc::new(Mutex::new(
            std::fs::File::create(&log_file).unwrap(),
        ))),
    );
    trace.set_trace_mask(None, TraceMask::ERROR | TraceMask::FLOW);

    let (db, _) = IocBuilder::new()
        .record(LOG_PV, deadbanded(100, 0))
        .record(ALARM_PV, deadbanded(100, 100))
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

    let hihi = client.create_channel(format!("{ALARM_PV}.HIHI"));
    hihi.wait_connected(FACT_BUDGET)
        .await
        .expect("HIHI connects");
    hihi.put_with_timeout(&EpicsValue::Long(10), FACT_BUDGET)
        .await
        .expect("put HIHI");
    let hhsv = client.create_channel(format!("{ALARM_PV}.HHSV"));
    hhsv.wait_connected(FACT_BUDGET)
        .await
        .expect("HHSV connects");
    hhsv.put_string("MAJOR", FACT_BUDGET)
        .await
        .expect("put HHSV");
    let log = client.create_channel(LOG_PV);
    let alarm = client.create_channel(ALARM_PV);
    log.wait_connected(FACT_BUDGET).await.expect("Log connects");
    alarm
        .wait_connected(FACT_BUDGET)
        .await
        .expect("Alarm connects");
    // Process both once so their UDF alarm clears before the attributes
    // subscribe; that clearing is itself an alarm change.
    for chan in [&log, &alarm] {
        chan.put_with_timeout(&EpicsValue::Long(0), FACT_BUDGET)
            .await
            .expect("process at 0");
    }

    let mut drv = NDArrayDriverBase::new(
        "ADMASK",
        1,
        1 << 20,
        ASYN_CANBLOCK,
        InterfaceMask::NONE,
        InterfaceMask::NONE,
    )
    .expect("driver");
    drv.set_ca_client(client.clone(), tokio::runtime::Handle::current());
    let xml = format!(
        "<Attributes><Attribute name=\"Log\" type=\"EPICS_PV\" source=\"{LOG_PV}\"/>\
         <Attribute name=\"Alarm\" type=\"EPICS_PV\" source=\"{ALARM_PV}\"/></Attributes>"
    );
    drv.port_base
        .set_string_param(drv.params.attributes_file, 0, &xml)
        .expect("set NDAttributesFile");
    drv.read_nd_attributes_file()
        .expect("a fed EPICS_PV attribute loads clean");
    wait_for(&drv, "Log", NDAttrValue::Int32(0)).await;
    wait_for(&drv, "Alarm", NDAttrValue::Int32(0)).await;
    let log_events = monitor_events(&log_file, LOG_PV);
    let alarm_events = monitor_events(&log_file, ALARM_PV);

    // Inside MDEL=100 but past ADEL=0: DBE_LOG only.
    log.put_with_timeout(&EpicsValue::Long(5), FACT_BUDGET)
        .await
        .expect("put Log");
    // Inside both deadbands, past HIHI=10: DBE_ALARM only.
    alarm
        .put_with_timeout(&EpicsValue::Long(20), FACT_BUDGET)
        .await
        .expect("put Alarm");
    // Past MDEL: DBE_VALUE, which the attribute does follow.
    log.put_with_timeout(&EpicsValue::Long(500), FACT_BUDGET)
        .await
        .expect("put Log");
    alarm
        .put_with_timeout(&EpicsValue::Long(500), FACT_BUDGET)
        .await
        .expect("put Alarm");
    wait_for(&drv, "Log", NDAttrValue::Int32(500)).await;
    wait_for(&drv, "Alarm", NDAttrValue::Int32(500)).await;
    // CA delivers a subscription's events in order, so a masked post that
    // had been delivered is traced before the 500 is.
    assert_eq!(monitor_events(&log_file, LOG_PV), log_events + 1);
    assert_eq!(monitor_events(&log_file, ALARM_PV), alarm_events + 1);
    let _ = std::fs::remove_file(&log_file);
}
