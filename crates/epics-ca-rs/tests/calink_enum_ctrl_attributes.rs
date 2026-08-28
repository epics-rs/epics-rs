//! A CA link's attribute get is a FIXED `DBR_CTRL_DOUBLE`, gated on the
//! native type — not a CTRL get at whatever the native type happens to be.
//!
//! C `dbCa.c` decides both halves from `pca->dbrType`, which is
//! `ca_field_type(chid)` (`:864`). At `:878-880` it sets
//! `CA_GET_ATTRIBUTES` for every channel whose native type is not
//! `DBR_STRING`, and at `:1210` it issues that request as
//! `ca_get_callback(DBR_CTRL_DOUBLE, ...)` — a fixed type, whatever the
//! channel's native type is. So for an ENUM target the server converts,
//! `getAttribEventCallback` (`:1042-1091`) sets `gotAttributes = TRUE`, and
//! `dbCaGetPrecision` (`:747-757`) then SUCCEEDS with precision 0, empty
//! units, zeroed display and control limits, and four NaN alarm limits —
//! `mbbiRecord.c:63` leaves `get_alarm_double` NULL, and C's `get_alarm`
//! seeds that group `epicsNAN` rather than memsetting it
//! (`dbAccess.c:294`). Measured on C softIoc R7.0.10:
//! `caget -d DBR_CTRL_DOUBLE <mbbi>` prints `nan` for all four.
//!
//! Requesting CTRL at the native type instead puts `DBR_CTRL_ENUM` on the
//! wire for an enum channel, and `struct dbr_ctrl_enum` (`db_access.h`) has
//! no precision, units or limit members at all — so every attribute came
//! back absent where C serves a value. ENUM is the only divergent native
//! type: DBR_STRING is parity because C skips the fetch entirely, and every
//! numeric native type already converts to the same DOUBLE attributes.

#![cfg(tokio_backend)]
#![cfg(feature = "client-core")]

use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::database::{LinkDbfType, LinkMetadata, LinkSet};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::mbbi::MbbiRecord;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::calink::CaLinkResolver;
use epics_ca_rs::client::{CaClient, CaClientConfig};
use epics_ca_rs::server::CaServer;
use serial_test::serial;

/// Point the ambient `EPICS_CA_*` env at `127.0.0.1:port`. Tests here are
/// `#[serial(epics_env)]` so the process-wide env is not raced.
fn pin_env(port: u16) {
    // SAFETY: serialized by `#[serial(epics_env)]`; no other thread
    // reads/writes these vars concurrently.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
}

/// Connect a resolver to `pv` on `port` and poll until the detached
/// attribute fetch has stored metadata satisfying `ready`.
async fn metadata_when(
    resolver: &CaLinkResolver,
    pv: &str,
    ready: impl Fn(&LinkMetadata) -> bool,
) -> LinkMetadata {
    assert!(
        resolver
            .wait_for_link_connected(pv, budget::FACT_BUDGET)
            .await,
        "CA link must connect to the upstream CA server"
    );
    let deadline = std::time::Instant::now() + budget::FACT_BUDGET;
    loop {
        if let Some(md) = LinkSet::link_metadata(resolver, pv)
            && ready(&md)
        {
            return md;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "CA link metadata for {pv} never reached the expected shape"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The trigger. An mbbi target's attributes must land exactly as C's fixed
/// `DBR_CTRL_DOUBLE` delivers them — precision 0 and zeroed limits, present
/// rather than absent — while the state-label table still arrives, because
/// the labels ride their own native `DBR_CTRL_ENUM` get.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn an_enum_target_serves_the_converted_double_attributes() {
    let mut src = MbbiRecord::new(1);
    src.zrst = "off".into();
    src.onst = "on".into();
    let server = CaServer::builder()
        .port(0)
        .record("CALINK:ENUMATTR:SRC", src)
        .build()
        .await
        .expect("CA server");
    let port = server.udp_port();
    let _server = tokio::spawn(async move { server.run().await });

    pin_env(port);
    let client = Arc::new(
        CaClient::new_with_config(CaClientConfig::default())
            .await
            .expect("CA client"),
    );
    let resolver = CaLinkResolver::with_client(client);

    let md = metadata_when(&resolver, "CALINK:ENUMATTR:SRC", |md| {
        md.enum_choices.is_some() && md.precision.is_some()
    })
    .await;

    assert_eq!(md.dbf_type, Some(LinkDbfType::Enum), "mbbi VAL is DBF_ENUM");
    assert_eq!(md.element_count, Some(1));
    assert_eq!(
        md.precision,
        Some(0),
        "C's fixed DBR_CTRL_DOUBLE makes dbCaGetPrecision succeed with 0 (dbCa.c:747-757)"
    );
    assert_eq!(
        md.graphic_limits,
        Some((0.0, 0.0)),
        "the converted reply carries zeroed display limits, not no limits"
    );
    assert_eq!(md.control_limits, Some((0.0, 0.0)));
    // Not zero, and not absent: the alarm group is the one C seeds NaN, so
    // the link stores four NaNs. `assert_eq!` cannot express this.
    let (lolo, low, high, hihi) = md
        .alarm_limits
        .expect("the group is present, just not finite");
    for (name, v) in [("lolo", lolo), ("low", low), ("high", high), ("hihi", hihi)] {
        assert!(v.is_nan(), "{name} is {v}, C serves nan");
    }
    assert_eq!(
        md.units, None,
        "an empty units string stays None; C copies a zero-length string"
    );
    assert_eq!(
        md.enum_choices,
        Some(vec!["off".to_string(), "on".to_string()]),
        "the label table still arrives, on its own native DBR_CTRL_ENUM get"
    );
}

/// The fixed request must not cost a numeric target anything: an ai's real
/// EGU/PREC/HOPR/LOPR arrive unchanged, because C's DBR_CTRL_DOUBLE and the
/// native DBR_CTRL_DOUBLE are the same request for a DBF_DOUBLE channel.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_numeric_target_keeps_its_real_attributes() {
    let mut src = AiRecord::new(50.0);
    src.egu = "degC".into();
    src.hopr = 100.0;
    src.lopr = -50.0;
    src.prec = 3;
    let server = CaServer::builder()
        .port(0)
        .record("CALINK:ENUMATTR:AI", src)
        .build()
        .await
        .expect("CA server");
    let port = server.udp_port();
    let _server = tokio::spawn(async move { server.run().await });

    pin_env(port);
    let client = Arc::new(
        CaClient::new_with_config(CaClientConfig::default())
            .await
            .expect("CA client"),
    );
    let resolver = CaLinkResolver::with_client(client);

    let md = metadata_when(&resolver, "CALINK:ENUMATTR:AI", |md| {
        md.graphic_limits.is_some()
    })
    .await;

    assert_eq!(md.dbf_type, Some(LinkDbfType::Double));
    assert_eq!(md.graphic_limits, Some((-50.0, 100.0)));
    assert_eq!(md.control_limits, Some((-50.0, 100.0)));
    assert_eq!(md.precision, Some(3));
    assert_eq!(md.units.as_deref(), Some("degC"));
}

/// The other half of the same `pca->dbrType` gate: a DBR_STRING channel
/// gets no attribute request at all, so every attribute stays absent and
/// the owning record keeps its local defaults. This case reads the same
/// before and after the fixed-type change — a native CTRL get on a string
/// channel carried no attributes either — and is here to pin the gate, not
/// as evidence of it.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_string_target_fetches_no_attributes() {
    let server = CaServer::builder()
        .port(0)
        .pv("CALINK:ENUMATTR:STR", EpicsValue::String("hello".into()))
        .build()
        .await
        .expect("CA server");
    let port = server.udp_port();
    let _server = tokio::spawn(async move { server.run().await });

    pin_env(port);
    let client = Arc::new(
        CaClient::new_with_config(CaClientConfig::default())
            .await
            .expect("CA client"),
    );
    let resolver = CaLinkResolver::with_client(client);

    let md = metadata_when(&resolver, "CALINK:ENUMATTR:STR", |md| md.dbf_type.is_some()).await;

    assert_eq!(md.dbf_type, Some(LinkDbfType::String));
    assert_eq!(md.element_count, Some(1));
    assert_eq!(
        md.precision, None,
        "C never issues the get (dbCa.c:878-880)"
    );
    assert_eq!(md.graphic_limits, None);
    assert_eq!(md.control_limits, None);
    assert_eq!(md.alarm_limits, None);
    assert_eq!(md.units, None);
}

#[path = "common/budget.rs"]
mod budget;
