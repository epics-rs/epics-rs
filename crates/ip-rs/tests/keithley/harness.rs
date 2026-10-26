//! A Keithley program booted on its template against a [`Keithley`] mock,
//! and the steps its tests drive it with.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::runtime::task;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::server::scan::ScanOwner;
use epics_base_rs::types::EpicsValue;
use ip_rs::strparm::{DEVICE_ROWS, device_support_factory};

use crate::mock::Keithley;
use crate::support::{eventually, sh};

/// One program and the meter it drives.
pub struct Meter {
    /// The program.
    pub program: &'static str,
    /// The template's stem.
    pub template: &'static str,
    /// The `Dmm` macro.
    pub dmm: &'static str,
    /// The `model` macro.
    pub model: &'static str,
    /// The `conf_*` records' commands, in `dmm_modes` order.
    pub modes: &'static [&'static str],
    /// The `dmm_read` record's command.
    pub read: &'static str,
}

pub struct Ioc {
    db: Arc<PvDatabase>,
    prefix: String,
    pub mock: Keithley,
    _scan: ScanOwner,
}

/// [`load`], then [`Ioc::start`].
pub async fn boot(meter: &Meter, channels: u32) -> Ioc {
    let ioc = load(meter).await;
    ioc.start(meter, channels).await;
    ioc
}

/// The template loaded with `P=k:,Dmm=<dmm>` past `iocInit`, the program
/// not yet started.
pub async fn load(meter: &Meter) -> Ioc {
    let mock = Keithley::start(meter.modes, meter.read);
    ip_rs::register_ip_dbd();
    let no_db = Arc::new(PvDatabase::new());
    let port = format!(
        "drvAsynIPPortConfigure(\"serial1\", \"{}\", 0, 0, 0)",
        mock.host_info()
    );
    sh(&no_db, &port).await;
    let macros: HashMap<String, String> = [("P", "k:"), ("Dmm", meter.dmm), ("PORT", "serial1")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let (asyn_name, asyn_factory) = asyn_rs::asyn_record::asyn_record_factory();
    let (db, _) = IocBuilder::new()
        .register_record_type(asyn_name, move || asyn_factory())
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .register_dynamic_device_support(&DEVICE_ROWS, device_support_factory())
        .db_file(
            &format!("{}/{}.db", ip_rs::IP_DB_DIR, meter.template),
            &macros,
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let scan = ScanOwner::start(db.clone());
    // iocInit's PINI pass, which takes each record's lock set, ends before
    // st.cmd goes on.
    db.wait_for_pini().await;
    Ioc {
        db,
        prefix: format!("k:{}", meter.dmm),
        mock,
        _scan: scan,
    }
}

/// A record's lock set, held on a thread of its own until [`Self::release`].
pub struct Held(std::sync::mpsc::Sender<()>);

impl Held {
    pub fn release(self) {
        self.0.send(()).unwrap();
    }
}

impl Ioc {
    /// `seq &<program>, "P=k:,Dmm=<dmm>,channels=<channels>,model=<model>"`.
    pub async fn start(&self, meter: &Meter, channels: u32) {
        let seq = format!(
            r#"seq &{}, "P=k:,Dmm={},channels={channels},model={}""#,
            meter.program, meter.dmm, meter.model
        );
        sh(&self.db, &seq).await;
    }

    /// `{P}{Dmm}<record>`'s lock set, taken: a put of the program's to it
    /// waits, and with it the state set that put it, until the release.
    pub fn hold(&self, record: &str) -> Held {
        let (db, record) = (self.db.clone(), format!("{}{record}", self.prefix));
        let (locked, taken) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let _guard = db.lock_record(&record);
            locked.send(()).unwrap();
            let _ = released.recv();
        });
        taken.recv().unwrap();
        Held(release)
    }

    /// A CA client's put to `{P}{Dmm}<pv>`, which processes the record.
    pub async fn put(&self, pv: &str, value: EpicsValue) {
        self.db
            .put_record_field_from_ca(&format!("{}{pv}", self.prefix), "VAL", value)
            .await
            .unwrap();
    }

    /// `put`, then the commands the meter received for it: `expect` of
    /// them, and none in the quarter second past the last.
    pub async fn step(&self, pv: &str, value: f64, expect: &[&str]) {
        self.put(pv, EpicsValue::Double(value)).await;
        self.commands(expect).await;
    }

    pub async fn commands(&self, expect: &[&str]) {
        assert!(
            eventually(|| self.mock.pending() >= expect.len()).await,
            "expected {expect:?}"
        );
        task::sleep(Duration::from_millis(250)).await;
        assert_eq!(self.mock.take(), expect);
    }

    /// `done_read 1`: the read cycle, which ends with the program putting
    /// `done_read` back to 0.
    pub async fn read(&self, expect: &[&str]) {
        self.step("done_read", 1.0, expect).await;
        assert!(eventually(|| self.f64_of("done_read") == 0.0).await);
    }

    pub fn f64_of(&self, pv: &str) -> f64 {
        self.db
            .get_pv(format!("{}{pv}", self.prefix))
            .unwrap()
            .to_f64()
            .unwrap()
    }

    pub fn str_of(&self, pv: &str) -> String {
        match self.db.get_pv(format!("{}{pv}", self.prefix)).unwrap() {
            EpicsValue::String(s) => s.as_str_lossy().into_owned(),
            v => panic!("{pv}: {v:?}"),
        }
    }
}
