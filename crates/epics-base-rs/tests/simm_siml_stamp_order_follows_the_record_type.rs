//! Where `recGblGetTimeStampSimm(prec, prec->simm, NULL)` sits against the
//! `recGblGetSimm` in `writeValue` decides which cycle's SIMM it sees:
//! `lsoRecord.c:154` stamps after the write (this cycle's SIMM); ao, bo,
//! longout, int64out, mbbo, mbboDirect, stringout and aao stamp before it
//! (`aoRecord.c:192`, `aaoRecord.c:172`: the previous cycle's).
//!
//! softIoc `R7.0.10-146-g8f5015b66`, `TSE=-2`, SIML from a longin, one
//! `.PROC` per step (U unchanged from undefined, N new stamp, S same stamp):
//!
//! ```text
//!          first(SW=0)  SW=1  again  SW=0  again
//!   lso        U         N      N      S     S
//!   aao        U         U      N      N     S
//! ```

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const STALE: Duration = Duration::new(3_000_000, 333_333_333);

const DB_TEXT: &str = r#"
record(longin, "SW") {}
record(lso, "O_LSO") { field(SIML, "SW") field(TSE, "-2") field(SIZV, "20") }
record(aao, "O_AAO") { field(SIML, "SW") field(TSE, "-2") field(NELM, "2") field(FTVL, "LONG") }
"#;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Step {
    U,
    N,
    S,
}

#[epics_macros_rs::epics_test]
async fn the_stamp_sees_this_or_the_previous_cycles_simm_per_record_type() {
    let db = IocBuilder::new()
        .db_string(DB_TEXT, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let stale = SystemTime::UNIX_EPOCH + STALE;
    for (rec, want) in [
        ("O_LSO", [Step::U, Step::N, Step::N, Step::S, Step::S]),
        ("O_AAO", [Step::U, Step::U, Step::N, Step::N, Step::S]),
    ] {
        let cell = db.get_record(rec).unwrap();
        cell.write().common.time = stale;
        db.put_record_field_from_ca("SW", "VAL", EpicsValue::Long(0))
            .await
            .unwrap();
        let mut prev = stale;
        let mut got = Vec::new();
        for sw in [0, 1, 1, 0, 0] {
            db.put_record_field_from_ca("SW", "VAL", EpicsValue::Long(sw))
                .await
                .unwrap();
            std::thread::sleep(Duration::from_millis(5));
            let mut visited = epics_base_rs::server::database::ProcStack::new();
            db.process_record_with_links(rec, &mut visited)
                .await
                .unwrap();
            let t = cell.read().common.time;
            got.push(if t == stale {
                Step::U
            } else if t == prev {
                Step::S
            } else {
                Step::N
            });
            prev = t;
        }
        assert_eq!(got, want, "{rec}");
    }
}
