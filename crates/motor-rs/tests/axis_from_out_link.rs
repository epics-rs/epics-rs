//! A motor record reaches its axis through OUT at `init_record`, as C's
//! devMotorAsyn (`@asyn(PORT,ADDR)`, devMotorAsyn.c:276-291) and model-1
//! dsets (`#C card S signal`, motordevCom.cc:217-258) do. The DTYP rows are
//! static, so a record whose controller was never created still loads and
//! fails at init.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::interfaces::motor::MotorStatus;
use asyn_rs::manager::PortManager;
use asyn_rs::registry::open_owner;
use asyn_rs::trace::{TraceFile, TraceInfoMask};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::DevSup;
use epics_base_rs::types::EpicsValue;
use motor_rs::builder::{AxisSetup, MotorBuilder};
use motor_rs::device_state::{AxisSubscribers, StampedStatus};
use motor_rs::device_support::MotorAxis;
use motor_rs::devices::{
    device_support_factory, devices, model1_device, motor_interface_device, register_axis,
    register_controller_port, register_drv_motor_asyn_port, register_model1_device,
    register_motor_interface_device, register_motor_interface_driver,
};
use motor_rs::trace::TraceUser;
use tokio::sync::mpsc;

static MODEL1: DevSup = model1_device("devTestModel1", "TestModel1");
static SIM: DevSup = motor_interface_device("devTestSim", "TestSim");

/// A model-2 port with no axis open: a record on it never reaches a write.
fn axisless_port(name: &str, num_axes: usize) -> asyn_rs::port_handle::PortHandle {
    use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};

    struct Axisless(PortDriverBase);
    impl PortDriver for Axisless {
        fn base(&self) -> &PortDriverBase {
            &self.0
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.0
        }
    }
    let claim = asyn_rs::registry::claim_port(name).unwrap();
    let driver = Axisless(PortDriverBase::new(name, num_axes, PortFlags::default()));
    let (runtime, _actor) = asyn_rs::runtime::port::create_port_runtime(
        &claim,
        driver,
        asyn_rs::runtime::config::RuntimeConfig::default(),
    )
    .unwrap();
    runtime.port_handle().clone()
}

fn sim(card: i32, signal: i32) -> TraceUser {
    TraceUser::MotorInterface {
        device: SIM.choice,
        driver: "testSim",
        card,
        signal,
    }
}

/// An axis at `user` whose status cache reads `position`; no poller runs, as
/// `init_record` only reads that cache.
fn axis(user: TraceUser, position: f64) {
    let subscribers = AxisSubscribers::new();
    subscribers.store(StampedStatus {
        seq: 1,
        status: MotorStatus {
            position,
            done: true,
            ..MotorStatus::default()
        },
    });
    register_axis(unpolled(user, subscribers)).unwrap();
}

fn unpolled(user: TraceUser, subscribers: AxisSubscribers) -> MotorAxis {
    let (poll_tx, _) = mpsc::channel(16);
    let (cmd_tx, _) = mpsc::unbounded_channel();
    MotorAxis::new(user, Duration::from_secs(1), poll_tx, cmd_tx, subscribers)
}

/// `port`'s trace, written to a fresh file from now on.
struct Capture(std::path::PathBuf);

impl Capture {
    fn of(port: &str) -> Self {
        let path = std::env::temp_dir().join(format!("motor-out-{port}-{}", std::process::id()));
        let file = std::fs::File::create(&path).unwrap();
        PortManager::global()
            .trace_manager()
            .set_trace_file(Some(port), TraceFile::File(Arc::new(Mutex::new(file))));
        Capture(path)
    }

    fn text(&self) -> String {
        std::fs::read_to_string(&self.0).unwrap()
    }

    fn lines(&self) -> Vec<String> {
        self.text().lines().map(str::to_owned).collect()
    }

    /// The lines once one ends with `want`; the io_intr pass that prints it
    /// runs after the boot returns.
    async fn until(&self, want: &str) -> Vec<String> {
        for _ in 0..200 {
            let lines = self.lines();
            if lines.iter().any(|l| l.ends_with(want)) {
                return lines;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.lines()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn asyn(port: &str, addr: i32) -> TraceUser {
    TraceUser::Asyn {
        port: port.into(),
        addr,
    }
}

async fn boot(db: &str) -> Arc<PvDatabase> {
    register_model1_device(&MODEL1);
    register_motor_interface_device(&SIM);
    // The tests register testSim's axes themselves.
    register_motor_interface_driver("testSim", |_, _| false);
    let (name, factory) = motor_rs::motor_record_factory();
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .register_dynamic_device_support(&devices(), device_support_factory())
        .db_string(db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

fn field(db: &PvDatabase, pv: &str) -> EpicsValue {
    db.get_pv(pv).unwrap()
}

/// The errlog lines `body` produces.
async fn errlog_of<F: std::future::Future<Output = Arc<PvDatabase>>>(
    body: F,
) -> (Arc<PvDatabase>, Vec<String>) {
    use epics_base_rs::runtime::log::{errlog_add_listener, errlog_flush, errlog_remove_listener};
    let lines = Arc::new(Mutex::new(Vec::new()));
    let sink = lines.clone();
    let id = errlog_add_listener(move |m| sink.lock().unwrap().push(m.trim_end().to_owned()));
    let db = body.await;
    errlog_flush();
    errlog_remove_listener(id);
    let lines = lines.lock().unwrap().clone();
    (db, lines)
}

#[tokio::test]
async fn an_asyn_out_reaches_the_axis_at_its_port_and_address() {
    register_controller_port(&open_owner(), "OUTA", 2, ()).unwrap();
    axis(asyn("OUTA", 0), 10.0);
    axis(asyn("OUTA", 1), 20.0);
    let db = boot(
        r##"
record(motor, "A:m0") { field(DTYP, "asynMotor") field(OUT, "@asyn(OUTA,0)") field(MRES, "1") }
record(motor, "A:m1") { field(DTYP, "asynMotor") field(OUT, "@asyn(OUTA 1)") field(MRES, "1") }
"##,
    )
    .await;
    assert_eq!(field(&db, "A:m0.DRBV"), EpicsValue::Double(10.0));
    assert_eq!(field(&db, "A:m1.DRBV"), EpicsValue::Double(20.0));
    assert_eq!(field(&db, "A:m1.PACT"), EpicsValue::Char(0));
    assert_eq!(field(&db, "A:m1.CARD"), EpicsValue::Short(0));
}

#[tokio::test]
async fn a_model1_out_reaches_the_axis_at_its_card_and_signal() {
    let user = |card, signal| TraceUser::Model1 {
        device: MODEL1.choice,
        card,
        signal,
    };
    axis(user(1, 2), 30.0);
    axis(user(2, 1), 40.0);
    let db = boot(
        r##"
record(motor, "B:m") { field(DTYP, "TestModel1") field(OUT, "#C1 S2 @") field(MRES, "1") }
"##,
    )
    .await;
    assert_eq!(field(&db, "B:m.DRBV"), EpicsValue::Double(30.0));
    assert_eq!(field(&db, "B:m.CARD"), EpicsValue::Short(1));
}

/// devMotorSim.c:131-200: the drvet the parm names, at the OUT's card and
/// signal; it has no in-use check, so two records open one axis.
#[tokio::test]
async fn a_motor_interface_out_reaches_the_axis_of_the_driver_its_parm_names() {
    axis(sim(4, 1), 50.0);
    let (db, lines) = errlog_of(boot(
        r##"
record(motor, "H:m0") { field(DTYP, "TestSim") field(OUT, "#C4 S1 @testSim") field(MRES, "1") }
record(motor, "H:m1") { field(DTYP, "TestSim") field(OUT, "#C4 S1 @testSim") field(MRES, "1") }
"##,
    ))
    .await;
    assert!(!lines.iter().any(|l| l.contains("PV: H:m")), "{lines:?}");
    for pv in ["H:m0", "H:m1"] {
        assert_eq!(field(&db, &format!("{pv}.DRBV")), EpicsValue::Double(50.0));
        assert_eq!(field(&db, &format!("{pv}.CARD")), EpicsValue::Short(4));
    }
}

/// devMotorSim's init_record calls the drvet's `open` (devMotorSim.c:143):
/// the first record to name an axis opens it, later ones bind the same one,
/// and an axis the driver lacks fails init.
#[tokio::test]
async fn a_motor_interface_record_opens_its_axis_at_init() {
    let opens = Arc::new(Mutex::new(Vec::new()));
    let log = opens.clone();
    register_motor_interface_driver("testOpen", move |card, signal| {
        log.lock().unwrap().push((card, signal));
        if signal != 0 {
            return false;
        }
        let user = TraceUser::MotorInterface {
            device: SIM.choice,
            driver: "testOpen",
            card,
            signal,
        };
        let subscribers = AxisSubscribers::new();
        subscribers.store(StampedStatus {
            seq: 1,
            status: MotorStatus {
                position: 70.0,
                done: true,
                ..MotorStatus::default()
            },
        });
        register_axis(unpolled(user, subscribers)).is_ok()
    });
    let (db, lines) = errlog_of(boot(
        r##"
record(motor, "O:m0") { field(DTYP, "TestSim") field(OUT, "#C6 S0 @testOpen") field(MRES, "1") }
record(motor, "O:m1") { field(DTYP, "TestSim") field(OUT, "#C6 S0 @testOpen") field(MRES, "1") }
record(motor, "O:m2") { field(DTYP, "TestSim") field(OUT, "#C6 S4 @testOpen") }
"##,
    ))
    .await;
    assert_eq!(*opens.lock().unwrap(), [(6, 0), (6, 4)]);
    for pv in ["O:m0", "O:m1"] {
        assert_eq!(field(&db, &format!("{pv}.DRBV")), EpicsValue::Double(70.0));
    }
    let want = "recGblRecordError: devMotor (init_record) cannot open device support Error (513,1) PV: O:m2";
    assert!(lines.iter().any(|l| l == want), "{lines:?}");
}

/// devMotorSim.c:136-196 then motorRecord.cc:648-651: each refusal is a
/// recGblRecordError and CARD -1. M_drvSup has no symbol table.
#[tokio::test]
async fn a_motor_interface_record_reports_each_devmotorsim_refusal() {
    axis(sim(5, 0), 0.0);
    let (db, lines) = errlog_of(boot(
        r##"
record(motor, "I:m0") { field(DTYP, "TestSim") field(OUT, "#C5 S0 @noSuchDriver") }
record(motor, "I:m1") { field(DTYP, "TestSim") field(OUT, "#C5 S3 @testSim") }
record(motor, "I:m2") { field(DTYP, "TestSim") field(OUT, "@asyn(X,0)") }
"##,
    ))
    .await;
    for (pv, why) in [
        (
            "I:m0",
            "cannot find device support entry table Error (513,3)",
        ),
        ("I:m1", "cannot open device support Error (513,1)"),
        ("I:m2", "Illegal INP field Illegal field value"),
    ] {
        let want = format!("recGblRecordError: devMotor (init_record) {why} PV: {pv}");
        assert!(
            lines.iter().any(|l| l == &want),
            "{want:?} not in {lines:?}"
        );
        assert_eq!(field(&db, &format!("{pv}.CARD")), EpicsValue::Short(-1));
    }
}

/// devMotorAsyn.c:285-291 then `bad:` — the record loads, prints on the
/// unconnected user's (global) trace, and is left with PACT set.
#[tokio::test]
async fn an_asyn_record_whose_port_was_never_created_loads_and_dies_at_init() {
    let path = std::env::temp_dir().join(format!("motor-noport-{}", std::process::id()));
    let file = std::fs::File::create(&path).unwrap();
    PortManager::global()
        .trace_manager()
        .set_trace_file(None, TraceFile::File(Arc::new(Mutex::new(file))));
    let db = boot(
        r##"
record(motor, "C:m") { field(DTYP, "asynMotor") field(OUT, "@asyn(NEVER_CREATED,0)") }
"##,
    )
    .await;
    let text = std::fs::read_to_string(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    assert!(
        text.lines()
            .any(|l| l
                .ends_with("devMotorAsyn::init_record, C:m connectDevice failed to NEVER_CREATED")),
        "{text:?}"
    );
    assert_eq!(field(&db, "C:m.PACT"), EpicsValue::Char(1));
}

/// devMotorAsyn.c:276-282: a link `parseLink` refuses is reported through
/// the errlog with its `errorMessage`.
#[tokio::test]
async fn an_asyn_record_with_a_bad_link_says_so_and_dies_at_init() {
    let (db, lines) = errlog_of(boot(
        r##"
record(motor, "D:m") { field(DTYP, "asynMotor") field(OUT, "@NEVER_CREATED") }
"##,
    ))
    .await;
    assert!(
        lines.iter().any(|l| l
            == "devMotorAsyn::init_record D:m bad link invalid INST_IO Must be asyn(<port> <addr> <timeout>)userParams"),
        "{lines:?}"
    );
    assert_eq!(field(&db, "D:m.PACT"), EpicsValue::Char(1));
}

/// motordevCom.cc:229-256 and motorRecord.cc:648-651: a card nobody
/// configured is `recGblRecordError(S_db_badField, ...)`, an RA_PROBLEM
/// readback, and CARD -1. The record is not left active.
#[tokio::test]
async fn a_model1_record_whose_card_was_never_configured_fails_init() {
    let (db, lines) = errlog_of(boot(
        r##"
record(motor, "E:m") { field(DTYP, "TestModel1") field(OUT, "#C7 S0 @") }
"##,
    ))
    .await;
    assert!(
        lines.iter().any(|l| l
            == "recGblRecordError: motor_init_record_com(): card does not exist! Illegal field value PV: E:m"),
        "{lines:?}"
    );
    assert_eq!(field(&db, "E:m.CARD"), EpicsValue::Short(-1));
    assert_eq!(field(&db, "E:m.MSTA"), EpicsValue::ULong(0x200));
    assert_eq!(field(&db, "E:m.PACT"), EpicsValue::Char(0));
    // A failed dset init returns before monitor(): iocInit.c:524-525's UDF
    // alarm stands.
    assert_eq!(field(&db, "E:m.STAT"), EpicsValue::Short(17));
    assert_eq!(field(&db, "E:m.SEVR"), EpicsValue::Short(3));
}

/// motorRecord.cc:646-651: a failed dset init returns before init_record's
/// second half, so ERES is not seeded (692-696). dbLoadRecords' field()
/// writes are raw: no special() derives HLM from DHLM or marks either.
/// LVIO keeps its dbd initial 1 (motorRecord.dbd:654).
/// What check_speed_and_resolution marked stays: UREV, and each raw limit
/// that disagreed with its dial partner (3922-3927, 3982-3998).
#[tokio::test]
async fn a_failed_model1_init_keeps_the_fields_init_record_had_not_reached() {
    let db = boot(
        r##"
record(motor, "F:m") { field(DTYP, "TestModel1") field(OUT, "#C8 S0 @")
    field(DIR, "Pos") field(MRES, "0.01") field(DHLM, "10") field(DLLM, "-10") }
record(motor, "F:n") { field(DTYP, "TestModel1") field(OUT, "#C8 S1 @")
    field(MRES, "0.01") field(DHLM, "10") }
"##,
    )
    .await;
    assert_eq!(field(&db, "F:m.ERES"), EpicsValue::Double(0.0));
    assert_eq!(field(&db, "F:m.LVIO"), EpicsValue::Short(1));
    assert_eq!(field(&db, "F:m.HLM"), EpicsValue::Double(0.0));
    assert_eq!(field(&db, "F:m.LLM"), EpicsValue::Double(0.0));
    assert_eq!(field(&db, "F:m.MMAP"), EpicsValue::ULong(0));
    assert_eq!(field(&db, "F:n.MMAP"), EpicsValue::ULong(0));
    // M_UREV | M_RHLM | M_RLLM.
    assert_eq!(field(&db, "F:m.NMAP"), EpicsValue::ULong(0x1804));
    // DLLM 0 agrees with RLLM 0: M_UREV | M_RHLM.
    assert_eq!(field(&db, "F:n.NMAP"), EpicsValue::ULong(0x804));
}

/// devMotorAsyn.c:449-451's `bad:` returns 0, so init_record runs to its
/// closing monitor(), which clears every mark: ERES is seeded from MRES.
#[tokio::test]
async fn an_asyn_record_that_dies_at_init_still_runs_init_record_to_monitor() {
    let db = boot(
        r##"
record(motor, "G:m") { field(DTYP, "asynMotor") field(OUT, "@asyn(NEVER_CREATED_G,0)")
    field(MRES, "0.01") field(DHLM, "10") field(DLLM, "-10") field(DIR, "Pos") }
"##,
    )
    .await;
    assert_eq!(field(&db, "G:m.ERES"), EpicsValue::Double(0.01));
    assert_eq!(field(&db, "G:m.MMAP"), EpicsValue::ULong(0));
    assert_eq!(field(&db, "G:m.NMAP"), EpicsValue::ULong(0));
    assert_eq!(field(&db, "G:m.LVIO"), EpicsValue::Short(0));
}

/// motorRecord.cc init_record never seeds MLST/ALST from VAL; only
/// monitor() writes them, and only under a nonzero MDEL/ADEL (3485-3501).
#[tokio::test]
async fn init_leaves_mlst_and_alst_at_zero() {
    register_controller_port(&open_owner(), "MLSTA", 1, ()).unwrap();
    axis(asyn("MLSTA", 0), 500.0);
    let db = boot(
        r##"
record(motor, "H:m") { field(DTYP, "asynMotor") field(OUT, "@asyn(MLSTA,0)") field(MRES, "0.01") }
"##,
    )
    .await;
    assert_eq!(field(&db, "H:m.VAL"), EpicsValue::Double(5.0));
    assert_eq!(field(&db, "H:m.MLST"), EpicsValue::Double(0.0));
    assert_eq!(field(&db, "H:m.ALST"), EpicsValue::Double(0.0));
}

/// motordevCom.cc:237-247: a signal the card does not have, and a second
/// record on a signal already in use.
#[tokio::test]
async fn a_model1_signal_must_exist_and_serve_one_record() {
    axis(
        TraceUser::Model1 {
            device: MODEL1.choice,
            card: 3,
            signal: 0,
        },
        0.0,
    );
    let (db, lines) = errlog_of(boot(
        r##"
record(motor, "F:m0") { field(DTYP, "TestModel1") field(OUT, "#C3 S0 @") }
record(motor, "F:m1") { field(DTYP, "TestModel1") field(OUT, "#C3 S0 @") }
record(motor, "F:m2") { field(DTYP, "TestModel1") field(OUT, "#C3 S4 @") }
"##,
    ))
    .await;
    let refused = |pv: &str, why: &str| {
        lines.iter().any(|l| {
            l == &format!(
                "recGblRecordError: motor_init_record_com(): {why} Illegal field value PV: {pv}"
            )
        })
    };
    assert!(refused("F:m1", "motor already in use!"), "{lines:?}");
    assert!(refused("F:m2", "signal does not exist!"), "{lines:?}");
    assert!(!lines.iter().any(|l| l.ends_with("PV: F:m0")), "{lines:?}");
    assert_eq!(field(&db, "F:m0.CARD"), EpicsValue::Short(3));
    assert_eq!(field(&db, "F:m1.CARD"), EpicsValue::Short(-1));
}

/// motordevCom.cc:217-223: a model-1 DTYP needs a VME_IO OUT.
#[tokio::test]
async fn a_model1_record_with_an_inst_io_out_is_an_illegal_bus() {
    let (db, lines) = errlog_of(boot(
        r##"
record(motor, "G:m") { field(DTYP, "TestModel1") field(OUT, "@asyn(X,0)") }
"##,
    ))
    .await;
    assert!(
        lines.iter().any(|l| l
            == "recGblRecordError: motor_init_record_com(): Illegal OUT Bus Type Error (514,13) PV: G:m"),
        "{lines:?}"
    );
    assert_eq!(field(&db, "G:m.CARD"), EpicsValue::Short(-1));
    assert_eq!(field(&db, "G:m.MSTA"), EpicsValue::ULong(0));
}

/// devMotorAsyn.c:405-417 then init_controller: a live controller port that
/// never created the axis answers `asynError` with an empty `errorMessage`.
/// The record is kept, and each command fails at the driver with no
/// `asynMotorController` line.
#[tokio::test]
async fn an_asyn_record_on_a_live_port_without_its_axis_is_kept() {
    register_controller_port(&open_owner(), "HOLE", 2, ()).unwrap();
    axis(asyn("HOLE", 0), 5.0);
    let trace = Capture::of("HOLE");
    let db = boot(
        r##"
record(motor, "H:m") { field(DTYP, "asynMotor") field(OUT, "@asyn(HOLE,1)") field(MRES, "1") }
"##,
    )
    .await;
    db.scan_control().run();
    let ratio = "devMotorAsyn::init_controller, H:m failed to set encoder ratio to 1.000000";
    let lines = trace.until(ratio).await;
    // init_record prints with no newline, as C does, so the next print runs on.
    let has = |lines: &[String], want: &str| lines.iter().any(|l| l.contains(want));
    assert!(
        has(
            &lines,
            "devMotorAsyn::init_record: H:m pasynGenericPointer->read returned "
        ),
        "{lines:?}"
    );
    assert!(has(&lines, ratio), "{lines:?}");
    let refused = "devMotorAsyn::asynCallback: H:m pasyn{Float64,Int32}->write returned ";
    let before = lines.iter().filter(|l| l.ends_with(refused)).count();

    db.put_record_field_from_ca("H:m", "DHLM", EpicsValue::Double(30.0))
        .await
        .unwrap();
    let mut lines = trace.lines();
    for _ in 0..200 {
        if lines.iter().filter(|l| l.ends_with(refused)).count() > before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        lines = trace.lines();
    }
    assert!(
        lines.iter().filter(|l| l.ends_with(refused)).count() > before,
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("asynMotorController:")),
        "{lines:?}"
    );
    assert_eq!(field(&db, "H:m.PACT"), EpicsValue::Char(0));
    assert_eq!(field(&db, "H:m.DRBV"), EpicsValue::Double(0.0));
}

/// asynPortDriver.cpp:1908-1912 and devMotorAsyn.c:244-251: an address
/// past `numAxes` fails `drvUserCreate`, so init_record takes `bad:`.
#[tokio::test]
async fn an_asyn_record_past_the_ports_axis_count_dies_at_init() {
    register_controller_port(&open_owner(), "SHORT", 2, ()).unwrap();
    let trace = Capture::of("SHORT");
    let db = boot(
        r##"
record(motor, "I:m") { field(DTYP, "asynMotor") field(OUT, "@asyn(SHORT,2)") }
"##,
    )
    .await;
    let lines = trace.lines();
    let tails: Vec<&str> = lines
        .iter()
        .map(|l| l.split_once(' ').unwrap().1.split_once(' ').unwrap().1)
        .collect();
    assert_eq!(
        tails,
        [
            "asynPortDriver:getAddress: SHORT invalid address=2, must be in range 0 to 1",
            "devMotorAsyn::findDrvInfo, I:m drvUserCreate failed for MOTOR_MOVE_REL",
            "asynPortDriver:getAddress: SHORT invalid address=2, must be in range 0 to 1",
            "SHORT 2 autoConnect could not connect: ",
            "asynPortDriver:getAddress: SHORT invalid address=2, must be in range 0 to 1",
            "devMotorAsyn::asynCallback: I:m pasyn{Float64,Int32}->write returned ",
            "asynPortDriver:getAddress: SHORT invalid address=2, must be in range 0 to 1",
            "devMotorAsyn::asynCallback: I:m pasyn{Float64,Int32}->write returned ",
        ]
    );
    assert_eq!(field(&db, "I:m.PACT"), EpicsValue::Char(1));
    // `bad:` returns 0, so init_record still ends in monitor()'s alarm
    // reset (motorRecord.cc:747).
    assert_eq!(field(&db, "I:m.STAT"), EpicsValue::Short(0));
    assert_eq!(field(&db, "I:m.SEVR"), EpicsValue::Short(0));
}

/// drvMotorAsyn.c:762-775: a model-2 port has no address check in
/// drvUserCreate; its reads and writes name the axis they refuse.
#[tokio::test]
async fn a_model2_record_on_a_missing_axis_is_kept_with_the_drivers_message() {
    register_drv_motor_asyn_port("DRV2", 2, axisless_port("DRV2", 2)).unwrap();
    let trace = Capture::of("DRV2");
    let db = boot(
        r##"
record(motor, "J:m0") { field(DTYP, "asynMotor") field(OUT, "@asyn(DRV2,1)") }
record(motor, "J:m1") { field(DTYP, "asynMotor") field(OUT, "@asyn(DRV2,4)") }
"##,
    )
    .await;
    let lines = trace.lines();
    // init_record prints with no newline, as C does, so the next print runs on.
    let has = |want: &str| lines.iter().any(|l| l.contains(want));
    assert!(
        has(
            "devMotorAsyn::init_record: J:m0 pasynGenericPointer->read returned drvMotorAsyn::readGenericPointer Uninitialised axis 1"
        ),
        "{lines:?}"
    );
    assert!(
        has(
            "devMotorAsyn::init_record: J:m1 pasynGenericPointer->read returned drvMotorAsyn::readGenericPointer Invalid axis 4"
        ),
        "{lines:?}"
    );
    assert_eq!(field(&db, "J:m0.PACT"), EpicsValue::Char(0));
    assert_eq!(field(&db, "J:m1.PACT"), EpicsValue::Char(0));
}

#[test]
fn an_asyn_axis_needs_its_port_registered_and_in_range() {
    let refused = |user: TraceUser| register_axis(unpolled(user, AxisSubscribers::new())).is_err();
    assert!(refused(asyn("UNREGISTERED", 0)));
    register_controller_port(&open_owner(), "RANGE", 1, ()).unwrap();
    assert!(refused(asyn("RANGE", 1)));
    assert!(refused(TraceUser::Model2 {
        port: "RANGE".into(),
        addr: 0
    }));
    assert!(register_controller_port(&open_owner(), "RANGE", 1, ()).is_err());
}

/// devMotorAsyn.c:376-387: each record registers its own status callback on
/// the axis, so a second record on one asyn axis binds and both follow the
/// poller. A move one record starts reaches the other's readback.
#[tokio::test]
async fn two_asyn_records_on_one_axis_both_follow_its_poller() {
    register_controller_port(&open_owner(), "TWIN", 1, ()).unwrap();
    let motor: Arc<Mutex<dyn asyn_rs::interfaces::motor::AsynMotor>> =
        Arc::new(Mutex::new(motor_rs::sim_motor::SimMotor::new()));
    let AxisSetup {
        axis,
        poll_loop,
        poll_cmd_tx,
    } = MotorBuilder::new(motor)
        .trace_user(asyn("TWIN", 0))
        .poll_interval(Duration::from_millis(10))
        .build_axis();
    register_axis(axis).unwrap();
    tokio::spawn(poll_loop.run());
    let db = boot(
        r##"
record(motor, "K:a") { field(DTYP, "asynMotor") field(OUT, "@asyn(TWIN,0)") field(MRES, "0.001") field(VELO, "100") }
record(motor, "K:b") { field(DTYP, "asynMotor") field(OUT, "@asyn(TWIN,0)") field(MRES, "0.001") field(VELO, "100") }
"##,
    )
    .await;
    db.scan_control().run();
    poll_cmd_tx
        .send(motor_rs::poll_loop::PollCommand::ForcePoll)
        .await
        .unwrap();
    assert_eq!(field(&db, "K:b.PACT"), EpicsValue::Char(0));
    let settled = |pv: &str| matches!(field(&db, pv), EpicsValue::Short(1));
    for _ in 0..300 {
        if settled("K:a.DMOV") && settled("K:b.DMOV") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    db.put_record_field_from_ca("K:a", "VAL", EpicsValue::Double(0.5))
        .await
        .unwrap();
    let at = |pv: &str| matches!(field(&db, pv), EpicsValue::Double(v) if (v - 0.5).abs() < 1e-6);
    for _ in 0..500 {
        if at("K:a.DRBV") && at("K:b.DRBV") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(at("K:a.DRBV"), "{:?}", field(&db, "K:a.DRBV"));
    assert!(at("K:b.DRBV"), "{:?}", field(&db, "K:b.DRBV"));
    let _ = poll_cmd_tx
        .send(motor_rs::poll_loop::PollCommand::Shutdown)
        .await;
}

/// devMotorAsyn.c:405-424: init_record runs init_controller itself and a
/// record on a missing axis gets no callback, so nothing ever processes it;
/// its init_record still ends in monitor()'s alarm reset.
#[tokio::test]
async fn a_record_on_a_missing_axis_is_initialised_without_a_process() {
    register_drv_motor_asyn_port("NOPASS", 2, axisless_port("NOPASS", 2)).unwrap();
    let trace = Capture::of("NOPASS");
    PortManager::global()
        .trace_manager()
        .set_trace_info_mask(Some("NOPASS"), TraceInfoMask::empty());
    let db = boot(
        r##"
record(motor, "N:m") { field(DTYP, "asynMotor") field(OUT, "@asyn(NOPASS,1)") field(FLNK, "N:n") }
record(calc, "N:n") { field(CALC, "A+1") field(INPA, "N:n NPP") }
"##,
    )
    .await;
    db.scan_control().run();
    tokio::time::sleep(Duration::from_millis(200)).await;
    // init_record's line has no newline in C, so the next print runs on.
    assert_eq!(
        trace.text(),
        concat!(
            "devMotorAsyn::init_record: N:m pasynGenericPointer->read returned drvMotorAsyn::readGenericPointer Uninitialised axis 1",
            "devMotorAsyn::init_controller, N:m failed to set encoder ratio to 1.000000\n",
            "devMotorAsyn::asynCallback: N:m pasyn{Float64,Int32}->write returned drvMotorAsyn::writeFloat64 Uninitialised axis 1\n",
            "devMotorAsyn::asynCallback: N:m pasyn{Float64,Int32}->write returned drvMotorAsyn::writeFloat64 Uninitialised axis 1\n",
        )
    );
    assert_eq!(field(&db, "N:n"), EpicsValue::Double(0.0), "no FLNK");
    assert_eq!(field(&db, "N:m.STAT"), EpicsValue::Short(0));
    assert_eq!(field(&db, "N:m.SEVR"), EpicsValue::Short(0));
}

/// devMotorAsyn.c:756-763: `statusCallback` processes on the `dbScanLockOK`
/// latched at `finishDevSup`, which `iocPause` never clears, so a move
/// started before the pause still completes on the paused IOC.
#[tokio::test]
async fn a_move_completes_while_the_ioc_is_paused() {
    register_controller_port(&open_owner(), "PAUSED", 1, ()).unwrap();
    let motor: Arc<Mutex<dyn asyn_rs::interfaces::motor::AsynMotor>> =
        Arc::new(Mutex::new(motor_rs::sim_motor::SimMotor::new()));
    let AxisSetup {
        axis,
        poll_loop,
        poll_cmd_tx,
    } = MotorBuilder::new(motor)
        .trace_user(asyn("PAUSED", 0))
        .poll_interval(Duration::from_millis(10))
        .build_axis();
    register_axis(axis).unwrap();
    tokio::spawn(poll_loop.run());
    let db = boot(
        r##"
record(motor, "P:m") { field(DTYP, "asynMotor") field(OUT, "@asyn(PAUSED,0)") field(MRES, "0.001") field(VELO, "1") field(FLNK, "P:n") }
record(calc, "P:n") { field(CALC, "A+1") field(INPA, "P:n NPP") }
"##,
    )
    .await;
    db.scan_control().run();
    db.put_record_field_from_ca("P:m", "VAL", EpicsValue::Double(0.2))
        .await
        .unwrap();
    db.scan_control().pause();
    let done = || matches!(field(&db, "P:m.DMOV"), EpicsValue::Short(1));
    for _ in 0..300 {
        if done() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(done(), "the move finished on the paused IOC");
    assert_eq!(field(&db, "P:m.DRBV"), EpicsValue::Double(0.2));
    assert_eq!(field(&db, "P:n"), EpicsValue::Double(1.0), "one FLNK");
    let _ = poll_cmd_tx
        .send(motor_rs::poll_loop::PollCommand::Shutdown)
        .await;
}

/// devMotorAsyn.c:159-162: `dbScanLockOK` is set at `finishDevSup`, so a
/// put before `scanRun` finishes through its callbacks and leaves no move
/// pending to swallow the ones after `scanRun`.
#[tokio::test]
async fn a_put_before_scan_run_completes_its_move() {
    register_controller_port(&open_owner(), "EARLY", 1, ()).unwrap();
    let motor: Arc<Mutex<dyn asyn_rs::interfaces::motor::AsynMotor>> =
        Arc::new(Mutex::new(motor_rs::sim_motor::SimMotor::new()));
    let AxisSetup {
        axis,
        poll_loop,
        poll_cmd_tx,
    } = MotorBuilder::new(motor)
        .trace_user(asyn("EARLY", 0))
        .poll_interval(Duration::from_millis(10))
        .build_axis();
    register_axis(axis).unwrap();
    tokio::spawn(poll_loop.run());
    let db = boot(
        r##"
record(motor, "E:m") { field(DTYP, "asynMotor") field(OUT, "@asyn(EARLY,0)") field(MRES, "0.001") field(VELO, "1") }
"##,
    )
    .await;
    let done = || matches!(field(&db, "E:m.DMOV"), EpicsValue::Short(1));
    let wait = || async {
        for _ in 0..300 {
            if done() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    db.put_record_field_from_ca("E:m", "VAL", EpicsValue::Double(0.2))
        .await
        .unwrap();
    wait().await;
    assert!(done(), "the move finished before scanRun");
    assert_eq!(field(&db, "E:m.DRBV"), EpicsValue::Double(0.2));

    db.scan_control().run();
    db.put_record_field_from_ca("E:m", "VAL", EpicsValue::Double(0.3))
        .await
        .unwrap();
    wait().await;
    assert!(done(), "the move after scanRun finished");
    assert_eq!(field(&db, "E:m.DRBV"), EpicsValue::Double(0.3));
    let _ = poll_cmd_tx
        .send(motor_rs::poll_loop::PollCommand::Shutdown)
        .await;
}

/// devMotorAsyn.c:529-532: `build_trans` returns ERROR without queueing
/// while the record's STAT is COMM_ALARM, so the pass after the one that
/// raised it sends the driver nothing.
#[tokio::test]
async fn a_record_in_comm_alarm_sends_no_command() {
    register_controller_port(&open_owner(), "COMM", 1, ()).unwrap();
    let subscribers = AxisSubscribers::new();
    subscribers.store(StampedStatus {
        seq: 1,
        status: MotorStatus {
            done: true,
            comms_error: true,
            ..MotorStatus::default()
        },
    });
    let (poll_tx, _poll_rx) = mpsc::channel(16);
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
    register_axis(MotorAxis::new(
        asyn("COMM", 0),
        Duration::from_secs(1),
        poll_tx,
        cmd_tx,
        subscribers,
    ))
    .unwrap();
    let db = boot(
        r##"
record(motor, "C:m") { field(DTYP, "asynMotor") field(OUT, "@asyn(COMM,0)") field(MRES, "0.001") field(VELO, "1") }
"##,
    )
    .await;
    db.scan_control().run();
    while cmd_rx.try_recv().is_ok() {}

    // A plain pass raises COMM_ALARM from MSTA.
    db.put_record_field_from_ca("C:m", "PROC", EpicsValue::Char(1))
        .await
        .unwrap();
    assert_eq!(field(&db, "C:m.STAT"), EpicsValue::Short(9), "COMM_ALARM");
    while cmd_rx.try_recv().is_ok() {}

    db.put_record_field_from_ca("C:m", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    let sent = tokio::time::timeout(Duration::from_millis(200), cmd_rx.recv()).await;
    assert!(sent.is_err(), "nothing reaches the driver in COMM_ALARM");
}

/// motordevCom has no COMM_ALARM check: a model-1 record in COMM_ALARM
/// still sends its move, as C retries on a dead line until RTRY runs out.
#[tokio::test]
async fn a_model1_record_in_comm_alarm_still_sends_its_move() {
    let subscribers = AxisSubscribers::new();
    subscribers.store(StampedStatus {
        seq: 1,
        status: MotorStatus {
            done: true,
            comms_error: true,
            ..MotorStatus::default()
        },
    });
    let (poll_tx, _poll_rx) = mpsc::channel(16);
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
    register_axis(MotorAxis::new(
        TraceUser::Model1 {
            device: MODEL1.choice,
            card: 9,
            signal: 0,
        },
        Duration::from_secs(1),
        poll_tx,
        cmd_tx,
        subscribers,
    ))
    .unwrap();
    let db = boot(
        r##"
record(motor, "C1:m") { field(DTYP, "TestModel1") field(OUT, "#C9 S0 @") field(MRES, "0.001") field(VELO, "1") }
"##,
    )
    .await;
    db.scan_control().run();
    while cmd_rx.try_recv().is_ok() {}

    db.put_record_field_from_ca("C1:m", "PROC", EpicsValue::Char(1))
        .await
        .unwrap();
    assert_eq!(field(&db, "C1:m.STAT"), EpicsValue::Short(9), "COMM_ALARM");
    while cmd_rx.try_recv().is_ok() {}

    db.put_record_field_from_ca("C1:m", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    let sent = tokio::time::timeout(Duration::from_millis(200), cmd_rx.recv())
        .await
        .expect("the move reaches the driver")
        .unwrap();
    assert!(sent.commands.iter().any(|c| c.is_move()), "{sent:?}");
}

/// motorRecord.cc:1503-1507: process_exit runs alarm_sub and monitor()'s
/// recGblResetAlarms on the move-start pass too, not only once DMOV is back.
/// alarm_sub clears the MSTA comm bit it raises on, so a move start as the
/// first pass commits COMM_ALARM and one after a raising pass resets it.
#[tokio::test]
async fn a_move_start_pass_commits_and_resets_the_alarm() {
    for port in ["MVAL0", "MVAL1"] {
        register_controller_port(&open_owner(), port, 1, ()).unwrap();
        let subscribers = AxisSubscribers::new();
        subscribers.store(StampedStatus {
            seq: 1,
            status: MotorStatus {
                done: true,
                comms_error: true,
                ..MotorStatus::default()
            },
        });
        let (poll_tx, _poll_rx) = mpsc::channel(16);
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        register_axis(MotorAxis::new(
            asyn(port, 0),
            Duration::from_secs(1),
            poll_tx,
            cmd_tx,
            subscribers,
        ))
        .unwrap();
    }
    let db = boot(
        r##"
record(motor, "A:m0") { field(DTYP, "asynMotor") field(OUT, "@asyn(MVAL0,0)") field(MRES, "0.001") field(VELO, "1") }
record(motor, "A:m1") { field(DTYP, "asynMotor") field(OUT, "@asyn(MVAL1,0)") field(MRES, "0.001") field(VELO, "1") }
"##,
    )
    .await;
    db.scan_control().run();

    db.put_record_field_from_ca("A:m0", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    assert_eq!(field(&db, "A:m0.DMOV"), EpicsValue::Short(0));
    assert_eq!(field(&db, "A:m0.STAT"), EpicsValue::Short(9), "raised");

    db.put_record_field_from_ca("A:m1", "PROC", EpicsValue::Char(1))
        .await
        .unwrap();
    assert_eq!(field(&db, "A:m1.STAT"), EpicsValue::Short(9));
    db.put_record_field_from_ca("A:m1", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    assert_eq!(field(&db, "A:m1.DMOV"), EpicsValue::Short(0));
    assert_eq!(field(&db, "A:m1.STAT"), EpicsValue::Short(0), "reset");
}

/// motorRecord.cc:648-651 returns before set_dial_highlimit/lowlimit
/// (717-718): a failed dset init keeps the raw limits
/// check_speed_and_resolution seeded with |MRES| (3993-4017). The
/// devMotorAsyn `bad:` arm returns 0, so that record gets 717-718's signed
/// ones.
#[tokio::test]
async fn only_a_record_past_dset_init_resets_its_limits() {
    let limits = r#"field(MRES, "-1") field(DHLM, "10") field(DLLM, "-10")"#;
    let db = boot(&format!(
        r##"
record(motor, "L:failed") {{ field(DTYP, "TestModel1") field(OUT, "#C9 S0 @") {limits} }}
record(motor, "L:bad") {{ field(DTYP, "asynMotor") field(OUT, "@NEVER_CREATED") {limits} }}
"##
    ))
    .await;
    let limits = |pv: &str, fields: &[&str]| -> Vec<EpicsValue> {
        fields
            .iter()
            .map(|f| field(&db, &format!("{pv}.{f}")))
            .collect()
    };
    let d = EpicsValue::Double;
    assert_eq!(limits("L:failed", &["RHLM", "RLLM"]), [d(-10.0), d(10.0)]);
    assert_eq!(
        limits("L:bad", &["HLM", "LLM", "RHLM", "RLLM"]),
        [d(10.0), d(-10.0), d(10.0), d(-10.0)]
    );
}
