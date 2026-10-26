//! C `dbNameToAddr` addresses a field by its `dbfType` (`dbAccess.c:636-647`):
//! the declared `DBF_*` code, retyped only by a `SPC_DBADDR` field's
//! `cvt_dbaddr` — never `mapDBFToDBR`'s collapse of it. A menu stays
//! `DBF_MENU`, `DTYP` `DBF_DEVICE`, a link its link class, and a field nothing
//! retypes (`TIME`, `BKPT`, `MLOK`, `calc.RPCL`) `DBF_NOACCESS`. sseq keeps the
//! code in `DTn`/`LTn` (`sseqRecord.c:208-211`, `:227-230`) and its step
//! switches have no case for `DBF_NOACCESS` (`:643-705`, `:714-792`), so such
//! a step neither reads nor puts; `devsCalcoutSoft` puts `OSV` to a target
//! whose address is `DBF_STRING` (`devsCalcoutSoft.c:127-134`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::types::EpicsValue;

async fn ioc(db: &str) -> Arc<PvDatabase> {
    let ioc = IocBuilder::new()
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .db_string(db, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    db
}

/// Runs one cycle of `rec` and waits until `done` holds.
async fn run(db: &PvDatabase, rec: &str, done: impl Fn(&PvDatabase) -> bool) {
    db.process_record(rec).await.unwrap();
    for _ in 0..400 {
        if done(db) {
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!("{rec} never finished");
}

fn idle(sseq: &'static str) -> impl Fn(&PvDatabase) -> bool {
    move |db| db.get_pv(format!("{sseq}.BUSY")).unwrap() == EpicsValue::Short(0)
}

#[epics_macros_rs::epics_test]
async fn sseq_keeps_the_dbf_code_its_link_targets_address_as() {
    let db = ioc("record(ai, \"X\") {}\n\
         record(waveform, \"W\") { field(FTVL, \"LONG\") field(NELM, \"8\") }\n\
         record(lso, \"L\") { field(SIZV, \"80\") }\n\
         record(calc, \"C\") {}\n\
         record(sseq, \"SQ\") {\n\
           field(DOL1, \"X.TIME\") field(LNK1, \"X.BKPT\")\n\
           field(DOL2, \"X.SCAN\") field(LNK2, \"X.DTYP\")\n\
           field(DOL3, \"X.INP\") field(LNK3, \"X.FLNK\")\n\
           field(DOL4, \"W.VAL\") field(LNK4, \"X.VAL\")\n\
           field(DOL5, \"X.RTYP\") field(LNK5, \"L.VAL\")\n\
           field(DOL6, \"X.MLOK\") field(LNK6, \"C.RPCL\")\n\
         }\n")
    .await;
    let code = |f: &str| db.get_pv(format!("SQ.{f}")).unwrap();
    // DBF_NOACCESS 17, MENU 12, DEVICE 13, INLINK 14, FWDLINK 16, LONG 5,
    // DOUBLE 10, STRING 0.
    let want = [
        ("DT1", 17),
        ("LT1", 17),
        ("DT2", 12),
        ("LT2", 13),
        ("DT3", 14),
        ("LT3", 16),
        ("DT4", 5),
        ("LT4", 10),
        ("DT5", 0),
        ("LT5", 0),
        ("DT6", 17),
        ("LT6", 17),
    ];
    let wrong: Vec<_> = want
        .into_iter()
        .filter(|(field, dbf)| code(field) != EpicsValue::Short(*dbf))
        .map(|(field, dbf)| format!("SQ.{field} = {:?}, C {dbf}", code(field)))
        .collect();
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// `LNK1` to `BKPT`: the destination switch's `default:` puts nothing, so
/// the target refuses nothing and no `LINK` alarm is raised.
#[epics_macros_rs::epics_test]
async fn sseq_puts_nothing_to_a_noaccess_target() {
    let db = ioc("record(ai, \"X\") {}\n\
         record(sseq, \"SQ\") { field(DO1, \"5\") field(LNK1, \"X.BKPT\") }\n")
    .await;
    run(&db, "SQ", idle("SQ")).await;
    assert_eq!(db.get_pv("SQ.SEVR").unwrap(), EpicsValue::Short(0));
    assert_eq!(db.get_pv("SQ.STAT").unwrap(), EpicsValue::Short(0));
}

/// `DOL1` from `TIME`: the source switch's `default:` reads nothing, so
/// `DO1` keeps its value and the step's put carries it.
#[epics_macros_rs::epics_test]
async fn sseq_reads_nothing_from_a_noaccess_source() {
    let db = ioc("record(ai, \"X\") {}\n\
         record(ao, \"T\") {}\n\
         record(sseq, \"SQ\") { field(DO1, \"5\") field(DOL1, \"X.TIME\") field(LNK1, \"T.VAL\") }\n")
    .await;
    run(&db, "SQ", idle("SQ")).await;
    assert_eq!(db.get_pv("SQ.SEVR").unwrap(), EpicsValue::Short(0));
    assert_eq!(db.get_pv("T.VAL").unwrap(), EpicsValue::Double(5.0));
}

/// A `FTVL=STRING` waveform's `VAL` addresses as `DBF_STRING` once its
/// `cvt_dbaddr` has run, so scalcout puts `OSV` there, not `OVAL`.
#[epics_macros_rs::epics_test]
async fn scalcout_puts_its_string_to_a_string_typed_waveform() {
    let db = ioc("record(waveform, \"W\") { field(FTVL, \"STRING\") field(NELM, \"2\") }\n\
         record(scalcout, \"S\") { field(AA, \"hello\") field(CALC, \"AA\") field(OUT, \"W.VAL\") }\n")
    .await;
    run(&db, "S", |db| {
        db.get_pv("W.NORD").unwrap() != EpicsValue::Long(0)
    })
    .await;
    assert_eq!(
        db.get_pv("W.VAL").unwrap(),
        EpicsValue::StringArray(vec!["hello".into()].into())
    );
}

#[epics_macros_rs::epics_test]
async fn name_to_dbaddr_gives_the_dbf_type_of_every_field_class() {
    use epics_base_rs::types::DbfCode;
    let db = ioc("record(ai, \"X\") {}\n\
         record(waveform, \"W\") { field(FTVL, \"LONG\") field(NELM, \"8\") }\n\
         record(lso, \"L\") {}\n\
         record(calc, \"C\") {}\n\
         record(sseq, \"SQ\") {}\n")
    .await;
    let h = db.async_handle();
    let addr = |pv| {
        h.name_to_dbaddr(pv, None)
            .map(|a| (a.field_type, a.no_elements))
    };
    let want = [
        ("X.TIME", DbfCode::NoAccess, 1),
        ("X.BKPT", DbfCode::NoAccess, 1),
        ("X.MLOK", DbfCode::NoAccess, 1),
        ("C.RPCL", DbfCode::NoAccess, 1),
        ("X.SCAN", DbfCode::Menu, 1),
        ("X.DTYP", DbfCode::Device, 1),
        ("X.INP", DbfCode::Inlink, 1),
        ("SQ.LNK1", DbfCode::Outlink, 1),
        ("X.FLNK", DbfCode::Fwdlink, 1),
        ("X.RTYP", DbfCode::String, 1),
        ("L.VAL", DbfCode::String, 1),
        ("W.VAL", DbfCode::Long, 8),
        ("X.VAL", DbfCode::Double, 1),
    ];
    for (pv, dbf, n) in want {
        assert_eq!(addr(pv), Some((dbf, n)), "{pv}");
    }
    for pv in ["X.TIME$", "X.MLOK$", "C.RPCL$"] {
        assert_eq!(addr(pv), None, "{pv}");
    }
}
