//! subArray's INDX/NELM clamp is `readValue`'s, run at every process ahead of
//! `read_sa`; the init load (`devSASoft init_record`) works on the unclamped
//! values. Expected `(NORD, INDX, NELM)` after init and after one
//! process are softIoc 7.0.10's for the same db.

use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

// (name, MALM, NELM, INDX)
const CASES: [(&str, u32, u32, u32); 7] = [
    ("A", 4, 4, 2),
    ("B", 4, 4, 3),
    ("C", 4, 4, 4),
    ("D", 4, 4, 5),
    ("E", 4, 3, 2),
    ("F", 4, 2, 1),
    ("G", 6, 4, 3),
];

type State = (i64, i64, i64);

// `(init, after one process)` per case, in CASES order.
const WANT: [(State, State); 7] = [
    ((2, 2, 4), (2, 2, 4)),
    ((1, 3, 4), (1, 3, 4)),
    ((0, 4, 4), (1, 3, 4)),
    ((0, 5, 4), (1, 3, 4)),
    ((2, 2, 3), (2, 2, 3)),
    ((2, 1, 2), (2, 1, 2)),
    ((2, 3, 4), (2, 3, 4)),
];

async fn state(db: &PvDatabase, r: &str) -> (i64, i64, i64) {
    let n = |f: &str| db.get_pv(format!("{r}.{f}")).unwrap().to_f64().unwrap() as i64;
    (n("NORD"), n("INDX"), n("NELM"))
}

#[epics_macros_rs::epics_test]
async fn indx_clamps_at_process_and_never_at_init() {
    let mut text = String::new();
    for (n, m, ne, ix) in CASES {
        text += &format!(
            "record(subArray, \"{n}\") {{ field(FTVL, \"LONG\") field(MALM, \"{m}\") field(NELM, \"{ne}\") field(INDX, \"{ix}\") field(INP, \"[1,2,3,4,5]\") }}\n"
        );
    }
    let db = IocBuilder::new()
        .db_string(&text, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let mut wrong = Vec::new();
    for (i, (n, ..)) in CASES.iter().enumerate() {
        let init = state(&db, n).await;
        let mut v = ProcStack::new();
        db.process_record_with_links(n, &mut v).await.unwrap();
        let after = state(&db, n).await;
        let (wi, wa) = &WANT[i];
        if &init != wi {
            wrong.push(format!("{n} init: {init:?}, want {wi:?}"));
        }
        if &after != wa {
            wrong.push(format!("{n} process: {after:?}, want {wa:?}"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
    // The values: B (INDX 3) serves [4]; C/D serve [4] only after process.
    let val = |r: &str| db.get_pv(r).unwrap();
    assert_eq!(
        val("B").to_string(),
        EpicsValue::LongArray(vec![4].into()).to_string()
    );
}
