//! R18-108: the array records' element-count / index / offset fields are
//! DBF_ULONG (or DBF_SHORT / DBF_USHORT) in C, not DBF_LONG.
//!
//! The `.dbd.pod` declarations:
//!
//! ```text
//! waveformRecord   NELM  DBF_ULONG   NORD  DBF_ULONG   HASH  DBF_ULONG
//! aaiRecord/aaoRecord — same NELM/NORD
//! subArrayRecord   MALM  DBF_ULONG   NELM  DBF_ULONG   INDX  DBF_ULONG
//!                  NORD  DBF_LONG                       <- genuinely signed
//! compressRecord   NSAM  DBF_ULONG   N     DBF_ULONG   OFF   DBF_ULONG
//!                  NUSE  DBF_ULONG   OUSE  DBF_ULONG   INX   DBF_ULONG
//!                  INPN  DBF_LONG                       <- genuinely signed
//! histogramRecord  NELM  DBF_USHORT  MDEL  DBF_SHORT   MCNT  DBF_SHORT
//! ```
//!
//! The port declared every one of them `DbFieldType::Long` / served
//! `EpicsValue::Long`, so both wires derived the wrong type from the wrong
//! declaration: PVA introspected `int32` where pvxs serves `uint32`
//! (`ioc/typeutils.cpp:43-44`), and CA served DBR_LONG where C promotes
//! DBF_ULONG to DBR_DOUBLE (`db_convert.h`:
//! `dbDBRnewToDBRold[DBR_ULONG] = DBR_DOUBLE`).
//!
//! Ground truth — `cainfo` against the compiled softIoc
//! (`/home/stevek/work/epics-base/bin/linux-x86_64`):
//!
//! ```text
//! WF.NELM  DBF_DOUBLE     SA.NELM  DBF_DOUBLE     CMP.NSAM  DBF_DOUBLE
//! WF.NORD  DBF_DOUBLE     SA.NORD  DBF_LONG       CMP.NUSE  DBF_DOUBLE
//!                         SA.MALM  DBF_DOUBLE     CMP.OUSE  DBF_DOUBLE
//!                         SA.INDX  DBF_DOUBLE     CMP.OFF   DBF_DOUBLE
//!                                                 CMP.INX   DBF_DOUBLE
//!                                                 CMP.N     DBF_DOUBLE
//!                                                 CMP.INPN  DBF_LONG
//! HG.NELM  DBF_LONG   (DBF_USHORT promotes to DBR_LONG)
//! HG.MDEL  DBF_SHORT      HG.MCNT  DBF_SHORT
//! ```

// RTEMS-EXEC-MODEL-ALLOW(4): checked - these run and pass in the feature-ON suite.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(waveform, "WF")  { field(FTVL,"DOUBLE") field(NELM,"8") }
record(aai,      "AAI") { field(FTVL,"DOUBLE") field(NELM,"8") }
record(aao,      "AAO") { field(FTVL,"DOUBLE") field(NELM,"8") }
record(subArray, "SA")  { field(FTVL,"DOUBLE") field(MALM,"8") field(NELM,"4")
                          field(INDX,"1") field(INP,"WF") }
record(compress, "CMP") { field(NSAM,"4") field(ALG,"Circular Buffer") field(INP,"WF") }
record(histogram,"HG")  { field(NELM,"4") field(SVL,"1") field(LLIM,"0") field(ULIM,"4")
                          field(MDEL,"2") }
"#;

async fn build() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn field(db: &PvDatabase, pv: &str) -> EpicsValue {
    db.get_pv(pv)
        .unwrap_or_else(|e| panic!("{pv} must be served: {e:?}"))
}

/// The unsigned-long family: every count / index / offset field on
/// waveform, aai, aao, subArray and compress.
#[tokio::test]
async fn the_count_fields_are_unsigned_long() {
    let db = build().await;

    for pv in [
        "WF.NELM", "WF.NORD", "AAI.NELM", "AAI.NORD", "AAO.NELM", "AAO.NORD", "SA.MALM", "SA.NELM",
        "SA.INDX", "CMP.NSAM", "CMP.N", "CMP.OFF", "CMP.NUSE", "CMP.OUSE", "CMP.INX",
    ] {
        let v = field(&db, pv).await;
        assert!(
            matches!(v, EpicsValue::ULong(_)),
            "{pv} is DBF_ULONG in the dbd, got {v:?}"
        );
    }
}

/// The two that are genuinely signed. A blanket "make the counters unsigned"
/// sweep would have broken these; the dbd distinguishes them.
#[tokio::test]
async fn subarray_nord_and_compress_inpn_stay_signed_long() {
    let db = build().await;

    for pv in ["SA.NORD", "CMP.INPN"] {
        let v = field(&db, pv).await;
        assert!(
            matches!(v, EpicsValue::Long(_)),
            "{pv} is DBF_LONG in the dbd, got {v:?}"
        );
    }
}

/// histogram's counters are the 16-bit pair, and NELM is the unsigned one.
#[tokio::test]
async fn histogram_nelm_is_ushort_and_mdel_mcnt_are_short() {
    let db = build().await;

    let nelm = field(&db, "HG.NELM").await;
    assert!(
        matches!(nelm, EpicsValue::UShort(4)),
        "histogram NELM is DBF_USHORT (histogramRecord.dbd.pod:163), got {nelm:?}"
    );
    for pv in ["HG.MDEL", "HG.MCNT"] {
        let v = field(&db, pv).await;
        assert!(
            matches!(v, EpicsValue::Short(_)),
            "{pv} is DBF_SHORT in the dbd, got {v:?}"
        );
    }
}

/// The point of the declaration: it is what each wire projects the native
/// type from. This pins the CA half against the `cainfo` transcript above —
/// DBF_ULONG promotes to DBR_DOUBLE, DBF_USHORT to DBR_LONG, DBF_SHORT stays.
#[tokio::test]
async fn the_ca_native_type_matches_the_compiled_ioc() {
    let db = build().await;

    for pv in [
        "WF.NELM", "WF.NORD", "SA.MALM", "SA.NELM", "SA.INDX", "CMP.NSAM", "CMP.NUSE", "CMP.OUSE",
        "CMP.OFF", "CMP.INX", "CMP.N",
    ] {
        assert_eq!(
            field(&db, pv).await.dbr_type(),
            DbFieldType::Double,
            "cainfo {pv} -> Native data type: DBF_DOUBLE"
        );
    }
    for pv in ["SA.NORD", "CMP.INPN", "HG.NELM"] {
        assert_eq!(
            field(&db, pv).await.dbr_type(),
            DbFieldType::Long,
            "cainfo {pv} -> Native data type: DBF_LONG"
        );
    }
    for pv in ["HG.MDEL", "HG.MCNT"] {
        assert_eq!(
            field(&db, pv).await.dbr_type(),
            DbFieldType::Short,
            "cainfo {pv} -> Native data type: DBF_SHORT"
        );
    }
}
