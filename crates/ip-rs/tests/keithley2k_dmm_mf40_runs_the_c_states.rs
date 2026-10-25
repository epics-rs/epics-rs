//! The compiled `Keithley2kDMM_mf40.st` on the shipped
//! `Keithley2kDMM_mf40.db`, driving the [`Keithley2700`] mock through the
//! template's devXxStrParm records. Each step's commands, and the values it
//! leaves, are the ones a C IOC (seq 2.2.9, asyn, calc, ip's devXxStrParm)
//! sent and posted against the same mock, template and puts.

mod keithley2700_mock;
mod support;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::runtime::task;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::server::scan::ScanOwner;
use epics_base_rs::types::{EpicsValue, PvString};
use ip_rs::strparm::{DEVICE_ROWS, device_support_factory};

use keithley2700_mock::Keithley2700;
use support::{eventually, sh};

const P: &str = "k:D1:";

struct Ioc {
    db: Arc<PvDatabase>,
    mock: Keithley2700,
    _scan: ScanOwner,
}

/// The template loaded with `P=k:,Dmm=D1:` past `iocInit`, and
/// `seq &Keithley2kDMM_mf40, "P=k:,Dmm=D1:,channels=4,model=2700"`.
async fn boot() -> Ioc {
    let mock = Keithley2700::start();
    ip_rs::register_ip_dbd();
    let no_db = Arc::new(PvDatabase::new());
    let port = format!(
        "drvAsynIPPortConfigure(\"serial1\", \"{}\", 0, 0, 0)",
        mock.host_info()
    );
    sh(&no_db, &port).await;
    let macros: HashMap<String, String> = [("P", "k:"), ("Dmm", "D1:"), ("PORT", "serial1")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let (db, _) = IocBuilder::new()
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .register_dynamic_device_support(&DEVICE_ROWS, device_support_factory())
        .db_file(
            &format!("{}/Keithley2kDMM_mf40.db", ip_rs::IP_DB_DIR),
            &macros,
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let scan = ScanOwner::start(db.clone());
    sh(
        &db,
        r#"seq &Keithley2kDMM_mf40, "P=k:,Dmm=D1:,channels=4,model=2700""#,
    )
    .await;
    Ioc {
        db,
        mock,
        _scan: scan,
    }
}

impl Ioc {
    /// A CA client's put to `{P}{Dmm}<pv>`, which processes the record.
    async fn put(&self, pv: &str, value: EpicsValue) {
        self.db
            .put_record_field_from_ca(&format!("{P}{pv}"), "VAL", value)
            .await
            .unwrap();
    }

    /// `put`, then the commands the meter received for it: `expect` of
    /// them, and none in the quarter second past the last.
    async fn step(&self, pv: &str, value: f64, expect: &[&str]) {
        self.put(pv, EpicsValue::Double(value)).await;
        self.commands(expect).await;
    }

    async fn commands(&self, expect: &[&str]) {
        assert!(
            eventually(|| self.mock.pending() >= expect.len()).await,
            "expected {expect:?}"
        );
        task::sleep(Duration::from_millis(250)).await;
        assert_eq!(self.mock.take(), expect);
    }

    /// `done_read 1`: the read cycle, which ends with the program putting
    /// `done_read` back to 0.
    async fn read(&self, expect: &[&str]) {
        self.step("done_read", 1.0, expect).await;
        assert!(eventually(|| self.f64_of("done_read") == 0.0).await);
    }

    fn f64_of(&self, pv: &str) -> f64 {
        self.db
            .get_pv(format!("{P}{pv}"))
            .unwrap()
            .to_f64()
            .unwrap()
    }

    fn str_of(&self, pv: &str) -> String {
        match self.db.get_pv(format!("{P}{pv}")).unwrap() {
            EpicsValue::String(s) => s.as_str_lossy().into_owned(),
            v => panic!("{pv}: {v:?}"),
        }
    }
}

/// Boot's commands: `mode_change` configures DC volts and closes channel
/// 1, `read_meter` resets the meter through `init_string` and, 1 s on, sets
/// its reading format through `init_string1`. The two state sets race, in C
/// too, so only each one's own order is fixed.
async fn booted(ioc: &Ioc) {
    assert!(eventually(|| ioc.mock.pending() >= 4).await);
    task::sleep(Duration::from_millis(250)).await;
    let (reset, mode): (Vec<_>, Vec<_>) = ioc
        .mock
        .take()
        .into_iter()
        .partition(|c| c == "*rst" || c == "form:elem read");
    assert_eq!(reset, ["*rst", "form:elem read"]);
    assert_eq!(mode, [":conf:volt:dc", "rout:clos (@101)"]);
}

/// Single-channel reads: each sends `read?` behind whatever mode and relay
/// changed since the last, and `Dmm_raw` takes the reading. The channel and
/// mode are put first, as the boot race leaves `previous_mode` and
/// `previous_chan` at either 0 or -1.
#[epics_base_rs::epics_test]
async fn single_reads_follow_the_selected_channel_and_mode() {
    let ioc = boot().await;
    booted(&ioc).await;
    assert_eq!(ioc.str_of("model"), "2700");
    assert_eq!(ioc.f64_of("channels"), 4.0);

    ioc.step("dmm_chan", 2.0, &["rout:clos (@103)"]).await;
    ioc.step("ch_mode_sel", 1.0, &[":conf:volt:ac"]).await;
    assert_eq!(ioc.str_of("units"), "VAC");
    ioc.read(&["read?"]).await;
    assert_eq!(ioc.f64_of("Dmm_raw"), 103.1);

    // 4-wire ohms opens every relay, so the next read closes channel 3 again.
    ioc.step("ch_mode_sel", 3.0, &[":conf:fres", "rout:open:all"])
        .await;
    assert_eq!(ioc.str_of("units"), "OHM");
    assert_eq!(ioc.str_of("ch_close"), "rout:open:all");
    ioc.read(&["rout:clos (@103)", "read?"]).await;
    assert_eq!(ioc.f64_of("Dmm_raw"), 103.3);

    // With 4 channels, dmm_chan2 closes nothing itself; the 11-20 range
    // still reads channel 15 through it.
    ioc.step("dmm_ch_range", 1.0, &[]).await;
    ioc.step("dmm_chan2", 4.0, &[]).await;
    ioc.read(&["rout:clos (@115)", "read?"]).await;
    assert_eq!(ioc.f64_of("Dmm_raw"), 115.3);
    ioc.step("dmm_ch_range", 0.0, &[]).await;
    ioc.read(&["rout:clos (@103)", "read?"]).await;
    assert_eq!(ioc.f64_of("Dmm_raw"), 103.3);
}

/// A multi-channel scan reads each channel that is on, in its own mode,
/// into its `Ch<n>_raw`; AC volts goes through `dmm_delay_read`. A channel's
/// mode change rewrites every channel's units label. `init_dmm` resets the
/// meter. 4-wire ohms first leaves every relay open and the meter off DC
/// volts, so the scan sends channel 1's mode and relay too.
#[epics_base_rs::epics_test]
async fn a_multi_scan_reads_each_channel_that_is_on_in_its_own_mode() {
    let ioc = boot().await;
    booted(&ioc).await;
    ioc.step("ch_mode_sel", 3.0, &[":conf:fres", "rout:open:all"])
        .await;
    ioc.step("single_multi", 1.0, &[]).await;
    for n in [1, 3, 4] {
        ioc.put(
            &format!("ch{n}_units"),
            EpicsValue::String(PvString::from("zz")),
        )
        .await;
    }
    ioc.step("ch2_mode_sel", 1.0, &[]).await;
    for (n, units) in [(1, "VDC"), (2, "VAC"), (3, "VDC"), (4, "VDC")] {
        assert_eq!(ioc.str_of(&format!("ch{n}_units")), units, "ch{n}");
    }
    assert_eq!(ioc.str_of("ch5_units"), "VDC");
    ioc.step("Ch3_on_off", 1.0, &[]).await;

    ioc.read(&[
        ":conf:volt:dc",
        "rout:clos (@101)",
        "read?",
        ":conf:volt:ac",
        "rout:clos (@102)",
        "read?",
        ":conf:volt:dc",
        "rout:clos (@104)",
        "read?",
    ])
    .await;
    assert_eq!(ioc.f64_of("Ch1_raw"), 101.0);
    assert_eq!(ioc.f64_of("Ch2_raw"), 102.1);
    assert_ne!(ioc.f64_of("Ch3_raw.UDF"), 0.0, "Ch3_raw was read");
    assert_eq!(ioc.f64_of("Ch4_raw"), 104.0);

    ioc.step("init_dmm", 1.0, &["*rst", "form:elem read"]).await;
    assert_eq!(ioc.f64_of("init_dmm"), 0.0);
}
