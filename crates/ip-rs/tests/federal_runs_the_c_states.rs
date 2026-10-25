//! The compiled `Federal.st` on the shipped `Federal.db`, booted with the
//! st.cmd lines a C IOC runs (`drvAsynIPPortConfigure`, an STX output and a
//! CR input terminator, `seq &Federal`) and reading the [`Federal`] mock
//! through the template's asyn record. The expected values are the ones the
//! C IOC (seq 2.2.9, asyn, calc) posted against the same mock and template.

mod federal_mock;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use asyn_rs::asyn_record::AsynRecord;
use asyn_rs::manager::PortManager;
use epics_base_rs::runtime::task::{self, BlockingBridge};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::iocsh::registry::CommandOutcome;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::server::scan::ScanOwner;
use epics_base_rs::types::EpicsValue;

use federal_mock::Federal;

const P: &str = "fed:Fed:s1:";

struct Ioc {
    db: Arc<PvDatabase>,
    mock: Federal,
    _scan: ScanOwner,
}

/// `line` on a shell over `db` that carries the asyn commands and every
/// registered one, run off the runtime's threads as st.cmd is.
async fn sh(db: &Arc<PvDatabase>, line: &str) {
    let db = db.clone();
    let bridge = BlockingBridge::capture();
    let owned = line.to_owned();
    let result = task::spawn_blocking(move || {
        let shell = IocShell::new(db, bridge);
        asyn_rs::iocsh::register_asyn_commands_on_shell(&shell, PortManager::global());
        shell.execute_line(&owned)
    })
    .await
    .unwrap();
    match result {
        Ok(CommandOutcome::Continue) => {}
        Ok(_) => panic!("{line}: failed"),
        Err(e) => panic!("{line}: {e}"),
    }
}

/// The mock answering `readings`, the template loaded with `P=fed:,S=s1`
/// past `iocInit`, and `seq &Federal, "P=fed:,S=s1"`.
async fn boot(readings: &[(i32, f64)]) -> Ioc {
    let mock = Federal::start(readings);
    ip_rs::register_ip_dbd();
    let no_db = Arc::new(PvDatabase::new());
    for line in [
        format!(
            "drvAsynIPPortConfigure(\"serial1\", \"{}\", 0, 0, 0)",
            mock.host_info()
        ),
        r#"asynOctetSetOutputEos("serial1", 0, "\x02")"#.to_owned(),
        r#"asynOctetSetInputEos("serial1", 0, "\r")"#.to_owned(),
    ] {
        sh(&no_db, &line).await;
    }
    let macros: HashMap<String, String> = [("P", "fed:"), ("S", "s1"), ("PORT", "serial1")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let (db, _) = IocBuilder::new()
        .register_record_type("asyn", || Box::new(AsynRecord::default()))
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .db_file(&format!("{}/Federal.db", ip_rs::IP_DB_DIR), &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let scan = ScanOwner::start(db.clone());
    sh(&db, r#"seq &Federal, "P=fed:,S=s1""#).await;
    Ioc {
        db,
        mock,
        _scan: scan,
    }
}

/// A CA client's put to `{P}Fed:{S}:<pv>`, which processes the record.
async fn put(ioc: &Ioc, pv: &str, v: f64) {
    ioc.db
        .put_record_field_from_ca(&format!("{P}{pv}"), "VAL", EpicsValue::Double(v))
        .await
        .unwrap();
}

fn f64_of(ioc: &Ioc, pv: &str) -> f64 {
    ioc.db.get_pv(format!("{P}{pv}")).unwrap().to_f64().unwrap()
}

/// Whether `probe_<n>` was never written: its ai keeps the UDF it loads with.
fn untouched(ioc: &Ioc, n: usize) -> bool {
    f64_of(ioc, &format!("probe_{n}.UDF")) != 0.0
}

/// A monitor on `{P}Fed:{S}:<pv>`, past the value it connects with.
async fn monitor(ioc: &Ioc, pv: &str) -> DbSubscription {
    let mut sub = DbSubscription::subscribe(&ioc.db, &format!("{P}{pv}"))
        .await
        .unwrap();
    posts(&mut sub);
    sub
}

/// The values `sub` has posted since the last call.
fn posts(sub: &mut DbSubscription) -> Vec<f64> {
    let mut values = Vec::new();
    while let Ok(e) = sub.try_recv_event() {
        values.extend(e.value().as_value().and_then(|v| v.to_f64()));
    }
    values
}

/// Poll `probe` for up to ten seconds: the program runs on its own thread.
async fn eventually(mut probe: impl FnMut() -> bool) -> bool {
    for _ in 0..1000 {
        if probe() {
            return true;
        }
        task::sleep(Duration::from_millis(10)).await;
    }
    false
}

/// Longer than one `get_readings` cycle over three probes and the 0.5 s
/// `wait` (`Federal.st:65`, `:83`).
async fn quiet() {
    task::sleep(Duration::from_millis(2000)).await;
}

/// Each reading lands in its probe's record, the probes past `num_probes`
/// are never written, and `run` 0 stops the readings. C posted probe_1 1.5
/// once over three cycles, its ai's MDEL 0 holding back each repeat, and
/// position each reading.
#[epics_base_rs::epics_test]
async fn each_reading_lands_in_its_probe_until_run_stops() {
    let ioc = boot(&[(1, 1.5), (2, 2.25), (3, 3.125)]).await;
    let mut probe_1 = monitor(&ioc, "probe_1").await;
    let mut position = monitor(&ioc, "position").await;
    put(&ioc, "num_probes", 3.0).await;
    put(&ioc, "run", 1.0).await;
    assert!(
        eventually(|| f64_of(&ioc, "probe_3") == 3.125).await,
        "probe_3 {}, {} readings",
        f64_of(&ioc, "probe_3"),
        ioc.mock.requests()
    );
    assert_eq!(f64_of(&ioc, "probe_1"), 1.5);
    assert_eq!(f64_of(&ioc, "probe_2"), 2.25);
    for n in 4..=10 {
        assert!(untouched(&ioc, n), "probe_{n} was written");
    }

    assert!(eventually(|| ioc.mock.requests() >= 9).await);
    put(&ioc, "run", 0.0).await;
    quiet().await;
    let stopped = ioc.mock.requests();
    quiet().await;
    assert_eq!(ioc.mock.requests(), stopped, "readings went on after run 0");
    assert_eq!(posts(&mut probe_1), [1.5]);
    let position = posts(&mut position);
    let cycle = [1.5, 2.25, 3.125];
    assert_eq!(position.len(), stopped, "{position:?}");
    for (i, p) in position.iter().enumerate() {
        assert_eq!(*p, cycle[i % 3], "{position:?}");
    }
}

/// A probe number past `num_probes` ends the cycle without a write
/// (`Federal.st:75`); the next cycle starts on the following reading, which
/// C posted as probe_2 2.25.
#[epics_base_rs::epics_test]
async fn a_probe_past_num_probes_ends_the_cycle() {
    let ioc = boot(&[(1, 1.5), (5, 9.0), (2, 2.25)]).await;
    put(&ioc, "num_probes", 3.0).await;
    put(&ioc, "run", 1.0).await;
    assert!(
        eventually(|| f64_of(&ioc, "probe_2") == 2.25).await,
        "probe_2 {}, {} readings",
        f64_of(&ioc, "probe_2"),
        ioc.mock.requests()
    );
    assert_eq!(f64_of(&ioc, "probe_1"), 1.5);
    assert!(untouched(&ioc, 5), "probe_5 was written");
}
