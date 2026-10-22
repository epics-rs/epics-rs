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

use epics_base_rs::runtime::stdlib::{c_isspace, strtod};
use epics_base_rs::runtime::task::BlockingBridge;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::snl::{SeqMacros, seq, spawn_program};

use crate::snl::filter_drive::FilterDriveConfig;
use crate::snl::flex_combined_motion::FlexConfig;
use crate::snl::hr_ctl::HrConfig;
use crate::snl::io::IoConfig;
use crate::snl::ml_mono_ctl::MlMonoConfig;
use crate::snl::pf4::Pf4Config;

// `SEQ_PROGRAMS`: every program `build.rs` compiled from `src/snl/*.st`.
include!(concat!(env!("OUT_DIR"), "/seq_programs.rs"));

/// C `sscanf(s, "%d", &n)` into a variable SNL zeroed: glibc's `strtol`
/// stored through the `int`, or 0 when no digit converts.
fn scan_int(s: &str) -> i32 {
    let s = s.trim_start_matches(c_isspace);
    let (negative, digits) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    let magnitude = digits[..end]
        .bytes()
        .fold(0i128, |n, d| (n * 10 + i128::from(d - b'0')).min(1 << 64));
    let long = if negative { -magnitude } else { magnitude };
    long.clamp(i64::MIN.into(), i64::MAX.into()) as i64 as i32
}

/// An optics program, configured from its macros as its `.st` reads them.
enum Program {
    HrCtl(HrConfig),
    MlMonoCtl(MlMonoConfig),
    FilterDrive(FilterDriveConfig),
    Pf4(Pf4Config),
    Io(IoConfig),
    FlexCombinedMotion(FlexConfig),
}

impl Program {
    /// Each arm's parameters are its `.st`'s `program` statement. A macro
    /// with no value reads as "", what `seqMacEval` puts for its `{name}`.
    fn new(program: &str, macro_def: &str) -> Result<(Self, SeqMacros), String> {
        let macros = |params| SeqMacros::new(params, macro_def);
        Ok(match program {
            "hrCtl" => {
                let m = macros("P=xxx:, N=1, M_PHI1=m9, M_PHI2=m10");
                let v = |name| m.value(name).unwrap_or_default();
                (
                    Self::HrCtl(HrConfig::new(v("P"), v("N"), v("M_PHI1"), v("M_PHI2"))),
                    m,
                )
            }
            "ml_monoCtl" => {
                let m =
                    macros("P=xxx:, M_THETA=m9, M_THETA2=m12, M_Y=m10, M_Z=m11, Y_OFF=35, GEOM=1");
                let v = |name| m.value(name).unwrap_or_default();
                (
                    Self::MlMonoCtl(MlMonoConfig::new(
                        v("P"),
                        v("M_THETA"),
                        v("M_THETA2"),
                        v("M_Y"),
                        v("M_Z"),
                        strtod(v("Y_OFF").as_bytes()).value,
                    )),
                    m,
                )
            }
            "filterDrive" => {
                let m = macros("NAME=filterDrive, P=TEST:, R=filter:, NUM_FILTERS=8");
                let v = |name| m.value(name).unwrap_or_default();
                let mut config = FilterDriveConfig::new(v("P"), v("R"), scan_int(v("NUM_FILTERS")));
                config.name = v("NAME").to_string();
                (Self::FilterDrive(config), m)
            }
            "pf4" => {
                let m = macros("name=pf4,P=,H=,B=,BP=,B1=,B2=,B3=,B4=,M=");
                let v = |name| m.value(name).unwrap_or_default();
                let mut config = Pf4Config::new(v("P"), v("H"), v("B"));
                config.name = v("name").to_string();
                config.mono = v("M").to_string();
                config.bit_prefix = v("BP").to_string();
                config.bits = ["B1", "B2", "B3", "B4"].map(|b| v(b).to_string());
                (Self::Pf4(config), m)
            }
            "Io" => {
                let m = macros("P=7idd:Io:,MONO=7id:HHLM:energy_get,VSC=7idd:scaler1");
                let v = |name| m.value(name).unwrap_or_default();
                (Self::Io(IoConfig::new(v("P"), v("MONO"), v("VSC"))), m)
            }
            "flexCombinedMotion" => {
                let m =
                    macros("name=flexCombinedMotion,P=xxx:,M=m1,C=cap1,FM=pi:c0:m1,CM=nf:c0:m1");
                let v = |name| m.value(name).unwrap_or_default();
                (
                    Self::FlexCombinedMotion(FlexConfig::new(
                        v("P"),
                        v("M"),
                        v("C"),
                        v("FM"),
                        v("CM"),
                    )),
                    m,
                )
            }
            _ => return Err(format!("Can't find sequencer `{program}'.")),
        })
    }

    fn spawn(self, macros: &SeqMacros, bridge: &BlockingBridge, db: &PvDatabase) {
        match self {
            Self::HrCtl(config) => spawn_program(bridge, db, "hrCtl", macros, move |db| {
                crate::snl::hr_ctl::run(config, db)
            }),
            Self::MlMonoCtl(config) => spawn_program(bridge, db, "ml_monoCtl", macros, move |db| {
                crate::snl::ml_mono_ctl::run(config, db)
            }),
            Self::FilterDrive(config) => {
                spawn_program(bridge, db, "filterDrive", macros, move |db| {
                    crate::snl::filter_drive::run(config, db)
                })
            }
            Self::Pf4(config) => spawn_program(bridge, db, "pf4", macros, move |db| {
                crate::snl::pf4::run(config, db)
            }),
            Self::Io(config) => seq(bridge, "Io", macros, crate::snl::io::run(config)),
            Self::FlexCombinedMotion(config) => seq(
                bridge,
                "flexCombinedMotion",
                macros,
                crate::snl::flex_combined_motion::run(config),
            ),
        }
    }
}

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
    bridge: &BlockingBridge,
    db: &PvDatabase,
) -> Result<(), String> {
    if let Some(compiled) = SEQ_PROGRAMS.iter().find(|p| p.prog_name() == program) {
        let _ = compiled.start(macro_str, 0, db);
        return Ok(());
    }
    let (program, macros) = Program::new(program, macro_str)?;
    program.spawn(&macros, bridge, db);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// With no macros at all, each program reads what its `.st`'s `program`
    /// statement defines.
    #[test]
    fn every_program_configures_on_its_program_defaults() {
        let Ok((Program::HrCtl(c), _)) = Program::new("hrCtl", "") else {
            panic!("hrCtl")
        };
        assert_eq!(
            [&c.prefix, &c.n, &c.m_phi1, &c.m_phi2],
            ["xxx:", "1", "m9", "m10"]
        );
        let Ok((Program::MlMonoCtl(c), _)) = Program::new("ml_monoCtl", "") else {
            panic!("ml_monoCtl")
        };
        assert_eq!(
            [&c.prefix, &c.m_theta, &c.m_theta2, &c.m_y, &c.m_z],
            ["xxx:", "m9", "m12", "m10", "m11"]
        );
        assert_eq!(c.y_offset, 35.0);
        let Ok((Program::FilterDrive(c), _)) = Program::new("filterDrive", "") else {
            panic!("filterDrive")
        };
        assert_eq!(
            [&c.name, &c.prefix, &c.record],
            ["filterDrive", "TEST:", "filter:"]
        );
        assert_eq!(c.num_filters, 8);
        let Ok((Program::Pf4(c), _)) = Program::new("pf4", "") else {
            panic!("pf4")
        };
        assert_eq!(c.name, "pf4");
        assert_eq!(
            [&c.prefix, &c.hardware, &c.bank, &c.mono, &c.bit_prefix],
            ["", "", "", "", ""]
        );
        assert_eq!(c.bits, ["", "", "", ""]);
        let Ok((Program::Io(c), _)) = Program::new("Io", "") else {
            panic!("Io")
        };
        assert_eq!(
            [&c.prefix, &c.mono_pv, &c.scaler_pv],
            ["7idd:Io:", "7id:HHLM:energy_get", "7idd:scaler1"]
        );
        let Ok((Program::FlexCombinedMotion(c), _)) = Program::new("flexCombinedMotion", "") else {
            panic!("flexCombinedMotion")
        };
        assert_eq!(
            [&c.prefix, &c.motor, &c.cap, &c.fine_motor, &c.coarse_motor],
            ["xxx:", "m1", "cap1", "pi:c0:m1", "nf:c0:m1"]
        );
    }

    /// A macro's text converted as the `.st` converts it: Y_OFF by `atof`
    /// (`ml_monoCtl.st:523`), NUM_FILTERS by `%d` (`filterDrive.st:341`).
    #[test]
    fn macro_text_converts_as_the_st_reads_it() {
        let y_off = |def| match Program::new("ml_monoCtl", def) {
            Ok((Program::MlMonoCtl(c), _)) => c.y_offset,
            _ => panic!("ml_monoCtl"),
        };
        assert_eq!(y_off("Y_OFF=40mm"), 40.0);
        assert_eq!(y_off("Y_OFF= 4e1"), 40.0);
        assert_eq!(y_off("Y_OFF=mm"), 35.0);
        let filters = |def| match Program::new("filterDrive", def) {
            Ok((Program::FilterDrive(c), _)) => c.num_filters,
            _ => panic!("filterDrive"),
        };
        assert_eq!(filters("NUM_FILTERS=+4"), 4);
        assert_eq!(filters("NUM_FILTERS=-1"), -1);
        assert_eq!(filters("NUM_FILTERS= 3 blades"), 3);
        assert_eq!(filters("NUM_FILTERS=x"), 0);
    }

    /// The capacitance sensor is `{P}{C}:pos` (`flexCombinedMotion.st:32`).
    #[test]
    fn flex_reads_its_sensor_from_c() {
        let Ok((Program::FlexCombinedMotion(c), _)) =
            Program::new("flexCombinedMotion", "P=F:,C=cap9")
        else {
            panic!("flexCombinedMotion")
        };
        assert_eq!(c.pos_monitor_pv(), "F:cap9:pos");
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
        let db = PvDatabase::new();
        db.add_record(
            "F:f:NumFilters",
            Box::new(epics_base_rs::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();
        db.mark_pini_done();
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
        let db = PvDatabase::new();
        db.add_record(
            "N:f:NumFilters",
            Box::new(epics_base_rs::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();
        db.mark_pini_done();
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

    /// ml_monoCtl's y offset is the `Y_OFF` macro (`ml_monoCtl.st:587`),
    /// put by initSequence; with every motor readback at 0, chkMotorLimits
    /// then holds and nothing overwrites it.
    #[epics_base_rs::epics_test]
    async fn ml_mono_takes_its_y_offset_from_y_off() {
        let db = PvDatabase::new();
        db.add_record(
            "Y:ml_mono_yOffset",
            Box::new(epics_base_rs::server::records::ao::AoRecord::new(0.0)),
        )
        .await
        .unwrap();
        db.mark_pini_done();
        let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
        seq_start("ml_monoCtl", "P=Y:,M_THETA=th,Y_OFF=40", &bridge, &db).unwrap();
        for _ in 0..300 {
            if db.get_pv("Y:ml_mono_yOffset").unwrap().to_f64() == Some(40.0) {
                return;
            }
            epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("yOffset is {:?}", db.get_pv("Y:ml_mono_yOffset").unwrap());
    }

    /// A motor macro the command line leaves out takes the program's default
    /// (`ml_monoCtl.st:1`), which initSequence names in `{P}ml_monoTheta2Pv`.
    #[epics_base_rs::epics_test]
    async fn ml_mono_theta2_motor_defaults_to_m12() {
        let db = PvDatabase::new();
        db.add_record(
            "Y:ml_monoTheta2Pv",
            Box::new(epics_base_rs::server::records::stringin::StringinRecord::new("")),
        )
        .await
        .unwrap();
        db.mark_pini_done();
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
