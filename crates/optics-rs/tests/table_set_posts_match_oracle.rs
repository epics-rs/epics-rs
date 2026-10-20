//! table SSET/SUSE posts against the optics oracle IOC (optics a750b958,
//! `oracleTableIoc st-table.cmd`, `record(table, "T") {}`). `special()` posts
//! SET with a literal DBE_VALUE (tableRecord.c:658, :663); neither field is
//! pp, so T is not processed; `dbPut` then posts the written field with
//! DBE_VALUE|DBE_LOG (dbAccess.c:1418). `camonitor -m v` / `-m l` on SET,
//! SSET, SUSE, then `caget`:
//!
//! ```text
//! dbpf T.SSET 1   -v SET Set, SSET 1   -l SSET 1   after SET Set, SSET 1, SUSE 0
//! dbpf T.SUSE 1   -v SET Use, SUSE 1   -l SUSE 1   after SET Use, SSET 1, SUSE 1
//! ```
//!
//! The same through a DB link, `longout W1 OUT "T.SSET"` and `longout W2 OUT
//! "T.SUSE"` with `dbpf W1 1` then `dbpf W2 1`: `dbPutLink` reaches the same
//! `dbPut` and C posts the same.
//!
//! C never clears SSET. C posts NO_ALARM throughout; the alarm half is
//! `TableRecord::init_resets_alarms`'s and is not compared here.

// RTEMS-EXEC-MODEL-ALLOW(2): checked, not waived — both pass on the exec backend
// (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p optics-rs
// --test table_set_posts_match_oracle`); no CA server is built.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::{DbFieldType, EpicsValue};
use optics_rs::records::table::TableRecord;

const PVS: [&str; 3] = ["SET", "SSET", "SUSE"];

/// The value as camonitor/caget print it: an enum by its state string.
fn as_dbr_string(s: &Snapshot) -> String {
    match (&s.value, &s.enums) {
        (EpicsValue::Enum(i), Some(e)) => e.strings[usize::from(*i)].to_string(),
        (v, _) => v.text_g(),
    }
}

struct Cam {
    db: Arc<PvDatabase>,
    subs: Vec<(char, &'static str, EventReader)>,
}

impl Cam {
    async fn new() -> Self {
        let (db, _) = IocBuilder::new()
            .register_record_type("table", || Box::new(TableRecord::default()))
            .db_string(
                r#"
record(table, "T") {}
record(longout, "W1") { field(OUT, "T.SSET") }
record(longout, "W2") { field(OUT, "T.SUSE") }
"#,
                &HashMap::new(),
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        let mut subs = Vec::new();
        for (m, mask) in [('v', EventMask::VALUE), ('l', EventMask::LOG)] {
            for field in PVS {
                let rx = db
                    .get_record("T")
                    .unwrap()
                    .write()
                    .add_subscriber(field, subs.len() as u32, DbFieldType::String, mask.bits())
                    .unwrap();
                subs.push((m, field, rx));
            }
        }
        let mut cam = Self { db, subs };
        cam.drain(); // the connect values
        cam
    }

    fn drain(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        for (m, field, rx) in &mut self.subs {
            while let Ok(e) = rx.try_recv() {
                // Enum strings are read at delivery, over the posted log.
                let rec = self.db.get_record("T").unwrap();
                let mut s = self
                    .db
                    .channel_snapshot_for_field(&rec, field, false)
                    .unwrap();
                e.write_log_over(&mut s);
                out.push(format!("{m} T.{field} {}", as_dbr_string(&s)));
            }
        }
        out
    }

    /// `dbpf <rec>.<field> 1`, every post it made on T, then `caget` of SET
    /// SSET SUSE.
    async fn dbpf(&mut self, rec: &str, field: &str) -> (Vec<String>, Vec<String>) {
        self.db
            .put_record_field_from_ca(rec, field, EpicsValue::String("1".into()))
            .await
            .unwrap();
        let posts = self.drain();
        let after = PVS
            .iter()
            .map(|f| {
                let snap = self
                    .db
                    .get_record("T")
                    .unwrap()
                    .read()
                    .snapshot_for_field(f);
                format!("T.{f} {}", as_dbr_string(&snap.unwrap()))
            })
            .collect();
        (posts, after)
    }
}

async fn sset_then_suse(sset: (&str, &str), suse: (&str, &str)) {
    let mut cam = Cam::new().await;
    assert_eq!(
        cam.dbpf(sset.0, sset.1).await,
        (
            vec![
                "v T.SET Set".to_string(),
                "v T.SSET 1".into(),
                "l T.SSET 1".into(),
            ],
            vec![
                "T.SET Set".to_string(),
                "T.SSET 1".into(),
                "T.SUSE 0".into()
            ],
        )
    );
    assert_eq!(
        cam.dbpf(suse.0, suse.1).await,
        (
            vec![
                "v T.SET Use".to_string(),
                "v T.SUSE 1".into(),
                "l T.SUSE 1".into(),
            ],
            vec![
                "T.SET Use".to_string(),
                "T.SSET 1".into(),
                "T.SUSE 1".into()
            ],
        )
    );
}

#[tokio::test]
async fn sset_then_suse_posts_match_oracle() {
    sset_then_suse(("T", "SSET"), ("T", "SUSE")).await;
}

#[tokio::test]
async fn sset_then_suse_through_a_db_link_posts_match_oracle() {
    sset_then_suse(("W1", "VAL"), ("W2", "VAL")).await;
}
