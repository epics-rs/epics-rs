//! orient reads motPut_Auto and motGet_Auto as C's monitored variables,
//! written as each monitor arrives (orient_st.st:132, :142, no efflag).

use epics_base_rs::runtime::task;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::server::records::transform::TransformRecord;
use epics_base_rs::server::scan::ScanOwner;
use epics_base_rs::types::EpicsValue;
use epics_seq_rs::{SeqThreadId, seq, seq_stop};
use optics_rs::snl::orient::orient;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

const ORIENT_DB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/db/orient.db");
const MOTORS: &str = r#"
record(motor, "X:m:tth") { field(HLM, "180") field(LLM, "-180") }
record(motor, "X:m:th") { field(HLM, "180") field(LLM, "-180") }
record(motor, "X:m:chi") { field(HLM, "180") field(LLM, "-180") }
record(motor, "X:m:phi") { field(HLM, "180") field(LLM, "-180") }
record(ao, "X:m:plain") {}
"#;
const P: &str = "X:orient1:";

/// The running orient instance, stopped when the test drops it, and the
/// scan owner of its database, which drops after it.
struct Orient {
    prog: SeqThreadId,
    _scan: ScanOwner,
}

impl Drop for Orient {
    fn drop(&mut self) {
        seq_stop(self.prog);
    }
}

/// `seq(&orient, ...)` with `X:m:<phi>` as the PHI motor.
fn start(ioc: &Arc<PvDatabase>, phi: &str) -> Orient {
    // The program waits for PINI, which the scan owner's pass publishes.
    let scan = ScanOwner::start(ioc.clone());
    let macros = format!("P={P},PM=X:m:,mTTH=tth,mTH=th,mCHI=chi,mPHI={phi}");
    Orient {
        prog: seq(&orient, &macros, 0, ioc).expect("orient starts"),
        _scan: scan,
    }
}

/// The four motors on simulated axes: a drive takes about half a second
/// and completes its put callback when it finishes.
const SIM_MOTORS: &str = r#"
record(motor, "X:m:tth") { field(DTYP, "asynMotor") field(OUT, "@asyn($(PORT)tth,0)") field(MRES, "0.0001") field(VELO, "100") field(HLM, "180") field(LLM, "-180") }
record(motor, "X:m:th") { field(DTYP, "asynMotor") field(OUT, "@asyn($(PORT)th,0)") field(MRES, "0.0001") field(VELO, "100") field(HLM, "180") field(LLM, "-180") }
record(motor, "X:m:chi") { field(DTYP, "asynMotor") field(OUT, "@asyn($(PORT)chi,0)") field(MRES, "0.0001") field(VELO, "100") field(HLM, "180") field(LLM, "-180") }
record(motor, "X:m:phi") { field(DTYP, "asynMotor") field(OUT, "@asyn($(PORT)phi,0)") field(MRES, "0.0001") field(VELO, "100") field(HLM, "180") field(LLM, "-180") }
"#;

async fn f64_of(ioc: &PvDatabase, pv: &str) -> f64 {
    ioc.get_pv(pv).unwrap().to_f64().unwrap()
}

async fn string_of(ioc: &PvDatabase, pv: &str) -> String {
    match ioc.get_pv(pv).unwrap() {
        EpicsValue::String(s) => s.to_string(),
        v => format!("{v:?}"),
    }
}

/// Boot with both of init's matrices good and wait until orient is live.
async fn boot() -> (Arc<PvDatabase>, PvDatabase, Orient) {
    let (ioc, db, prog) = boot_with(&good_inputs()).await;
    wait_live(&ioc).await;
    (ioc, db, prog)
}

/// Silicon's cubic cell; orient.db leaves the lattice at zero.
fn lattice() -> Vec<(String, EpicsValue)> {
    [
        ("a", 5.431),
        ("b", 5.431),
        ("c", 5.431),
        ("alpha", 90.0),
        ("beta", 90.0),
        ("gamma", 90.0),
    ]
    .into_iter()
    .map(|(f, v)| (format!("{P}{f}"), EpicsValue::Double(v)))
    .collect()
}

/// Boot with `preset` posted before orient starts.
async fn boot_with(preset: &[(String, EpicsValue)]) -> (Arc<PvDatabase>, PvDatabase, Orient) {
    boot_with_phi(preset, "phi").await
}

/// As `boot_with`, with `X:m:<phi>` as the PHI motor.
async fn boot_with_phi(
    preset: &[(String, EpicsValue)],
    phi: &str,
) -> (Arc<PvDatabase>, PvDatabase, Orient) {
    let builder = IocBuilder::new()
        .register_record_type("motor", || Box::new(motor_rs::MotorRecord::new()))
        .db_string(MOTORS, &HashMap::new())
        .unwrap();
    boot_on(builder, preset, phi).await
}

/// [`boot`] with the motors on simulated axes.
async fn boot_sim() -> (Arc<PvDatabase>, PvDatabase, Orient) {
    use motor_rs::builder::{AxisSetup, MotorBuilder};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static BOOTS: AtomicUsize = AtomicUsize::new(0);
    let port = format!("ORSIM{}_", BOOTS.fetch_add(1, Ordering::Relaxed));
    for m in ["tth", "th", "chi", "phi"] {
        let name = format!("{port}{m}");
        let motor: Arc<Mutex<dyn asyn_rs::interfaces::motor::AsynMotor>> = Arc::new(Mutex::new(
            motor_rs::sim_motor::SimMotor::new().with_limits(-1e9, 1e9),
        ));
        let AxisSetup {
            axis, poll_loop, ..
        } = MotorBuilder::new(motor)
            .trace_user(motor_rs::trace::TraceUser::Asyn {
                port: name.clone(),
                addr: 0,
            })
            .poll_interval(Duration::from_millis(10))
            .build_axis();
        motor_rs::devices::register_controller_port(&asyn_rs::registry::open_owner(), &name, 1, ())
            .unwrap();
        motor_rs::devices::register_axis(axis).unwrap();
        poll_loop.start();
    }
    let (name, factory) = motor_rs::motor_record_factory();
    let builder = IocBuilder::new()
        .register_record_type(name, factory)
        .register_dynamic_device_support(
            &motor_rs::devices::devices(),
            motor_rs::devices::device_support_factory(),
        )
        .db_string(SIM_MOTORS, &HashMap::from([("PORT".to_string(), port)]))
        .unwrap();
    let (ioc, db, prog) = boot_on(builder, &good_inputs(), "phi").await;
    wait_live(&ioc).await;
    (ioc, db, prog)
}

/// Load orient.db beside the motors `builder` holds, post `preset`, and
/// start orient.
async fn boot_on(
    builder: IocBuilder,
    preset: &[(String, EpicsValue)],
    phi: &str,
) -> (Arc<PvDatabase>, PvDatabase, Orient) {
    let macros: HashMap<String, String> = [("P", "X:"), ("O", "1"), ("PREC", "4")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let ioc = builder
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .register_record_type("transform", || Box::new(TransformRecord::default()))
        .db_file(ORIENT_DB, &macros)
        .unwrap()
        .build_served()
        .await
        .unwrap()
        .0;
    let db = PvDatabase::clone(&ioc);
    post(&db, preset);
    let prog = start(&ioc, phi);
    (ioc, db, prog)
}

/// Post each value as a monitor, in order. A monitored variable is read
/// from its queue when an event is taken, so it holds every post made
/// before that event's.
fn post(db: &epics_base_rs::server::database::PvDatabase, posts: &[(String, EpicsValue)]) {
    for (pv, v) in posts {
        let rec = db.get_record(pv).unwrap();
        let mut inst = rec.write();
        inst.record.put_field("VAL", v.clone()).unwrap();
        inst.notify_field("VAL", EventMask::VALUE | EventMask::LOG);
    }
}

/// A CA put to `{P}field`, processing the record as a client's would.
async fn put(db: &epics_base_rs::server::database::PvDatabase, field: &str, v: f64) {
    db.put_record_field_from_ca(format!("{P}{field}"), "VAL", EpicsValue::Double(v))
        .await
        .unwrap();
}

async fn wait_for(ioc: &PvDatabase, pv: &str, want: f64) {
    for _ in 0..100 {
        if f64_of(ioc, pv).await == want {
            return;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    panic!("{pv} = {}, want {want}", f64_of(ioc, pv).await);
}

/// The silicon (400)/(040) pair of the unit tests, posted as monitors.
fn reflections() -> Vec<(String, EpicsValue)> {
    [
        ("H1", 4.0),
        ("TTH1", 24.3414),
        ("TH1", 12.1707),
        ("K2", 4.0),
        ("TTH2", 24.3414),
        ("TH2", 12.1707),
        ("PHI2", 90.0),
    ]
    .into_iter()
    .map(|(f, v)| (format!("{P}{f}"), EpicsValue::Double(v)))
    .collect()
}

/// With both matrices good at boot, init waits 10 s in enter_monitor_all
/// and clears the H..PHI and Mode flags raised meanwhile, so an HKL written
/// then drives nothing (orient_st.st:588-621).
#[epics_base_rs::epics_test]
async fn going_live_clears_the_hkl_written_during_the_init_delay() {
    let mut preset = good_inputs();
    preset.push((format!("{P}motPut_Auto"), EpicsValue::Enum(1)));
    let (ioc, db, _orient) = boot_with(&preset).await;
    wait_for_msg(&ioc, "Successful OMTX calc").await;
    put(&db, "H", 4.0).await;
    put(&db, "K", 4.0).await;
    wait_live(&ioc).await;
    task::sleep(Duration::from_millis(300)).await;
    assert_eq!(f64_of(&ioc, "X:m:tth").await, 0.0);
}

/// Set each motor's RBV and post it.
fn set_rbvs(db: &PvDatabase, rbvs: &[(&str, f64)]) {
    for &(m, v) in rbvs {
        let rec = db.get_record(format!("X:m:{m}")).unwrap();
        let mut inst = rec.write();
        inst.record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<motor_rs::MotorRecord>())
            .unwrap()
            .pos
            .rbv = v;
        inst.notify_field("RBV", EventMask::VALUE | EventMask::LOG);
    }
}

/// ss readback's zero clause tests its own unmonitored H/K/L_RBV, which
/// start at 0, so a value put from outside stands while a matrix is not
/// good (orient_st.st:187, :193, :199, :1216-1219).
#[epics_base_rs::epics_test]
async fn a_bad_matrix_leaves_an_outside_readback_value() {
    let (ioc, _db, _orient) = boot_with(&[(format!("{P}H_RBV"), EpicsValue::Double(1.5))]).await;
    wait_for(&ioc, &format!("{P}A0_state"), 2.0).await;
    task::sleep(Duration::from_millis(500)).await;
    assert_eq!(f64_of(&ioc, &format!("{P}H_RBV")).await, 1.5);
}

/// Any of the four readbacks raises motRBV_mon, and a bad live OMTX
/// zeroes the readbacks (orient_st.st:155-158, :1215-1231).
#[epics_base_rs::epics_test]
async fn each_readback_converts_and_a_bad_omtx_zeroes_them() {
    let (ioc, db, _orient) = boot().await;
    set_rbvs(&db, &[("tth", 24.3414), ("th", 12.1707)]);
    wait_near(&ioc, &format!("{P}H_RBV"), 4.0).await;
    task::sleep(Duration::from_millis(200)).await;
    set_rbvs(&db, &[("phi", 90.0)]);
    wait_near(&ioc, &format!("{P}K_RBV"), 4.0).await;
    post(&db, &[(format!("{P}K2"), EpicsValue::Double(0.0))]);
    post(&db, &[(format!("{P}OMTX_Calc"), EpicsValue::Enum(1))]);
    wait_for(&ioc, &format!("{P}K_RBV"), 0.0).await;
}

/// ss readback is its own state set, so the motor readbacks convert to
/// H/K/L_RBV through enter_monitor_all's 10 s delay (orient_st.st:1201-1232).
#[epics_base_rs::epics_test]
async fn readbacks_convert_during_the_init_delay() {
    let (ioc, db, _orient) = boot_with(&good_inputs()).await;
    wait_for_msg(&ioc, "Successful OMTX calc").await;
    set_rbvs(
        &db,
        &[
            ("tth", 24.3414),
            ("th", 12.1707),
            ("chi", 0.0),
            ("phi", 0.0),
        ],
    );
    for _ in 0..20 {
        if (f64_of(&ioc, &format!("{P}H_RBV")).await - 4.0).abs() < 1e-3 {
            assert_ne!(
                string_of(&ioc, &format!("{P}Msg")).await,
                "Initialization complete; going live"
            );
            return;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    panic!("H_RBV = {}", f64_of(&ioc, &format!("{P}H_RBV")).await);
}

/// ss stop is its own state set, so stopMotors stops the motors while ss
/// orient is still retrying a failed init (orient_st.st:1235-1248).
#[epics_base_rs::epics_test]
async fn stop_motors_stops_them_during_init() {
    let (ioc, db, _orient) = boot_with(&[]).await;
    wait_for(&ioc, &format!("{P}A0_state"), 2.0).await;
    let mut stops = Vec::new();
    for m in ["tth", "th", "chi", "phi"] {
        stops.push(
            DbSubscription::subscribe(&db, format!("X:m:{m}.STOP"))
                .await
                .unwrap(),
        );
    }
    put(&db, "stopMotors", 1.0).await;
    wait_for(&ioc, &format!("{P}stopMotors"), 0.0).await;
    assert_eq!(string_of(&ioc, &format!("{P}Msg")).await, "stopping motors");
    for (m, sub) in ["tth", "th", "chi", "phi"].iter().zip(&mut stops) {
        let mut put_one = false;
        while let Ok(ev) = sub.try_recv_event() {
            put_one |= ev.value().as_value().and_then(EpicsValue::to_f64) == Some(1.0);
        }
        assert!(put_one, "{m}.STOP was not put to 1");
    }
}

/// A PM+mPHI of 40 chars or more is built in tmpstr, so its .STOP is
/// assigned and motPHI_name shows the first 39 chars; upstream C built it
/// in the 40-byte motPHI_name and ran past it.
#[epics_base_rs::epics_test]
async fn a_long_motor_name_is_assigned_and_shown_cut() {
    const PHI: &str = "phi-motor-on-a-long-record-name-for-orient";
    let long = format!(r#"record(motor, "X:m:{PHI}") {{ field(HLM, "180") field(LLM, "-180") }}"#);
    let builder = IocBuilder::new()
        .register_record_type("motor", || Box::new(motor_rs::MotorRecord::new()))
        .db_string(&format!("{MOTORS}{long}\n"), &HashMap::new())
        .unwrap();
    let (ioc, db, _orient) = boot_on(builder, &[], PHI).await;
    // ss connections and ss orient put these from their own delay(1)
    // (orient_st.st:1187, :465), in either order, as C's do.
    wait_for(&ioc, &format!("{P}motPHI_Connected"), 1.0).await;
    let name = format!("X:m:{PHI}");
    wait_for_string(&ioc, &format!("{P}motPHI_name"), &name[..39]).await;
    let mut stop = DbSubscription::subscribe(&db, format!("{name}.STOP"))
        .await
        .unwrap();
    put(&db, "stopMotors", 1.0).await;
    wait_for(&ioc, &format!("{P}stopMotors"), 0.0).await;
    let mut put_one = false;
    while let Ok(ev) = stop.try_recv_event() {
        put_one |= ev.value().as_value().and_then(EpicsValue::to_f64) == Some(1.0);
    }
    assert!(put_one, "{name}.STOP was not put to 1");
}

/// Wait out enter_monitor_all's 10 s delay (orient_st.st:613-621).
async fn wait_live(ioc: &PvDatabase) {
    for _ in 0..300 {
        if string_of(ioc, &format!("{P}Msg")).await == "Initialization complete; going live" {
            return;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "never went live: Msg = {:?}",
        string_of(ioc, &format!("{P}Msg")).await
    );
}

/// Inputs that make both of init's matrices good.
fn good_inputs() -> Vec<(String, EpicsValue)> {
    let mut preset = [lattice(), reflections()].concat();
    preset.push((format!("{P}energy"), EpicsValue::Double(21.657)));
    preset.push((
        format!("{P}lambda"),
        EpicsValue::Double(12.3984244 / 21.657),
    ));
    preset
}

/// The sequencer publishes A0_state and OMTX_state (orient_st.st:328-341):
/// both good at init, OMTX failed after a calculation from a zero
/// secondary reflection.
#[epics_base_rs::epics_test]
async fn a0_and_omtx_states_are_published() {
    let (ioc, db, _orient) = boot().await;
    assert_eq!(f64_of(&ioc, &format!("{P}A0_state")).await, 1.0);
    assert_eq!(f64_of(&ioc, &format!("{P}OMTX_state")).await, 1.0);
    post(&db, &[(format!("{P}K2"), EpicsValue::Double(0.0))]);
    post(&db, &[(format!("{P}OMTX_Calc"), EpicsValue::Enum(1))]);
    wait_for(&ioc, &format!("{P}OMTX_state"), 2.0).await;
    assert_eq!(f64_of(&ioc, &format!("{P}A0_state")).await, 1.0);
}

/// orient.db's zero lattice fails calc_A0's check, as C keeps it
/// (orient_st.st:511-527).
#[epics_base_rs::epics_test]
async fn a_zero_lattice_fails_a0() {
    let mut preset = reflections();
    preset.push((format!("{P}energy"), EpicsValue::Double(21.657)));
    preset.push((
        format!("{P}lambda"),
        EpicsValue::Double(12.3984244 / 21.657),
    ));
    let (ioc, _db, _orient) = boot_with(&preset).await;
    wait_for(&ioc, &format!("{P}A0_state"), 2.0).await;
    assert_ne!(
        string_of(&ioc, &format!("{P}Msg")).await,
        "Successful A0 calc"
    );
}

/// Init puts no message of its own before A0 succeeds; C has no
/// "Orient initialized" (orient_st.st:370-590).
#[epics_base_rs::epics_test]
async fn a_failing_init_leaves_msg_empty() {
    let (ioc, _db, _orient) = boot_with(&[]).await;
    wait_for(&ioc, &format!("{P}A0_state"), 2.0).await;
    task::sleep(Duration::from_millis(300)).await;
    assert_eq!(string_of(&ioc, &format!("{P}Msg")).await, "");
}

/// Init stays in init/init_wait while OMTX fails, so an HKL write drives
/// nothing, and retries each second until good reflections arrive
/// (orient_st.st:370-375, :534-590).
#[epics_base_rs::epics_test]
async fn init_retries_omtx_until_the_reflections_are_good() {
    let mut preset = lattice();
    preset.push((format!("{P}energy"), EpicsValue::Double(21.657)));
    preset.push((
        format!("{P}lambda"),
        EpicsValue::Double(12.3984244 / 21.657),
    ));
    preset.push((format!("{P}motPut_Auto"), EpicsValue::Enum(1)));
    let (ioc, db, _orient) = boot_with(&preset).await;
    wait_for(&ioc, &format!("{P}A0_state"), 1.0).await;
    wait_for(&ioc, &format!("{P}OMTX_state"), 2.0).await;
    // A failed init OMTX sets motPut_Auto to manual (orient_st.st:575-580).
    wait_for(&ioc, &format!("{P}motPut_Auto"), 0.0).await;
    wait_for(&ioc, &format!("{P}Alert"), 1.0).await;
    post(&db, &[(format!("{P}motPut_Auto"), EpicsValue::Enum(1))]);
    put(&db, "H", 4.0).await;
    task::sleep(Duration::from_millis(1500)).await;
    assert_eq!(f64_of(&ioc, "X:m:tth").await, 0.0);
    assert_ne!(
        string_of(&ioc, &format!("{P}Msg")).await,
        "Initialization complete; going live"
    );
    post(&db, &reflections());
    wait_live(&ioc).await;
    assert_eq!(f64_of(&ioc, &format!("{P}OMTX_state")).await, 1.0);
}

/// A motor post queued behind motGet_Auto=1 already sees auto mode and
/// copies the motor into the trial angle, though the motor is taken first.
#[epics_base_rs::epics_test]
async fn a_motor_post_queued_after_mot_get_auto_copies_the_angle() {
    let (ioc, db, _orient) = boot().await;
    post(
        &db,
        &[
            (format!("{P}motGet_Auto"), EpicsValue::Enum(1)),
            ("X:m:tth".to_string(), EpicsValue::Double(12.5)),
        ],
    );
    for _ in 0..100 {
        if f64_of(&ioc, &format!("{P}TTH")).await == 12.5 {
            return;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    panic!("TTH {} not copied", f64_of(&ioc, &format!("{P}TTH")).await);
}

async fn wait_for_msg(ioc: &PvDatabase, want: &str) {
    wait_for_string(ioc, &format!("{P}Msg"), want).await;
}

async fn wait_for_string(ioc: &PvDatabase, pv: &str, want: &str) {
    for _ in 0..100 {
        if string_of(ioc, pv).await == want {
            return;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    panic!("{pv} = {:?}, want {want:?}", string_of(ioc, pv).await);
}

/// Put the energy with the reflections posted, and wait out the passes it
/// starts. With both matrices good, its putAll writes the angles through
/// loadTTH, whose first disable of TTH_event posts the event
/// (dbAccess.c:582-591), and that newAngles pass's loadH write does the same
/// to H_event. energy_event's putBusy and each angle or HKL arm raise Busy,
/// each putAll drops it (orient_st.st:750-780, :1153); H or K written before
/// the third drop is overwritten, or its busy record cleared, by those passes.
async fn set_energy(ioc: &PvDatabase, db: &PvDatabase, energy: f64) {
    let mut busy = DbSubscription::subscribe(db, format!("{P}Busy"))
        .await
        .unwrap();
    put(db, "energy", energy).await;
    wait_for_msg(ioc, "Successful A0 calc").await;
    let (mut raised, mut drops) = (false, 0);
    while drops < 3 {
        let v = task::timeout(Duration::from_secs(10), busy.recv_f64())
            .await
            .expect("the energy change's passes did not finish")
            .unwrap();
        drops += usize::from(raised && v == 0.0);
        raised = v != 0.0;
    }
}

/// OMTX_Calc wakes the sequencer, the calculation reads the reflections
/// as monitored variables, and the command resets to 0
/// (orient_st.st:175, :228-246, :646-674).
#[epics_base_rs::epics_test]
async fn omtx_calc_takes_the_posted_reflections() {
    let (ioc, db, _orient) = boot().await;
    set_energy(&ioc, &db, 21.657).await;
    post(&db, &[(format!("{P}OMTX_Calc"), EpicsValue::Enum(1))]);
    wait_for(&ioc, &format!("{P}OMTX_Calc"), 0.0).await;
    // C kohzuIoc oracle: 0.999992 from this pair, 1 (identity) without it.
    let omtx_11 = f64_of(&ioc, &format!("{P}OMTX_11")).await;
    assert!((omtx_11 - 0.999992).abs() < 1e-6, "OMTX_11 = {omtx_11}");
    assert_eq!(
        string_of(&ioc, &format!("{P}Msg")).await,
        "Successful A0 calc"
    );
}

/// A bad OMTX calculation writes motPut_Auto to manual
/// (orient_st.st:669-673, :977-979).
#[epics_base_rs::epics_test]
async fn a_bad_omtx_calc_sets_mot_put_auto_to_manual() {
    let (ioc, db, _orient) = boot().await;
    post(
        &db,
        &[
            (format!("{P}motPut_Auto"), EpicsValue::Enum(1)),
            (format!("{P}K2"), EpicsValue::Double(0.0)),
        ],
    );
    post(&db, &[(format!("{P}OMTX_Calc"), EpicsValue::Enum(1))]);
    // A stringout holds 39 characters.
    wait_for_msg(&ioc, "Bad OMTX calc; motPut_Auto set to Manua").await;
    wait_for(&ioc, &format!("{P}motPut_Auto"), 0.0).await;
}

/// A live OMTX calculation runs calc_OMTX without init's zero-reflection
/// gate and puts errAngle on every outcome: from a zero secondary
/// reflection the matrix is identity and checkOMTX divides by a zero norm
/// (orient_st.st:646-668, orient.c:279-285, :296-298).
#[epics_base_rs::epics_test]
async fn a_live_omtx_calc_puts_err_angle() {
    let (ioc, db, _orient) = boot().await;
    assert!(f64_of(&ioc, &format!("{P}errAngle")).await.is_finite());
    post(&db, &[(format!("{P}K2"), EpicsValue::Double(0.0))]);
    post(&db, &[(format!("{P}OMTX_Calc"), EpicsValue::Enum(1))]);
    wait_for(&ioc, &format!("{P}OMTX_state"), 2.0).await;
    wait_for(&ioc, &format!("{P}OMTX_Calc"), 0.0).await;
    assert!(f64_of(&ioc, &format!("{P}errAngle")).await.is_nan());
    assert_eq!(f64_of(&ioc, &format!("{P}OMTX_11")).await, 1.0);
}

/// ss connections puts each motor's connection state
/// (orient_st.st:1170-1197).
#[epics_base_rs::epics_test]
async fn motor_connections_are_published() {
    let (ioc, _db, _orient) = boot_with(&[]).await;
    for a in ["TTH", "TH", "CHI", "PHI"] {
        wait_for(&ioc, &format!("{P}mot{a}_Connected"), 1.0).await;
    }
}

/// The motor PVs are assigned statically, so under snc's default `+c` a
/// missing one holds the whole program in seq_connect (seq_task.c:61,
/// seq_ca.c:87-134): no state set runs, and an HKL write moves nothing.
#[epics_base_rs::epics_test]
async fn a_missing_motor_holds_the_program_at_connect() {
    let mut preset = good_inputs();
    preset.push((format!("{P}motPut_Auto"), EpicsValue::Enum(1)));
    let (ioc, db, _orient) = boot_with_phi(&preset, "nope").await;
    task::sleep(Duration::from_secs(2)).await;
    put(&db, "H", 4.0).await;
    task::sleep(Duration::from_millis(1500)).await;
    assert_eq!(string_of(&ioc, &format!("{P}Msg")).await, "");
    assert_eq!(string_of(&ioc, &format!("{P}motTTH_name")).await, "");
    assert_eq!(f64_of(&ioc, &format!("{P}motTTH_Connected")).await, 0.0);
    assert_eq!(f64_of(&ioc, "X:m:tth").await, 0.0);
}

/// A motor without a STOP field holds monitor_all too
/// (orient_st.st:641-643).
#[epics_base_rs::epics_test]
async fn a_motor_without_stop_holds_monitor_all() {
    let (ioc, _db, _orient) = boot_with_phi(&good_inputs(), "plain").await;
    // enter_monitor_all's delay.
    task::sleep(Duration::from_secs(10)).await;
    wait_for_msg(&ioc, "Waiting for motor-stop connection(s)").await;
    assert_eq!(f64_of(&ioc, &format!("{P}motPHI_Connected")).await, 1.0);
}

const CHILD: &str = "ORIENT_ALERT_CHILD";

/// A bad OMTX calculation after going live alerts in the OMTX_Calc clause
/// and again in newOMTX, the OMTX_mon its own OMTX puts raise fails the
/// check and alerts in newOMTX once more, and an HKL write then alerts in
/// newHKL; each alert sets Alert and prints Msg (orient_st.st:89, :672,
/// :721-747, :979, :993).
#[epics_base_rs::epics_test]
async fn a_live_bad_omtx_calc_alerts() {
    if std::env::var_os(CHILD).is_none() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "a_live_bad_omtx_calc_alerts",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let lines: Vec<String> = String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .filter_map(|l| l.find("orient").map(|i| l[i..].to_string()))
            .collect();
        assert_eq!(
            lines,
            [
                "orient: Bad OMTX calc; motPut_Auto set to Manua",
                "orient: Bad OMTX calc; motPut_Auto set to Manua",
                "orient: Bad OMTX calc; motPut_Auto set to Manua",
                "orient: We don't have a good OMTX matrix",
            ]
        );
        return;
    }
    let (ioc, db, _orient) = boot().await;
    post(
        &db,
        &[
            (format!("{P}motPut_Auto"), EpicsValue::Enum(1)),
            (format!("{P}K2"), EpicsValue::Double(0.0)),
        ],
    );
    post(&db, &[(format!("{P}OMTX_Calc"), EpicsValue::Enum(1))]);
    wait_for(&ioc, &format!("{P}Alert"), 1.0).await;
    wait_for(&ioc, &format!("{P}motPut_Auto"), 0.0).await;
    put(&db, "H", 1.0).await;
    wait_for_msg(&ioc, "We don't have a good OMTX matrix").await;
}

async fn wait_near(ioc: &PvDatabase, pv: &str, want: f64) {
    for _ in 0..100 {
        if (f64_of(ioc, pv).await - want).abs() < 1e-3 {
            return;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    panic!("{pv} = {}, want {want}", f64_of(ioc, pv).await);
}

/// orient wakes on K_event, which posts on every process where K (MDEL 0)
/// posts only a change, so rewriting the standing K drives a hand-moved
/// motor back (orient_st.st:190, :756-760; C kohzuIoc oracle: tth 10 ->
/// 34.6923).
#[epics_base_rs::epics_test]
async fn rewriting_the_standing_k_drives_the_motors_again() {
    let (ioc, db, _orient) = boot_sim().await;
    set_energy(&ioc, &db, 21.657).await;
    post(&db, &[(format!("{P}motPut_Auto"), EpicsValue::Enum(1))]);
    put(&db, "H", 4.0).await;
    put(&db, "K", 4.0).await;
    wait_near(&ioc, "X:m:tth", 34.6923).await;
    wait_for(&ioc, &format!("{P}waiting4Mot"), 0.0).await;

    post(&db, &[("X:m:tth".to_string(), EpicsValue::Double(10.0))]);
    task::sleep(Duration::from_millis(300)).await;
    assert_eq!(f64_of(&ioc, "X:m:tth").await, 10.0);

    put(&db, "K", 4.0).await;
    wait_near(&ioc, "X:m:tth", 34.6923).await;
}

/// orient's own motor drive is not taken for an outside move, and the
/// move's completion clears Busy and waiting4Mot, so motGet_Auto copies a
/// later outside move (orient_st.st:821-895, :1073).
#[epics_base_rs::epics_test]
async fn a_finished_drive_clears_busy_and_lets_mot_get_auto_copy_a_move() {
    let (ioc, db, _orient) = boot_sim().await;
    set_energy(&ioc, &db, 21.657).await;
    post(
        &db,
        &[
            (format!("{P}motPut_Auto"), EpicsValue::Enum(1)),
            (format!("{P}motGet_Auto"), EpicsValue::Enum(1)),
        ],
    );
    let mut busy_mon = DbSubscription::subscribe(&db, format!("{P}Busy"))
        .await
        .unwrap();
    put(&db, "H", 4.0).await;
    put(&db, "K", 4.0).await;
    wait_near(&ioc, "X:m:tth", 34.6923).await;
    // Busy holds until the drive completes.
    let busy = f64_of(&ioc, &format!("{P}Busy")).await;
    assert_eq!(
        f64_of(&ioc, "X:m:tth.DMOV").await,
        0.0,
        "drive already done"
    );
    assert_eq!(busy, 1.0);
    // The completion's motGet_Auto copy writes loadH/loadK, the first
    // disabled H_event/K_event pass since the client's, which posts
    // (dbAccess.c:579-593), so orient runs newHKL and raises Busy once more
    // (orient_st.st:750, :1103): it is idle only after that second drop.
    let (mut raised, mut drops) = (false, 0);
    while drops < 2 {
        let v = task::timeout(Duration::from_secs(10), busy_mon.recv_f64())
            .await
            .expect("the drive and the copy's echo did not finish")
            .unwrap();
        drops += usize::from(raised && v == 0.0);
        raised = v != 0.0;
    }
    assert_eq!(f64_of(&ioc, &format!("{P}waiting4Mot")).await, 0.0);
    wait_near(&ioc, &format!("{P}K"), 4.0).await;

    post(&db, &[("X:m:tth".to_string(), EpicsValue::Double(10.0))]);
    wait_near(&ioc, &format!("{P}TTH"), 10.0).await;
}

/// A K write sets K_busy through K_event; the drive's completion clears
/// it, and a rewrite that moves no motor clears it in putAll
/// (orient_st.st:883-895, :1152-1161).
#[epics_base_rs::epics_test]
async fn put_all_and_the_drive_completion_clear_the_busy_records() {
    let (ioc, db, _orient) = boot_sim().await;
    set_energy(&ioc, &db, 21.657).await;
    post(&db, &[(format!("{P}motPut_Auto"), EpicsValue::Enum(1))]);
    // K_busy is 1 only while the drive runs, which a read can miss, so
    // take its monitors: C's 0 comes with the drive done (orient_st.st:821-889).
    let mut k_busy = DbSubscription::subscribe(&db, format!("{P}K_busy"))
        .await
        .unwrap();
    put(&db, "H", 4.0).await;
    put(&db, "K", 4.0).await;
    let mut raised = false;
    loop {
        let v = task::timeout(Duration::from_secs(10), k_busy.recv_f64())
            .await
            .expect("K_busy did not rise and fall")
            .unwrap();
        if v != 0.0 {
            raised = true;
        } else if raised {
            break;
        }
    }
    assert_eq!(f64_of(&ioc, "X:m:tth.DMOV").await, 1.0);
    let rbv = f64_of(&ioc, "X:m:tth.RBV").await;
    assert!((rbv - 34.6923).abs() < 1e-3, "X:m:tth.RBV = {rbv}");
    wait_for(&ioc, &format!("{P}Busy"), 0.0).await;
    wait_for(&ioc, &format!("{P}H_busy"), 0.0).await;

    put(&db, "K", 4.0).await;
    task::sleep(Duration::from_millis(300)).await;
    assert_eq!(f64_of(&ioc, &format!("{P}waiting4Mot")).await, 0.0);
    wait_for(&ioc, &format!("{P}K_busy"), 0.0).await;
    wait_for(&ioc, &format!("{P}Busy"), 0.0).await;
}

/// Energy goes straight to newHKL: OMTX is not recalculated from the
/// reflections as they now stand (orient_st.st:677-703).
#[epics_base_rs::epics_test]
async fn an_energy_change_leaves_the_omtx() {
    let (ioc, db, _orient) = boot().await;
    post(&db, &[(format!("{P}K2"), EpicsValue::Double(0.0))]);
    put(&db, "energy", 15.0).await;
    wait_for_msg(&ioc, "Successful A0 calc").await;
    task::sleep(Duration::from_millis(500)).await;
    assert_eq!(f64_of(&ioc, &format!("{P}OMTX_state")).await, 1.0);
    let omtx_11 = f64_of(&ioc, &format!("{P}OMTX_11")).await;
    assert!((omtx_11 - 0.999992).abs() < 1e-6, "OMTX_11 = {omtx_11}");
}

/// A lattice edit recalculates A0, and OMTX_Calc stays pending until A0
/// is good again (orient_st.st:646, :677-703). A cell this large leaves
/// A0's determinant under invertArray's SMALL.
#[epics_base_rs::epics_test]
async fn omtx_calc_waits_for_a_good_a0() {
    let (ioc, db, _orient) = boot().await;
    let si_a0_11 = f64_of(&ioc, &format!("{P}A0_11")).await;
    post(&db, &[(format!("{P}a"), EpicsValue::Double(1.0e9))]);
    wait_for(&ioc, &format!("{P}A0_state"), 2.0).await;
    wait_for_msg(&ioc, "We don't have a good A0 matrix").await;
    post(&db, &[(format!("{P}OMTX_Calc"), EpicsValue::Enum(1))]);
    task::sleep(Duration::from_millis(300)).await;
    assert_eq!(f64_of(&ioc, &format!("{P}OMTX_Calc")).await, 1.0);
    post(&db, &[(format!("{P}a"), EpicsValue::Double(6.0))]);
    wait_for(&ioc, &format!("{P}A0_state"), 1.0).await;
    wait_for(&ioc, &format!("{P}OMTX_Calc"), 0.0).await;
    let a0_11 = f64_of(&ioc, &format!("{P}A0_11")).await;
    assert!(
        (a0_11 - si_a0_11 * 5.431 / 6.0).abs() < 1e-9,
        "A0_11 = {a0_11}"
    );
}

/// Init puts the motor names, and newHKL, newAngles and putAll put the
/// state PVs C's db holds (orient_st.st:410-465, :1012-1013, :1043-1045,
/// :1103-1115).
#[epics_base_rs::epics_test]
async fn init_and_new_hkl_put_their_state_pvs() {
    let (ioc, db, _orient) = boot().await;
    for (a, m) in [("TTH", "tth"), ("TH", "th"), ("CHI", "chi"), ("PHI", "phi")] {
        assert_eq!(
            string_of(&ioc, &format!("{P}mot{a}_name")).await,
            format!("X:m:{m}")
        );
    }
    let mut new_angles = DbSubscription::subscribe(&db, format!("{P}newAngles"))
        .await
        .unwrap();
    put(&db, "H", 4.0).await;
    let v = task::timeout(Duration::from_secs(5), new_angles.recv_f64())
        .await
        .expect("newAngles was not put")
        .unwrap();
    assert_eq!(v, 1.0);
}

/// A user's A0 element is copied and inverted; a singular A0 fails and
/// newA0 goes back to monitor_all (orient_st.st:706-718, :963-967).
#[epics_base_rs::epics_test]
async fn a_singular_user_a0_fails() {
    let (ioc, db, _orient) = boot().await;
    post(&db, &[(format!("{P}A0_11"), EpicsValue::Double(0.0))]);
    wait_for_msg(&ioc, "Could not invert A0 array").await;
    wait_for(&ioc, &format!("{P}A0_state"), 2.0).await;
}

/// A user's OMTX element is checked against the secondary reflection of
/// the last calc_OMTX; one that fails sends newOMTX to manual
/// (orient_st.st:721-747, :977-979).
#[epics_base_rs::epics_test]
async fn a_user_omtx_failing_the_check_sets_mot_put_auto_to_manual() {
    let (ioc, db, _orient) = boot().await;
    let omtx_22 = f64_of(&ioc, &format!("{P}OMTX_22")).await;
    post(&db, &[(format!("{P}motPut_Auto"), EpicsValue::Enum(1))]);
    post(
        &db,
        &[(format!("{P}OMTX_22"), EpicsValue::Double(-omtx_22))],
    );
    wait_for(&ioc, &format!("{P}OMTX_state"), 2.0).await;
    wait_for(&ioc, &format!("{P}motPut_Auto"), 0.0).await;
    wait_for_msg(&ioc, "Bad OMTX calc; motPut_Auto set to Manua").await;
    assert!(f64_of(&ioc, &format!("{P}errAngle")).await > 1.0);
}

/// refGet1 copies H..PHI into the primary reflection, and ref_show resets
/// the button half a second later (orient_st.st:598-610, :927-930).
#[epics_base_rs::epics_test]
async fn ref_get1_copies_the_current_values() {
    let (ioc, db, _orient) = boot().await;
    post(
        &db,
        &[
            (format!("{P}L"), EpicsValue::Double(1.5)),
            (format!("{P}CHI"), EpicsValue::Double(7.0)),
        ],
    );
    post(&db, &[(format!("{P}refGet1"), EpicsValue::Enum(1))]);
    wait_for(&ioc, &format!("{P}L1"), 1.5).await;
    wait_for(&ioc, &format!("{P}CHI1"), 7.0).await;
    assert_eq!(f64_of(&ioc, &format!("{P}refGet1")).await, 1.0);
    wait_for(&ioc, &format!("{P}refGet1"), 0.0).await;
}

/// refPut2 writes the secondary reflection into H..PHI through the
/// records themselves (orient_st.st:943-946).
#[epics_base_rs::epics_test]
async fn ref_put2_writes_the_secondary_reflection() {
    let (ioc, db, _orient) = boot().await;
    post(&db, &[(format!("{P}refPut2"), EpicsValue::Enum(1))]);
    wait_for(&ioc, &format!("{P}K"), 4.0).await;
    wait_for(&ioc, &format!("{P}refPut2"), 0.0).await;
}

/// OperAck erases Msg and clears Alert (orient_st.st:949-953).
#[epics_base_rs::epics_test]
async fn oper_ack_clears_msg_and_alert() {
    let (ioc, db, _orient) = boot().await;
    post(&db, &[(format!("{P}A0_11"), EpicsValue::Double(0.0))]);
    put(&db, "H", 1.0).await;
    wait_for(&ioc, &format!("{P}Alert"), 1.0).await;
    post(&db, &[(format!("{P}OperAck"), EpicsValue::Enum(1))]);
    wait_for(&ioc, &format!("{P}Alert"), 0.0).await;
    wait_for(&ioc, &format!("{P}OperAck"), 0.0).await;
    assert_eq!(string_of(&ioc, &format!("{P}Msg")).await, "");
}

/// With Debug on, PUTMSG prints Msg and ss orient prints its debug lines;
/// ss stop's PUTMSG prints too (orient_st.st:88, :505, :513, :545, :751,
/// :1101-1109, :1238-1240).
#[epics_base_rs::epics_test]
async fn debug_prints_msg_and_the_debug_lines() {
    if std::env::var_os(CHILD).is_none() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "debug_prints_msg_and_the_debug_lines",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8(out.stdout).unwrap();
        let mut want = [
            "orient: calculated energy",
            "orient: calling calc_A0",
            "orient: Successful A0 calc",
            "orient: calling calc_OMTX",
            "orient: Successful OMTX calc",
            "orient: Initialization complete; going live",
            "",
            "orient: client wrote H",
            "orient:putAll: writing Angles",
            "orient:putAll: all complete (no motors)",
            "orient: stopping motors",
        ]
        .into_iter()
        .peekable();
        // The first line follows libtest's own.
        for line in stdout.lines() {
            let line = line.find("orient").map_or(line, |i| &line[i..]);
            if want.peek() == Some(&line) {
                want.next();
            }
        }
        assert_eq!(want.next(), None, "{stdout}");
        return;
    }
    let mut preset = good_inputs();
    preset.push((format!("{P}Debug"), EpicsValue::Enum(1)));
    let (ioc, db, _orient) = boot_with(&preset).await;
    wait_live(&ioc).await;
    task::sleep(Duration::from_millis(300)).await;
    put(&db, "H", 4.0).await;
    wait_for(&ioc, &format!("{P}H_busy"), 0.0).await;
    put(&db, "stopMotors", 1.0).await;
    wait_for(&ioc, &format!("{P}stopMotors"), 0.0).await;
}

/// seq refuses pvPut(motX, ASYNC) while the previous ASYNC put on motX is
/// pending (seq_if.c:128-139), so a new HKL during a move leaves the
/// motor's target where the first put set it. These motors never finish.
#[epics_base_rs::epics_test]
async fn a_second_drive_during_a_move_is_refused() {
    let (ioc, db, _orient) = boot().await;
    set_energy(&ioc, &db, 21.657).await;
    post(&db, &[(format!("{P}motPut_Auto"), EpicsValue::Enum(1))]);
    put(&db, "H", 4.0).await;
    put(&db, "K", 4.0).await;
    wait_near(&ioc, "X:m:tth", 34.6923).await;
    assert_eq!(f64_of(&ioc, "X:m:tth.DMOV").await, 0.0);
    put(&db, "H", 2.0).await;
    task::sleep(Duration::from_millis(500)).await;
    let tth = f64_of(&ioc, "X:m:tth").await;
    assert!((tth - 34.6923).abs() < 1e-3, "X:m:tth retargeted to {tth}");
}

/// Drive H=4, K=4 with X:m:tth's `field` put to `v` first.
async fn drive_with_tth(field: &str, v: EpicsValue) -> (Arc<PvDatabase>, PvDatabase, Orient) {
    let (ioc, db, prog) = boot_sim().await;
    set_energy(&ioc, &db, 21.657).await;
    db.put_record_field_from_ca("X:m:tth", field, v)
        .await
        .unwrap();
    post(&db, &[(format!("{P}motPut_Auto"), EpicsValue::Enum(1))]);
    put(&db, "H", 4.0).await;
    put(&db, "K", 4.0).await;
    wait_near(&ioc, &format!("{P}TTH"), 34.6923).await;
    (ioc, db, prog)
}

/// dbProcess completes the put callback of a disabled record without
/// processing it, so the drive finishes with TTH never moved.
#[epics_base_rs::epics_test]
async fn a_drive_completes_on_a_disabled_motor() {
    let (ioc, _db, _orient) = drive_with_tth("DISA", EpicsValue::Short(1)).await;
    wait_for(&ioc, &format!("{P}waiting4motTTH"), 0.0).await;
    wait_for(&ioc, &format!("{P}waiting4Mot"), 0.0).await;
    wait_for(&ioc, &format!("{P}Busy"), 0.0).await;
    assert_eq!(f64_of(&ioc, "X:m:tth.RBV").await, 0.0);
}

/// At SPMG=Stop the motor record processes the put without moving and
/// without its forward link, so the callback stays pending (motorRecord.cc
/// do_work :2237, process :1509) until SPMG=Go runs the move.
#[epics_base_rs::epics_test]
async fn a_drive_on_a_stopped_motor_waits_for_go() {
    let (ioc, db, _orient) = drive_with_tth("SPMG", EpicsValue::Enum(0)).await;
    task::sleep(Duration::from_secs(1)).await;
    assert_eq!(f64_of(&ioc, &format!("{P}waiting4motTTH")).await, 1.0);
    assert_eq!(f64_of(&ioc, &format!("{P}Busy")).await, 1.0);
    // A plain caput: a put callback would wait behind orient's.
    db.put_record_field_from_ca_no_notify("X:m:tth", "SPMG", EpicsValue::Enum(3))
        .await
        .unwrap();
    wait_for(&ioc, &format!("{P}waiting4motTTH"), 0.0).await;
    wait_for(&ioc, &format!("{P}Busy"), 0.0).await;
    wait_near(&ioc, "X:m:tth.RBV", 34.6923).await;
}

/// a=0 live gives calc_A0 a NaN matrix, which invertArray's
/// `fabs(det) > SMALL` refuses, so A0 fails and is left identity.
#[epics_base_rs::epics_test]
async fn a_zero_a_live_fails_a0() {
    let (ioc, db, _orient) = boot().await;
    put(&db, "a", 0.0).await;
    wait_for(&ioc, &format!("{P}A0_state"), 2.0).await;
    assert_eq!(f64_of(&ioc, &format!("{P}A0_11")).await, 1.0);
}

/// The four RBV connect monitors raise motRBV_mon (orient_st.st:155-158),
/// so once both matrices are good ss readback converts the standing
/// readbacks without any motor moving.
#[epics_base_rs::epics_test]
async fn standing_readbacks_convert_when_the_matrices_go_good() {
    let macros: HashMap<String, String> = [("P", "X:"), ("O", "1"), ("PREC", "4")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let ioc = IocBuilder::new()
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .register_record_type("transform", || Box::new(TransformRecord::default()))
        .register_record_type("motor", || Box::new(motor_rs::MotorRecord::new()))
        .db_string(MOTORS, &macros)
        .unwrap()
        .db_file(ORIENT_DB, &macros)
        .unwrap()
        .build_served()
        .await
        .unwrap()
        .0;
    let db = PvDatabase::clone(&ioc);
    post(&db, &good_inputs());
    set_rbvs(&db, &[("tth", 24.3414), ("th", 12.1707)]);
    let _orient = start(&ioc, "phi");
    wait_for_msg(&ioc, "Successful OMTX calc").await;
    wait_near(&ioc, &format!("{P}H_RBV"), 4.0).await;
}
