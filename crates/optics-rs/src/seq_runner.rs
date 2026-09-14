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

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — all 1 ran and passed
// on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p optics-rs
// --all-features`, 412/412). optics-rs became a census subject when its
// `build.rs` began deriving `tokio_backend`; nothing here builds a CA
// server, and the reactor these obtain comes from `#[tokio::test]`
// itself, which the backend does not remove.

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::snl::spawn_program;

/// Parse a macro string like `"P=mini:,M_THETA=dcm:theta"` into a HashMap.
pub fn parse_macros(input: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for part in input.split(',') {
        let part = part.trim();
        if let Some((k, v)) = part.split_once('=') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    map
}

/// Helper to get a macro value or return an error.
fn require_macro(
    macros: &HashMap<String, String>,
    key: &str,
    program: &str,
) -> Result<String, String> {
    macros
        .get(key)
        .cloned()
        .ok_or_else(|| format!("{program}: required macro '{key}' not specified"))
}

/// Helper to get a macro value with a default.
fn macro_or(macros: &HashMap<String, String>, key: &str, default: &str) -> String {
    macros
        .get(key)
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

/// Start an optics SNL program by name.
///
/// This is the Rust equivalent of the C EPICS `seq &program, "macros"` command.
/// Spawns a tokio task that runs the state machine asynchronously.
///
/// # Supported programs
///
/// | Name | Macros | Description |
/// |------|--------|-------------|
/// | `kohzuCtl` | P, M_THETA, M_Y, M_Z, GEOM(opt) | Kohzu double-crystal monochromator |
/// | `kohzuCtl_soft` | P, MONO(opt) | Kohzu soft motor variant |
/// | `hrCtl` | P, N(opt), M_PHI1, M_PHI2 | High-resolution analyzer |
/// | `ml_monoCtl` | P, M_THETA, M_THETA2(opt), M_Z(opt) | Multi-layer monochromator |
/// | `orient` | P, PM, mTTH, mTH, mCHI, mPHI | 4-circle diffractometer |
/// | `filterDrive` | P, R | Automatic filter selection |
/// | `pf4` | P, R, H(opt) | XIA PF4 dual filter bank |
/// | `Io` | P, R | Ion chamber intensity |
/// | `flexCombinedMotion` | P, CM, FM | Coarse+fine flexure stage |
///
/// Returns `Ok(())` if the program was found and spawned, or `Err` with a message.
///
/// Must be called with the runtime access captured for the shell thread (use
/// `CommandContext::bridge()` from st.cmd startup commands, since st.cmd runs
/// on a blocking thread the runtime is otherwise unreachable from).
pub fn seq_start(
    program: &str,
    macro_str: &str,
    bridge: &epics_base_rs::runtime::task::BlockingBridge,
    db: &PvDatabase,
) -> Result<(), String> {
    let macros = parse_macros(macro_str);

    match program {
        "kohzuCtl" => {
            let config = crate::snl::kohzu_ctl::KohzuConfig::new(
                &require_macro(&macros, "P", program)?,
                &require_macro(&macros, "M_THETA", program)?,
                &require_macro(&macros, "M_Y", program)?,
                &require_macro(&macros, "M_Z", program)?,
                macro_or(&macros, "GEOM", "0").parse::<i32>().unwrap_or(0),
            );
            spawn_program(bridge, db, "kohzuCtl", move |db| {
                crate::snl::kohzu_ctl::run(config, db)
            });
        }
        "kohzuCtl_soft" => {
            let config = crate::snl::kohzu_ctl_soft::KohzuSoftConfig::new(
                &require_macro(&macros, "P", program)?,
                &macro_or(&macros, "MONO", ""),
                &require_macro(&macros, "M_THETA", program)?,
                &require_macro(&macros, "M_Y", program)?,
                &require_macro(&macros, "M_Z", program)?,
                macro_or(&macros, "GEOM", "0").parse::<i32>().unwrap_or(0),
            );
            spawn_program(bridge, db, "kohzuCtl_soft", move |db| {
                crate::snl::kohzu_ctl_soft::run(config, db)
            });
        }
        "hrCtl" => {
            let config = crate::snl::hr_ctl::HrConfig::new(
                &require_macro(&macros, "P", program)?,
                &macro_or(&macros, "N", "1"),
                &require_macro(&macros, "M_PHI1", program)?,
                &require_macro(&macros, "M_PHI2", program)?,
            );
            spawn_program(bridge, db, "hrCtl", move |db| {
                crate::snl::hr_ctl::run(config, db)
            });
        }
        "ml_monoCtl" => {
            let config = crate::snl::ml_mono_ctl::MlMonoConfig::new(
                &require_macro(&macros, "P", program)?,
                &require_macro(&macros, "M_THETA", program)?,
                &macro_or(&macros, "M_THETA2", ""),
                &macro_or(&macros, "M_Y", ""),
                &macro_or(&macros, "M_Z", ""),
                macro_or(&macros, "Y_OFFSET", "35.0")
                    .parse::<f64>()
                    .unwrap_or(35.0),
                macro_or(&macros, "GEOM", "0").parse::<i32>().unwrap_or(0),
            );
            spawn_program(bridge, db, "ml_monoCtl", move |db| {
                crate::snl::ml_mono_ctl::run(config, db)
            });
        }
        "orient" => {
            let config = crate::snl::orient::OrientConfig::new(
                &require_macro(&macros, "P", program)?,
                &require_macro(&macros, "PM", program)?,
                &require_macro(&macros, "mTTH", program)?,
                &require_macro(&macros, "mTH", program)?,
                &require_macro(&macros, "mCHI", program)?,
                &require_macro(&macros, "mPHI", program)?,
            );
            spawn_program(bridge, db, "orient", move |db| {
                crate::snl::orient::run(config, db)
            });
        }
        "filterDrive" => {
            let config = crate::snl::filter_drive::FilterDriveConfig::new(
                &require_macro(&macros, "P", program)?,
                &require_macro(&macros, "R", program)?,
                macro_or(&macros, "N", "8").parse::<usize>().unwrap_or(8),
            );
            spawn_program(bridge, db, "filterDrive", move |db| {
                crate::snl::filter_drive::run(config, db)
            });
        }
        "pf4" => {
            let config = crate::snl::pf4::Pf4Config::new(
                &require_macro(&macros, "P", program)?,
                &macro_or(&macros, "H", ""),
                &require_macro(&macros, "B", program)?,
            );
            spawn_program(bridge, db, "pf4", move |db| {
                crate::snl::pf4::run(config, db)
            });
        }
        "Io" => {
            let config = crate::snl::io::IoConfig::new(
                &require_macro(&macros, "P", program)?,
                &macro_or(&macros, "MONO", ""),
                &macro_or(&macros, "VSC", ""),
            );
            bridge.spawn(async move {
                if let Err(e) = crate::snl::io::run(config).await {
                    eprintln!("Io error: {e}");
                }
            });
        }
        "flexCombinedMotion" => {
            let config = crate::snl::flex_combined_motion::FlexConfig::new(
                &require_macro(&macros, "P", program)?,
                &require_macro(&macros, "M", program)?,
                &macro_or(&macros, "CAP", ""),
                &require_macro(&macros, "FM", program)?,
                &require_macro(&macros, "CM", program)?,
            );
            bridge.spawn(async move {
                if let Err(e) = crate::snl::flex_combined_motion::run(config).await {
                    eprintln!("flexCombinedMotion error: {e}");
                }
            });
        }
        _ => {
            return Err(format!(
                "unknown program '{program}'. Available: kohzuCtl, kohzuCtl_soft, hrCtl, ml_monoCtl, orient, filterDrive, pf4, Io, flexCombinedMotion"
            ));
        }
    }

    println!("seq {program} started with macros: {macro_str}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_macros() {
        let m = parse_macros("P=mini:,M_THETA=dcm:theta, M_Y = dcm:y");
        assert_eq!(m.get("P").unwrap(), "mini:");
        assert_eq!(m.get("M_THETA").unwrap(), "dcm:theta");
        assert_eq!(m.get("M_Y").unwrap(), "dcm:y");
    }

    #[test]
    fn test_parse_macros_empty() {
        let m = parse_macros("");
        assert!(m.is_empty());
    }

    #[test]
    fn test_unknown_program() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let bridge = {
            let _guard = rt.enter();
            epics_base_rs::runtime::task::BlockingBridge::capture()
        };
        let db = PvDatabase::new();
        let result = seq_start("nonexistent", "P=x:", &bridge, &db);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown program"));
    }

    /// The lattice records kohzuCtl reads on its way to `ready`, as
    /// `kohzuSeq.db` declares them, plus the d-spacing record it writes. The
    /// motors are left out on purpose: a missing PV reads as 0 and swallows a
    /// put, which is enough to reach the d-spacing write without a motor
    /// record type. `Bragg2dSpacingAO` starts at -1 so "never written" is
    /// told apart from the 0 an ungated start computes from H=K=L=0.
    const KOHZU_LATTICE_DB: &str = r#"
record(ao, "T:BraggHAO") { field(PINI, "YES") field(DOL, "1") }
record(ao, "T:BraggKAO") { field(PINI, "YES") field(DOL, "1") }
record(ao, "T:BraggLAO") { field(PINI, "YES") field(DOL, "1") }
record(ao, "T:BraggAAO") { field(PINI, "YES") field(DOL, "5.43102") }
record(ao, "T:Bragg2dSpacingAO") { field(PINI, "YES") field(DOL, "1") field(VAL, "-1") }
"#;

    fn two_d(db: &PvDatabase) -> f64 {
        db.get_pv("T:Bragg2dSpacingAO").unwrap().to_f64().unwrap()
    }

    /// Load the fixture the way `st.cmd`'s `dbLoadRecords` does — into a
    /// database still in its load phase, so every record's `init_record`
    /// (where a constant `DOL` seeds `VAL`) is owed to `iocInit` and has NOT
    /// run when `seqStart` follows.
    async fn load_lattice(db: &PvDatabase) {
        use epics_base_rs::server::db_loader;
        db.begin_load().unwrap();
        for mut def in db_loader::parse_db(KOHZU_LATTICE_DB, &HashMap::new()).unwrap() {
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

    /// `seqStart` is issued from `st.cmd`, before `iocInit`. The program must
    /// not read the lattice until `iocInit`'s PINI pass has run — before it
    /// the Miller indices are the unprocessed 0, and a d-spacing computed from
    /// them is then overwritten by the pass, which is the `two_d=0.0000` boot
    /// the mini-beamline IOC showed. Si(111), a = 5.43102 Å: 2d = 2a/√3.
    #[epics_base_rs::epics_test]
    async fn kohzu_ctl_reads_the_lattice_only_after_pini() {
        let db = PvDatabase::new();
        load_lattice(&db).await;
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
