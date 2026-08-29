//! W10-E3: the acalcout OUT write buffer is chosen by the RESOLVED TARGET
//! element count, and IVOA=Set_output_to_IVOV sets the scalar `OVAL` only.
//!
//! C `devaCalcoutSoft.c::write_acalcout` (65-88): target nelm from
//! `dbCaGetNelements` (CA link — stays 1 when the call fails) or
//! `dbNameToAddr` `no_elements` (DB link), clamped by
//! `i = (nuse > 0) ? nuse : nelm; if (i < nelm) nelm = i;`, then
//! `pBuffer = nelm == 1 ? &val : aval` (DOPT=Use VAL) or
//! `nelm == 1 ? &oval : oav` (DOPT=Use OCAL).
//!
//! C `aCalcoutRecord.c::execOutput` (:924): `pcalc->oval = pcalc->ivov;`
//! — the scalar alone. `oval`/`oav` are otherwise filled together by the
//! OCAL `aCalcPerform` call (:1289), so IVOV substitution is the ONLY
//! point where `OVAL` and `OAV[0]` decouple — which makes the buffer
//! choice observable: a scalar target gets `IVOV`, an array target gets
//! the stale `OAV`, and under DOPT=Use VAL the substitution is a no-op
//! (a C quirk this port reproduces).
//!
//! One test per invariant boundary:
//! - scalar local target × Use OCAL × IVOV  → IVOV delivered
//! - array  local target × Use OCAL × IVOV  → stale OAV delivered
//! - scalar local target × Use VAL  × IVOV  → computed VAL (no-op)
//! - 1-element source    × array target     → scalar buffer (source clamp)
//! - external target, metadata count > 1    → array buffer
//! - external target, no metadata (C failed dbCaGetNelements) → scalar

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::{LinkMetadata, LinkPutOp, LinkSet, PvDatabase};
use epics_base_rs::server::record::{FieldDesc, ProcessOutcome, Record};
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::types::EpicsValue;

/// OUT-link target whose VAL reads back as a SCALAR (`no_elements == 1`
/// in C terms). Records the raw value shape each write delivers.
struct ScalarProbe {
    last: Arc<Mutex<Option<EpicsValue>>>,
}

impl Record for ScalarProbe {
    fn record_type(&self) -> &'static str {
        "nelm_scalar_probe"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(0.0)),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        if name == "VAL" {
            *self.last.lock().unwrap() = Some(value);
        }
        Ok(())
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        &[]
    }
}

/// OUT-link target whose VAL reads back as an ARRAY (`no_elements > 1`).
struct ArrayProbe {
    last: Arc<Mutex<Option<EpicsValue>>>,
}

impl Record for ArrayProbe {
    fn record_type(&self) -> &'static str {
        "nelm_array_probe"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::DoubleArray(vec![0.0; 3])),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        if name == "VAL" {
            *self.last.lock().unwrap() = Some(value);
        }
        Ok(())
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        &[]
    }
}

/// External lset: reports a configurable element count via
/// `link_metadata` (the `dbCaGetNelements` analogue) and records what
/// `put_value` delivers.
struct CountingLset {
    element_count: Option<i64>,
    last_put: Arc<Mutex<Option<EpicsValue>>>,
}

#[epics_base_rs::async_trait]
impl LinkSet for CountingLset {
    fn is_connected(&self, _name: &str) -> bool {
        self.element_count.is_some()
    }
    /// The subject of these tests is which BUFFER the record hands the OUT
    /// put (`&oval` vs `oav`), driven by the cached element count. Admit
    /// every write so the buffer choice is observable: the separate
    /// connection gate C applies before staging
    /// (`dbCaPutLinkCallback`, `dbCa.c:529-532`) is covered by
    /// `out_link_failure_alarm.rs`, and folding it in here would make the
    /// no-metadata case assert nothing at all.
    fn put_admission(&self, _name: &str) -> epics_base_rs::server::database::PutAdmission {
        epics_base_rs::server::database::PutAdmission::Connected
    }
    fn get_cached_value(&self, _name: &str) -> Option<EpicsValue> {
        None
    }
    async fn get_value(&self, name: &str) -> Option<EpicsValue> {
        self.get_cached_value(name)
    }
    async fn put_value(
        &self,
        _name: &str,
        value: EpicsValue,
        _op: LinkPutOp,
    ) -> Result<(), String> {
        *self.last_put.lock().unwrap() = Some(value);
        Ok(())
    }
    fn link_metadata(&self, _name: &str) -> Option<LinkMetadata> {
        self.element_count.map(|n| LinkMetadata {
            element_count: Some(n),
            ..Default::default()
        })
    }
}

/// Build the acalcout under test: CALC="10" trips HIHI=5/HHSV=INVALID
/// (record INVALID, no calc failure), IVOA=Set_output_to_IVOV, IVOV=42.
/// OCAL="7" fills OVAL=7 / OAV=[7; nelm] together, exactly the state C's
/// `aCalcPerform` leaves before the IVOV substitution decouples OVAL.
fn invalid_ivov_record(dopt: i16, nelm: u32, out: &str) -> AcalcoutRecord {
    let mut a = AcalcoutRecord::default();
    a.put_field("NELM", EpicsValue::ULong(nelm)).unwrap();
    a.put_field("CALC", EpicsValue::String("10".into()))
        .unwrap();
    a.special("CALC", true).unwrap();
    a.put_field("DOPT", EpicsValue::Short(dopt)).unwrap();
    a.put_field("OCAL", EpicsValue::String("7".into())).unwrap();
    a.special("OCAL", true).unwrap();
    a.put_field("HIHI", EpicsValue::Double(5.0)).unwrap();
    a.put_field("HHSV", EpicsValue::Short(3)).unwrap();
    a.put_field("IVOA", EpicsValue::Short(2)).unwrap();
    a.put_field("IVOV", EpicsValue::Double(42.0)).unwrap();
    a.put_field("OUT", EpicsValue::String(out.into())).unwrap();
    a
}

async fn process(db: &PvDatabase, name: &str) {
    let mut v = HashSet::new();
    db.process_record_with_links(name, &mut v, 0).await.unwrap();
    // An external OUT put is staged on the link-put queue and the record
    // returns (C `dbCaPutLink`, `dbCa.c:593-595`); `dbCaSync`
    // (`dbCa.c:1126-1129`) is the barrier that makes it observable.
    db.sync_external_link_puts().await;
}

/// Scalar local target, DOPT=Use OCAL: effective nelm 1 → `&pcalc->oval`
/// (devaCalcoutSoft.c:87) → the target receives IVOV, not the stale OAV[0].
#[epics_macros_rs::epics_test]
async fn ivov_reaches_a_scalar_target_under_use_ocal() {
    let db = PvDatabase::new();
    let last = Arc::new(Mutex::new(None));
    db.add_record("PROBE", Box::new(ScalarProbe { last: last.clone() }))
        .await
        .unwrap();
    db.add_record("AC", Box::new(invalid_ivov_record(1, 3, "PROBE")))
        .await
        .unwrap();

    process(&db, "AC").await;

    assert_eq!(
        *last.lock().unwrap(),
        Some(EpicsValue::Double(42.0)),
        "scalar target ⇒ nelm==1 ⇒ &oval ⇒ IVOV"
    );
}

/// Array local target, DOPT=Use OCAL: effective nelm > 1 → `pcalc->oav`
/// — IVOV touched only OVAL, so the target receives the STALE OAV.
#[epics_macros_rs::epics_test]
async fn ivov_bypasses_an_array_target_under_use_ocal() {
    let db = PvDatabase::new();
    let last = Arc::new(Mutex::new(None));
    db.add_record("APROBE", Box::new(ArrayProbe { last: last.clone() }))
        .await
        .unwrap();
    db.add_record("AC", Box::new(invalid_ivov_record(1, 3, "APROBE")))
        .await
        .unwrap();

    process(&db, "AC").await;

    assert_eq!(
        *last.lock().unwrap(),
        Some(EpicsValue::DoubleArray(vec![7.0, 7.0, 7.0])),
        "array target ⇒ oav buffer ⇒ stale OCAL result, NOT IVOV"
    );
}

/// DOPT=Use VAL: the buffer is `&val`/`aval`, which IVOV never touches —
/// C's substitution is a no-op and the computed value is driven.
#[epics_macros_rs::epics_test]
async fn ivov_is_a_no_op_under_use_val() {
    let db = PvDatabase::new();
    let last = Arc::new(Mutex::new(None));
    db.add_record("PROBE", Box::new(ScalarProbe { last: last.clone() }))
        .await
        .unwrap();
    db.add_record("AC", Box::new(invalid_ivov_record(0, 3, "PROBE")))
        .await
        .unwrap();

    process(&db, "AC").await;

    assert_eq!(
        *last.lock().unwrap(),
        Some(EpicsValue::Double(10.0)),
        "Use VAL ⇒ &val buffer ⇒ IVOV substitution (oval=ivov) is a no-op — C quirk"
    );
}

/// 1-element SOURCE (NELM=1), array target: C's clamp
/// `i = (nuse>0 ? nuse : nelm) = 1` forces nelm to 1 ⇒ `&oval` ⇒ the
/// array target still receives the scalar buffer, i.e. IVOV.
#[epics_macros_rs::epics_test]
async fn a_single_element_source_picks_the_scalar_buffer() {
    let db = PvDatabase::new();
    let last = Arc::new(Mutex::new(None));
    db.add_record("APROBE", Box::new(ArrayProbe { last: last.clone() }))
        .await
        .unwrap();
    db.add_record("AC", Box::new(invalid_ivov_record(1, 1, "APROBE")))
        .await
        .unwrap();

    process(&db, "AC").await;

    assert_eq!(
        *last.lock().unwrap(),
        Some(EpicsValue::Double(42.0)),
        "source count 1 ⇒ nelm==1 ⇒ &oval, whatever the target"
    );
}

/// External target reporting element_count=3 (`dbCaGetNelements`
/// succeeded): array buffer — the stale OAV reaches the lset put.
#[epics_macros_rs::epics_test]
async fn an_external_array_target_gets_the_array_buffer() {
    let db = PvDatabase::new();
    let last_put = Arc::new(Mutex::new(None));
    db.register_link_set(
        "ca",
        Arc::new(CountingLset {
            element_count: Some(3),
            last_put: last_put.clone(),
        }),
    )
    .await;
    db.add_record("AC", Box::new(invalid_ivov_record(1, 3, "ca://EXT")))
        .await
        .unwrap();

    process(&db, "AC").await;

    assert_eq!(
        *last_put.lock().unwrap(),
        Some(EpicsValue::DoubleArray(vec![7.0, 7.0, 7.0])),
        "connected CA array target ⇒ oav buffer ⇒ stale OCAL result"
    );
}

/// External target with NO metadata — C's `dbCaGetNelements` fails and
/// `nelm` keeps its initializer 1 (devaCalcoutSoft.c:68) ⇒ scalar buffer.
#[epics_macros_rs::epics_test]
async fn an_unresolved_external_target_defaults_to_the_scalar_buffer() {
    let db = PvDatabase::new();
    let last_put = Arc::new(Mutex::new(None));
    db.register_link_set(
        "ca",
        Arc::new(CountingLset {
            element_count: None,
            last_put: last_put.clone(),
        }),
    )
    .await;
    db.add_record("AC", Box::new(invalid_ivov_record(1, 3, "ca://EXT")))
        .await
        .unwrap();

    process(&db, "AC").await;

    assert_eq!(
        *last_put.lock().unwrap(),
        Some(EpicsValue::Double(42.0)),
        "no cached element count ⇒ C's nelm=1 default ⇒ &oval ⇒ IVOV"
    );
}
