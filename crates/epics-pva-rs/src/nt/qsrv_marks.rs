//! Which NT leaves a QSRV read ASSIGNS — the marking half of the NT contract.
//!
//! [`scalar::NTScalar`](super::scalar::NTScalar) owns the *shape* a QSRV
//! channel declares (pvxs `src/nt.cpp`). This module owns the other half: of
//! that declared shape, which leaves a read actually assigns, and therefore
//! which ones reach a Delta-format client at all.
//!
//! The two are deliberately not the same set. pvxs reads into a
//! `cloneEmpty()`, so only assigned leaves carry `valid` and only those are
//! serialized by `to_wire_valid` (`serverget.cpp:104`). A declared-but-
//! unassigned leaf — `control.minStep`, `valueAlarm.active`, the four
//! `valueAlarm.*Severity`, `valueAlarm.hysteresis` — is invisible on the wire
//! even though introspection advertises it.
//!
//! Marking a fabricated default is not a cosmetic difference: the mark tells
//! the client the value is authoritative. An `mbbo` whose rset supplies no
//! `get_graphic_double` must read as "no display limits provided", not as
//! limits of zero.

use epics_base_rs::server::snapshot::PropertySupport;

/// `display.form.choices` — the fixed seven-entry menu pvxs publishes for
/// every numeric NTScalar / NTScalarArray (the `Q:form` info-tag menu).
///
/// Mirrors pvxs `ioc/iocsource.cpp:43-51` (`IOCSource::initialize`).
pub const FORM_CHOICES: [&str; 7] = [
    "Default",
    "String",
    "Binary",
    "Decimal",
    "Hex",
    "Exponential",
    "Engineering",
];

/// The leaves pvxs `IOCSource::getProperties` assigns for a record whose
/// rset supplies `props` (`ioc/iocsource.cpp:252-310`).
///
/// `getProperties` asks `dbChannelGet` for all six DBR property groups, and
/// `dbGet` **clears** the option bit of every group the record type left NULL
/// (`dbAccess.c:336-430`). Each NT leaf is then assigned only under its
/// surviving bit, so [`PropertySupport`] — already narrowed to the addressed
/// field by the DB layer — is the whole input to this decision.
///
/// A returned path that the concrete descriptor does not have (e.g.
/// `display.precision` on a string NT, `value.choices` on a plain scalar)
/// simply matches no leaf downstream — the same no-op as `getProperties`'s
/// own `if(auto x = node["…"])` guard.
///
/// `display.form.index` / `.choices` are NOT here: they come from
/// `IOCSource::initialize`, not `getProperties`, and so are assigned on a
/// read but never by a property event. See [`FORM_CHOICES`] and the callers'
/// read-leaf assembly.
pub fn property_leaves(props: PropertySupport) -> Vec<&'static str> {
    let mut leaves = Vec::new();
    if props.units {
        leaves.push("display.units");
    }
    if props.enum_strs {
        leaves.push("value.choices");
    }
    if props.graphic_double {
        leaves.push("display.limitLow");
        leaves.push("display.limitHigh");
        // pvxs nests the precision assignment INSIDE the `DBR_GR_DOUBLE`
        // branch (`iocsource.cpp:288-291`), so a record type that supplies
        // precision but no graphic limits assigns neither.
        if props.precision {
            leaves.push("display.precision");
        }
    }
    if props.control_double {
        leaves.push("control.limitLow");
        leaves.push("control.limitHigh");
    }
    if props.alarm_double {
        leaves.push("valueAlarm.lowAlarmLimit");
        leaves.push("valueAlarm.lowWarningLimit");
        leaves.push("valueAlarm.highWarningLimit");
        leaves.push("valueAlarm.highAlarmLimit");
    }
    // Unconditional — pvxs assigns DESC with no option gate at all
    // ("cheating at the moment. DESC is not marked DBE_PROPERTY",
    // `iocsource.cpp:307-310`).
    leaves.push("display.description");
    leaves
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The seven leaves `getProperties` never assigns, for ANY record type.
    /// These are what a Delta client must not see on a QSRV GET; the port
    /// declares them all, so nothing but this list keeps them off the wire.
    #[test]
    fn never_assigned_leaves_are_absent_for_every_property_mask() {
        let never = [
            "control.minStep",
            "valueAlarm.active",
            "valueAlarm.lowAlarmSeverity",
            "valueAlarm.lowWarningSeverity",
            "valueAlarm.highWarningSeverity",
            "valueAlarm.highAlarmSeverity",
            "valueAlarm.hysteresis",
        ];
        // Every one of the 64 possible rset support masks.
        for bits in 0u8..64 {
            let props = PropertySupport {
                units: bits & 1 != 0,
                precision: bits & 2 != 0,
                graphic_double: bits & 4 != 0,
                control_double: bits & 8 != 0,
                alarm_double: bits & 16 != 0,
                enum_strs: bits & 32 != 0,
            };
            let leaves = property_leaves(props);
            for n in never {
                assert!(
                    !leaves.contains(&n),
                    "getProperties never assigns {n}, but mask {bits:06b} produced it"
                );
            }
        }
    }

    /// `ai`/`ao`/`calc`: every rset slot, `DBF_DOUBLE` VAL. Pinned against the
    /// measured pvxs `softIocPVX` delta for `record(ai,…){}`.
    #[test]
    fn full_numeric_record_matches_the_measured_pvxs_delta() {
        assert_eq!(
            property_leaves(PropertySupport::NUMERIC),
            vec![
                "display.units",
                "display.limitLow",
                "display.limitHigh",
                "display.precision",
                "control.limitLow",
                "control.limitHigh",
                "valueAlarm.lowAlarmLimit",
                "valueAlarm.lowWarningLimit",
                "valueAlarm.highWarningLimit",
                "valueAlarm.highAlarmLimit",
                "display.description",
            ],
        );
    }

    /// `longin`/`longout`: the rset HAS `get_precision`, but `dbGet` clears
    /// `DBR_PRECISION` for a non-`DBF_FLOAT`/`DBF_DOUBLE` field, so the
    /// narrowed mask arrives with `precision: false` and no `display.precision`
    /// is assigned. Measured: pvxs emits no `display.precision` for a `longin`.
    #[test]
    fn integer_record_assigns_limits_but_no_precision() {
        let props = PropertySupport {
            precision: false,
            ..PropertySupport::NUMERIC
        };
        let leaves = property_leaves(props);
        assert!(leaves.contains(&"display.limitLow"));
        assert!(!leaves.contains(&"display.precision"));
    }

    /// `stringin`/`stringout`: no rset property slot at all. pvxs marks
    /// `display.description` and nothing else — notably NOT `display.units`,
    /// which the string NT declares.
    #[test]
    fn record_with_no_property_slots_assigns_only_description() {
        assert_eq!(
            property_leaves(PropertySupport::NONE),
            vec!["display.description"],
        );
    }

    /// `mbbo`: `DBF_ENUM` VAL retyped `USHORT` by `cvt_dbaddr`, and the mbbo
    /// rset has none of the `get_*_double` / `get_units` slots. pvxs assigns
    /// only `display.description` (plus the form menu, which is
    /// `initialize`'s, not `getProperties`').
    #[test]
    fn mbbo_style_record_assigns_only_description() {
        let props = PropertySupport {
            enum_strs: false,
            ..PropertySupport::NONE
        };
        assert_eq!(property_leaves(props), vec!["display.description"]);
    }

    /// A `graphic_double`-less record must not assign precision even when the
    /// rset supplies it: pvxs nests the precision assignment inside the
    /// `DBR_GR_DOUBLE` branch.
    #[test]
    fn precision_without_graphic_limits_assigns_neither() {
        let props = PropertySupport {
            precision: true,
            graphic_double: false,
            ..PropertySupport::NONE
        };
        let leaves = property_leaves(props);
        assert!(!leaves.contains(&"display.precision"));
        assert!(!leaves.contains(&"display.limitLow"));
    }
}
