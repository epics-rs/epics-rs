//! R17-65: a long-string record loads its constant link through C's
//! `dbLoadLinkLS` (dbLink.c:244) — a lset entry of its own, not
//! `recGblInitConstantLink`.
//!
//! `lso` runs it on DOL (`lsoRecord.c:82`), `lsi`'s soft device support on INP
//! (`devLsiSoft.c:24`), and the two lsets that implement `loadLS` behave
//! differently:
//!
//! * plain CONSTANT (`dbConstLink.c:178`) → `dbLSConvertJSON` of the link text.
//!   A plain constant link is CONSTANT *because* its text is a number, and
//!   `dbLSConvertJSON` (dbConvertJSON.c:191-236) never calls
//!   `yajl_complete_parse` — so the pending number token fires no callback, the
//!   buffer is untouched, and `*plen = pdest - pdest + 1` yields LEN=1.
//! * JSON `{const:…}` (`lnkConst.c:419`) → the string value is copied; a
//!   numeric value is `S_db_badField` and loads nothing.
//!
//! Either way, the record's init tail (`if (prec->len) { strcpy(oval, val);
//! olen = len; udf = FALSE; }`) makes a loaded link DEFINE the record.
//!
//! The port instead copied the constant TEXT into VAL (an lso with
//! `field(DOL,"5")` held "5"), left UDF=1, and `lsi` never seeded at all — the
//! generic scalar seed tried `set_val(Double)` on a long-string VAL and got a
//! TypeMismatch.
//!
//! softIoc (EPICS 7.0.10, linux-x86_64), `dbgf` after `iocInit`:
//!
//! ```text
//! record(lso,"L1"){field(DOL,"5")}                 VAL ""          LEN 1  UDF 0
//! record(lso,"L2"){field(DOL,{const:"hello"})}     VAL "hello"     LEN 6  UDF 0
//! record(lsi,"L3"){field(INP,{const:"hi there"})}  VAL "hi there"  LEN 9  UDF 0
//! record(lsi,"L4"){field(INP,"5")}                 VAL ""          LEN 1  UDF 0
//! ```

// RTEMS-EXEC-MODEL-ALLOW(4): checked - these run and pass in the feature-ON suite.

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

async fn build(db_text: &str) -> std::sync::Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

/// `(VAL, LEN, UDF)` — the three fields softIoc was probed on.
async fn state(db: &PvDatabase, rec: &str) -> (String, u32, bool) {
    let r = db.get_record(rec).unwrap();
    let inst = r.read();
    let val = match inst.record.get_field("VAL").unwrap() {
        EpicsValue::CharArray(bytes) => {
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            String::from_utf8_lossy(&bytes[..end]).into_owned()
        }
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        other => panic!("{rec}.VAL is not a long string: {other:?}"),
    };
    let len = match inst.record.get_field("LEN").unwrap() {
        EpicsValue::ULong(v) => v,
        other => panic!("{rec}.LEN: {other:?}"),
    };
    (val, len, inst.common.udf != 0)
}

#[tokio::test]
async fn a_numeric_constant_link_defines_the_record_without_text() {
    let db = build(
        r#"
        record(lso, "L1") { field(DOL, "5") }
        record(lsi, "L4") { field(INP, "5") }
        "#,
    )
    .await;

    assert_eq!(
        state(&db, "L1").await,
        (String::new(), 1, false),
        "lso DOL=\"5\": dbLSConvertJSON's number token fires no callback — \
         VAL stays empty, LEN=1, and the non-zero LEN clears UDF"
    );
    assert_eq!(
        state(&db, "L4").await,
        (String::new(), 1, false),
        "lsi INP=\"5\" through devLsiSoft's dbLoadLinkLS: same result"
    );
}

#[tokio::test]
async fn a_json_const_string_is_the_only_text_form() {
    let db = build(
        r#"
        record(lso, "L2") { field(DOL, {const:"hello"}) }
        record(lsi, "L3") { field(INP, {const:"hi there"}) }
        "#,
    )
    .await;

    assert_eq!(
        state(&db, "L2").await,
        ("hello".to_string(), 6, false),
        "lso DOL={{const:\"hello\"}}: lnkConst_loadLS copies the string, LEN=strlen+1"
    );
    assert_eq!(
        state(&db, "L3").await,
        ("hi there".to_string(), 9, false),
        "lsi INP={{const:\"hi there\"}}: same, through the soft device support"
    );
}

/// SIZV bounds the copy (`strncpy(pbuffer, pstr, --size)`, lnkConst.c:446-448),
/// so the text is truncated to `SIZV-1` and LEN counts the NUL.
#[tokio::test]
async fn a_json_const_string_is_clamped_at_sizv() {
    let db = build(
        r#"
        record(lso, "L5") { field(SIZV, "16") field(DOL, {const:"0123456789abcdefghij"}) }
        "#,
    )
    .await;

    assert_eq!(
        state(&db, "L5").await,
        ("0123456789abcde".to_string(), 16, false),
        "SIZV=16 keeps 15 characters plus the NUL"
    );
}

/// A PV link has no init-time `loadLS` at all, so the record stays undefined.
#[tokio::test]
async fn a_pv_link_loads_nothing_and_leaves_the_record_undefined() {
    let db = build(
        r#"
        record(lsi, "SRC") { field(INP, {const:"x"}) }
        record(lso, "L6")  { field(DOL, "SRC") field(OMSL, "closed_loop") }
        "#,
    )
    .await;

    let (val, len, udf) = state(&db, "L6").await;
    assert_eq!(val, "", "a PV DOL delivers nothing at init");
    assert_eq!(len, 0, "LEN stays 0");
    assert!(udf, "and the record stays UDF=1 until it processes");
}
