//! NORD at load is the count the DEVICE SUPPORT put in the buffer.
//!
//! The record's `init_record` seeds `nord = (nelm == 1)` on waveform, aai and
//! aao (`waveformRecord.c:100`, `aaiRecord.c:113`, `aaoRecord.c:116-120`), but
//! the soft dset's own `init_record` runs in the same iocInit and OVERWRITES it
//! on two of the three. Measured against the compiled softIoc (7.0.10.1-DEV),
//! one row per link shape — this is the table the port is checked against:
//!
//! ```text
//! record                                          C NORD   C UDF
//! waveform, NELM=1, no INP                            0      1
//! waveform, NELM=1, INP="OTHER:PV"                    0      1
//! waveform, NELM=5, INP="[1,2,3]"                     3      0
//! waveform, NELM=5, INP="OTHER:PV"                    0      1
//! aai,      NELM=1, no INP                            1      1
//! aai,      NELM=1, INP="OTHER:PV"                    1      1
//! aai,      NELM=5, INP="[1,2,3]"                     3      0
//! aai,      NELM=5, INP="OTHER:PV"                    0      1
//! aao,      NELM=1, no OUT                            0      1
//! aao,      NELM=1, OUT="OTHER:PV"                    0      1
//! aao,      NELM=5, OUT="OTHER:PV"                    0      1
//! aao,      NELM=5, OMSL=closed_loop, DOL="[1,2]"     2      0
//! subArray, MALM=1, no INP                            0      1
//! subArray, MALM=5, NELM=2, INDX=1, INP="[1,2,3,4]"   2      0
//! ```
//!
//! The three kinds differ because their three dsets differ:
//!
//! * `devWfSoft.c:39-51` calls `dbLoadLinkArray` unconditionally and sets
//!   `nord = 0` when it fails — and it fails for anything but a constant
//!   (`dbLink.c:255-264`: no `loadArray` lset ⇒ `S_db_noLSET`). The seed never
//!   survives.
//! * `devAaiSoft.c:55` loads only `if (dbLinkIsConstant(plink))` and leaves the
//!   record's state alone otherwise. The seed survives.
//! * `devAaoSoft.c:43-51` is `if (dbLinkIsConstant(&prec->out)) prec->nord = 0;`
//!   and runs at pass 0, BEFORE `doResolveLinks` (`iocInit.c::initDatabase`),
//!   when every link still reads as a constant. The seed never survives; only a
//!   constant closed-loop DOL (`fetchValue(prec,1)`, pass 1) puts elements in.
//!
//! The boundaries here are (kind) x (constant INP / real link / no link) x
//! (NELM == 1 / NELM > 1) — the two axes that decide which of the three rules
//! above is the one that answers.

// RTEMS-EXEC-MODEL-ALLOW(2): checked - these run and pass in the feature-ON suite.

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

async fn db_of(db_text: &str) -> Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(db_text, &std::collections::HashMap::new())
        .expect("db parses")
        .build()
        .await
        .expect("db loads")
        .0
}

async fn nord_udf(db: &PvDatabase, rec: &str) -> (i64, i64) {
    let n = match db.get_pv(&format!("{rec}.NORD")).unwrap() {
        EpicsValue::ULong(v) => v as i64,
        EpicsValue::Long(v) => v as i64,
        other => panic!("{rec}.NORD: unexpected type {other:?}"),
    };
    let u = match db.get_pv(&format!("{rec}.UDF")).unwrap() {
        EpicsValue::UChar(v) => v as i64,
        EpicsValue::Char(v) => v as i64,
        other => panic!("{rec}.UDF: unexpected type {other:?}"),
    };
    (n, u)
}

const DB: &str = r#"
record(ai,"T:SRC") { field(VAL,"7") }
record(waveform,"W:BARE") {}
record(aai,"A:BARE") {}
record(aao,"O:BARE") {}
record(subArray,"S:BARE") {}
record(waveform,"W:CONST") { field(FTVL,"DOUBLE") field(NELM,"5") field(INP,"[1,2,3]") }
record(waveform,"W:DBLINK") { field(FTVL,"DOUBLE") field(NELM,"5") field(INP,"T:SRC") }
record(waveform,"W:ONE_DB") { field(FTVL,"DOUBLE") field(NELM,"1") field(INP,"T:SRC") }
record(aai,"A:CONST") { field(FTVL,"DOUBLE") field(NELM,"5") field(INP,"[1,2,3]") }
record(aai,"A:DBLINK") { field(FTVL,"DOUBLE") field(NELM,"5") field(INP,"T:SRC") }
record(aai,"A:ONE_DB") { field(FTVL,"DOUBLE") field(NELM,"1") field(INP,"T:SRC") }
record(aao,"O:OUTLINK") { field(FTVL,"DOUBLE") field(NELM,"1") field(OUT,"T:SRC") }
record(aao,"O:BIGOUT") { field(FTVL,"DOUBLE") field(NELM,"5") field(OUT,"T:SRC") }
record(aao,"O:DOLCONST") { field(FTVL,"DOUBLE") field(NELM,"5") field(OMSL,"closed_loop")
                           field(DOL,"[1,2]") field(OUT,"T:SRC") }
record(subArray,"S:CONST") { field(FTVL,"DOUBLE") field(MALM,"5") field(NELM,"2")
                             field(INDX,"1") field(INP,"[1,2,3,4]") }
"#;

/// Every row of the measured table, in one pass over one loaded database — the
/// same `.db` the softIoc was given.
#[tokio::test]
async fn r21_nord_at_init_matches_the_soft_dset() {
    let db = db_of(DB).await;

    // (record, NORD, UDF) as C serves them.
    let expected = [
        ("W:BARE", 0, 1),
        ("A:BARE", 1, 1),
        ("O:BARE", 0, 1),
        ("S:BARE", 0, 1),
        ("W:CONST", 3, 0),
        ("W:DBLINK", 0, 1),
        ("W:ONE_DB", 0, 1),
        ("A:CONST", 3, 0),
        ("A:DBLINK", 0, 1),
        ("A:ONE_DB", 1, 1),
        ("O:OUTLINK", 0, 1),
        ("O:BIGOUT", 0, 1),
        ("O:DOLCONST", 2, 0),
        ("S:CONST", 2, 0),
    ];

    let mut wrong = Vec::new();
    for (rec, nord, udf) in expected {
        let got = nord_udf(&db, rec).await;
        if got != (nord, udf) {
            wrong.push(format!(
                "{rec}: C (NORD={nord}, UDF={udf}), port (NORD={}, UDF={})",
                got.0, got.1
            ));
        }
    }
    assert!(wrong.is_empty(), "NORD/UDF at init off C:\n{wrong:#?}");
}

/// The NELM=1 seed is the thing the three dsets disagree about, so it gets its
/// own case: same NELM, same (absent) link, three different answers, and each
/// answer is its dset's.
#[tokio::test]
async fn r21_the_nelm_one_seed_survives_only_on_aai() {
    let db = db_of(DB).await;
    assert_eq!(nord_udf(&db, "W:BARE").await.0, 0, "devWfSoft zeroes it");
    assert_eq!(nord_udf(&db, "A:BARE").await.0, 1, "devAaiSoft keeps it");
    assert_eq!(nord_udf(&db, "O:BARE").await.0, 0, "devAaoSoft zeroes it");
}
