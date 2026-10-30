//! sncqxbpm against `qxbpm.db` and an asyn record on a `drvAsynIPPort` to
//! the mock BPM in `qxbpm_mock`. Every expected value is the C oracle IOC's:
//! this `sncqxbpm.st` built by snc, `drvAsynIPPortConfigure("QX1",host,0,0,0)`,
//! the same asyn record, and `seq sncqxbpm "name=qxbpm,P=T:,S=T:asyn"` on the
//! same mock.
//!
//! The commands are compared with the CURRALL reads dropped and repeats
//! collapsed, as C's run repeated some (ranges 1 and 2 in set_offsets) as
//! its timing fell. A bo or mbbo is read at RVAL, the field the oracle
//! read.

mod qxbpm_mock;

use asyn_rs::asyn_record::AsynRecord;
use asyn_rs::drivers::ip_port::DrvAsynIPPort;
use asyn_rs::registry::{claim_port, open_owner};
use asyn_rs::runtime::{PortRuntimeHandle, RuntimeConfig, create_port_runtime};
use epics_base_rs::runtime::task;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::db_access::DbChannel;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::scan::ScanOwner;
use epics_base_rs::types::EpicsValue;
use epics_seq_rs::{SeqThreadId, seq, seq_stop};
use optics_rs::snl::qxbpm::sncqxbpm;
use qxbpm_mock::Qxbpm;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const QXBPM_DB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/db/qxbpm.db");
const CURRALL: &str = ":READ0:CURRALL?";

/// The IOC, the mock and the running program. The program is stopped
/// before the port goes.
struct Ioc {
    prog: SeqThreadId,
    db: Arc<PvDatabase>,
    _scan: ScanOwner,
    bpm: Qxbpm,
    _port: PortRuntimeHandle,
}

impl Drop for Ioc {
    fn drop(&mut self) {
        seq_stop(self.prog);
    }
}

/// Boot with the mock answering raw counts 1000 2000 3000 4000, the
/// program started and `{P}enable` still 0.
async fn boot() -> Ioc {
    static BOOTS: AtomicUsize = AtomicUsize::new(0);
    let name = format!("QX{}", BOOTS.fetch_add(1, Ordering::Relaxed));
    let bpm = Qxbpm::start();
    bpm.set_raw([1000, 2000, 3000, 4000]);
    // drvAsynIPPortConfigure(name, host, 0, 0, 0)
    let claim = claim_port(&name).unwrap();
    let driver = DrvAsynIPPort::new_configured(&name, &bpm.host_info(), false, false).unwrap();
    let (port, _jh) = create_port_runtime(&claim, driver, RuntimeConfig::default()).unwrap();
    claim
        .publish(port.port_handle().clone(), &open_owner())
        .unwrap();
    let (db, _) = IocBuilder::new()
        .register_record_type("asyn", || Box::new(AsynRecord::default()))
        .db_string(
            &format!("record(asyn, \"T:asyn\") {{ field(PORT, \"{name}\") field(ADDR, \"0\") }}"),
            &HashMap::new(),
        )
        .unwrap()
        .db_file(
            QXBPM_DB,
            &HashMap::from([("P".to_string(), "T:".to_string())]),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    // The program waits for PINI, which the scan owner's pass publishes.
    let scan = ScanOwner::start(db.clone());
    let prog = seq(&sncqxbpm, "name=qxbpm,P=T:,S=T:asyn", 0, &db).expect("sncqxbpm starts");
    Ioc {
        prog,
        db,
        _scan: scan,
        bpm,
        _port: port,
    }
}

/// [`boot`], enabled, past init and the first reading.
async fn boot_enabled() -> Ioc {
    let ioc = boot().await;
    wait_for(&ioc, "T:init.RVAL", 1.0).await;
    put(&ioc, "T:enable", EpicsValue::Double(1.0)).await;
    wait_for(&ioc, "T:current:a:raw", 1000.0).await;
    wait_for(&ioc, "T:init.RVAL", 0.0).await;
    ioc
}

/// [`boot_enabled`] after `{P}set_defaults`, so the trims are non-zero.
async fn boot_with_defaults() -> Ioc {
    let ioc = boot_enabled().await;
    put(&ioc, "T:set_defaults", EpicsValue::Double(1.0)).await;
    wait_for(&ioc, "T:set_defaults.RVAL", 0.0).await;
    reads(&ioc, 2).await;
    ioc
}

/// A CA put, processing the record as a client's would.
async fn put(ioc: &Ioc, pv: &str, v: EpicsValue) {
    let (rec, field) = pv.split_once('.').unwrap_or((pv, "VAL"));
    ioc.db
        .put_record_field_from_ca(rec, field, v)
        .await
        .unwrap();
}

fn f64_of(ioc: &Ioc, pv: &str) -> f64 {
    ioc.db.get_pv(pv).unwrap().to_f64().unwrap()
}

fn string_of(ioc: &Ioc, pv: &str) -> String {
    match ioc.db.get_pv(pv).unwrap() {
        EpicsValue::String(s) => s.to_string(),
        v => format!("{v:?}"),
    }
}

/// `want` to C's six significant digits, the precision the oracle printed.
fn assert_near(ioc: &Ioc, pv: &str, want: f64) {
    let got = f64_of(ioc, pv);
    assert!(
        (got - want).abs() <= want.abs() * 1e-6,
        "{pv} = {got}, want {want}"
    );
}

/// Up to 20 s, the longest being set_offsets' sweep of about 7 s.
async fn wait_for(ioc: &Ioc, pv: &str, want: f64) {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut got = f64_of(ioc, pv);
    while got != want && Instant::now() < deadline {
        task::sleep(Duration::from_millis(20)).await;
        got = f64_of(ioc, pv);
    }
    assert_eq!(got, want, "{pv}");
}

fn read_count(ioc: &Ioc) -> usize {
    ioc.bpm.seen().iter().filter(|c| *c == CURRALL).count()
}

/// Wait for `n` more CURRALL reads. The program reads in one state set,
/// so the second has gone out only once the first's posts are done.
async fn reads(ioc: &Ioc, n: usize) {
    let want = read_count(ioc) + n;
    let deadline = Instant::now() + Duration::from_secs(10);
    while read_count(ioc) < want && Instant::now() < deadline {
        task::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        read_count(ioc) >= want,
        "{} reads, want {want}",
        read_count(ioc)
    );
}

/// The commands since the last clear, without the reads and with repeats
/// collapsed.
fn commands(ioc: &Ioc) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in ioc.bpm.seen() {
        if c != CURRALL && out.last() != Some(&c) {
            out.push(c);
        }
    }
    out
}

/// Wait until [`commands`] is `want`, then for two more reads, and check
/// nothing followed.
async fn expect_commands(ioc: &Ioc, want: &[&str]) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while commands(ioc) != want && Instant::now() < deadline {
        task::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(commands(ioc), want);
    reads(ioc, 2).await;
    assert_eq!(commands(ioc), want);
}

/// startup posts init and the current flags and sends nothing while
/// disabled; enabling runs init's asyn setup and BPM reset, then reads.
#[epics_base_rs::epics_test]
async fn enabling_runs_init_and_reads() {
    let ioc = boot().await;
    wait_for(&ioc, "T:init.RVAL", 1.0).await;
    wait_for(&ioc, "T:current:low.RVAL", 1.0).await;
    assert_eq!(string_of(&ioc, "T:port"), "T:asyn");
    assert_eq!(f64_of(&ioc, "T:period.LOPR"), 0.05);
    assert_eq!(f64_of(&ioc, "T:current:ok.RVAL"), 0.0);
    assert_eq!(f64_of(&ioc, "T:set_offsets.RVAL"), 0.0);
    task::sleep(Duration::from_secs(2)).await;
    assert!(ioc.bpm.seen().is_empty(), "{:?}", ioc.bpm.seen());

    put(&ioc, "T:enable", EpicsValue::Double(1.0)).await;
    expect_commands(&ioc, &["*RST0", ":CONF0:CURR:RANG 1", ":CONF0:SINGLE"]).await;
    assert_eq!(f64_of(&ioc, "T:init.RVAL"), 0.0);
    // Write/Read, ASCII, Passive: index 0 of each menu.
    assert_eq!(f64_of(&ioc, "T:asyn.TMOD"), 0.0);
    assert_eq!(f64_of(&ioc, "T:asyn.TMOT"), 0.25);
    assert_eq!(string_of(&ioc, "T:asyn.OEOS"), r"\n");
    assert_eq!(string_of(&ioc, "T:asyn.IEOS"), r"\n");
    assert_eq!(f64_of(&ioc, "T:asyn.NRRD"), 0.0);
    assert_eq!(f64_of(&ioc, "T:asyn.SCAN"), 0.0);
    assert_eq!(f64_of(&ioc, "T:asyn.OFMT"), 0.0);
    assert_eq!(f64_of(&ioc, "T:asyn.IFMT"), 0.0);
    for (pv, want) in [
        ("T:current:a:raw", 1000.0),
        ("T:current:b:raw", 2000.0),
        ("T:current:c:raw", 3000.0),
        ("T:current:d:raw", 4000.0),
        // The trims start at 0, and the default threshold 0 makes every
        // count ok.
        ("T:current:a", 0.0),
        ("T:current:total", 0.0),
        ("T:current:low.RVAL", 0.0),
        ("T:current:ok.RVAL", 1.0),
        ("T:error.RVAL", 0.0),
    ] {
        assert_eq!(f64_of(&ioc, pv), want, "{pv}");
    }
    assert_eq!(string_of(&ioc, "T:errMsg"), "");
}

/// set_defaults writes the WINDOW mode, the settings and every gain's
/// trim, and the next reads use the gain-0 trims.
#[epics_base_rs::epics_test]
async fn set_defaults_writes_the_trims_the_reads_use() {
    let ioc = boot_enabled().await;
    ioc.bpm.clear_seen();
    put(&ioc, "T:set_defaults", EpicsValue::Double(1.0)).await;
    wait_for(&ioc, "T:set_defaults.RVAL", 0.0).await;
    expect_commands(&ioc, &[":CONF0:WDWCURR 30"]).await;
    for (pv, want) in [
        ("T:mode", 2.0),
        ("T:buflen", 30.0),
        ("T:current:low:raw", 1000.0),
        ("T:period", 0.1),
        ("T:GX", 4.5),
        ("T:GY", 4.5),
        ("T:settling", 2.5),
        ("T:current:low.RVAL", 0.0),
        ("T:current:ok.RVAL", 1.0),
    ] {
        assert_eq!(f64_of(&ioc, pv), want, "{pv}");
    }
    for (pv, want) in [
        ("T:r1:A1", 3.5e-13),
        ("T:r2:B1", 7e-13),
        ("T:r3:C1", 1.4e-12),
        ("T:r4:D1", 7e-12),
        ("T:r5:A1", 7e-11),
        ("T:r6:D1", 7e-10),
        ("T:current:a", 3.5e-10),
        ("T:current:b", 7e-10),
        ("T:current:c", 1.05e-9),
        ("T:current:d", 1.4e-9),
        ("T:current:total", 3.5e-9),
        ("T:pos:x", -1.5),
        ("T:pos:y", -2.25),
    ] {
        assert_near(&ioc, pv, want);
    }
}

/// current:low and current:ok at each side of the threshold, a negative
/// one compared as a signed number.
#[epics_base_rs::epics_test]
async fn the_threshold_sets_low_and_ok() {
    let ioc = boot_enabled().await;
    for (threshold, low, ok) in [
        (-1, 0.0, 1.0),
        (1000, 0.0, 1.0),
        (1001, 0.0, 0.0),
        (4001, 1.0, 0.0),
    ] {
        put(&ioc, "T:current:low:raw", EpicsValue::Long(threshold)).await;
        reads(&ioc, 2).await;
        assert_eq!(f64_of(&ioc, "T:current:low.RVAL"), low, "{threshold}");
        assert_eq!(f64_of(&ioc, "T:current:ok.RVAL"), ok, "{threshold}");
    }
}

/// A gain past the six ranges sends no range and leaves the currents as
/// they were; gain 3 sends range 4 and reads with its trims.
#[epics_base_rs::epics_test]
async fn an_out_of_range_gain_sends_nothing() {
    let ioc = boot_with_defaults().await;
    ioc.bpm.clear_seen();
    put(&ioc, "T:gain", EpicsValue::Enum(9)).await;
    expect_commands(&ioc, &[]).await;
    assert_near(&ioc, "T:current:a", 3.5e-10);
    assert_near(&ioc, "T:current:b", 7e-10);
    assert_near(&ioc, "T:current:total", 3.5e-9);
    assert_near(&ioc, "T:pos:x", -1.5);

    ioc.bpm.clear_seen();
    put(&ioc, "T:gain", EpicsValue::Enum(3)).await;
    expect_commands(&ioc, &[":CONF0:CURR:RANG 4"]).await;
    assert_near(&ioc, "T:current:a", 7e-9);
    assert_near(&ioc, "T:current:d", 2.8e-8);
    assert_near(&ioc, "T:current:total", 7e-8);
}

/// buflen is clamped to its LOPR/HOPR before the mode command goes out,
/// and period to its own.
#[epics_base_rs::epics_test]
async fn buflen_and_period_are_clamped() {
    let ioc = boot_enabled().await;
    for (pv, v, buflen, sent) in [
        ("T:mode", EpicsValue::Enum(1), 1.0, ":CONF0:AVGCURR 1"),
        ("T:buflen", EpicsValue::Long(99), 50.0, ":CONF0:AVGCURR 50"),
        ("T:buflen", EpicsValue::Long(0), 1.0, ":CONF0:AVGCURR 1"),
        ("T:mode", EpicsValue::Enum(2), 1.0, ":CONF0:WDWCURR 1"),
    ] {
        ioc.bpm.clear_seen();
        put(&ioc, pv, v).await;
        expect_commands(&ioc, &[sent]).await;
        assert_eq!(f64_of(&ioc, "T:buflen"), buflen, "{pv}");
    }
    put(&ioc, "T:period", EpicsValue::Double(0.01)).await;
    wait_for(&ioc, "T:period", 0.05).await;
    put(&ioc, "T:period", EpicsValue::Double(20000.0)).await;
    wait_for(&ioc, "T:period", 10000.0).await;
}

/// No reply raises the comm error; it stays through the device answering
/// again, since comm_error waits for init, and init's first good read
/// clears it.
#[epics_base_rs::epics_test]
async fn a_comm_error_holds_until_init_reads_again() {
    let ioc = boot_enabled().await;
    ioc.bpm.set_silent(true);
    wait_for(&ioc, "T:error.RVAL", 2.0).await;
    // idle puts errMsg after error (sncqxbpm.st:429-433).
    let deadline = Instant::now() + Duration::from_secs(20);
    while string_of(&ioc, "T:errMsg") != "communications error: 2" && Instant::now() < deadline {
        task::sleep(Duration::from_millis(20)).await;
    }
    let error = DbChannel::new(&ioc.db, "T:error");
    assert_eq!(error.get_string().await, "comm error");
    assert_eq!(string_of(&ioc, "T:errMsg"), "communications error: 2");
    // MAJOR
    assert_eq!(f64_of(&ioc, "T:asyn.SEVR"), 2.0);

    ioc.bpm.set_silent(false);
    ioc.bpm.set_raw([5, 6, 7, 8]);
    task::sleep(Duration::from_secs(2)).await;
    assert_eq!(f64_of(&ioc, "T:error.RVAL"), 2.0);
    assert_eq!(f64_of(&ioc, "T:current:a:raw"), 1000.0);

    ioc.bpm.clear_seen();
    put(&ioc, "T:init", EpicsValue::Double(1.0)).await;
    expect_commands(&ioc, &["*RST0", ":CONF0:CURR:RANG 1", ":CONF0:SINGLE"]).await;
    assert_eq!(f64_of(&ioc, "T:error.RVAL"), 0.0);
    assert_eq!(error.get_string().await, "no error");
    assert_eq!(string_of(&ioc, "T:errMsg"), "");
    assert_eq!(f64_of(&ioc, "T:current:a:raw"), 5.0);
    assert_eq!(f64_of(&ioc, "T:init.RVAL"), 0.0);
}

/// A reply short of four counts, an empty one or the prefix alone leaves
/// the reading as it was and raises no error; a full one updates it.
#[epics_base_rs::epics_test]
async fn a_short_reply_leaves_the_reading() {
    let ioc = boot_with_defaults().await;
    for reply in [">5 6", "", ">"] {
        ioc.bpm.set_reply(Some(reply));
        reads(&ioc, 3).await;
        for (pv, want) in [
            ("T:current:a:raw", 1000.0),
            ("T:current:b:raw", 2000.0),
            ("T:current:c:raw", 3000.0),
            ("T:current:d:raw", 4000.0),
            ("T:error.RVAL", 0.0),
        ] {
            assert_eq!(f64_of(&ioc, pv), want, "{pv} after {reply:?}");
        }
        assert_near(&ioc, "T:current:a", 3.5e-10);
        assert_near(&ioc, "T:current:c", 1.05e-9);
    }
    ioc.bpm.set_reply(None);
    ioc.bpm.set_raw([5, 6, 7, 8]);
    reads(&ioc, 2).await;
    assert_eq!(f64_of(&ioc, "T:current:a:raw"), 5.0);
    assert_eq!(f64_of(&ioc, "T:current:d:raw"), 8.0);
}

/// set_offsets steps through the six ranges, stores each one's offsets from
/// the reading and puts the gain back.
#[epics_base_rs::epics_test]
async fn set_offsets_reads_each_gain_and_restores_it() {
    let ioc = boot_enabled().await;
    // RANG follows the gain_mon event flag, so two gains put within one read
    // cycle send one RANG, as in C. macOS runners stretched a read cycle past
    // 0.1 s; a 1 s settling outlasts it.
    put(&ioc, "T:settling", EpicsValue::Double(1.0)).await;
    put(&ioc, "T:gain", EpicsValue::Enum(2)).await;
    expect_commands(
        &ioc,
        &[
            "*RST0",
            ":CONF0:CURR:RANG 1",
            ":CONF0:SINGLE",
            ":CONF0:CURR:RANG 3",
        ],
    )
    .await;
    ioc.bpm.clear_seen();
    put(&ioc, "T:set_offsets", EpicsValue::Double(1.0)).await;
    wait_for(&ioc, "T:set_offsets.RVAL", 0.0).await;
    let ranges: Vec<String> = (1..=6)
        .chain([3])
        .map(|r| format!(":CONF0:CURR:RANG {r}"))
        .collect();
    expect_commands(&ioc, &ranges.iter().map(String::as_str).collect::<Vec<_>>()).await;
    assert_eq!(f64_of(&ioc, "T:gain"), 2.0);
    for (pv, want) in [
        ("T:r1:A2", 1000.0),
        ("T:r1:B2", 2000.0),
        ("T:r1:C2", 3000.0),
        ("T:r1:D2", 4000.0),
        ("T:r4:A2", 1000.0),
        ("T:r6:D2", 4000.0),
    ] {
        assert_eq!(f64_of(&ioc, pv), want, "{pv}");
    }
}

/// Disabled, the program sends nothing; enabled again, it runs init.
#[epics_base_rs::epics_test]
async fn disabling_stops_the_commands_and_enabling_inits() {
    let ioc = boot_enabled().await;
    put(&ioc, "T:enable", EpicsValue::Double(0.0)).await;
    task::sleep(Duration::from_secs(1)).await;
    ioc.bpm.clear_seen();
    task::sleep(Duration::from_secs(2)).await;
    assert!(ioc.bpm.seen().is_empty(), "{:?}", ioc.bpm.seen());

    put(&ioc, "T:enable", EpicsValue::Double(1.0)).await;
    expect_commands(&ioc, &["*RST0", ":CONF0:CURR:RANG 1", ":CONF0:SINGLE"]).await;
    assert_eq!(f64_of(&ioc, "T:init.RVAL"), 0.0);
}
