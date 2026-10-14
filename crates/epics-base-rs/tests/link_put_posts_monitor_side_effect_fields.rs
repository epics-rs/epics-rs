//! Spec §5 test 7: a CA put (`…1`) and a PP OUT link put (`…2`) of the same
//! field post the same side-effect fields at put time; whether the target
//! then processes follows the C column, not the CA arm (a PP link processes
//! a target whose field is not `pp`). Pinned to oracleStdIoc (C ground truth
//! §5.7, its DB verbatim). Cross-channel order is not reliable in C, so
//! each field's own post sequence is asserted.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(calcout,"CO1"){field(CALC,"A") field(OCAL,"A") field(DOPT,"Use OCAL")}
record(calcout,"CO2"){field(CALC,"A") field(OCAL,"A") field(DOPT,"Use OCAL")}
record(stringout,"SCO"){field(OUT,"CO2.CALC PP")}
record(stringout,"SCOO"){field(OUT,"CO2.OCAL PP")}
record(sseq,"SQ1"){}
record(sseq,"SQ2"){}
record(ao,"ASQ"){field(OUT,"SQ2.DLY1 PP")}
record(mbbo,"MO1"){field(ZRST,"z") field(ONST,"o")}
record(mbbo,"MO2"){field(ZRST,"z") field(ONST,"o")}
record(stringout,"SMO"){field(OUT,"MO2.ZRST PP")}
record(stringout,"SMO1"){field(OUT,"MO2.ONST PP")}
record(mbbi,"MI1"){field(ZRST,"z") field(ONST,"o")}
record(mbbi,"MI2"){field(ZRST,"z") field(ONST,"o")}
record(stringout,"SMI"){field(OUT,"MI2.ZRST PP")}
record(scalcout,"SC1"){field(CALC,"A") field(OCAL,"A") field(DOPT,"Use OCAL")}
record(scalcout,"SC2"){field(CALC,"A") field(OCAL,"A") field(DOPT,"Use OCAL")}
record(stringout,"SSC"){field(OUT,"SC2.CALC PP")}
record(stringout,"SSCO"){field(OUT,"SC2.OCAL PP")}
record(acalcout,"AC1"){field(CALC,"A") field(OCAL,"A") field(DOPT,"Use OCAL") field(NELM,"4")}
record(acalcout,"AC2"){field(CALC,"A") field(OCAL,"A") field(DOPT,"Use OCAL") field(NELM,"4")}
record(stringout,"SAC"){field(OUT,"AC2.CALC PP")}
record(stringout,"SACO"){field(OUT,"AC2.OCAL PP")}
record(compress,"CP1"){field(NSAM,"4") field(ALG,"N to 1 Average") field(N,"1")}
record(compress,"CP2"){field(NSAM,"4") field(ALG,"N to 1 Average") field(N,"1")}
record(ao,"ACR"){field(OUT,"CP2.RES PP")}
record(longout,"LCN"){field(OUT,"CP2.N PP")}
"#;

const UDF: u16 = 17;
const CALC: u16 = 12;
const LINK: u16 = 14;
const INVALID: u16 = 3;

/// One post: the value where the log copies one (`None` for a field whose
/// log is read at delivery), and the alarm it carries.
type Post = (Option<f64>, u16, u16);

const STRING_FIELDS: [&str; 4] = ["CALC", "OCAL", "ZRST", "ONST"];

struct Watch(Vec<(String, EventReader)>);

impl Watch {
    fn new(db: &PvDatabase, rec: &str, fields: &[&str]) -> Self {
        let r = db.get_record(rec).unwrap();
        let mut g = r.write();
        let mask = (EventMask::VALUE | EventMask::ALARM).bits();
        let mut readers = Vec::new();
        for (id, &f) in fields.iter().enumerate() {
            let ty = if STRING_FIELDS.contains(&f) {
                DbFieldType::String
            } else {
                DbFieldType::Double
            };
            let mut rx = g.add_subscriber(f, id as u32 + 1, ty, mask).unwrap();
            while rx.try_recv().is_ok() {}
            readers.push((f.to_string(), rx));
        }
        Self(readers)
    }

    /// Every watched field's posts since the last drain.
    fn drain(&mut self) -> Vec<(String, Vec<Post>)> {
        self.0
            .iter_mut()
            .map(|(f, rx)| {
                let mut posts = Vec::new();
                while let Ok(ev) = rx.try_recv() {
                    let v = if STRING_FIELDS.contains(&f.as_str()) {
                        None
                    } else {
                        ev.value().as_value().and_then(EpicsValue::to_f64)
                    };
                    posts.push((v, ev.stat(), ev.sevr()));
                }
                (f.clone(), posts)
            })
            .collect()
    }
}

fn want(rows: &[(&str, &[Post])]) -> Vec<(String, Vec<Post>)> {
    rows.iter()
        .map(|(f, p)| (f.to_string(), p.to_vec()))
        .collect()
}

async fn ioc() -> Arc<PvDatabase> {
    IocBuilder::new()
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .register_record_type("acalcout", || Box::new(AcalcoutRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn s(v: &str) -> EpicsValue {
    EpicsValue::String(v.into())
}

/// Puts `value` into `ca` by CA and into `src`'s VAL (whose OUT reaches the
/// `link` record's same field), and asserts both arms post `expect`.
async fn both_arms(
    db: &PvDatabase,
    (ca, ca_watch): (&str, &mut Watch),
    (src, link_watch): (&str, &mut Watch),
    field: &str,
    value: EpicsValue,
    expect: &[(&str, &[Post])],
) {
    db.put_record_field_from_ca(ca, field, value.clone())
        .await
        .unwrap();
    assert_eq!(ca_watch.drain(), want(expect), "CA {ca}.{field}");
    db.put_record_field_from_ca(src, "VAL", value)
        .await
        .unwrap();
    assert_eq!(link_watch.drain(), want(expect), "link {src}");
    assert_eq!(
        db.get_pv(format!("{src}.SEVR")).unwrap().to_f64(),
        Some(0.0),
        "{src}"
    );
}

#[epics_macros_rs::epics_test]
async fn a_link_put_posts_what_a_ca_put_posts() {
    let db = ioc().await;
    let calc = ["VAL", "CALC", "CLCV", "OCAL", "OCLV"];
    // VAL after a bad CALC: calcout keeps it (`calcoutRecord.c:238-239`),
    // scalcout sets -1 (`sCalcoutRecord.c:360-364`).
    for (one, two, scalc, socal, bad_val) in [
        ("CO1", "CO2", "SCO", "SCOO", 0.0),
        ("SC1", "SC2", "SSC", "SSCO", -1.0),
        ("AC1", "AC2", "SAC", "SACO", 0.0),
    ] {
        let mut w1 = Watch::new(&db, one, &calc);
        let mut w2 = Watch::new(&db, two, &calc);
        // CALC is pp: CLCV then CALC, and the record processes.
        both_arms(
            &db,
            (one, &mut w1),
            (scalc, &mut w2),
            "CALC",
            s("B"),
            &[
                ("VAL", &[(Some(0.0), 0, 0)]),
                ("CALC", &[(None, UDF, INVALID)]),
                ("CLCV", &[(Some(0.0), UDF, INVALID)]),
                ("OCAL", &[]),
                ("OCLV", &[]),
            ],
        )
        .await;
        // OCAL is not pp, and the CA arm does not process; C's link arm
        // shows no process either.
        both_arms(
            &db,
            (one, &mut w1),
            (socal, &mut w2),
            "OCAL",
            s("A+B"),
            &[
                ("VAL", &[]),
                ("CALC", &[]),
                ("CLCV", &[]),
                ("OCAL", &[(None, 0, 0)]),
                ("OCLV", &[(Some(0.0), 0, 0)]),
            ],
        )
        .await;
        // A bad CALC is stored, not refused: CLCV -1, then CALC/INVALID.
        both_arms(
            &db,
            (one, &mut w1),
            (scalc, &mut w2),
            "CALC",
            s("A+"),
            &[
                ("VAL", &[(Some(bad_val), CALC, INVALID)]),
                ("CALC", &[(None, 0, 0)]),
                ("CLCV", &[(Some(-1.0), 0, 0)]),
                ("OCAL", &[]),
                ("OCLV", &[]),
            ],
        )
        .await;
    }

    let mut q1 = Watch::new(&db, "SQ1", &["DLY1"]);
    let mut q2 = Watch::new(&db, "SQ2", &["DLY1"]);
    both_arms(
        &db,
        ("SQ1", &mut q1),
        ("ASQ", &mut q2),
        "DLY1",
        EpicsValue::Double(1.5),
        &[(
            "DLY1",
            &[(Some(1.5), UDF, INVALID), (Some(1.5), UDF, INVALID)],
        )],
    )
    .await;

    let mbbo = ["VAL", "ZRST", "ONST"];
    let mut m1 = Watch::new(&db, "MO1", &mbbo);
    let mut m2 = Watch::new(&db, "MO2", &mbbo);
    both_arms(
        &db,
        ("MO1", &mut m1),
        ("SMO", &mut m2),
        "ZRST",
        s("zz"),
        &[
            ("VAL", &[(None, UDF, INVALID)]),
            ("ZRST", &[(None, UDF, INVALID)]),
            ("ONST", &[]),
        ],
    )
    .await;
    both_arms(
        &db,
        ("MO1", &mut m1),
        ("SMO1", &mut m2),
        "ONST",
        s("oo"),
        &[
            ("VAL", &[]),
            ("ZRST", &[]),
            ("ONST", &[(None, UDF, INVALID)]),
        ],
    )
    .await;

    let mut i1 = Watch::new(&db, "MI1", &["VAL", "ZRST"]);
    let mut i2 = Watch::new(&db, "MI2", &["VAL", "ZRST"]);
    both_arms(
        &db,
        ("MI1", &mut i1),
        ("SMI", &mut i2),
        "ZRST",
        s("zz"),
        &[
            ("VAL", &[(Some(0.0), UDF, INVALID), (Some(0.0), 0, 0)]),
            ("ZRST", &[(None, UDF, INVALID)]),
        ],
    )
    .await;

    // compress RES/N are not pp: the CA put does not process, the PP link
    // put does, and finds no INP.
    let comp = ["VAL", "RES", "N", "NUSE"];
    let mut c1 = Watch::new(&db, "CP1", &comp);
    let mut c2 = Watch::new(&db, "CP2", &comp);
    db.put_record_field_from_ca("CP1", "RES", EpicsValue::Short(1))
        .await
        .unwrap();
    assert_eq!(
        c1.drain(),
        want(&[
            ("VAL", &[(None, 0, 0)]),
            ("RES", &[(Some(0.0), 0, 0)]),
            ("N", &[]),
            ("NUSE", &[(Some(0.0), 0, 0)]),
        ]),
        "CA CP1.RES"
    );
    db.put_record_field_from_ca("ACR", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    assert_eq!(
        c2.drain(),
        want(&[
            ("VAL", &[(None, 0, 0)]),
            ("RES", &[(Some(0.0), 0, 0)]),
            ("N", &[]),
            ("NUSE", &[(Some(0.0), 0, 0), (Some(0.0), LINK, INVALID)]),
        ]),
        "link ACR"
    );
    db.put_record_field_from_ca("CP1", "N", EpicsValue::Long(2))
        .await
        .unwrap();
    assert_eq!(
        c1.drain(),
        want(&[
            ("VAL", &[(None, 0, 0)]),
            ("RES", &[]),
            ("N", &[(Some(2.0), 0, 0)]),
            ("NUSE", &[]),
        ]),
        "CA CP1.N"
    );
    db.put_record_field_from_ca("LCN", "VAL", EpicsValue::Long(2))
        .await
        .unwrap();
    assert_eq!(
        c2.drain(),
        want(&[
            ("VAL", &[(None, 0, 0)]),
            ("RES", &[]),
            ("N", &[(Some(2.0), 0, 0)]),
            ("NUSE", &[(Some(0.0), 0, 0), (Some(0.0), LINK, INVALID)]),
        ]),
        "link LCN"
    );
}
