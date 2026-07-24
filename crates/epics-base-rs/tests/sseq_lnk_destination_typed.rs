//! R16-1: an sseq step forwards the view of its value that the `LNKn`
//! DESTINATION asks for — never the one its `DOLn` SOURCE happened to
//! deliver.
//!
//! C `sseqRecord.c::processCallback` (706-793) resolves the destination at
//! fire time (`dbGetLinkDBFtype(&lnk)` / `dbGetNelements(&lnk)`) and
//! switches on it:
//!
//!   * `DBF_STRING`/`ENUM`/`MENU`/`DEVICE`/`INLINK`/`OUTLINK`/`FWDLINK`
//!     (:714-736) → `DBR_STRING` from `s` (`STRn`), which for a numeric
//!     source is `cvtDoubleToString(dov, s, pR->prec)` — the sseq's own PREC.
//!   * `DBF_SHORT`..`DBF_DOUBLE` (:738-760) → `DBR_DOUBLE` from `dov`
//!     (`DOn`), which for a string source is `atof(s)`.
//!   * `DBF_CHAR`/`DBF_UCHAR` with `n_elements > 1` (:762-790) → the 40-byte
//!     `s` as a char array (the long-string idiom).
//!   * anything else, including an unresolvable target (:792 `default:`) →
//!     NO put at all.
//!
//! One test per boundary of that switch.

// RTEMS-EXEC-MODEL-ALLOW(6): checked - these run and pass in the feature-ON suite.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::{LinkDbfType, LinkMetadata, LinkPutOp, LinkSet, PvDatabase};
use epics_base_rs::server::record::{FieldDesc, ProcessOutcome, Record};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::server::records::stringin::StringinRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// An OUT-link target of a chosen DBF class that records the raw value each
/// write delivers — the C `dbNameToAddr` `field_type` / `no_elements` the
/// sseq switch reads.
struct TypedProbe {
    /// VAL's read-back shape, which is what the port's target resolution
    /// derives the element count from (a scalar → 1, an array → its length).
    readback: EpicsValue,
    fields: &'static [FieldDesc],
    last: Arc<Mutex<Option<EpicsValue>>>,
}

impl Record for TypedProbe {
    fn record_type(&self) -> &'static str {
        "typed_probe"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(self.readback.clone()),
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
        self.fields
    }
}

static STRING_VAL: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::String, false)];
static DOUBLE_VAL: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Double, false)];
static CHAR_VAL: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Char, false)];

fn probe(
    fields: &'static [FieldDesc],
    readback: EpicsValue,
    last: Arc<Mutex<Option<EpicsValue>>>,
) -> TypedProbe {
    TypedProbe {
        readback,
        fields,
        last,
    }
}

/// An external link set that reports metadata (the `dbCaGetLinkDBFtype` /
/// `dbCaGetNelements` analogue) and records every put.
struct ExtLset {
    metadata: Option<LinkMetadata>,
    last_put: Arc<Mutex<Option<EpicsValue>>>,
}

#[epics_base_rs::async_trait]
impl LinkSet for ExtLset {
    fn is_connected(&self, _name: &str) -> bool {
        self.metadata.is_some()
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
        self.metadata.clone()
    }
}

async fn kick(db: &PvDatabase, name: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(name, &mut visited, 0)
        .await
        .unwrap();
}

/// Poll the probe until it has been written, then return the value. The sseq
/// machine fires each step in a spawned re-entry, so the kick returns first.
async fn poll_put(cell: &Arc<Mutex<Option<EpicsValue>>>, label: &str) -> EpicsValue {
    for _ in 0..400 {
        if let Some(v) = cell.lock().unwrap().clone() {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("{label}: LNK1 was never written");
}

/// Settle long enough that a write, had one been issued, would have landed.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(120)).await;
}

/// Boundary — NUMERIC source, STRING-class destination: C puts `DBR_STRING`
/// from `s`, which the numeric arm rendered with `cvtDoubleToString(dov,
/// s, pR->prec)`. With PREC=2 the wire value is "1.23", NOT the full-
/// precision double the source delivered.
#[tokio::test]
async fn numeric_source_string_destination_puts_the_prec_rendered_string() {
    let db = PvDatabase::new();
    let last = Arc::new(Mutex::new(None));
    db.add_record("SRC_NUM", Box::new(AiRecord::new(1.23456789)))
        .await
        .unwrap();
    db.add_record(
        "TGT_STR",
        Box::new(probe(
            STRING_VAL,
            EpicsValue::String("".into()),
            last.clone(),
        )),
    )
    .await
    .unwrap();

    let mut sseq = SseqRecord::new();
    sseq.prec = 2;
    sseq.put_field("DOL1", EpicsValue::String("SRC_NUM.VAL".into()))
        .unwrap();
    sseq.put_field("LNK1", EpicsValue::String("TGT_STR.VAL PP".into()))
        .unwrap();
    db.add_record("SSEQ_NS", Box::new(sseq)).await.unwrap();

    kick(&db, "SSEQ_NS").await;

    assert_eq!(
        poll_put(&last, "numeric → string").await,
        EpicsValue::String("1.23".into()),
        "a string-class LNKn takes DBR_STRING from STRn = cvtDoubleToString(dov, PREC)"
    );
}

/// Boundary — STRING source, NUMERIC destination: C puts `DBR_DOUBLE` from
/// `dov`, which the string arm set to `atof(s)`. A non-numeric string is
/// 0.0 on the wire — the put still happens (and processes the target).
#[tokio::test]
async fn string_source_numeric_destination_puts_atof_of_the_string() {
    let db = PvDatabase::new();
    let last = Arc::new(Mutex::new(None));
    db.add_record("SRC_STR", Box::new(StringinRecord::new("")))
        .await
        .unwrap();
    db.put_pv("SRC_STR", EpicsValue::String("abc".into()))
        .await
        .unwrap();
    db.add_record(
        "TGT_NUM",
        Box::new(probe(DOUBLE_VAL, EpicsValue::Double(0.0), last.clone())),
    )
    .await
    .unwrap();

    let mut sseq = SseqRecord::new();
    sseq.put_field("DOL1", EpicsValue::String("SRC_STR.VAL".into()))
        .unwrap();
    sseq.put_field("LNK1", EpicsValue::String("TGT_NUM.VAL PP".into()))
        .unwrap();
    db.add_record("SSEQ_SN", Box::new(sseq)).await.unwrap();

    kick(&db, "SSEQ_SN").await;

    assert_eq!(
        poll_put(&last, "string → numeric").await,
        EpicsValue::Double(0.0),
        "a numeric LNKn takes DBR_DOUBLE from DOn = atof(STRn) — never the string itself"
    );
}

/// Boundary — CHAR destination with `n_elements > 1` (the long-string
/// idiom): C puts `min(n_elements, 40)` bytes of the 40-byte `s` as a char
/// array, NUL-padded — not the double.
#[tokio::test]
async fn char_array_destination_puts_the_string_bytes() {
    let db = PvDatabase::new();
    let last = Arc::new(Mutex::new(None));
    db.add_record("SRC_C", Box::new(AiRecord::new(7.0)))
        .await
        .unwrap();
    db.add_record(
        "TGT_CHAR",
        Box::new(probe(
            CHAR_VAL,
            EpicsValue::CharArray(vec![0; 8]),
            last.clone(),
        )),
    )
    .await
    .unwrap();

    let mut sseq = SseqRecord::new();
    sseq.prec = 1;
    sseq.put_field("DOL1", EpicsValue::String("SRC_C.VAL".into()))
        .unwrap();
    sseq.put_field("LNK1", EpicsValue::String("TGT_CHAR.VAL PP".into()))
        .unwrap();
    db.add_record("SSEQ_CA", Box::new(sseq)).await.unwrap();

    kick(&db, "SSEQ_CA").await;

    let mut want = b"7.0".to_vec();
    want.resize(8, 0);
    assert_eq!(
        poll_put(&last, "numeric → CHAR array").await,
        EpicsValue::CharArray(want),
        "a CHAR/UCHAR LNKn with n_elements > 1 takes the STRn bytes, NUL-padded to n"
    );
}

/// Boundary — a CHAR destination with `n_elements == 1` is a scalar, and C's
/// CHAR arm falls back to `DBR_DOUBLE` from `dov` (sseqRecord.c:786-789).
#[tokio::test]
async fn scalar_char_destination_puts_the_double() {
    let db = PvDatabase::new();
    let last = Arc::new(Mutex::new(None));
    db.add_record("SRC_C1", Box::new(AiRecord::new(7.0)))
        .await
        .unwrap();
    db.add_record(
        "TGT_CHAR1",
        Box::new(probe(CHAR_VAL, EpicsValue::Char(0), last.clone())),
    )
    .await
    .unwrap();

    let mut sseq = SseqRecord::new();
    sseq.put_field("DOL1", EpicsValue::String("SRC_C1.VAL".into()))
        .unwrap();
    sseq.put_field("LNK1", EpicsValue::String("TGT_CHAR1.VAL PP".into()))
        .unwrap();
    db.add_record("SSEQ_C1", Box::new(sseq)).await.unwrap();

    kick(&db, "SSEQ_C1").await;

    // The buffer put is `DBR_DOUBLE`; the target converts it to its own
    // DBF_CHAR field, exactly as C's `dbPutLink(DBR_DOUBLE)` does. The wire
    // shape that matters here is "one scalar", not the 40-byte char array the
    // `n_elements > 1` arm sends.
    assert_eq!(
        poll_put(&last, "numeric → scalar CHAR").await,
        EpicsValue::Char(7),
        "a scalar CHAR LNKn takes DBR_DOUBLE from DOn, converted at the target"
    );
}

/// Boundary — an UNRESOLVABLE destination (C `dbGetLinkDBFtype` → -1, i.e.
/// a disconnected CA link) hits `default: break`: NO put is issued at all.
/// The port previously wrote the source-typed value regardless.
#[tokio::test]
async fn unresolved_destination_is_not_written_at_all() {
    let db = PvDatabase::new();
    let last_put = Arc::new(Mutex::new(None));
    db.register_link_set(
        "ca",
        Arc::new(ExtLset {
            // Connected, but the remote field type is unknown — exactly what
            // `dbCaGetLinkDBFtype` reports as -1 = DBF_unknown.
            metadata: Some(LinkMetadata {
                element_count: Some(1),
                dbf_type: None,
                ..Default::default()
            }),
            last_put: last_put.clone(),
        }),
    )
    .await;
    db.add_record("SRC_U", Box::new(AiRecord::new(5.0)))
        .await
        .unwrap();

    let mut sseq = SseqRecord::new();
    sseq.put_field("DOL1", EpicsValue::String("SRC_U.VAL".into()))
        .unwrap();
    sseq.put_field("LNK1", EpicsValue::String("ca://EXT_UNKNOWN".into()))
        .unwrap();
    db.add_record("SSEQ_UN", Box::new(sseq)).await.unwrap();

    kick(&db, "SSEQ_UN").await;
    settle().await;

    assert_eq!(
        *last_put.lock().unwrap(),
        None,
        "an LNKn whose DBF type does not resolve gets no put (C default: break)"
    );
    // The sequence still completes — the step is not stranded.
    assert_eq!(
        db.get_pv("SSEQ_UN.BUSY").unwrap(),
        EpicsValue::Short(0),
        "the sequence finishes even though the step wrote nothing"
    );
}

/// Boundary — a RESOLVED external destination is written, typed by the
/// remote field's class (C `dbCaGetLinkDBFtype`): a remote string channel
/// takes the STRn view.
#[tokio::test]
async fn resolved_external_string_destination_takes_the_string_view() {
    let db = PvDatabase::new();
    let last_put = Arc::new(Mutex::new(None));
    db.register_link_set(
        "ca",
        Arc::new(ExtLset {
            metadata: Some(LinkMetadata {
                element_count: Some(1),
                dbf_type: Some(LinkDbfType::String),
                ..Default::default()
            }),
            last_put: last_put.clone(),
        }),
    )
    .await;
    db.add_record("SRC_E", Box::new(AiRecord::new(4.5)))
        .await
        .unwrap();

    let mut sseq = SseqRecord::new();
    sseq.prec = 3;
    sseq.put_field("DOL1", EpicsValue::String("SRC_E.VAL".into()))
        .unwrap();
    sseq.put_field("LNK1", EpicsValue::String("ca://EXT_STR".into()))
        .unwrap();
    db.add_record("SSEQ_EX", Box::new(sseq)).await.unwrap();

    kick(&db, "SSEQ_EX").await;
    settle().await;

    assert_eq!(
        *last_put.lock().unwrap(),
        Some(EpicsValue::String("4.500".into())),
        "a connected CA string channel takes DBR_STRING from STRn (PREC=3)"
    );
}
