//! General-purpose SNL program launcher.
//!
//! Provides `seq_start(program, macros)` — a single entry point to start any
//! optics state machine by name, matching the C EPICS `seq &program, "macros"`
//! pattern.
//!
//! # Usage from st.cmd
//!
//! ```text
//! seqStart("kohzuCtl", "P=mini:,M_THETA=dcm:theta,M_Y=dcm:y,M_Z=dcm:z")
//! seqStart("hrCtl", "P=mini:,N=1,M_PHI1=hr:phi1,M_PHI2=hr:phi2")
//! seqStart("orient", "P=mini:,PM=mini:,mTTH=tth,mTH=th,mCHI=chi,mPHI=phi")
//! ```

// RTEMS-EXEC-MODEL-ALLOW(2): checked, not waived — all 2 ran and passed
// on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p optics-rs
// --all-features`, 459/459). optics-rs became a census subject when its
// `build.rs` began deriving `tokio_backend`; nothing here builds a CA
// server, and the reactor these obtain comes from `#[tokio::test]`
// itself, which the backend does not remove.

use epics_base_rs::runtime::task::BlockingBridge;
use epics_base_rs::server::database::PvDatabase;

// `SEQ_PROGRAMS`: every program `build.rs` compiled from `src/snl/*.st`.
include!(concat!(env!("OUT_DIR"), "/seq_programs.rs"));

/// Start an optics SNL program by name.
///
/// This is the Rust equivalent of the C EPICS `seq &program, "macros"` command:
/// the macros are parsed over the program's `program` parameters, so a macro
/// left out takes the default its `.st` gives it.
///
/// | Name | Description |
/// |------|-------------|
/// | `kohzuCtl` | Kohzu double-crystal monochromator |
/// | `kohzuCtl_soft` | Kohzu soft motor variant |
/// | `hrCtl` | High-resolution analyzer |
/// | `ml_monoCtl` | Multi-layer monochromator |
/// | `orient` | 4-circle diffractometer |
/// | `filterDrive` | Automatic filter selection |
/// | `pf4` | XIA PF4 filter bank |
/// | `Io` | Ion chamber intensity |
/// | `flexCombinedMotion` | Coarse+fine flexure stage |
///
/// Returns `Ok(())` if the program was found and spawned, or `Err` with
/// seq_cmd.c's console line for one it does not know. A compiled program is
/// started by `seq`, whose result seq_cmd.c:159-160 ignores.
///
/// Must be called with the runtime access captured for the shell thread (use
/// `CommandContext::bridge()` from st.cmd startup commands, since st.cmd runs
/// on a blocking thread the runtime is otherwise unreachable from).
pub fn seq_start(
    program: &str,
    macro_str: &str,
    _bridge: &BlockingBridge,
    db: &PvDatabase,
) -> Result<(), String> {
    if let Some(compiled) = SEQ_PROGRAMS.iter().find(|p| p.prog_name() == program) {
        let _ = compiled.start(macro_str, 0, db);
        return Ok(());
    }
    Err(format!("Can't find sequencer `{program}'."))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use epics_base_rs::server::database::db_access::DbSubscription;
    use epics_base_rs::server::ioc_builder::IocBuilder;
    use epics_base_rs::server::records::busy::BusyRecord;

    use super::*;

    /// `records` and the shipped `db`, past `iocInit`'s PINI pass. snc builds
    /// `+c` by default, so a program starts only once every channel it
    /// assigns has connected: the database must hold all of them.
    async fn ioc(records: &str, db: &str, macros: &[(&str, &str)]) -> Arc<PvDatabase> {
        let macros: HashMap<String, String> = macros
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let (db, _) = IocBuilder::new()
            .register_record_type("busy", || Box::new(BusyRecord::default()))
            .register_record_type("motor", || Box::new(motor_rs::MotorRecord::new()))
            .db_string(records, &macros)
            .unwrap()
            .db_file(&format!("{}/db/{db}", env!("CARGO_MANIFEST_DIR")), &macros)
            .unwrap()
            .build()
            .await
            .unwrap();
        db.ioc_init().await;
        db.mark_pini_done();
        db
    }

    /// `filterDrive.db` under `P`, `R=f:`.
    async fn filter_drive_ioc(p: &str) -> Arc<PvDatabase> {
        let kev = format!("{p}kev");
        ioc(
            &format!("record(ao, \"{kev}\") {{}}\n"),
            "filterDrive.db",
            &[("P", p), ("R", "f:"), ("DESC", "fd"), ("KEV", &kev)],
        )
        .await
    }

    /// `ml_monoSeq.db` under `P=Y:`, with the `th` the tests pass as
    /// `M_THETA` and the program's default `m12`, `m10` and `m11` motors.
    async fn ml_mono_ioc() -> Arc<PvDatabase> {
        ioc(
            "record(motor, \"Y:th\") {}\nrecord(motor, \"Y:m12\") {}\n\
             record(motor, \"Y:m10\") {}\nrecord(motor, \"Y:m11\") {}\n",
            "ml_monoSeq.db",
            &[("P", "Y:")],
        )
        .await
    }

    /// `seq_main.c:67`, `:140-142`: every start posts the sequencer release
    /// and the spawn on errlog, the thread named by the `name` macro, else by
    /// the program. Io starts without the PINI wait and posts them the same.
    #[epics_base_rs::epics_test]
    async fn every_start_posts_seqs_errlog_lines() {
        let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = lines.clone();
        epics_base_rs::runtime::log::errlog_add_listener(move |m| {
            if m.contains("equencer") {
                sink.lock().unwrap().push(m.trim_end().to_owned());
            }
        });
        let db = PvDatabase::new();
        let bridge = BlockingBridge::capture();
        seq_start("Io", "name=io1", &bridge, &db).unwrap();
        seq_start("kohzuCtl", "P=T:", &bridge, &db).unwrap();
        epics_base_rs::runtime::log::errlog_flush();
        let lines: Vec<String> = lines
            .lock()
            .unwrap()
            .iter()
            .map(|l| {
                let (head, tail) = l.split_once("thread 0x").unwrap_or((l, ""));
                match tail.split_once(':') {
                    Some((_, rest)) => format!("{head}thread *:{rest}"),
                    None => l.clone(),
                }
            })
            .collect();
        let release = "sevr=info Sequencer release ";
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert!(lines[0].starts_with(release) && lines[2].starts_with(release));
        assert_eq!(
            lines[1],
            "sevr=info Spawning sequencer program \"Io\", thread *: \"io1\""
        );
        assert_eq!(
            lines[3],
            "sevr=info Spawning sequencer program \"kohzuCtl\", thread *: \"kohzuCtl\""
        );
    }

    /// seq_cmd.c:162.
    #[test]
    fn test_unknown_program() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let bridge = {
            let _guard = rt.enter();
            epics_base_rs::runtime::task::BlockingBridge::capture()
        };
        let db = PvDatabase::new();
        assert_eq!(
            seq_start("nonexistent", "P=x:", &bridge, &db),
            Err("Can't find sequencer `nonexistent'.".to_string())
        );
    }

    /// filterDrive's blade count is the `NUM_FILTERS` macro
    /// (`filterDrive.st:338`), published to `NumFilters` by `start`.
    #[epics_base_rs::epics_test]
    async fn filter_drive_takes_its_blade_count_from_num_filters() {
        let db = filter_drive_ioc("F:").await;
        let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
        seq_start(
            "filterDrive",
            "NAME=fd,P=F:,R=f:,NUM_FILTERS=2",
            &bridge,
            &db,
        )
        .unwrap();
        for _ in 0..300 {
            if db.get_pv("F:f:NumFilters").unwrap().to_f64() == Some(2.0) {
                return;
            }
            epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("NumFilters is {:?}", db.get_pv("F:f:NumFilters").unwrap());
    }

    /// A negative count assigns no blade and is published as it is
    /// (`filterDrive.st:344-367`).
    #[epics_base_rs::epics_test]
    async fn filter_drive_publishes_a_negative_num_filters() {
        let db = filter_drive_ioc("N:").await;
        let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
        seq_start("filterDrive", "P=N:,R=f:,NUM_FILTERS=-1", &bridge, &db).unwrap();
        for _ in 0..300 {
            if db.get_pv("N:f:NumFilters").unwrap().to_f64() == Some(-1.0) {
                return;
            }
            epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("NumFilters is {:?}", db.get_pv("N:f:NumFilters").unwrap());
    }

    /// initSequence puts the `Y_OFF` macro into the y offset
    /// (`ml_monoCtl.st:571-582`), and updatePsuedo puts the y motor readback,
    /// 0 here, once the readback's first monitor sets `yMotRdbk_mon`, as every
    /// monitor sets its synced flag (`seq_ca.c:328-329`, `ml_monoCtl.st:1281-1284`).
    /// The two state sets run on their own threads, so either put may be the
    /// last, but each puts its own value, where C could put one twice.
    #[epics_base_rs::epics_test]
    async fn ml_mono_puts_y_off_and_the_y_readback_into_its_y_offset() {
        let db = ml_mono_ioc().await;
        let mut sub = DbSubscription::subscribe(&db, "Y:ml_mono_yOffset")
            .await
            .unwrap();
        let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
        seq_start("ml_monoCtl", "P=Y:,M_THETA=th,Y_OFF=40", &bridge, &db).unwrap();
        let mut posts = Vec::new();
        for _ in 0..300 {
            // initSequence's puts end on the y motor's name (`ml_monoCtl.st:588`)
            let init_done = db.get_pv("Y:ml_monoYPv").unwrap().text_g() == "Y:m10";
            while let Ok(e) = sub.try_recv_event() {
                posts.push(e.value().as_value().and_then(|v| v.to_f64()));
            }
            if init_done && posts.contains(&Some(0.0)) {
                assert!(
                    posts.contains(&Some(40.0))
                        && posts.iter().all(|&p| p == Some(40.0) || p == Some(0.0)),
                    "y offset posts {posts:?}"
                );
                return;
            }
            epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "y offset posts {posts:?}, y motor {:?}",
            db.get_pv("Y:ml_monoYPv").unwrap()
        );
    }

    /// A motor macro the command line leaves out takes the program's default
    /// (`ml_monoCtl.st:1`), which initSequence names in `{P}ml_monoTheta2Pv`.
    #[epics_base_rs::epics_test]
    async fn ml_mono_theta2_motor_defaults_to_m12() {
        let db = ml_mono_ioc().await;
        let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
        seq_start("ml_monoCtl", "P=Y:,M_THETA=th", &bridge, &db).unwrap();
        for _ in 0..300 {
            if db.get_pv("Y:ml_monoTheta2Pv").unwrap().text_g() == "Y:m12" {
                return;
            }
            epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "theta2MotName is {:?}",
            db.get_pv("Y:ml_monoTheta2Pv").unwrap()
        );
    }

    /// `kohzuCtl.st:1` defines every macro the program reads, so `seq` starts
    /// it on its prefix alone.
    #[test]
    fn kohzu_ctl_starts_on_its_program_defaults() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let bridge = {
            let _guard = rt.enter();
            epics_base_rs::runtime::task::BlockingBridge::capture()
        };
        let db = PvDatabase::new();
        assert_eq!(seq_start("kohzuCtl", "P=T:", &bridge, &db), Ok(()));
    }

    /// The motors `kohzuSeq.db` leaves to the IOC. kohzuCtl is built `+c`,
    /// so it starts only once every pv it assigns has connected.
    const KOHZU_MOTORS: &str = r#"
record(motor, "T:th") {}
record(motor, "T:y") {}
record(motor, "T:z") {}
"#;

    fn two_d(db: &PvDatabase) -> f64 {
        db.get_pv("T:Bragg2dSpacingAO").unwrap().to_f64().unwrap()
    }

    /// Load `kohzuSeq.db` and the motors the way `st.cmd`'s `dbLoadRecords`
    /// does — into a database still in its load phase, so every record's
    /// `init_record` (where a constant `DOL` seeds `VAL`) is owed to
    /// `iocInit` and has NOT run when `seqStart` follows.
    /// `Bragg2dSpacingAO` is set to -1, which neither the pass (its `DOL` 1,
    /// then the program's 2a/√3) nor an ungated start (2A/0 from the
    /// unprocessed lattice) writes.
    async fn load_kohzu(db: &PvDatabase) {
        use epics_base_rs::server::db_loader;
        db_loader::register_record_type(
            "motor",
            Box::new(|| Box::new(motor_rs::MotorRecord::new())),
        );
        db_loader::register_record_type(
            "busy",
            Box::new(|| Box::new(epics_base_rs::server::records::busy::BusyRecord::default())),
        );
        let macros: HashMap<String, String> = [
            ("P", "T:"),
            ("M_THETA", "th"),
            ("M_Y", "y"),
            ("M_Z", "z"),
            ("yOffHi", "50"),
            ("yOffLo", "-50"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let kohzu_db =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/db/kohzuSeq.db"))
                .unwrap();
        db.begin_load().unwrap();
        for text in [kohzu_db.as_str(), KOHZU_MOTORS] {
            for mut def in db_loader::parse_db(text, &macros).unwrap() {
                let mut record = db_loader::create_record(&def.record_type).unwrap();
                let mut common_fields = Vec::new();
                db_loader::apply_fields(&mut record, &def.fields, &mut common_fields).unwrap();
                db.add_loaded_record(
                    &def.name,
                    record,
                    epics_base_rs::server::database::RecordLoad {
                        common_fields,
                        info_tags: std::mem::take(&mut def.info_tags),
                    },
                )
                .await
                .unwrap();
            }
        }
        db.get_record("T:Bragg2dSpacingAO")
            .unwrap()
            .write()
            .record
            .put_field("VAL", epics_base_rs::types::EpicsValue::Double(-1.0))
            .unwrap();
    }

    /// `seqStart` is issued from `st.cmd`, before `iocInit`. The program must
    /// not read the lattice until `iocInit`'s PINI pass has run — before it
    /// the Miller indices are the unprocessed 0, and a d-spacing computed from
    /// them is then overwritten by the pass, which is the `two_d=0.0000` boot
    /// the mini-beamline IOC showed. Si(111), a = 5.43102 Å: 2d = 2a/√3.
    #[epics_base_rs::epics_test]
    async fn kohzu_ctl_reads_the_lattice_only_after_pini() {
        let db = PvDatabase::new();
        load_kohzu(&db).await;
        let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
        seq_start("kohzuCtl", "P=T:,M_THETA=th,M_Y=y,M_Z=z", &bridge, &db).unwrap();

        epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(two_d(&db), -1.0, "kohzuCtl wrote the d-spacing before PINI");

        db.ioc_init().await;
        db.pini_process(epics_base_rs::server::record::PiniMode::Yes)
            .await;
        db.mark_pini_done();
        let expected = 2.0 * 5.43102 / 3f64.sqrt();
        for _ in 0..300 {
            if (two_d(&db) - expected).abs() < 1e-4 {
                return;
            }
            epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "kohzuCtl left Bragg2dSpacingAO at {} after PINI, expected {expected:.4}",
            two_d(&db)
        );
    }
}
