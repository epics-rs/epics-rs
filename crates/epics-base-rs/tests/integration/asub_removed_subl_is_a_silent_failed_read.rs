//! A refused `SUBL` put removes the link (`dbRemoveLink` precedes the
//! `SPC_NOMOD` refusal, `dbAccess.c:1167-1174`), and a removed link reads as
//! C `dbGetLink`'s silent -1 (`dbLink.c:335-337`): aSub's `fetch_values`
//! returns it, so `do_sub` does not run, and no alarm is raised.
//!
//! softIoc R7.0.10-146-g8f5015b66:
//!
//! ```text
//! record(stringin, "SN") { field(VAL, "") }
//! record(aSub, "AS") { field(LFLG, "READ") field(SUBL, "SN") }
//!
//! dbpf AS.VAL 5; dbpf AS.PROC 1   -> VAL 0, NO_ALARM   (empty SNAM: do_sub returns 0)
//! dbpf AS.VAL 5; dbpf AS.SUBL "SN" -> dbPut Attempt to modify noMod field
//! dbpf AS.PROC 1                  -> VAL 5, STAT NO_ALARM, SEVR NO_ALARM
//! ```

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(stringin, "SN") { field(VAL, "") }
record(aSub, "AS") { field(LFLG, "READ") field(SUBL, "SN") }
"#;

#[epics_macros_rs::epics_test]
async fn a_removed_subl_skips_the_subroutine_and_raises_nothing() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let process = || async {
        let mut visited = epics_base_rs::server::database::ProcStack::new();
        db.process_record_with_links("AS", &mut visited)
            .await
            .unwrap();
    };
    let get = |pv: &str| db.get_pv(pv).unwrap().to_f64();

    db.put_pv("AS.VAL", EpicsValue::Long(5)).await.unwrap();
    process().await;
    assert_eq!(get("AS.VAL"), Some(0.0), "a live SUBL runs do_sub");

    db.put_pv("AS.VAL", EpicsValue::Long(5)).await.unwrap();
    assert!(
        db.put_record_field_from_ca_no_notify("AS", "SUBL", EpicsValue::String("SN".into()))
            .await
            .is_err(),
        "SUBL is SPC_NOMOD"
    );
    process().await;
    assert_eq!(get("AS.VAL"), Some(5.0), "the removed link's read fails");
    assert_eq!(get("AS.STAT"), Some(0.0), "and dbGetLink raises nothing");
    assert_eq!(get("AS.SEVR"), Some(0.0));
}
