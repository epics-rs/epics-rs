//! `table` link usability is the link's KIND, not its text.
//!
//! C `tableRecord.c::checkLinks` (2318-2352) asks each of the seven links per
//! axis two questions — `plink->type == CONSTANT` and, for a `CA_LINK`,
//! `dbCaIsLinkConnected(plink)`. Neither is answerable from the link string:
//! `dbLink.c:118-130` turns a name no local record carries into a `CA_LINK`
//! that never connects, and the module's own `table.db:145-156` declares a
//! five-motor table by leaving a macro empty, which still expands to link
//! TEXT. A table that classified links by `!is_empty()` therefore counted a
//! dangling sixth axis as a working motor: it never engaged the five-motor
//! geometry, drove `M2Z` from the user `Z` put, and emitted a write to a
//! record that does not exist.

#![cfg(tokio_backend)]
// Both cases reach the table through `table_with_sixth_axis`, which
// builds a real CA server. The reactor-free `exec_backend` — selected on
// a host build by `EPICS_RS_BUILD_EXEC_BACKEND=thread`, and
// unconditionally on RTEMS and VxWorks — has no
// `epics_ca_rs::server::CaServer` to build, so this file has no subject
// there. `[[test]] required-features` cannot name a build-script cfg,
// which is why the gate is here and not in `Cargo.toml`.

use std::collections::HashMap;

use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::server::CaServerBuilder;
use optics_rs::records::table::TableRecord;

/// A six-axis SRI table whose sixth axis (`M2Z`) is wired to `m2z_drive` /
/// `m2z_rbv`; the other five are on local `ao` records.
async fn table_with_sixth_axis(m2z_drive: &str, m2z_rbv: &str) -> epics_ca_rs::server::CaServer {
    let db_str = format!(
        r#"
record(ao, "M1") {{ }}
record(ao, "M2") {{ }}
record(ao, "M3") {{ }}
record(ao, "M4") {{ }}
record(ao, "M5") {{ }}
record(ao, "M6") {{ }}
record(table, "TBL") {{
    field(GEOM, "SRI")
    field(LX, "200")
    field(LZ, "300")
    field(SX, "100")
    field(SY, "50")
    field(SZ, "150")
    field(M0XL, "M1.VAL PP MS")
    field(M0YL, "M2.VAL PP MS")
    field(M1YL, "M3.VAL PP MS")
    field(M2XL, "M4.VAL PP MS")
    field(M2YL, "M5.VAL PP MS")
    field(M2ZL, "{m2z_drive}")
    field(R0XI, "M1.VAL NPP NMS")
    field(R0YI, "M2.VAL NPP NMS")
    field(R1YI, "M3.VAL NPP NMS")
    field(R2XI, "M4.VAL NPP NMS")
    field(R2YI, "M5.VAL NPP NMS")
    field(R2ZI, "{m2z_rbv}")
}}
"#
    );
    let macros = HashMap::new();
    CaServerBuilder::new()
        .port(0)
        .register_record_type("table", || Box::new(TableRecord::default()))
        .register_record_type("ao", || Box::new(AoRecord::default()))
        .db_string(&db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap()
}

async fn drive_z(server: &epics_ca_rs::server::CaServer, z: f64) {
    server
        .database()
        .put_record_field_from_ca("TBL", "Z", EpicsValue::Double(z))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
}

async fn as_f64(server: &epics_ca_rs::server::CaServer, pv: &str) -> f64 {
    match server.get(pv).await.unwrap() {
        EpicsValue::Double(v) => v,
        other => panic!("{pv} is not a double: {other:?}"),
    }
}

/// A sixth axis whose links name a record no IOC carries is C's disconnected
/// `CA_LINK`: `can_RW_drive` is 0, so `Z` is absorbed by the five-motor
/// geometry and `M2Z` is never driven.
#[tokio::test]
async fn dangling_sixth_axis_does_not_count_as_a_motor() {
    let server = table_with_sixth_axis("NOSUCH:m6.VAL PP MS", "NOSUCH:m6.VAL NPP NMS").await;
    drive_z(&server, 5.0).await;

    assert_eq!(
        as_f64(&server, "TBL.M2Z").await,
        0.0,
        "a dangling M2ZL/R2ZI pair must not be driven — C checkLinks reports \
         can_RW_drive = 0 for a CA_LINK that is not connected"
    );
    assert_eq!(
        as_f64(&server, "M6.VAL").await,
        0.0,
        "no write may reach a sixth-axis record the table is not linked to"
    );
}

/// The same table with all six axes on local records still drives all six —
/// the fix must not disable a link that IS usable.
#[tokio::test]
async fn six_local_axes_all_count_as_motors() {
    let server = table_with_sixth_axis("M6.VAL PP MS", "M6.VAL NPP NMS").await;
    drive_z(&server, 5.0).await;

    assert_eq!(
        as_f64(&server, "TBL.M2Z").await,
        5.0,
        "a local DB link is C's DB_LINK: the axis is drivable"
    );
    assert_eq!(
        as_f64(&server, "M6.VAL").await,
        5.0,
        "the sixth motor record must receive the drive value"
    );
}
