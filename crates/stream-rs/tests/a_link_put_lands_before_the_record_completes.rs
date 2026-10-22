//! A bus callback's `%(record.FIELD)` input into a link field puts it once
//! the callback lets the record go, and the protocol's finish requests the
//! record's `processCallback` after that put, as C's `dbPutField` completes
//! before `callbackRequest`: when the record has completed, the link reads
//! its new target.

mod redirect;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use epics_base_rs::runtime::task::{sleep, test_block_on};
use epics_base_rs::types::EpicsValue;
use redirect::{REPLIES, WRITES, db_path, get, ioc};

/// `ax` fills 39 fields of `t3` and then `t3.FLNK = by`; `by` does the same
/// to `t4`, naming `ax`. The fills hold the record long enough for the
/// completion to be waiting on its set when the hold ends.
#[test]
fn each_record_completes_after_its_link_put() {
    let fill = vec!["a"; 39].join(" ");
    REPLIES.lock().extend([
        ("LX", format!("{fill} by").leak() as &str),
        ("LY", format!("{fill} ax").leak()),
    ]);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("a_link_put_lands_before_the_record_completes");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("c.proto");
    let inputs = |target: &str| {
        let mut formats = vec![format!("%({target}.DESC)s"); 39];
        formats.push(format!("%({target}.FLNK)s"));
        formats.join(" ")
    };
    std::fs::write(
        &file,
        format!(
            "Terminator = LF;\nx {{ out \"LX\"; in \"{}\"; }}\ny {{ out \"LY\"; in \"{}\"; }}\n",
            inputs("t3"),
            inputs("t4")
        ),
    )
    .unwrap();
    test_block_on(async {
        let db = ioc(&format!(
            r#"
record(ao, "t3") {{ }}
record(ao, "t4") {{ }}
record(ai, "ax") {{ field(DTYP, "stream") field(INP, "@{0} x R") }}
record(ai, "by") {{ field(DTYP, "stream") field(INP, "@{0} y R") }}
"#,
            db_path(&file)
        ))
        .await;
        for round in 0..200 {
            for t in ["t3", "t4"] {
                db.put_record_field_from_ca_no_notify(t, "FLNK", EpicsValue::String("".into()))
                    .await
                    .unwrap();
            }
            let before = WRITES.lock().len();
            for r in ["ax", "by"] {
                db.put_record_field_from_ca_no_notify(r, "PROC", EpicsValue::Long(1))
                    .await
                    .unwrap();
            }
            let deadline = Instant::now() + Duration::from_secs(10);
            while WRITES.lock().len() < before + 2
                || ["ax", "by"]
                    .iter()
                    .any(|r| db.get_record(r).unwrap().read().is_processing())
            {
                assert!(Instant::now() < deadline, "{round}: still processing");
                sleep(Duration::from_millis(1)).await;
            }
            assert_eq!(get(&db, "t3.FLNK").text_g(), "by", "{round}");
            assert_eq!(get(&db, "t4.FLNK").text_g(), "ax", "{round}");
        }
    });
}
