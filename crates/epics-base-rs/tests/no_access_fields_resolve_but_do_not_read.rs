//! A `DBF_NOACCESS` field is PRESENT and UNREADABLE, and those are two facts.
//!
//! C `dbNameToAddr` resolves every field the `.dbd` declares — `dbCommon`'s
//! `BKPT` and `TIME` included (`dbCommon.dbd.pod:543-548`, `:564-569`) — so the
//! name is found and `dbGet`'s validity gate is what refuses it, with
//! `S_db_badDbrtype` for `field_type > DBF_DEVICE` (`dbAccess.c:667-675`
//! @R7.0.10). Measured against `softIoc` R7.0.10:
//!
//! ```text
//! dbgf T:AI.TIME    -> DBF_CHAR:           failed.      (stdout)
//!                      recGblDbaddrError: … PV: T:AI.TIME  (stderr)
//! dbgf T:AI.NOSUCH  -> PV 'T:AI.NOSUCH' not found
//! dba  T:AI.TIME    ->     Field Type: 17 = DBF_NOACCESS
//!                          Field Size: 8
//! caget T:AI.TIME   -> Channel connect timed out: … not found.
//! ```
//!
//! `DBF_CHAR` there is C reading off the end of its own table and is not
//! reproduced; see `dbgf_failed`.
//!
//! The port used to drop these rows from the field table entirely, so the two
//! facts collapsed into one and `REC.TIME` was indistinguishable from a typo.
//! Carrying the descriptors fixes the presence half — and, on its own, breaks
//! the other half: with a descriptor in hand `declared_default` synthesises the
//! declared type's zero, so `caget T:AI.TIME` CONNECTED and read 0, which C
//! never does. Both halves are asserted here for that reason.

mod module_records;

use std::collections::BTreeSet;

use epics_base_rs::error::CaError;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{Special, dbd_generated, declared_field};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// The record type's own table UNION `dbCommon` — C `dbFindFieldPart`, which
/// searches both, and where `BKPT` and `TIME` live.
fn desc(
    record_type: &str,
    field: &str,
) -> Option<&'static epics_base_rs::server::record::FieldDesc> {
    declared_field(record_type, field)
}

/// The presence half: the declaration is in the table, with the width C's
/// `pdbFldDes->size` carries and the `extra(...)` text it comes from.
#[test]
fn the_declaration_is_carried_with_the_width_its_extra_names() {
    for (field, size, extra_starts) in [("BKPT", 1u16, "epicsUInt8"), ("TIME", 8, "epicsTimeStamp")]
    {
        let d = desc("ai", field).unwrap_or_else(|| panic!("ai.{field} must be declared"));
        assert!(d.no_access(), "{field} is declared DBF_NOACCESS");
        assert!(d.unreadable(), "{field} carries no cvt_dbaddr re-typing");
        assert_eq!(
            d.size, size,
            "{field} width is sizeof() of its extra member"
        );
        assert!(
            d.extra.unwrap_or_default().starts_with(extra_starts),
            "{field} keeps the C declaration its width came from, got {:?}",
            d.extra
        );
        assert_eq!(d.declared_special, Special::NoMod, "{field} is SPC_NOMOD");
    }
}

/// The boundary the whole rule turns on: `special(SPC_DBADDR)` is what makes
/// C's `cvt_dbaddr` run and overwrite `paddr->field_type`
/// (`dbAccess.c:640-647`), so a `DBF_NOACCESS` row that has it is READABLE and
/// one that does not is not. `waveform.VAL` and `dbCommon.TIME` are the two
/// sides, and both are `DBF_NOACCESS`.
#[test]
fn spc_dbaddr_is_what_separates_a_readable_no_access_row_from_an_unreadable_one() {
    let wf_val = desc("waveform", "VAL").expect("waveform.VAL");
    assert!(wf_val.no_access(), "waveform.VAL is declared DBF_NOACCESS");
    assert_eq!(wf_val.declared_special, Special::DbAddr);
    assert!(
        !wf_val.unreadable(),
        "cvt_dbaddr re-types it, so it passes dbGet's gate"
    );

    let time = desc("ai", "TIME").expect("ai.TIME");
    assert!(time.no_access() && time.unreadable());
}

/// A row whose `extra(...)` names an ARRAY has no width the C type alone
/// gives — `calc.RPCL` is `extra("char rpcl[INFIX_TO_POSTFIX_SIZE(160)]")`,
/// whose size is that macro expanded — so it is not carried at all and stays a
/// bare name. Reading only the leading `char` would have carried it at one
/// byte.
#[test]
fn an_extra_naming_an_array_is_not_carried() {
    for (record_type, field) in [("calc", "RPCL"), ("calcout", "ORPC")] {
        assert!(
            desc(record_type, field).is_none(),
            "{record_type}.{field} has no width this port can state"
        );
    }
}

/// The unreadable half, at the boundary that matters to a CA client: the read
/// FAILS, and it fails differently from a name that does not exist. A
/// `ChannelNotFound` here is what let `caget REC.TIME` be reported as a typo;
/// an `Ok` here is what let it connect and read a zero C cannot produce.
#[epics_macros_rs::epics_test]
async fn a_declared_no_access_field_refuses_the_read_without_claiming_to_be_absent() {
    let db = PvDatabase::new();
    db.add_record("T:AI", Box::new(AiRecord::new(7.0)))
        .await
        .unwrap();

    for field in ["TIME", "BKPT"] {
        let name = format!("T:AI.{field}");

        // Present: the record instance resolves the descriptor...
        assert!(desc("ai", field).is_some(), "{name} is declared");
        // ...and unreadable: the READ is what fails.
        assert!(
            db.get_record("T:AI")
                .expect("T:AI")
                .read()
                .resolve_field(field)
                .is_none(),
            "{name} must not synthesise a value"
        );
        assert!(
            matches!(db.get_pv(&name), Err(CaError::BadDbrType(_))),
            "{name} must fail the READ, not report a missing channel; got {:?}",
            db.get_pv(&name)
        );
    }

    // An undeclared field and a missing record keep C's not-found answer, so
    // the two outcomes stay distinguishable.
    for name in ["T:AI.NOSUCH", "T:NOPE.VAL"] {
        assert!(
            matches!(db.get_pv(name), Err(CaError::ChannelNotFound(_))),
            "{name} resolves to nothing, got {:?}",
            db.get_pv(name)
        );
    }

    // Controls that must keep reading: `UTAG` is the DBF_UINT64 dbCommon field
    // sitting beside TIME, and VAL is the record's own.
    assert_eq!(db.get_pv("T:AI.VAL").unwrap(), EpicsValue::Double(7.0));
    assert_eq!(db.get_pv("T:AI.UTAG").unwrap(), EpicsValue::UInt64(0));
}

/// The SEARCH is still answered, and that is not the same assertion as the read
/// failing: C's `dbNameToAddr` resolves the name, so a CA client gets a reply
/// and the refusal lands at channel CREATE (measured against `softIocPVX`:
/// `pvxget ORACLE:AI.MLOK` -> `Refused to create Channel`).
///
/// Worth pinning separately because the arm that answers it MOVED. `BKPT` and
/// `TIME` used to answer off `DB_COMMON_NOACCESS`, the name list; carrying
/// their descriptors took them out of it, and `has_name` now answers off
/// `field_desc`. A row still in the name list (`MLOK`) is here as the control,
/// so the test fails if either arm is lost rather than only if both are.
#[epics_macros_rs::epics_test]
async fn the_search_for_an_unreadable_field_is_still_answered() {
    let db = PvDatabase::new();
    db.add_record("T:AI", Box::new(AiRecord::new(7.0)))
        .await
        .unwrap();

    // Answered off the carried descriptor.
    for field in ["TIME", "BKPT"] {
        let name = format!("T:AI.{field}");
        assert!(desc("ai", field).is_some(), "{name} answers off field_desc");
        assert!(db.has_name(&name).await, "{name} must answer the SEARCH");
    }
    // Answered off the name list, which those two have left.
    assert!(
        db.has_name("T:AI.MLOK").await,
        "MLOK must answer the SEARCH"
    );
    assert!(
        desc("ai", "MLOK").is_none(),
        "MLOK has no carried descriptor"
    );

    // The refusal is still a refusal: a name that does not exist gets no reply.
    assert!(!db.has_name("T:AI.NOSUCH").await);
    assert!(!db.has_name("T:NOPE.VAL").await);
}

/// The closure, over every record type the port builds rather than over `ai`:
/// the set of fields `dbGet` refuses is EXACTLY the set the `.dbd` declares
/// unreadable, and the declaration is answered by either of two routes.
///
/// Two routes because the codegen carries a `DBF_NOACCESS` row's descriptor
/// only when its `extra(...)` names a plain scalar whose width is stateable;
/// everything else survives as a bare name in `record_noaccess_fields` /
/// `DB_COMMON_NOACCESS`. Per-record tests cannot see a route being lost — the
/// two-way set difference below is what catches a row that stopped being
/// refused (readable garbage on the wire) or started being refused without a
/// declaration (a field C serves and this port would not).
#[epics_macros_rs::epics_test]
async fn the_refused_set_is_exactly_the_declared_set_across_every_record_type() {
    let db = PvDatabase::new();
    let mut refused = BTreeSet::new();
    let mut declared = BTreeSet::new();
    let (mut by_name, mut by_desc) = (BTreeSet::new(), BTreeSet::new());

    for &rt in dbd_generated::RECORD_TYPES {
        // Not every declared type has a factory in this build; the ones that
        // do are the ones a client can reach.
        let Ok(record) = module_records::create_any(rt) else {
            continue;
        };
        let name = format!("P:{rt}");
        db.add_record(&name, record).await.unwrap();
        let Some(order) = dbd_generated::record_declaration_order(rt) else {
            continue;
        };
        for field in order {
            let key = format!("{rt}.{field}");
            if matches!(
                db.get_pv(&format!("{name}.{field}")),
                Err(CaError::BadDbrType(_))
            ) {
                refused.insert(key.clone());
            }
            let own = dbd_generated::record_noaccess_fields(rt).unwrap_or(&[]);
            if own.contains(field) || dbd_generated::DB_COMMON_NOACCESS.contains(field) {
                by_name.insert(key.clone());
                declared.insert(key.clone());
            }
            if declared_field(rt, field).is_some_and(|d| d.unreadable()) {
                by_desc.insert(key.clone());
                declared.insert(key);
            }
        }
    }

    assert_eq!(
        refused.difference(&declared).collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "refused without a DBF_NOACCESS declaration"
    );
    assert_eq!(
        declared.difference(&refused).collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "declared DBF_NOACCESS and still readable"
    );

    // Both routes carry rows, so losing either one fails here rather than
    // quietly halving the refused set. `BKPT`/`TIME` moved from the first to
    // the second when their descriptors started being carried; `MLOK`, whose
    // `extra(...)` names a pointer, stayed.
    assert!(by_name.contains("ai.MLOK"), "name route: {}", by_name.len());
    assert!(by_desc.contains("ai.TIME"), "desc route: {}", by_desc.len());
    assert!(!by_name.contains("ai.TIME"), "TIME left the name list");
    assert!(
        !by_desc.contains("ai.MLOK"),
        "MLOK has no carried descriptor"
    );
}

/// A `DBF_NOACCESS` row that IS re-typed still reads, so the gate above did not
/// close the servable case with it.
#[epics_macros_rs::epics_test]
async fn a_cvt_dbaddr_retyped_no_access_field_still_reads() {
    use epics_base_rs::server::records::waveform::WaveformRecord;

    let db = PvDatabase::new();
    db.add_record(
        "T:WF",
        Box::new(WaveformRecord::new(4, DbFieldType::Double)),
    )
    .await
    .unwrap();
    db.put_pv_and_post("T:WF", EpicsValue::DoubleArray(vec![1.0, 2.0]))
        .await
        .unwrap();

    assert!(
        db.get_pv("T:WF.VAL").is_ok(),
        "waveform.VAL is DBF_NOACCESS but cvt_dbaddr re-types it"
    );
}
