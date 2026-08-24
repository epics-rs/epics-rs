//! NormativeTypes — wire-compatible builders for the standard PVA structure
//! IDs used across EPICS:
//!
//! - `epics:nt/NTScalar:1.0` / `epics:nt/NTScalarArray:1.0` ([`scalar`])
//! - `epics:nt/NTEnum:1.0` ([`enum_t`])
//! - `epics:nt/NTTable:1.0` ([`table`])
//! - `epics:nt/NTURI:1.0` ([`uri`]) — RPC argument passing
//! - `epics:nt/NTAttribute:1.0` ([`attribute`]) — standalone (NTNDArray embeds its own)
//! - `epics:nt/NTNDArray:1.0` ([`nd_array`]) — areaDetector image
//!
//! Each module produces both the `FieldDesc` introspection and a `PvField`
//! value for use with the native client/server. Mirrors pvxs nt.cpp.

pub mod attribute;
pub mod enum_t;
pub mod meta;
pub mod nd_array;
pub mod qsrv_marks;
pub mod scalar;
pub mod table;
pub mod typed;
pub mod uri;

pub use attribute::NTAttribute;
pub use enum_t::NTEnum;
pub use qsrv_marks::{FORM_CHOICES, event_leaves, property_leaves};
pub use scalar::NTScalar;
pub use table::NTTable;
pub use typed::{Alarm, TimeStamp, TypedNT, TypedNTError};
pub use uri::NTURI;

/// Proc-macro re-exports. Kept under a sub-namespace so the
/// `#[derive(NTScalar)]` macro doesn't collide with the runtime
/// [`scalar::NTScalar`] builder type at `epics_pva_rs::nt::NTScalar`.
///
/// ```ignore
/// use epics_pva_rs::nt::derive::NTScalar;
///
/// #[derive(NTScalar)]
/// struct MotorPos {
///     value: f64,
///     #[nt(meta)] alarm: epics_pva_rs::nt::Alarm,
///     #[nt(meta)] timestamp: epics_pva_rs::nt::TimeStamp,
/// }
/// ```
pub mod derive {
    pub use epics_macros_rs::{NTScalar, NTTable};
}
