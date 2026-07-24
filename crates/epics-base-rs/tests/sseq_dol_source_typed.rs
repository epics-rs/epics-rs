//! R17-2: an sseq step reads `DOLn` with the DBR class the SOURCE's DBF class
//! asks for — C `sseqRecord.c::processCallback`'s input switch
//! (sseqRecord.c:640-705) — never with the source's native link type.
//!
//!   * `DBF_STRING`/`ENUM`/`MENU`/`DEVICE`/`INLINK`/`OUTLINK`/`FWDLINK`
//!     (:644-661) → `dbGetLink(DBR_STRING)` into `s`/`STRn`, then
//!     `dov = atof(s)`. An `ENUM` source therefore delivers its state LABEL,
//!     and `DOn` becomes `atof(label)` — not the state index.
//!   * `DBF_SHORT`..`DBF_DOUBLE` (:663-680) → `dbGetLink(DBR_DOUBLE)` into
//!     `dov`/`DOn`, then `STRn` re-rendered at PREC.
//!   * `DBF_CHAR`/`DBF_UCHAR` (:682-700) → `min(n_elements, 40)` bytes into the
//!     40-byte `s`, then `dov = atof(s)`.
//!   * anything else (:703 `default:`) → NO read at all. A CONSTANT `DOLn` is
//!     pinned to `DBF_NOACCESS` at `init_record` (:206) and seeded ONCE there
//!     (:204-205 + the `atof` tail at :243-245).
//!
//! One test per boundary of that switch, plus the failed-store path the read
//! owner used to discard silently.

// RTEMS-EXEC-MODEL-ALLOW(6): checked - these run and pass in the feature-ON suite.

use std::collections::HashSet;
use std::time::Duration;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{
    FieldDesc, ProcessAction, ProcessOutcome, Record, RecordProcessResult,
};
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::mbbi::MbbiRecord;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::server::records::stringout::StringoutRecord;
use epics_base_rs::server::records::waveform::WaveformRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// Run one sseq cycle and wait for its single step to have fired (the LNK write
/// lands after the DOL read in the same Fire cycle, so a settled destination
/// means the read is done).
async fn run_step(db: &PvDatabase, sseq: &str, dst: &str, label: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(sseq, &mut visited, 0)
        .await
        .unwrap();
    for _ in 0..400 {
        if let Some(rec) = db.get_record(dst) {
            let landed = match rec.read().record.get_field("VAL") {
                Some(EpicsValue::String(s)) => !s.is_empty(),
                Some(EpicsValue::Double(d)) => d != 0.0,
                _ => false,
            };
            if landed {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("{label}: {dst}.VAL never received the step's forwarded value");
}

async fn field(db: &PvDatabase, record: &str, name: &str) -> EpicsValue {
    db.get_record(record)
        .unwrap()
        .read()
        .record
        .get_field(name)
        .unwrap_or_else(|| panic!("{record}.{name} missing"))
}

/// `DBF_ENUM` source (mbbi): C reads it with `DBR_STRING`, so `STRn` gets the
/// state LABEL and `DOn` gets `atof(label)`. Pre-fix the port stored the index
/// (`DO1 = 2`, `STR1 = "2"`).
#[tokio::test]
async fn sseq_enum_dol_delivers_state_label_not_index() {
    let db = PvDatabase::new();

    let mut src = MbbiRecord::new(0);
    src.put_field("ZRST", EpicsValue::String("Idle".into()))
        .unwrap();
    src.put_field("ONST", EpicsValue::String("Busy".into()))
        .unwrap();
    src.put_field("TWST", EpicsValue::String("Fault".into()))
        .unwrap();
    src.put_field("VAL", EpicsValue::Enum(2)).unwrap();
    db.add_record("SS_ENUM_SRC", Box::new(src)).await.unwrap();

    db.add_record("SS_ENUM_DST", Box::new(StringoutRecord::new("")))
        .await
        .unwrap();

    let mut sseq = SseqRecord::new();
    sseq.put_field("SELM", EpicsValue::Short(0)).unwrap();
    sseq.put_field("DOL1", EpicsValue::String("SS_ENUM_SRC.VAL".into()))
        .unwrap();
    sseq.put_field("LNK1", EpicsValue::String("SS_ENUM_DST".into()))
        .unwrap();
    db.add_record("SS_ENUM", Box::new(sseq)).await.unwrap();

    run_step(&db, "SS_ENUM", "SS_ENUM_DST", "enum DOL").await;

    assert_eq!(
        field(&db, "SS_ENUM", "STR1").await,
        EpicsValue::String("Fault".into()),
        "an ENUM DOL source is read with DBR_STRING: STR1 must be the state label"
    );
    assert_eq!(
        field(&db, "SS_ENUM", "DO1").await,
        EpicsValue::Double(0.0),
        "C makes dov agree with s: atof(\"Fault\") == 0, not the state index 2"
    );
    assert_eq!(
        field(&db, "SS_ENUM_DST", "VAL").await,
        EpicsValue::String("Fault".into()),
        "the label is what reaches a string LNK target"
    );
}

/// `DBF_MENU` source (a menu-index field — here the mbbi's `SIMM`): the same
/// `DBR_STRING` arm, resolved through the field's `menu()` choices.
#[tokio::test]
async fn sseq_menu_dol_delivers_choice_label() {
    let db = PvDatabase::new();

    let mut src = MbbiRecord::new(0);
    src.put_field("SIMM", EpicsValue::Short(1)).unwrap(); // menuSimm: NO, YES, RAW
    db.add_record("SS_MENU_SRC", Box::new(src)).await.unwrap();
    db.add_record("SS_MENU_DST", Box::new(StringoutRecord::new("")))
        .await
        .unwrap();

    let mut sseq = SseqRecord::new();
    sseq.put_field("SELM", EpicsValue::Short(0)).unwrap();
    sseq.put_field("DOL1", EpicsValue::String("SS_MENU_SRC.SIMM".into()))
        .unwrap();
    sseq.put_field("LNK1", EpicsValue::String("SS_MENU_DST".into()))
        .unwrap();
    db.add_record("SS_MENU", Box::new(sseq)).await.unwrap();

    run_step(&db, "SS_MENU", "SS_MENU_DST", "menu DOL").await;

    assert_eq!(
        field(&db, "SS_MENU", "STR1").await,
        EpicsValue::String("YES".into()),
        "a MENU DOL source is read with DBR_STRING: STR1 must be the choice label"
    );
}

/// `DBF_CHAR` array source (the long-string idiom): C reads up to 40 bytes into
/// `s` and sets `dov = atof(s)`. Pre-fix the native `CharArray` reached `DOn`,
/// `to_f64()` failed, and the error was discarded — `DO1`/`STR1` stayed stale.
#[tokio::test]
async fn sseq_char_array_dol_delivers_string_and_atof() {
    let db = PvDatabase::new();

    let mut src = WaveformRecord::new(40, DbFieldType::Char);
    src.put_field(
        "VAL",
        EpicsValue::CharArray(b"12.5\0                                   ".to_vec()),
    )
    .unwrap();
    db.add_record("SS_CHAR_SRC", Box::new(src)).await.unwrap();
    db.add_record("SS_CHAR_DST", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    let mut sseq = SseqRecord::new();
    sseq.put_field("SELM", EpicsValue::Short(0)).unwrap();
    sseq.put_field("DOL1", EpicsValue::String("SS_CHAR_SRC.VAL".into()))
        .unwrap();
    sseq.put_field("LNK1", EpicsValue::String("SS_CHAR_DST".into()))
        .unwrap();
    db.add_record("SS_CHAR", Box::new(sseq)).await.unwrap();

    run_step(&db, "SS_CHAR", "SS_CHAR_DST", "char-array DOL").await;

    assert_eq!(
        field(&db, "SS_CHAR", "STR1").await,
        EpicsValue::String("12.5".into()),
        "a CHAR-array DOL source spells the string; the bytes stop at the NUL"
    );
    assert_eq!(
        field(&db, "SS_CHAR", "DO1").await,
        EpicsValue::Double(12.5),
        "C makes dov agree with s: atof(\"12.5\")"
    );
    assert_eq!(
        field(&db, "SS_CHAR_DST", "VAL").await,
        EpicsValue::Double(12.5),
        "the numeric view is what reaches a DOUBLE LNK target"
    );
}

/// A numeric source keeps the `DBR_DOUBLE` arm: `DOn` is the value and `STRn`
/// is re-rendered at the record's PREC.
#[tokio::test]
async fn sseq_numeric_dol_keeps_double_arm() {
    let db = PvDatabase::new();

    db.add_record("SS_NUM_SRC", Box::new(AoRecord::new(42.5)))
        .await
        .unwrap();
    db.add_record("SS_NUM_DST", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    let mut sseq = SseqRecord::new();
    sseq.put_field("SELM", EpicsValue::Short(0)).unwrap();
    sseq.put_field("PREC", EpicsValue::Short(2)).unwrap();
    sseq.put_field("DOL1", EpicsValue::String("SS_NUM_SRC.VAL".into()))
        .unwrap();
    sseq.put_field("LNK1", EpicsValue::String("SS_NUM_DST".into()))
        .unwrap();
    db.add_record("SS_NUM", Box::new(sseq)).await.unwrap();

    run_step(&db, "SS_NUM", "SS_NUM_DST", "numeric DOL").await;

    assert_eq!(field(&db, "SS_NUM", "DO1").await, EpicsValue::Double(42.5));
    assert_eq!(
        field(&db, "SS_NUM", "STR1").await,
        EpicsValue::String("42.50".into()),
        "the numeric arm re-renders STRn at PREC"
    );
}

/// A CONSTANT `DOLn` is C's `default:` arm at process time: seeded ONCE at
/// init (the link TEXT into `s`, `dov = atof(s)`), never re-read. A client put
/// to `DO1` therefore survives the next cycle.
#[tokio::test]
async fn sseq_constant_dol_seeds_at_init_and_is_never_re_read() {
    let db = PvDatabase::new();

    db.add_record("SS_CONST_DST", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    let mut sseq = SseqRecord::new();
    sseq.put_field("SELM", EpicsValue::Short(0)).unwrap();
    sseq.put_field("PREC", EpicsValue::Short(3)).unwrap();
    sseq.put_field("DOL1", EpicsValue::String("1.50".into()))
        .unwrap();
    sseq.put_field("LNK1", EpicsValue::String("SS_CONST_DST".into()))
        .unwrap();
    db.add_record("SS_CONST", Box::new(sseq)).await.unwrap();

    // C seeds `s` with the link TEXT (`cvt_st_st` is a strncpy), so "1.50"
    // stays "1.50" — not the PREC=3 rendering an f64 round-trip would give.
    assert_eq!(
        field(&db, "SS_CONST", "STR1").await,
        EpicsValue::String("1.50".into()),
        "a constant DOL seeds STRn with the link text at init"
    );
    assert_eq!(
        field(&db, "SS_CONST", "DO1").await,
        EpicsValue::Double(1.5),
        "and DOn follows as atof(s)"
    );

    // A client value on DO1 must not be overwritten by a re-read of the
    // constant: C never calls dbGetLink on it again (dol_field_type =
    // DBF_NOACCESS -> default: break).
    db.put_pv("SS_CONST.DO1", EpicsValue::Double(9.0))
        .await
        .unwrap();
    run_step(&db, "SS_CONST", "SS_CONST_DST", "constant DOL").await;
    assert_eq!(
        field(&db, "SS_CONST", "DO1").await,
        EpicsValue::Double(9.0),
        "the constant must not be re-applied on the process cycle"
    );
    assert_eq!(
        field(&db, "SS_CONST_DST", "VAL").await,
        EpicsValue::Double(9.0),
        "the step forwards the client's value, not the constant"
    );
}

/// The read owner's failed-store path: a value the target field REJECTS is a
/// failed `dbGetLink` (a conversion error is a non-zero status → `setLinkAlarm`,
/// `dbLink.c:316-323`), not a silent no-op. Pre-fix the store's `Err` went to
/// `let _ =`, so the record kept its stale field with no alarm.
struct PickyReader {
    val: f64,
}

static PICKY_FIELDS: &[FieldDesc] = &[
    FieldDesc::new("VAL", DbFieldType::Double, false),
    FieldDesc::new("INP", DbFieldType::String, false),
];

impl Record for PickyReader {
    fn record_type(&self) -> &'static str {
        "picky"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome {
            result: RecordProcessResult::Complete,
            actions: Vec::new(),
            device_did_compute: false,
        })
    }
    fn pre_process_actions(&mut self) -> Vec<ProcessAction> {
        vec![ProcessAction::ReadDbLink {
            link_field: "INP",
            target_field: "VAL",
        }]
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            // The link string lives in `common.inp`; the record answers with the
            // configured source so the framework's read seam can resolve it.
            "INP" => Some(EpicsValue::String("SS_REJECT_SRC.VAL".into())),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match (name, value) {
            ("VAL", EpicsValue::Double(v)) => {
                self.val = v;
                Ok(())
            }
            ("VAL", _) => Err(CaError::TypeMismatch("VAL".into())),
            ("INP", _) => Ok(()),
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }
    /// The record owns its internal-put coercion (as `sseq` does for `DOn`), and
    /// REJECTS a value it cannot convert — the C `dbGetLink` conversion failure.
    fn put_field_internal(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        if name == "VAL" {
            let v = value
                .to_f64()
                .ok_or_else(|| CaError::TypeMismatch("VAL".into()))?;
            self.val = v;
            return Ok(());
        }
        self.put_field(name, value)
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        PICKY_FIELDS
    }
}

#[tokio::test]
async fn rejected_link_store_raises_link_invalid() {
    let db = PvDatabase::new();

    db.add_record(
        "SS_REJECT_SRC",
        Box::new(StringoutRecord::new("not a number")),
    )
    .await
    .unwrap();
    db.add_record("SS_REJECT", Box::new(PickyReader { val: 7.0 }))
        .await
        .unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("SS_REJECT", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("SS_REJECT").unwrap();
    let inst = rec.read();
    assert_eq!(
        inst.record.get_field("VAL"),
        Some(EpicsValue::Double(7.0)),
        "the rejected value must not land"
    );
    assert_eq!(
        inst.common.sevr,
        epics_base_rs::server::record::AlarmSeverity::Invalid,
        "a store the target rejects is a failed dbGetLink: INVALID severity"
    );
    assert_eq!(
        inst.common.stat,
        epics_base_rs::server::recgbl::alarm_status::LINK_ALARM,
        "...with LINK_ALARM status, C's setLinkAlarm"
    );
}
