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

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;

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
/// Must be called with a tokio runtime handle (use `CommandContext::runtime_handle()`
/// from st.cmd startup commands, since st.cmd runs on a non-tokio thread).
pub fn seq_start(
    program: &str,
    macro_str: &str,
    handle: &tokio::runtime::Handle,
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
            let db = db.clone();
            handle.spawn(async move {
                if let Err(e) = crate::snl::kohzu_ctl::run(config, db).await {
                    eprintln!("kohzuCtl error: {e}");
                }
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
            let db = db.clone();
            handle.spawn(async move {
                if let Err(e) = crate::snl::kohzu_ctl_soft::run(config, db).await {
                    eprintln!("kohzuCtl_soft error: {e}");
                }
            });
        }
        "hrCtl" => {
            let config = crate::snl::hr_ctl::HrConfig::new(
                &require_macro(&macros, "P", program)?,
                &macro_or(&macros, "N", "1"),
                &require_macro(&macros, "M_PHI1", program)?,
                &require_macro(&macros, "M_PHI2", program)?,
            );
            let db = db.clone();
            handle.spawn(async move {
                if let Err(e) = crate::snl::hr_ctl::run(config, db).await {
                    eprintln!("hrCtl error: {e}");
                }
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
            let db = db.clone();
            handle.spawn(async move {
                if let Err(e) = crate::snl::ml_mono_ctl::run(config, db).await {
                    eprintln!("ml_monoCtl error: {e}");
                }
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
            let db = db.clone();
            handle.spawn(async move {
                if let Err(e) = crate::snl::orient::run(config, db).await {
                    eprintln!("orient error: {e}");
                }
            });
        }
        "filterDrive" => {
            let config = crate::snl::filter_drive::FilterDriveConfig::new(
                &require_macro(&macros, "P", program)?,
                &require_macro(&macros, "R", program)?,
                macro_or(&macros, "N", "8").parse::<usize>().unwrap_or(8),
            );
            let db = db.clone();
            handle.spawn(async move {
                if let Err(e) = crate::snl::filter_drive::run(config, db).await {
                    eprintln!("filterDrive error: {e}");
                }
            });
        }
        "pf4" => {
            let config = crate::snl::pf4::Pf4Config::new(
                &require_macro(&macros, "P", program)?,
                &macro_or(&macros, "H", ""),
                &require_macro(&macros, "B", program)?,
            );
            let db = db.clone();
            handle.spawn(async move {
                if let Err(e) = crate::snl::pf4::run(config, db).await {
                    eprintln!("pf4 error: {e}");
                }
            });
        }
        "Io" => {
            let config = crate::snl::io::IoConfig::new(
                &require_macro(&macros, "P", program)?,
                &macro_or(&macros, "MONO", ""),
                &macro_or(&macros, "VSC", ""),
            );
            handle.spawn(async move {
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
            handle.spawn(async move {
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
        let db = PvDatabase::new();
        let result = seq_start("nonexistent", "P=x:", rt.handle(), &db);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown program"));
    }
}
