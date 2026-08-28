//! Observations, the diff between them, and the verdict that follows.
//!
//! # The four outcomes, and why ERROR is one of them
//!
//! Every case lands in exactly one bucket, and the buckets must add up to the
//! number of cases run. There is no fifth, silent bucket:
//!
//! - **AGREED** — both sides produced a reading and the readings match.
//! - **EXPECTED DEVIATION** — they differ, and the difference is justified by a
//!   NOT-REPRODUCED entry (the port deliberately
//!   refuses to reproduce a C bug). Not a failure.
//! - **DEFECT** — they differ and nothing justifies it.
//! - **ERROR** — a reading could not be obtained at all (IOC would not boot, PV
//!   never connected, tool timed out). **This is never scored as agreement.**
//!   An unmeasured case is the exact failure mode that produced 21 false-clean
//!   verdicts in the audit loop; the harness treats "I could not look" and "I
//!   looked and it was fine" as categorically different answers.

use crate::catool::{CaInfo, MonitorTrace, PutOutcome, ToolError};

/// Everything one side reported about one field. Each part is independently
/// optional, because one probe failing must not throw away the others.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct Observation {
    /// `cainfo` — native DBF type, element count, access rights.
    pub info: Option<CaInfo>,
    /// `caget` default form: enum/menu fields come back as their **string**.
    pub value_string: Option<String>,
    /// `caget -n`: enum/menu fields come back as their **ordinal**.
    pub value_numeric: Option<String>,
    /// Alarm status/severity of the owning record after the operation.
    pub stat: Option<String>,
    pub sevr: Option<String>,
    /// The result of the put this case drove, if it drove one.
    pub put: Option<PutOutcome>,
    /// The monitor event stream this case drove, if it drove one.
    pub monitor: Option<MonitorTrace>,
    /// Anything that prevented a reading. Non-empty => the case is an ERROR.
    pub errors: Vec<ToolError>,
}

impl Observation {
    /// Did this side produce a usable reading at all?
    pub fn is_readable(&self) -> bool {
        self.errors.is_empty()
    }
}

/// The named observable surfaces a case can differ on. These strings are what
/// the allowlist's `surface = [...]` matches against, so they are part of the
/// data contract and must stay stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    NativeType,
    ElementCount,
    AccessRights,
    ValueString,
    ValueNumeric,
    Stat,
    Sevr,
    PutAccepted,
    PutError,
    MonitorEvents,
    MonitorCount,
}

impl Surface {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NativeType => "native_type",
            Self::ElementCount => "element_count",
            Self::AccessRights => "access_rights",
            Self::ValueString => "value_string",
            Self::ValueNumeric => "value_numeric",
            Self::Stat => "stat",
            Self::Sevr => "sevr",
            Self::PutAccepted => "put_accepted",
            Self::PutError => "put_error",
            Self::MonitorEvents => "monitor_events",
            Self::MonitorCount => "monitor_count",
        }
    }

    /// Every CA surface, so a classifier over the three surface vocabularies
    /// cannot silently omit one. The PVA lanes already keep the same list
    /// (`PvaSurface::ALL`, `MonSurface::ALL`).
    pub const ALL: [Surface; 11] = [
        Self::NativeType,
        Self::ElementCount,
        Self::AccessRights,
        Self::ValueString,
        Self::ValueNumeric,
        Self::Stat,
        Self::Sevr,
        Self::PutAccepted,
        Self::PutError,
        Self::MonitorEvents,
        Self::MonitorCount,
    ];
}

/// One concrete disagreement: what surface, what C said, what the port said.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Difference {
    pub surface: Surface,
    pub c: String,
    pub rust: String,
}

/// The outcome of comparing one case: what was actually looked at, and what
/// disagreed.
///
/// `compared` is not bookkeeping — it is the difference between "this deviation
/// stopped happening" and "this run could not have seen it". A `--phase read`
/// run never drives a put, so a `put_accepted` allowlist row cannot fire; without
/// `compared`, the harness reports that row as a stale finding, which is a
/// fabricated one. Staleness is only meaningful over a surface that was observed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Comparison {
    /// Every surface on which BOTH sides produced a reading, whether or not they
    /// agreed.
    pub compared: Vec<Surface>,
    pub differences: Vec<Difference>,
}

/// Compare the two sides on every surface for which **both** produced a
/// reading.
///
/// A surface where one side is `None` is not a difference — it is an absence,
/// and absences are handled by the ERROR path. Scoring "C said 5, port said
/// nothing" as a value difference would be a fabricated finding.
pub fn compare(c: &Observation, rust: &Observation) -> Comparison {
    let mut out = Comparison::default();

    let mut push = |surface: Surface, a: String, b: String| {
        out.compared.push(surface);
        if a != b {
            out.differences.push(Difference {
                surface,
                c: a,
                rust: b,
            });
        }
    };

    if let (Some(ci), Some(ri)) = (&c.info, &rust.info) {
        push(
            Surface::NativeType,
            ci.native_type.clone(),
            ri.native_type.clone(),
        );
        push(
            Surface::ElementCount,
            ci.element_count.to_string(),
            ri.element_count.to_string(),
        );
        push(Surface::AccessRights, ci.access.clone(), ri.access.clone());
    }
    if let (Some(a), Some(b)) = (&c.value_string, &rust.value_string) {
        push(Surface::ValueString, a.clone(), b.clone());
    }
    if let (Some(a), Some(b)) = (&c.value_numeric, &rust.value_numeric) {
        push(Surface::ValueNumeric, a.clone(), b.clone());
    }
    if let (Some(a), Some(b)) = (&c.stat, &rust.stat) {
        push(Surface::Stat, a.clone(), b.clone());
    }
    if let (Some(a), Some(b)) = (&c.sevr, &rust.sevr) {
        push(Surface::Sevr, a.clone(), b.clone());
    }
    if let (Some(a), Some(b)) = (&c.put, &rust.put) {
        push(
            Surface::PutAccepted,
            a.accepted.to_string(),
            b.accepted.to_string(),
        );
        // Only compare the *reason* when both refused. If one accepted and the
        // other refused, PutAccepted already says so and a second finding on
        // the error text would be double-counting the same divergence.
        if !a.accepted && !b.accepted {
            push(
                Surface::PutError,
                a.error.clone().unwrap_or_default(),
                b.error.clone().unwrap_or_default(),
            );
        }
    }
    if let (Some(a), Some(b)) = (&c.monitor, &rust.monitor) {
        // Count and sequence are reported separately: "posted the right values
        // but too many times" and "posted the wrong values" are different port
        // defects, and collapsing them loses the distinction.
        push(
            Surface::MonitorCount,
            a.events.len().to_string(),
            b.events.len().to_string(),
        );
        let seq = |t: &MonitorTrace| {
            t.events
                .iter()
                .map(|e| match &e.alarm {
                    Some(al) => format!("{}[{}]", e.value, al),
                    None => e.value.clone(),
                })
                .collect::<Vec<_>>()
                .join(" -> ")
        };
        push(Surface::MonitorEvents, seq(a), seq(b));
    }

    out
}

/// What the harness concluded about one case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Agreed,
    ExpectedDeviation,
    Defect,
    /// Both servers agreed on every surface AND neither reported the put
    /// complete. Its own bucket rather than [`Self::Agreed`], because "they
    /// behaved alike" and "the operation finished" are two different facts and
    /// a run that folded them together would report a completion it never saw.
    ///
    /// Not [`Self::Errored`]: the crate already rules that a refusal issued by
    /// the server is a reading and not a measurement failure
    /// (`tests/oracle.rs:250-258`), and a completion that never comes from
    /// EITHER side is the same kind of reading — the two IOCs did the same
    /// observable thing. A one-sided no-completion is the opposite: it is the
    /// difference this harness exists to find, so it stays `Errored`.
    NeitherCompleted,
    Errored,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catool::MonitorEvent;

    fn info(t: &str, n: u64, acc: &str) -> CaInfo {
        CaInfo {
            native_type: t.into(),
            element_count: n,
            access: acc.into(),
        }
    }

    #[test]
    fn identical_observations_produce_no_differences() {
        let o = Observation {
            info: Some(info("DBF_DOUBLE", 1, "read, write")),
            value_string: Some("1.5".into()),
            ..Default::default()
        };
        assert!(compare(&o, &o).differences.is_empty());
    }

    #[test]
    fn native_type_and_element_count_and_access_each_diff_separately() {
        let c = Observation {
            info: Some(info("DBF_ULONG", 1, "read, write")),
            ..Default::default()
        };
        let r = Observation {
            info: Some(info("DBF_LONG", 2, "read")),
            ..Default::default()
        };
        let d = compare(&c, &r).differences;
        let surfaces: Vec<_> = d.iter().map(|x| x.surface).collect();
        assert!(surfaces.contains(&Surface::NativeType));
        assert!(surfaces.contains(&Surface::ElementCount));
        assert!(surfaces.contains(&Surface::AccessRights));
        assert_eq!(d.len(), 3);
    }

    /// An enum agreeing on the ordinal but not the string is observably wrong,
    /// and must surface as a value_string difference.
    #[test]
    fn enum_string_mismatch_is_caught_even_when_the_ordinal_agrees() {
        let c = Observation {
            value_string: Some("On".into()),
            value_numeric: Some("1".into()),
            ..Default::default()
        };
        let r = Observation {
            value_string: Some("HIGH".into()),
            value_numeric: Some("1".into()),
            ..Default::default()
        };
        let d = compare(&c, &r).differences;
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].surface, Surface::ValueString);
    }

    /// A missing reading on one side is an ABSENCE, not a difference. Scoring
    /// it as a value diff would fabricate a finding; it must go to ERROR.
    #[test]
    fn a_missing_reading_is_not_reported_as_a_difference() {
        let c = Observation {
            value_string: Some("1.5".into()),
            ..Default::default()
        };
        let r = Observation {
            value_string: None,
            errors: vec![ToolError {
                side: crate::ioc::Side::Rust,
                tool: "caget".into(),
                message: "timed out".into(),
            }],
            ..Default::default()
        };
        assert!(
            compare(&c, &r).differences.is_empty(),
            "absence is not a difference"
        );
        assert!(!r.is_readable(), "...it is an ERROR");
    }

    #[test]
    fn put_rejection_reason_is_only_compared_when_both_sides_refused() {
        // One accepted, one refused: exactly one finding (PutAccepted).
        let c = Observation {
            put: Some(PutOutcome {
                accepted: false,
                error: Some("Invalid value".into()),
            }),
            ..Default::default()
        };
        let r = Observation {
            put: Some(PutOutcome {
                accepted: true,
                error: None,
            }),
            ..Default::default()
        };
        let d = compare(&c, &r).differences;
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].surface, Surface::PutAccepted);
    }

    #[test]
    fn both_refusing_for_different_reasons_is_a_difference() {
        let mk = |e: &str| Observation {
            put: Some(PutOutcome {
                accepted: false,
                error: Some(e.into()),
            }),
            ..Default::default()
        };
        let d = compare(&mk("Invalid value"), &mk("Write access denied")).differences;
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].surface, Surface::PutError);
    }

    fn trace(vals: &[&str]) -> MonitorTrace {
        MonitorTrace {
            initial: vec![],
            events: vals
                .iter()
                .map(|v| MonitorEvent {
                    pv: "T:AI".into(),
                    value: (*v).into(),
                    alarm: None,
                })
                .collect(),
        }
    }

    /// The monitor surface is where a lot of this port's divergence lives: a
    /// port that coalesces two updates into one, or posts an extra one, must be
    /// caught even though the FINAL value agrees.
    #[test]
    fn monitor_count_difference_is_caught_even_when_the_final_value_agrees() {
        let c = trace(&["1", "2", "3"]);
        let r = trace(&["1", "3"]); // coalesced the middle update away
        let d = compare(
            &Observation {
                monitor: Some(c),
                ..Default::default()
            },
            &Observation {
                monitor: Some(r),
                ..Default::default()
            },
        );
        let d = d.differences;
        let surfaces: Vec<_> = d.iter().map(|x| x.surface).collect();
        assert!(surfaces.contains(&Surface::MonitorCount));
        assert!(surfaces.contains(&Surface::MonitorEvents));
    }

    #[test]
    fn monitor_order_matters_not_just_the_set_of_values() {
        let d = compare(
            &Observation {
                monitor: Some(trace(&["1", "2"])),
                ..Default::default()
            },
            &Observation {
                monitor: Some(trace(&["2", "1"])),
                ..Default::default()
            },
        );
        let d = d.differences;
        assert!(d.iter().any(|x| x.surface == Surface::MonitorEvents));
        assert!(
            !d.iter().any(|x| x.surface == Surface::MonitorCount),
            "same count, different order"
        );
    }
}
