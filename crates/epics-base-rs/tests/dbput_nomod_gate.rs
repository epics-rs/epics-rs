//! R16-79: the `SPC_NOMOD` gate lives in `dbPut`, BELOW every put route.
//!
//! C `dbAccess.c:1327-1329` (`special == SPC_ATTRIBUTE` → `S_db_noMod`) and
//! `dbAccess.c:123-126` (`dbPutSpecial` pass 0, `SPC_NOMOD` → `S_db_noMod`) sit
//! inside `dbPut` — under `dbPutField` (CA / `dbpf`) AND under `dbPutLink` (a
//! record's OUT link). A refused `dbPutLink` then raises the WRITER's alarm
//! (`dbLink.c:444-446` → `setLinkAlarm` → LINK_ALARM / INVALID_ALARM).
//!
//! softIoc (EPICS 7.0.10, linux-x86_64):
//!
//! ```text
//! record(waveform,"WF"){field(FTVL,"DOUBLE") field(NELM,"10")}
//! record(ao,"AO"){field(OUT,"WF.NELM PP")}
//!
//! dbpf AO.VAL 2   -> recGblDbaddrError: dbPut Attempt to modify noMod field PV: WF.NELM
//! dbgf WF.NELM    -> 10          (unchanged)
//! dbgf AO.STAT    -> "LINK"
//! dbgf AO.SEVR    -> "INVALID"
//! dbpf WF.NELM 4  -> refused, still 10
//! ```
//!
//! Pre-fix the port enforced `read_only` only on the CA route, so the OUT link
//! truncated NELM (and the data with it) and the writer stayed NO_ALARM.

use std::collections::HashSet;

use epics_base_rs::error::CaError;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::{AlarmSeverity, Record};
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::waveform::WaveformRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

async fn build() -> PvDatabase {
    let db = PvDatabase::new();
    let wf = WaveformRecord::new(10, DbFieldType::Double);
    db.add_record("WF", Box::new(wf)).await.unwrap();

    db.add_record("AO", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.put_record_field_from_ca_no_notify("AO", "OUT", EpicsValue::String("WF.NELM PP".into()))
        .await
        .unwrap();

    db.put_pv_and_post("WF", EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0]))
        .await
        .unwrap();
    db
}

/// The OUT link is the route C gates in `dbPut` and the port did not: a record
/// writing `WF.NELM` must be refused, the waveform must keep its NELM *and its
/// data*, and the WRITER must go LINK/INVALID.
#[epics_macros_rs::epics_test]
async fn out_link_write_to_a_nomod_field_is_refused_and_alarms_the_writer() {
    let db = build().await;

    db.put_pv("AO.VAL", EpicsValue::Double(2.0)).await.unwrap();
    let mut v = HashSet::new();
    db.process_record_with_links("AO", &mut v, 0).await.unwrap();

    let wf = db.get_record("WF").unwrap();
    {
        let wf = wf.read();
        assert_eq!(
            wf.record.get_field("NELM").unwrap(),
            EpicsValue::ULong(10),
            "C: dbPut refuses the SPC_NOMOD NELM — the link cannot truncate a waveform"
        );
        assert_eq!(
            wf.record.get_field("NORD").unwrap(),
            EpicsValue::ULong(3),
            "and the data the waveform already holds is untouched"
        );
    }

    let ao = db.get_record("AO").unwrap();
    let ao = ao.read();
    assert_eq!(
        ao.common.stat,
        alarm_status::LINK_ALARM,
        "C `dbPutLink` (dbLink.c:444-446): a failed put alarms the WRITER"
    );
    assert_eq!(ao.common.sevr, AlarmSeverity::Invalid);
}

/// The gate is in the shared `dbPut` owner, so the internal `put_pv` /
/// `put_pv_and_post` routes are refused too — not only the CA route.
#[epics_macros_rs::epics_test]
async fn every_put_route_is_refused_on_a_nomod_field() {
    let db = build().await;

    for res in [
        db.put_pv("WF.NELM", EpicsValue::Long(4)).await,
        db.put_pv_and_post("WF.NELM", EpicsValue::Long(4)).await,
        db.put_record_field_from_ca_no_notify("WF", "NELM", EpicsValue::Long(4))
            .await,
        // NORD and FTVL are SPC_NOMOD in the same `.dbd`.
        db.put_pv("WF.NORD", EpicsValue::Long(1)).await,
        db.put_pv("WF.FTVL", EpicsValue::Short(0)).await,
        // dbCommon SPC_NOMOD triple.
        db.put_pv("WF.PACT", EpicsValue::Short(1)).await,
        db.put_pv("WF.LCNT", EpicsValue::Short(1)).await,
        db.put_pv("WF.PUTF", EpicsValue::Short(1)).await,
    ] {
        assert!(
            matches!(res, Err(CaError::ReadOnlyField(_))),
            "C S_db_noMod on every route, got {res:?}"
        );
    }

    let wf = db.get_record("WF").unwrap();
    let wf = wf.read();
    assert_eq!(wf.record.get_field("NELM").unwrap(), EpicsValue::ULong(10));
    assert_eq!(wf.record.get_field("NORD").unwrap(), EpicsValue::ULong(3));
}

/// The gate is a *runtime* gate: `dbLoadRecords` sets NELM through
/// `dbStaticLib`'s `dbPutString`, which never crosses `dbPut`. The load path
/// (`Record::put_field`) must therefore still size the array.
#[epics_macros_rs::epics_test]
async fn the_load_path_still_sets_nelm() {
    let db = PvDatabase::new();
    let mut wf = WaveformRecord::new(1, DbFieldType::Double);
    wf.put_field("NELM", EpicsValue::Long(7)).unwrap();
    db.add_record("WF2", Box::new(wf)).await.unwrap();

    let rec = db.get_record("WF2").unwrap();
    assert_eq!(
        rec.read().record.get_field("NELM").unwrap(),
        EpicsValue::ULong(7)
    );
}

/// R17-62: the gate's DECLARATION must be C's whole `dbCommon.dbd` `SPC_NOMOD`
/// set, not the PACT/LCNT/PUTF triple. Every one of these was client-writable —
/// alarm state (STAT/SEVR/NSEV/ACKS) was forgeable, and on a Passive record the
/// forged alarm is permanent.
///
/// softIoc (EPICS 7.0.10, `record(ai,"N1")`), every put silently refused, field
/// unchanged:
///
/// ```text
/// dbpf N1.SEVR 2  -> "NO_ALARM"   dbpf N1.STAT 3   -> "UDF"
/// dbpf N1.NSEV 2  -> "NO_ALARM"   dbpf N1.NSTA 3   -> "NO_ALARM"
/// dbpf N1.ACKS 2  -> "NO_ALARM"   dbpf N1.ACKT 0   -> "YES"
/// dbpf N1.RPRO 1  -> 0            dbpf N1.UTAG 7   -> 0
/// dbpf N1.NAME XX -> "N1"         dbpf N1.AMSG hi  -> ""
/// dbpf N1.NAMSG hi-> ""           dbpf N1.LCNT 3   -> 0
/// caput N1.SEVR 2 -> ERROR from put operation: Write access denied
/// ```
#[epics_macros_rs::epics_test]
async fn every_dbcommon_nomod_field_is_refused_on_every_route() {
    let db = PvDatabase::new();
    db.add_record("AO2", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    let (stat0, sevr0) = {
        let rec = db.get_record("AO2").unwrap();
        let inst = rec.read();
        (inst.common.stat, inst.common.sevr)
    };

    let fields: [(&str, EpicsValue); 13] = [
        ("NAME", EpicsValue::String("XX".into())),
        ("STAT", EpicsValue::Short(3)),
        ("SEVR", EpicsValue::Short(2)),
        ("AMSG", EpicsValue::String("hi".into())),
        ("NSTA", EpicsValue::Short(3)),
        ("NSEV", EpicsValue::Short(2)),
        ("NAMSG", EpicsValue::String("hi".into())),
        ("ACKS", EpicsValue::Short(2)),
        ("ACKT", EpicsValue::Short(0)),
        ("LCNT", EpicsValue::Short(3)),
        ("RPRO", EpicsValue::Char(1)),
        ("UTAG", EpicsValue::Double(7.0)),
        ("TIME", EpicsValue::Double(3.0)),
    ];

    for (field, value) in fields {
        for res in [
            db.put_record_field_from_ca_no_notify("AO2", field, value.clone())
                .await,
            db.put_pv(&format!("AO2.{field}"), value.clone()).await,
            db.put_pv_and_post(&format!("AO2.{field}"), value.clone())
                .await,
            db.check_external_put_preconditions("AO2", field).await,
        ] {
            assert!(
                matches!(res, Err(CaError::ReadOnlyField(_))),
                "{field}: C refuses with S_db_noMod on every route, got {res:?}"
            );
        }
    }

    // Nothing landed: the alarm state a client tried to forge is still clean.
    let rec = db.get_record("AO2").unwrap();
    let inst = rec.read();
    assert_eq!(inst.common.sevr, sevr0, "forged SEVR never landed");
    assert_eq!(inst.common.stat, stat0, "forged STAT never landed");
    assert_eq!(inst.common.acks, AlarmSeverity::NoAlarm);
    assert!(
        inst.common.ackt,
        "ACKT default YES survives the refused put"
    );
    assert!(inst.common.rpro == 0);
    assert_eq!(inst.common.utag, 0);
    assert_eq!(inst.name, "AO2");
}

/// The other half of R17-62: refusing the ACKS/ACKT *fields* must not break
/// alarm acknowledgement, because C never acknowledged through those fields.
/// `dbPut` dispatches on the DBR request type ABOVE the gate
/// (`dbAccess.c:1328-1332`), so the ack route stays open.
///
/// softIoc (`record(ai,"N1"){field(HIGH,"1") field(HSV,"MAJOR")}`, VAL=5 →
/// SEVR=MAJOR, ACKS=MAJOR):
///
/// ```text
/// ca_put(DBR_PUT_ACKS, "N1", 2) -> Normal successful completion
/// caget N1.ACKS                 -> NO_ALARM      (acknowledged)
/// caget N1.SEVR                 -> MAJOR         (the alarm itself stays)
/// caput N1.ACKS 2               -> ERROR: Write access denied
/// ```
#[epics_macros_rs::epics_test]
async fn alarm_acknowledge_travels_the_dbr_type_route_not_the_field() {
    use epics_base_rs::server::record::AlarmAck;

    let db = PvDatabase::new();
    db.add_record("ACK:AI", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    {
        let rec = db.get_record("ACK:AI").unwrap();
        let mut inst = rec.write();
        inst.common.sevr = AlarmSeverity::Major;
        inst.common.acks = AlarmSeverity::Major;
    }

    // The field put is refused (S_db_noMod) — ACKS is unchanged.
    assert!(matches!(
        db.put_record_field_from_ca_no_notify("ACK:AI", "ACKS", EpicsValue::Short(2))
            .await,
        Err(CaError::ReadOnlyField(_))
    ));
    let rec = db.get_record("ACK:AI").unwrap();
    assert_eq!(rec.read().common.acks, AlarmSeverity::Major);

    // A MINOR acknowledgement is too low: C's `*psev >= precord->acks` fails.
    db.put_alarm_ack_from_ca("ACK:AI", "VAL", AlarmAck::Severity, 1)
        .await
        .unwrap();
    assert_eq!(rec.read().common.acks, AlarmSeverity::Major);

    // A MAJOR acknowledgement clears ACKS and leaves SEVR alone.
    db.put_alarm_ack_from_ca("ACK:AI", "VAL", AlarmAck::Severity, 2)
        .await
        .unwrap();
    let inst = rec.read();
    assert_eq!(inst.common.acks, AlarmSeverity::NoAlarm);
    assert_eq!(inst.common.sevr, AlarmSeverity::Major);
}
