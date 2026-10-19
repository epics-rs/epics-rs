//! Kohzu soft-motor monochromator control -- native Rust port of `kohzuCtl_soft.st`.
//!
//! kohzuCtl.st under `{P}{MONO}` names, run by [`kohzu_ctl`]'s state
//! machine, allowing multiple monochromator instances on one IOC.
//!
//! PV naming: `{P}{MONO}E`, `{P}{MONO}Lambda`, `{P}{MONO}Theta`, etc.
//! Motor PVs still use `{P}{M_THETA}`, `{P}{M_Y}`, `{P}{M_Z}`.

use epics_base_rs::server::database::PvDatabase;

use crate::snl::kohzu_ctl::{self, Geometry, Program};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// PV name configuration for the soft-motor Kohzu variant.
pub struct KohzuSoftConfig {
    /// IOC prefix, e.g. "xxx:".
    pub prefix: String,
    /// Mono prefix, e.g. "Kohzu1:".
    pub mono: String,
    /// Motor record names.
    pub m_theta: String,
    pub m_y: String,
    pub m_z: String,
    /// Geometry type.
    pub geom: Geometry,
}

impl KohzuSoftConfig {
    pub fn new(prefix: &str, mono: &str, m_theta: &str, m_y: &str, m_z: &str, geom: i32) -> Self {
        Self {
            prefix: prefix.to_string(),
            mono: mono.to_string(),
            m_theta: m_theta.to_string(),
            m_y: m_y.to_string(),
            m_z: m_z.to_string(),
            geom: Geometry::from_i32(geom),
        }
    }

    /// Build a mono-prefixed PV name: {P}{MONO}suffix
    fn mono_pv(&self, suffix: &str) -> String {
        format!("{}{}{}", self.prefix, self.mono, suffix)
    }

    /// Build a motor PV name: {P}{motor}field
    #[cfg(test)]
    fn motor_pv(&self, motor: &str, field: &str) -> String {
        format!("{}{}{}", self.prefix, motor, field)
    }

    fn program(self) -> Program {
        Program {
            name: "kohzuCtl_soft",
            soft: true,
            p: self.mono_pv(""),
            prefix: self.prefix,
            motors: [self.m_theta, self.m_y, self.m_z],
            geom: self.geom,
        }
    }
}

// ---------------------------------------------------------------------------
// Async runner
// ---------------------------------------------------------------------------

/// Run the kohzuCtl_soft, updatePsuedo_soft and updateSet state sets.
pub async fn run(
    config: KohzuSoftConfig,
    db: PvDatabase,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    kohzu_ctl::run_program(config.program(), db).await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests -- physics is tested in kohzu_ctl; here we verify config building.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_pv_names() {
        let cfg = KohzuSoftConfig::new("xxx:", "Kohzu1:", "m9", "m10", "m11", 1);
        assert_eq!(cfg.mono_pv("E"), "xxx:Kohzu1:E");
        assert_eq!(cfg.motor_pv("m9", ".RBV"), "xxx:m9.RBV");
        assert_eq!(cfg.mono_pv("Lambda"), "xxx:Kohzu1:Lambda");
    }

    #[test]
    fn test_geometry_from_i32() {
        assert_eq!(Geometry::from_i32(1), Geometry::Standard);
        assert_eq!(Geometry::from_i32(2), Geometry::Alternate);
        assert_eq!(Geometry::from_i32(99), Geometry::Standard);
    }
}
