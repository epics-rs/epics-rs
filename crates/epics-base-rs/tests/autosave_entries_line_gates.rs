//! `restore_from_entries` (a save set's `restore_once`) gates a line by its
//! name as the boot restore does (dbrestore.c:898-911): `_X` is not a
//! letter or digit, so it is dropped before its value is read, and the
//! `#Y` comment's `@array@` value reads on to its `}`. The expected values
//! are the C oracle IOC's pass-1 restore of the same file.

use std::sync::Arc;

use epics_base_rs::server::autosave::save_set::restore_from_entries;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

const SAV: &str = "# save/restore V5.x\tAutomatically generated - DO NOT MODIFY - 261020-120000\n\
_X @array@ { \"1\"\n\
A 5\n\
\"2\" }\n\
#Y @array@ { \"1\"\n\
B 6\n\
\"2\" }\n\
C 7\n\
<END>\n";

#[epics_macros_rs::epics_test]
async fn a_line_is_gated_by_its_name_as_reboot_restore_does() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("b.sav");
    std::fs::write(&path, SAV).unwrap();
    let db = Arc::new(PvDatabase::new());
    for name in ["A", "B", "C"] {
        db.add_record(name, Box::new(AoRecord::new(3.0)))
            .await
            .unwrap();
    }

    restore_from_entries(&db, &path).await.unwrap();

    let values: Vec<EpicsValue> = ["A", "B", "C"]
        .into_iter()
        .map(|name| db.get_pv(name).unwrap())
        .collect();
    assert_eq!(
        values,
        [
            EpicsValue::Double(5.0),
            EpicsValue::Double(3.0),
            EpicsValue::Double(7.0)
        ]
    );
}
