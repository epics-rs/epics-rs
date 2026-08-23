//! The denominator: what "100% coverage" would actually mean.
//!
//! The audit loop's fatal flaw was that it had no denominator — nobody could
//! say what fraction of the surface had been swept, so a falling finding count
//! could not be told apart from "we looked less this time". This module fixes
//! that by deriving the surface mechanically from the expanded `.dbd`.
//!
//! # What is counted, and what is deliberately not
//!
//! The surface is `record types the port implements × their CA-observable
//! fields`. Two exclusions, both stated rather than hidden, because an inflated
//! denominator and a hidden exclusion are the same lie in opposite directions:
//!
//! - **`DBF_NOACCESS` fields are excluded.** They are raw C pointers in the
//!   record struct (`RPVT`, `DPVT`, `BPTR`, ...). No CA client can read or
//!   write them, so they are not observable behavior and cannot be diffed by
//!   any oracle. Counting them would silently inflate the denominator (they are
//!   594 of the declarations) and make coverage look worse than it is while
//!   measuring nothing.
//! - **Record types the port does not implement stay IN the denominator** and
//!   are named separately. Their fields are CA-observable — C serves every one
//!   of them — so the port not implementing the type is precisely why they went
//!   unmeasured, not a reason to stop counting them. Dropping them shrank the
//!   numerator and the denominator together, so a record type going dark left
//!   the coverage percent unchanged and read as the port getting better; that
//!   is the failure this module exists to make impossible.
//!
//! Which record types the port implements is **measured, not assumed** — see
//! [`probe_supported_record_types`]. The port's field tables are being
//! regenerated concurrently, so anything read out of its source would be stale
//! by the time it was used.

use std::collections::BTreeSet;

use crate::dbd::{Dbd, DbfType, FieldDef};

/// One addressable point of the observable surface: a field of a record type.
#[derive(Debug, Clone)]
pub struct FieldRef {
    pub record_type: String,
    pub field: FieldDef,
}

impl FieldRef {
    /// The CA channel name for this field on a given record instance.
    pub fn pv(&self, record: &str) -> String {
        format!("{record}.{}", self.field.name)
    }
}

/// The enumerated surface, plus the accounting needed to report coverage
/// honestly.
#[derive(Debug, Clone)]
pub struct Surface {
    /// Record types present in the `.dbd` **and** implemented by the port.
    pub covered_types: Vec<String>,
    /// Record types in the `.dbd` the port does not implement: a real coverage
    /// gap, named so it cannot be quietly dropped, and counted in the
    /// denominator so it cannot be quietly *shrunk* either.
    pub unimplemented_types: Vec<String>,
    /// Every CA-observable (record type, field) pair of **every** record type in
    /// the `.dbd`, implemented or not. This is the denominator.
    pub fields: Vec<FieldRef>,
    /// `DBF_NOACCESS` declarations excluded from the denominator, counted so
    /// the exclusion is auditable.
    pub excluded_noaccess: usize,
}

impl Surface {
    /// Build the surface from the spec, restricted to what the port implements.
    pub fn build(dbd: &Dbd, supported: &BTreeSet<String>) -> Self {
        let mut covered_types = Vec::new();
        let mut unimplemented_types = Vec::new();
        let mut fields = Vec::new();
        let mut excluded_noaccess = 0;

        for rt in &dbd.record_types {
            if supported.contains(&rt.name) {
                covered_types.push(rt.name.clone());
            } else {
                // Named, and its fields still counted below: an unimplemented
                // type is an unmeasured part of the surface, not a smaller
                // surface.
                unimplemented_types.push(rt.name.clone());
            }
            for f in &rt.fields {
                if !f.dbf.is_ca_observable() {
                    excluded_noaccess += 1;
                    continue;
                }
                fields.push(FieldRef {
                    record_type: rt.name.clone(),
                    field: f.clone(),
                });
            }
        }

        Self {
            covered_types,
            unimplemented_types,
            fields,
            excluded_noaccess,
        }
    }

    /// The denominator: CA-observable fields across every record type the
    /// `.dbd` declares, whether or not the port implements it.
    pub fn denominator(&self) -> usize {
        self.fields.len()
    }

    pub fn fields_of(&self, record_type: &str) -> impl Iterator<Item = &FieldRef> {
        self.fields
            .iter()
            .filter(move |f| f.record_type == record_type)
    }
}

/// Coverage of the enumerated surface, as counts that must add up.
///
/// Deliberately not a single number: a field that *errored* is not a field that
/// was measured, and collapsing the two is how a harness ends up claiming
/// coverage it does not have.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Coverage {
    /// Fields in the denominator.
    pub enumerated: usize,
    /// Fields for which a reading was obtained from **both** sides — the only
    /// fields that were actually diffed.
    pub measured: usize,
    /// Fields where at least one side failed to produce a reading. Counted as
    /// NOT covered.
    pub errored: usize,
}

impl Coverage {
    /// Percentage of the enumerated surface actually measured on both sides.
    pub fn percent(&self) -> f64 {
        if self.enumerated == 0 {
            return 0.0;
        }
        100.0 * self.measured as f64 / self.enumerated as f64
    }
}

/// Ask the **port**, at runtime, which record types it can actually load.
///
/// This is a measurement, not a reading of the port's source. It matters that
/// it is a measurement for two reasons: the port's field tables are being
/// regenerated concurrently (anything scraped from source would be stale), and
/// "the type appears in a source file" is not the same claim as "the db loader
/// accepts a record of this type and the server serves it".
///
/// A type that fails to load is reported as unimplemented rather than being
/// allowed to abort the run — but the *probe* failing to configure itself is
/// not that, so it is an error rather than an empty answer. An unconfigurable
/// probe would report every record type unimplemented, which now reads as a
/// wholly unmeasured surface rather than as a clean run.
///
/// The IOC is built through [`crate::register_port_ioc_devices`] and
/// [`crate::port_ioc_builder`] — the same pair `oracle-ioc` uses — so the
/// denominator is measured from the configuration actually under test. A bare
/// `IocBuilder` answers for a *different* IOC: `asyn` resolves to
/// epics-base-rs's CNCT-only stub record instead of asyn-rs's `AsynRecord` on
/// `ORACLEASYN`, so the probe would be vouching for a record type the measured
/// IOC never serves — in either direction, and silently.
pub async fn probe_supported_record_types(dbd: &Dbd) -> Result<BTreeSet<String>, String> {
    use std::collections::HashMap;

    let _devices = crate::register_port_ioc_devices()?;
    let mut supported = BTreeSet::new();
    let macros: HashMap<String, String> = HashMap::new();

    for rt in &dbd.record_types {
        let db = crate::record_stmt(&rt.name, "ORACLE:PROBE");
        // Parsing is not enough: `db_string` only reads the grammar, while
        // `build()` is what instantiates the record type. A type can parse and
        // still have no implementation behind it, so require the full build.
        let ok = match crate::port_ioc_builder().db_string(&db, &macros) {
            Ok(b) => b.build().await.is_ok(),
            Err(_) => false,
        };
        if ok {
            supported.insert(rt.name.clone());
        }
    }
    Ok(supported)
}

/// Is this field worth attempting a client write against?
///
/// Everything a CA client can reach is a candidate. Two classes are kept that
/// a naive harness would drop, and dropping either would blind the oracle to a
/// defect family it is specifically meant to catch:
///
/// - **`special(SPC_NOMOD)` fields are kept.** Their entire observable contract
///   is that the put is *refused*. Checking that both sides refuse it — and
///   refuse it with the same error — is one of the more valuable things here.
/// - **Link fields are kept, but only ever written a *constant*.** The reason to
///   be careful with links is that writing a PV reference rewires the record
///   graph, so the next case would be measuring a different database than it
///   thinks it is. Writing a bare constant (`0`) sets the link to a literal and
///   creates no edge to another record, so it is safe *and* it is the only way
///   to reach the put-rejection path on a link. That path is not hypothetical:
///   CBUG-F6 is exactly it (C's `calc` declares `special(SPC_MOD)` on
///   `INPM`..`INPU` and its `special()` then rejects the put, making nine
///   documented fields unwritable over CA). A harness that skipped link puts
///   would silently fail to observe it.
///
/// Only `DBF_NOACCESS` is excluded, because no client can reach it at all.
pub fn is_put_candidate(f: &FieldDef) -> bool {
    f.dbf.is_ca_observable()
}

/// Fields whose type is a plain scalar number — the ones with meaningful
/// numeric boundaries.
pub fn is_numeric(t: DbfType) -> bool {
    matches!(
        t,
        DbfType::Char
            | DbfType::UChar
            | DbfType::Short
            | DbfType::UShort
            | DbfType::Long
            | DbfType::ULong
            | DbfType::Int64
            | DbfType::UInt64
            | DbfType::Float
            | DbfType::Double
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
recordtype(ai) {
    field(VAL, DBF_DOUBLE) { pp(TRUE) }
    field(NAME, DBF_STRING) { size(61) special(SPC_NOMOD) }
    field(INP, DBF_INLINK) { prompt("Input") }
    field(RPVT, DBF_NOACCESS) { extra("void *rpvt") }
}
recordtype(aai) {
    field(VAL, DBF_NOACCESS) { extra("void *val") }
    field(NELM, DBF_ULONG) { initial("1") }
}
"#;

    fn surface_with(supported: &[&str]) -> Surface {
        let dbd = Dbd::parse(SAMPLE).unwrap();
        let set: BTreeSet<String> = supported.iter().map(|s| s.to_string()).collect();
        Surface::build(&dbd, &set)
    }

    #[test]
    fn denominator_excludes_noaccess_fields() {
        let s = surface_with(&["ai", "aai"]);
        // ai declares 4 fields (RPVT is DBF_NOACCESS) and aai declares 2 (VAL
        // is DBF_NOACCESS), so 4 CA-observable fields and 2 exclusions.
        assert_eq!(s.denominator(), 4);
        assert_eq!(s.excluded_noaccess, 2);
        assert!(s.fields.iter().all(|f| f.field.name != "RPVT"));
    }

    /// An unimplemented record type is an unmeasured part of the surface, not a
    /// smaller surface. If its fields left the denominator with it, a type that
    /// stopped booting would shrink numerator and denominator together and the
    /// coverage percent would not move.
    #[test]
    fn an_unimplemented_type_keeps_its_fields_in_the_denominator() {
        let all = surface_with(&["ai", "aai"]);
        let without_aai = surface_with(&["ai"]);
        assert_eq!(without_aai.unimplemented_types, ["aai"]);
        assert_eq!(
            without_aai.denominator(),
            all.denominator(),
            "the denominator is the .dbd's surface, not the port's"
        );
        assert!(without_aai.fields.iter().any(|f| f.record_type == "aai"));
    }

    #[test]
    fn coverage_counts_must_add_up_and_errors_are_not_coverage() {
        let cov = Coverage {
            enumerated: 10,
            measured: 6,
            errored: 4,
        };
        assert_eq!(cov.measured + cov.errored, cov.enumerated);
        assert!((cov.percent() - 60.0).abs() < 1e-9);
    }

    #[test]
    fn empty_surface_reports_zero_not_a_divide_by_zero() {
        assert_eq!(Coverage::default().percent(), 0.0);
    }

    #[test]
    fn put_candidates_keep_nomod_and_links_but_never_noaccess() {
        let dbd = Dbd::parse(SAMPLE).unwrap();
        let ai = dbd.record_type("ai").unwrap();
        // SPC_NOMOD must be probed: "the put is refused" IS the contract.
        assert!(is_put_candidate(ai.field("NAME").unwrap()));
        assert!(is_put_candidate(ai.field("VAL").unwrap()));
        // Links must be probed (constant-valued) or CBUG-F6's whole family --
        // a special() that rejects puts to link fields -- goes unobserved.
        assert!(is_put_candidate(ai.field("INP").unwrap()));
        // Nothing can reach a NOACCESS field.
        assert!(!is_put_candidate(ai.field("RPVT").unwrap()));
    }
}
