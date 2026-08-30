//! `special(SPC_NOMOD)` on the asyn record, on every runtime write route.
//!
//! `asynRecord.dbd` marks twelve fields `special(SPC_NOMOD)` — OPTR, OMAX,
//! AINP, TINP, IPTR, IMAX, NORD, EOMR, I32INP, UI32INP, F64INP, SPR — and
//! `cvt_dbaddr` promotes a thirteenth at name-resolution time,
//! `paddr->special = SPC_NOMOD` for ERRS (asynRecord.c:956-962). C refuses a
//! write to any of them in ONE place: `dbPut` runs `dbPutSpecial(paddr, 0)`,
//! whose first act is `if ((special == SPC_NOMOD) && (pass == 0)) return
//! S_db_noMod` (dbAccess.c:122-127). Every runtime entry lands there —
//! `dbPutField` calls `dbPut` (dbAccess.c:1262), so a CA put, `dbpf`, an OUT
//! link and an autosave `reboot_restore` are all refused by the same test.
//!
//! The port's gate is `field_io::check_no_mod`, and its `dbPutField`-analogue
//! (`put_pv_no_process`, the autosave restore route) was not calling it: the
//! `dbPut` bodies refused these twelve and the restore route wrote them. The
//! record's own `put_field` is deliberately NOT the gate — `dbLoadRecords`
//! goes through it, and C's load route is `dbStaticLib`'s `dbPutString`, which
//! never crosses `dbPut` and so never sees `special`. asynRecord.db relies on
//! exactly that: `field(OMAX,"$(OMAX)")` and `field(IMAX,"$(IMAX)")` are
//! SPC_NOMOD fields set from the `.db`. Both halves are pinned below.
//!
//! OPTR and IPTR are `DBF_NOACCESS` and have no accessible field at all; the
//! remaining eleven are the ones a client can name.

use std::collections::HashMap;

use epics_base_rs::server::database::{PvDatabase, RecordLoad};
use epics_base_rs::server::db_loader::{apply_fields, create_record, parse_db};
use epics_base_rs::types::EpicsValue;

/// The `special(SPC_NOMOD)` fields a client can name — the dbd's twelve less
/// `DBF_NOACCESS` OPTR/IPTR, plus the ERRS that `cvt_dbaddr` promotes.
const NO_MOD: &[&str] = &[
    "OMAX", "AINP", "TINP", "IMAX", "NORD", "EOMR", "I32INP", "UI32INP", "F64INP", "SPR", "ERRS",
];

async fn loaded(db_text: &str) -> PvDatabase {
    asyn_rs::asyn_record::register_asyn_record_type();
    let db = PvDatabase::new();
    for def in parse_db(db_text, &HashMap::new()).unwrap() {
        let mut rec = create_record(&def.record_type).unwrap();
        let mut common = Vec::new();
        apply_fields(&mut rec, &def.fields, &mut common).unwrap();
        db.add_loaded_record(&def.name, rec, RecordLoad::from_common_fields(common))
            .await
            .unwrap();
    }
    db
}

#[epics_base_rs::epics_test]
async fn no_runtime_route_writes_an_spc_nomod_field() {
    // The `.db` half, first: C's load route does not consult `special`, and
    // asynRecord.db sets both of these.
    let db = loaded(
        r#"
record(asyn, "NOMOD:ASYN") {
    field(OMAX, "200")
    field(IMAX, "300")
}
"#,
    )
    .await;
    assert_eq!(
        db.get_pv("NOMOD:ASYN.OMAX").unwrap(),
        EpicsValue::Long(200),
        "asynRecord.db's field(OMAX,...) must still load"
    );
    assert_eq!(
        db.get_pv("NOMOD:ASYN.IMAX").unwrap(),
        EpicsValue::Long(300),
        "asynRecord.db's field(IMAX,...) must still load"
    );

    for field in NO_MOD {
        let pv = format!("NOMOD:ASYN.{field}");
        let before = db.get_pv(&pv).unwrap();

        // `dbPut` / `dbPutField` with process — already gated.
        let put = db.put_pv(&pv, EpicsValue::Long(7)).await;
        assert!(
            matches!(put, Err(epics_base_rs::error::CaError::ReadOnlyField(_))),
            "{field}: dbPut must answer S_db_noMod, got {put:?}"
        );

        // `dbPutField` without process — the autosave restore route.
        let restore = db.put_pv_no_process(&pv, EpicsValue::Long(7)).await;
        assert!(
            matches!(
                restore,
                Err(epics_base_rs::error::CaError::ReadOnlyField(_))
            ),
            "{field}: a restore must answer S_db_noMod, got {restore:?}"
        );

        assert_eq!(
            db.get_pv(&pv).unwrap(),
            before,
            "{field}: a refused write must leave the field alone"
        );
    }
}

/// The CA wire route (`dbPutField` → `dbPut`) and the QSRV/PVA precondition
/// gate, for the same set.
///
/// `dbChannelSpecial(chan) == SPC_NOMOD` has TWO consumers in C, and the
/// second is `rsrvCheckPut` (camessage.c:2540-2551), which feeds the CA
/// `ACCESS_RIGHTS` write bit: a client is told read-only up front and never
/// sends the doomed put. Enforcing a field's immutability inside the record's
/// own `put_field` answers the first consumer only — the put is refused, but
/// after the client has already sent it. Both consumers read the DECLARATION,
/// so the declaration is what every one of these fields must carry.
#[epics_base_rs::epics_test]
async fn the_declaration_owner_answers_for_every_spc_nomod_field() {
    let db = loaded(
        r#"
record(asyn, "DECL:ASYN") {
}
"#,
    )
    .await;

    for field in NO_MOD {
        let gate = db
            .check_external_put_preconditions("DECL:ASYN", field)
            .await;
        assert!(
            matches!(gate, Err(epics_base_rs::error::CaError::ReadOnlyField(_))),
            "{field}: the access-rights gate must answer read-only, got {gate:?}"
        );

        // The CA wire put itself — `dbPutField` → `dbPut` → `dbPutSpecial`.
        let wire = db
            .put_record_field_from_ca_no_notify("DECL:ASYN", field, EpicsValue::Long(7))
            .await;
        assert!(
            matches!(wire, Err(epics_base_rs::error::CaError::ReadOnlyField(_))),
            "{field}: a CA put must answer S_db_noMod, got {wire:?}"
        );
    }
}
