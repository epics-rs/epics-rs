//! R11-62 — swait's simulation mode (SIML / SIMM / SIOL / SIMS / SVAL).
//!
//! C `swaitRecord.c:401-421`:
//!
//! ```c
//! /* Check for simulation mode */
//! status = dbGetLink(&(pwait->siml), DBR_ENUM, &(pwait->simm), 0, 0);
//! ...
//! if (pwait->simm == menuYesNoNO) {
//!     if (fetch_values(pwait)==0) {
//!         if (calcPerform(&pwait->a,&pwait->val,pwait->rpcl)) ...
//!     } else recGblSetSevr(pwait,READ_ALARM,INVALID_ALARM);
//! } else {      /* SIMULATION MODE */
//!     status = dbGetLink(&(pwait->siol),DBR_DOUBLE,&(pwait->sval),0,0);
//!     if (status==0) {
//!         pwait->val=pwait->sval;
//!         pwait->udf=FALSE;
//!     }
//!     recGblSetSevr(pwait,SIMM_ALARM,pwait->sims);
//! }
//! ```
//!
//! The five fields (`swaitRecord.dbd:497-517`) did not exist in the port, so a
//! swait could not be simulated at all. The shape matters as much as the fields:
//! the simulation branch substitutes `fetch_values()` + `calcPerform()` and
//! NOTHING else — the OOPT switch at `:424`, `execOutput`, the monitors and the
//! forward link all still run. It is not the whole-cycle `readValue` of ai/bi.

//! Widened site — `busy`: `busyRecord.dbd:127-147` declares the same
//! SIML/SIMM/SIOL/SIMS group and `busyRecord.c:389-416` is the bo-shaped OUTPUT
//! redirect. The port omitted the fields there too, so `check_simulation_mode`
//! saw an unconfigured record and a simulated busy drove its real output.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{AlarmSeverity, Record};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::types::EpicsValue;

const SIMM_ALARM: u16 = 19; // alarm.h — the STAT a simulated cycle raises
const READ_ALARM: u16 = 1;
const LINK_ALARM: u16 = 14; // alarm.h — C `setLinkAlarm` (dbLink.c:320)

/// W: CALC = "A+1" over INAN = SRC (10.0), OUT → DEST, OOPT = Every Time.
/// SIOL reads SIM (42.0); `siml` and `simm` per argument.
async fn sim_db(siml: &str, simm: i16, sims: i16, siol: &str) -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AiRecord::new(10.0)))
        .await
        .unwrap();
    db.add_record("SIM", Box::new(AiRecord::new(42.0)))
        .await
        .unwrap();
    db.add_record("MODE", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("DEST", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    let mut w = SwaitRecord::default();
    w.put_field("CALC", EpicsValue::String("A+1".into()))
        .unwrap();
    w.put_field("INAN", EpicsValue::String("SRC".into()))
        .unwrap();
    w.put_field("SIOL", EpicsValue::String(siol.into()))
        .unwrap();
    w.put_field("SIML", EpicsValue::String(siml.into()))
        .unwrap();
    w.put_field("SIMM", EpicsValue::Short(simm)).unwrap();
    w.put_field("SIMS", EpicsValue::Short(sims)).unwrap();
    db.add_record("W", Box::new(w)).await.unwrap();

    // OUT routes through RecordInstance::put_common_field (populates parsed_out).
    db.get_record("W")
        .unwrap()
        .write()
        .put_common_field("OUT", EpicsValue::String("DEST".into()))
        .unwrap();
    db
}

async fn process(db: &PvDatabase) {
    let mut visited = HashSet::new();
    db.process_record_with_links("W", &mut visited, 0)
        .await
        .unwrap();
}

async fn field(db: &PvDatabase, rec: &str, f: &str) -> f64 {
    let inst = db.get_record(rec).unwrap();
    let g = inst.read();
    g.record.get_field(f).unwrap().to_f64().unwrap()
}

async fn alarm(db: &PvDatabase) -> (AlarmSeverity, u16, bool) {
    let inst = db.get_record("W").unwrap();
    let g = inst.read();
    (g.common.sevr, g.common.stat, g.common.udf != 0)
}

/// The record's committed alarm message — C's `amsg`, written by
/// `recGblSetSevrMsg` (here: `setLinkAlarm`'s "field %s").
async fn amsg(db: &PvDatabase) -> String {
    let inst = db.get_record("W").unwrap();
    let g = inst.read();
    g.common.amsg.clone()
}

/// SIMM = YES: VAL comes from SIOL through SVAL, the calc does not run, and the
/// cycle carries SIMM_ALARM at SIMS.
#[epics_macros_rs::epics_test]
async fn r11_62_simm_yes_reads_siol_into_sval_and_val() {
    let db = sim_db("", 1, 2 /* MAJOR */, "SIM").await;

    process(&db).await;

    assert_eq!(
        field(&db, "W", "SVAL").await,
        42.0,
        "SIOL read lands in SVAL"
    );
    assert_eq!(
        field(&db, "W", "VAL").await,
        42.0,
        "C:418 — val = sval, NOT the calc result (which would be A+1 = 11)"
    );
    assert_eq!(
        field(&db, "W", "A").await,
        0.0,
        "C:415 — the simulation branch never calls fetch_values(), so A is not read"
    );

    let (sevr, stat, udf) = alarm(&db).await;
    assert_eq!(sevr, AlarmSeverity::Major, "C:421 — SIMM_ALARM at SIMS");
    assert_eq!(stat, SIMM_ALARM);
    assert!(!udf, "C:419 — udf = FALSE on a successful SIOL read");
}

/// The shape: swait's simulation substitutes the input stage only. The OOPT
/// switch and `execOutput` still run, so the simulated VAL is written out
/// through OUT. (A whole-cycle `readValue` simulation — ai/bi — would have
/// skipped the body and left DEST alone.)
#[epics_macros_rs::epics_test]
async fn r11_62_a_simulated_cycle_still_drives_the_output_link() {
    let db = sim_db("", 1, 0, "SIM").await;

    process(&db).await;

    assert_eq!(
        field(&db, "DEST", "VAL").await,
        42.0,
        "C:424 — the OOPT switch is outside the simulation branch; execOutput \
         writes the simulated VAL"
    );
}

/// SIML resolves SIMM on every process (C `:402`), so the mode can be driven
/// from another PV — and driving it back to NO restores the real calc.
#[epics_macros_rs::epics_test]
async fn r11_62_siml_refreshes_simm_every_cycle() {
    let db = sim_db("MODE", 0, 0, "SIM").await;

    // MODE = 0 (NO): the real calc runs.
    process(&db).await;
    assert_eq!(field(&db, "W", "SIMM").await, 0.0);
    assert_eq!(
        field(&db, "W", "VAL").await,
        11.0,
        "SIMM=NO — CALC 'A+1' over the fetched A = 10"
    );

    // MODE = 1 (YES): the next cycle is simulated.
    db.put_pv("MODE", EpicsValue::Double(1.0)).await.unwrap();
    process(&db).await;
    assert_eq!(
        field(&db, "W", "SIMM").await,
        1.0,
        "C:402 — dbGetLink(SIML) refreshes SIMM"
    );
    assert_eq!(field(&db, "W", "VAL").await, 42.0);

    // Back to NO: the calc runs again, and the SIMM alarm clears.
    db.put_pv("MODE", EpicsValue::Double(0.0)).await.unwrap();
    process(&db).await;
    assert_eq!(field(&db, "W", "VAL").await, 11.0);
    let (sevr, stat, _) = alarm(&db).await;
    assert_eq!(sevr, AlarmSeverity::NoAlarm);
    assert_eq!(stat, 0, "the SIMM alarm is per-cycle, not sticky");
}

/// W10-E4. A SIOL read that fails changes neither VAL nor UDF (C `:417` gates
/// both on `status == 0`) — but it is a plain `dbGetLink`, so its failure path
/// runs `setLinkAlarm` (dbLink.c:322) and raises LINK_ALARM/INVALID with
/// AMSG "field SIOL".
///
/// `recGblSetSevr(SIMM_ALARM, sims)` at `:420` then runs unconditionally, but it
/// only ever RAISES (strict-greater): with `SIMS = MINOR` it cannot beat the
/// INVALID already pending, so it is a no-op and the record publishes
/// LINK_ALARM/INVALID. Compiled C (`recGblSetSevrVMsg` verbatim, swait's order —
/// `dbGetLink` at :416 then `recGblSetSevr` at :420):
///
/// ```text
/// swait SIMS=MINOR, failed SIOL: nsev=3 nsta=14 namsg='field SIOL'
/// ```
///
/// The port used to raise ONLY the SIMM_ALARM here, publishing MINOR/SIMM.
#[epics_macros_rs::epics_test]
async fn w10_e4_a_failed_siol_read_raises_link_alarm() {
    let db = sim_db("MODE", 0, 1 /* MINOR */, "NOSUCHREC").await;

    // A real cycle first: VAL = 11 from the calc.
    process(&db).await;
    assert_eq!(field(&db, "W", "VAL").await, 11.0);

    db.put_pv("MODE", EpicsValue::Double(1.0)).await.unwrap();
    process(&db).await;

    assert_eq!(
        field(&db, "W", "VAL").await,
        11.0,
        "C:417-420 — a failed dbGetLink leaves VAL (and SVAL) alone"
    );
    assert_eq!(field(&db, "W", "SVAL").await, 0.0);
    let (sevr, stat, _) = alarm(&db).await;
    assert_eq!(
        sevr,
        AlarmSeverity::Invalid,
        "setLinkAlarm raises INVALID; SIMS=MINOR cannot beat it (strict-greater)"
    );
    assert_eq!(stat, LINK_ALARM, "STAT is LINK, not SIMM");
    assert_eq!(
        amsg(&db).await,
        "field SIOL",
        "C `setLinkAlarm`: \"field %s\""
    );
}

/// The other side of W10-E4's ordering boundary on swait: with
/// `SIMS = INVALID` the two alarms are EQUAL in severity, and swait raises
/// LINK first (`dbGetLink` at `:416`) and SIMM second (`:420`) — so the
/// strict-greater `recGblSetSevr` leaves LINK in place. (A base record reverses
/// this: `longinRecord.c:414` raises SIMM BEFORE its read, so SIMM wins there.
/// Same two alarms, opposite winner, decided purely by C's call order.)
#[epics_macros_rs::epics_test]
async fn w10_e4_swait_link_alarm_wins_the_tie_at_sims_invalid() {
    let db = sim_db("MODE", 0, 3 /* INVALID */, "NOSUCHREC").await;

    db.put_pv("MODE", EpicsValue::Double(1.0)).await.unwrap();
    process(&db).await;

    let (sevr, stat, _) = alarm(&db).await;
    assert_eq!(sevr, AlarmSeverity::Invalid);
    assert_eq!(
        stat, LINK_ALARM,
        "swait raises LINK before SIMM, so the equal-severity SIMM loses the tie"
    );
    assert_eq!(amsg(&db).await, "field SIOL");
}

/// Negative control: with SIMM = NO the record is untouched — the inputs are
/// fetched, the calc runs, and no SIMM alarm is raised. The failing-input gate
/// (READ_ALARM, C `:413`) belongs to the real branch...
#[epics_macros_rs::epics_test]
async fn r11_62_simm_no_runs_the_real_calc_and_its_read_gate() {
    let db = sim_db("MODE", 0, 1, "SIM").await;

    process(&db).await;
    assert_eq!(field(&db, "W", "VAL").await, 11.0, "CALC 'A+1' over A = 10");
    let (sevr, stat, _) = alarm(&db).await;
    assert_eq!(sevr, AlarmSeverity::NoAlarm);
    assert_eq!(stat, 0);

    // Break the input link: C's non-simulated branch raises READ_ALARM/INVALID.
    db.put_record_field_from_ca("W", "INAN", EpicsValue::String("NOSUCHREC".into()))
        .await
        .unwrap();
    process(&db).await;
    let (sevr, stat, _) = alarm(&db).await;
    assert_eq!(sevr, AlarmSeverity::Invalid);
    assert_eq!(stat, READ_ALARM, "C:413 — the fetch gate, not SIMM");

    // ...and NOT to the simulated one: the simulation branch runs no
    // fetch_values(), so the same broken input link raises no READ_ALARM. Only
    // SIMM_ALARM at SIMS survives, and the stale INVALID does not leak into it.
    db.put_pv("MODE", EpicsValue::Double(1.0)).await.unwrap();
    process(&db).await;
    let (sevr, stat, _) = alarm(&db).await;
    assert_eq!(sevr, AlarmSeverity::Minor);
    assert_eq!(stat, SIMM_ALARM);
    assert_eq!(field(&db, "W", "VAL").await, 42.0);
}

/// The widened site. `busy` carries the same C simulation group and the same
/// omission; its shape is the OUTPUT redirect the framework already owns, so
/// declaring the fields is the whole fix: SIMM=YES sends VAL to SIOL instead of
/// OUT, and the cycle carries SIMM_ALARM at SIMS.
#[epics_macros_rs::epics_test]
async fn r11_62_busy_simm_yes_redirects_the_output_to_siol() {
    let db = PvDatabase::new();
    db.add_record("B_OUT", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("B_SIM", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    let mut b = BusyRecord::default();
    b.put_field("SIOL", EpicsValue::String("B_SIM".into()))
        .unwrap();
    b.put_field("SIMS", EpicsValue::Short(1)).unwrap(); // MINOR
    b.put_field("SIMM", EpicsValue::Short(1)).unwrap(); // YES
    b.put_field("VAL", EpicsValue::Enum(1)).unwrap();
    db.add_record("B", Box::new(b)).await.unwrap();
    {
        let inst = db.get_record("B").unwrap();
        let mut w = inst.write();
        w.put_common_field("OUT", EpicsValue::String("B_OUT".into()))
            .unwrap();
        // busy is `clears_udf() == false` (busyRecord.c:195-208), so a bare
        // record stays UDF and its INVALID would mask the SIMM_ALARM/SIMS under
        // test. VAL was defined above; clear UDF as a real dbPut to VAL does.
        w.put_common_field("UDF", EpicsValue::Char(0)).unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("B", &mut visited, 0)
        .await
        .unwrap();

    assert_eq!(
        field(&db, "B_SIM", "VAL").await,
        1.0,
        "busyRecord.c:409 — dbPutLink(&siol, &val)"
    );
    assert_eq!(
        field(&db, "B_OUT", "VAL").await,
        0.0,
        "the real output is NOT driven on a simulated cycle"
    );

    let inst = db.get_record("B").unwrap();
    let g = inst.read();
    assert_eq!(g.common.sevr, AlarmSeverity::Minor, "busyRecord.c:414");
    assert_eq!(g.common.stat, SIMM_ALARM);
}

/// Negative control for the widened site: SIMM = NO drives the real OUT link and
/// raises no SIMM alarm.
#[epics_macros_rs::epics_test]
async fn r11_62_busy_simm_no_drives_the_real_output() {
    let db = PvDatabase::new();
    db.add_record("B2_OUT", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("B2_SIM", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    let mut b = BusyRecord::default();
    b.put_field("SIOL", EpicsValue::String("B2_SIM".into()))
        .unwrap();
    b.put_field("VAL", EpicsValue::Enum(1)).unwrap();
    db.add_record("B2", Box::new(b)).await.unwrap();
    {
        let inst = db.get_record("B2").unwrap();
        let mut w = inst.write();
        w.put_common_field("OUT", EpicsValue::String("B2_OUT".into()))
            .unwrap();
        // See r11_62_busy_simm_yes: busy stays UDF without an explicit clear
        // (clears_udf() == false); VAL was defined above, so clear UDF to assert
        // the NoAlarm the SIMM=NO negative control expects.
        w.put_common_field("UDF", EpicsValue::Char(0)).unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("B2", &mut visited, 0)
        .await
        .unwrap();

    assert_eq!(field(&db, "B2_OUT", "VAL").await, 1.0);
    assert_eq!(field(&db, "B2_SIM", "VAL").await, 0.0);
    let inst = db.get_record("B2").unwrap();
    assert_eq!(inst.read().common.sevr, AlarmSeverity::NoAlarm);
}
