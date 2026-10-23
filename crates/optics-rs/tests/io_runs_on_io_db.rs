//! `Io.st`, compiled by snc-rs, run against the shipped `Io.db`, with a
//! stand-in `scaler` record and an `ao` for the monochromator energy.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::task::sleep;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{FieldDesc, ProcessOutcome, Record};
use epics_base_rs::server::records::{ao::AoRecord, bo::BoRecord, mbbo::MbboRecord};
use epics_base_rs::types::{DbFieldType, EpicsValue};
use epics_seq_rs::{seq, seq_stop};
use optics_rs::snl::io::Io;

static SC_FIELDS: std::sync::LazyLock<Vec<FieldDesc>> = std::sync::LazyLock::new(|| {
    let mut v = vec![
        FieldDesc::new("CNT", DbFieldType::Enum, false),
        FieldDesc::new("FREQ", DbFieldType::Double, false),
    ];
    for n in 1..=15 {
        let s: &'static str = Box::leak(format!("S{n}").into_boxed_str());
        v.push(FieldDesc::new(s, DbFieldType::Double, false));
        let nm: &'static str = Box::leak(format!("NM{n}").into_boxed_str());
        v.push(FieldDesc::new(nm, DbFieldType::String, false));
    }
    v
});

#[derive(Default)]
struct Scaler {
    f: HashMap<String, EpicsValue>,
}

impl Record for Scaler {
    fn record_type(&self) -> &'static str {
        "scaler"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        let d = SC_FIELDS.iter().find(|d| d.name == name)?;
        Some(self.f.get(name).cloned().unwrap_or(match d.dbf_type {
            DbFieldType::String => EpicsValue::String("".into()),
            DbFieldType::Enum => EpicsValue::Enum(0),
            _ => EpicsValue::Double(1.0),
        }))
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        self.f.insert(name.to_owned(), value);
        Ok(())
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        &SC_FIELDS
    }
}

async fn ioc() -> Arc<PvDatabase> {
    let db = std::fs::read_to_string(format!("{}/Io.db", optics_rs::OPTICS_DB_DIR)).unwrap();
    let macros: HashMap<String, String> = [("P".to_string(), "io:".to_string())].into();
    let (db, _) = IocBuilder::new()
        .register_record_type("scaler", || Box::new(Scaler::default()))
        .register_record_type("ao", || Box::new(AoRecord::default()))
        .register_record_type("bo", || Box::new(BoRecord::default()))
        .register_record_type("mbbo", || Box::new(MbboRecord::default()))
        .db_string(
            &format!("{db}\nrecord(scaler, \"sc1\") {{}}\nrecord(ao, \"mono\") {{ field(VAL, \"10\") }}\n"),
            &macros,
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    db.mark_pini_done();
    db
}

fn desc(db: &PvDatabase) -> String {
    match db.get_pv("io:scaler.DESC").unwrap() {
        EpicsValue::String(v) => v.to_string(),
        other => panic!("{other:?}"),
    }
}

async fn eventually(secs: u64, mut probe: impl FnMut() -> bool) -> bool {
    for _ in 0..secs * 100 {
        if probe() {
            return true;
        }
        sleep(Duration::from_millis(10)).await;
    }
    probe()
}

/// A channel name reaches the scaler DESC as it is, `%` included
/// (Io.st:38, :356).
#[epics_base_rs::epics_test]
async fn a_percent_in_a_channel_name_is_copied() {
    let db = ioc().await;
    db.put_pv("sc1.NM2", EpicsValue::String("I0 50%% %d".into()))
        .await
        .unwrap();
    let tid = seq(&Io, "P=io:,MONO=mono,VSC=sc1", 0, &db).expect("Io starts");
    let ok = eventually(5, || desc(&db) == "I0 50%% %d").await;
    let got = desc(&db);
    seq_stop(tid);
    assert!(ok, "{got:?}");
}

/// Set in the child process `debug_lines_name_the_program` reruns itself as,
/// to capture the program's stdout.
const DEBUG_CHILD: &str = "IO_DEBUG_CHILD";

/// Started without a name macro, as io.iocsh starts it, the debug header
/// names the program, not `(null)` (Io.st:151-154).
#[epics_base_rs::epics_test]
async fn debug_lines_name_the_program() {
    if std::env::var_os(DEBUG_CHILD).is_none() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "debug_lines_name_the_program",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(DEBUG_CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            out.status.success(),
            "{stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let line = stdout
            .lines()
            .find(|l| l.ends_with("changed debug flag to 1"))
            .expect("no debug line");
        assert!(line.contains(",Io> "), "{line}");
        return;
    }
    let db = ioc().await;
    let tid = seq(&Io, "P=io:,MONO=mono,VSC=sc1", 0, &db).expect("Io starts");
    assert!(eventually(5, || desc(&db).is_empty()).await);
    sleep(Duration::from_millis(500)).await;
    db.put_pv_and_post("io:debug", EpicsValue::Short(1))
        .await
        .unwrap();
    sleep(Duration::from_millis(1000)).await;
    seq_stop(tid);
}
