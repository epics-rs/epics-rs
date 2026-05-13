//! Parameter limits query interface (asyn Issue #218 equivalent).
//!
//! Records that want to display or enforce parameter min/max bounds
//! query the driver via this trait. Returns `None` for parameters
//! without defined limits — matches C asyn's `getBounds = NULL`
//! sentinel for "no limits exposed".
//!
//! Two flavours: integer (bounded by i64 range, suitable for
//! `asynInt32` / `asynInt64`) and floating (bounded by f64,
//! suitable for `asynFloat64`). String / octet parameters have no
//! natural ordering, so they get no limits trait.
//!
//! Drivers expose typical metadata at `init_record` so records
//! (e.g. `ai`/`ao`'s DRVL/DRVH) can populate at startup. Runtime
//! re-query is allowed but uncommon — most drivers compute the
//! limits once at port init and cache.

use crate::error::AsynResult;
use crate::user::AsynUser;

/// Bounds for an integer parameter. Inclusive on both ends.
/// `None` for either field means that side is unbounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IntLimits {
    pub low: Option<i64>,
    pub high: Option<i64>,
}

impl IntLimits {
    pub fn new(low: Option<i64>, high: Option<i64>) -> Self {
        Self { low, high }
    }

    /// Bounded on both ends: `[low, high]`.
    pub fn range(low: i64, high: i64) -> Self {
        Self {
            low: Some(low),
            high: Some(high),
        }
    }

    /// `true` when at least one end is set.
    pub fn is_constrained(&self) -> bool {
        self.low.is_some() || self.high.is_some()
    }

    /// `true` when `value` falls inside both ends (inclusive).
    /// Unbounded sides always pass.
    pub fn contains(&self, value: i64) -> bool {
        self.low.map(|l| value >= l).unwrap_or(true)
            && self.high.map(|h| value <= h).unwrap_or(true)
    }
}

/// Bounds for a floating parameter.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FloatLimits {
    pub low: Option<f64>,
    pub high: Option<f64>,
}

impl FloatLimits {
    pub fn new(low: Option<f64>, high: Option<f64>) -> Self {
        Self { low, high }
    }

    pub fn range(low: f64, high: f64) -> Self {
        Self {
            low: Some(low),
            high: Some(high),
        }
    }

    pub fn is_constrained(&self) -> bool {
        self.low.is_some() || self.high.is_some()
    }

    pub fn contains(&self, value: f64) -> bool {
        self.low.map(|l| value >= l).unwrap_or(true)
            && self.high.map(|h| value <= h).unwrap_or(true)
    }
}

/// Driver-side query for parameter min/max bounds.
///
/// `read_*_limits` returns the limits for the parameter named by
/// `user.reason`. Drivers MUST return `Ok(IntLimits::default())` (or
/// `Ok(FloatLimits::default())`) for parameters without bounds —
/// reserve `Err` for genuine driver/protocol failures.
///
/// Records use the returned limits to populate DRVL/DRVH (drive
/// limits) on `ai`/`ao`/`longin`/`longout` at init time, and to
/// reject out-of-range writes (depending on `LINR`/`OOPT` config).
pub trait AsynLimits: Send + Sync {
    fn read_int_limits(&self, _user: &AsynUser) -> AsynResult<IntLimits> {
        Ok(IntLimits::default())
    }

    fn read_float_limits(&self, _user: &AsynUser) -> AsynResult<FloatLimits> {
        Ok(FloatLimits::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_limits_contains_inclusive() {
        let l = IntLimits::range(-10, 10);
        assert!(l.contains(-10));
        assert!(l.contains(0));
        assert!(l.contains(10));
        assert!(!l.contains(-11));
        assert!(!l.contains(11));
    }

    #[test]
    fn int_limits_unbounded_high_pass() {
        let l = IntLimits::new(Some(0), None);
        assert!(l.contains(0));
        assert!(l.contains(i64::MAX));
        assert!(!l.contains(-1));
    }

    #[test]
    fn float_limits_contains() {
        let l = FloatLimits::range(-1.0, 1.0);
        assert!(l.contains(-1.0));
        assert!(l.contains(0.5));
        assert!(l.contains(1.0));
        assert!(!l.contains(-1.0001));
        assert!(!l.contains(1.0001));
    }

    #[test]
    fn default_is_unconstrained() {
        let l = IntLimits::default();
        assert!(!l.is_constrained());
        assert!(l.contains(i64::MIN));
        assert!(l.contains(i64::MAX));

        let f = FloatLimits::default();
        assert!(!f.is_constrained());
        assert!(f.contains(f64::NEG_INFINITY));
        assert!(f.contains(f64::INFINITY));
    }

    /// Default trait impl returns unconstrained limits.
    #[test]
    fn default_trait_impl_returns_default() {
        struct Dummy;
        impl AsynLimits for Dummy {}
        let d = Dummy;
        let user = AsynUser::default();
        assert_eq!(
            d.read_int_limits(&user).unwrap(),
            IntLimits::default()
        );
        assert_eq!(
            d.read_float_limits(&user).unwrap(),
            FloatLimits::default()
        );
    }
}
