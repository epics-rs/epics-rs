pub mod ad_driver;
pub mod ndarray_driver;

// Re-export for backward compatibility
pub use ad_driver::ADDriverParams as ADBaseParams;

use asyn_rs::param::ParamType;

/// Definition of a single areaDetector parameter.
pub struct ParamDef {
    pub name: &'static str,
    pub param_type: ParamType,
    pub description: &'static str,
}
