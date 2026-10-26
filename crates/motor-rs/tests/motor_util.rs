//! `motorUtilInit` on an IOC of two sim motors, one with an alias, and
//! `$(MOTOR)/db/motorUtil.db`: `alldone`, `moving` and `movingDiff` follow
//! the motors' `.DMOV` and `allstop` stops every moving one, as
//! motorUtil.cc's handlers do.
//!
//! motorUtil's state is the process's, as C's statics are: one IOC here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::registry::open_owner;
use epics_base_rs::runtime::log::{errlog_add_listener, errlog_flush};
use epics_base_rs::runtime::task::{BlockingBridge, set_thread_ok_to_block, sleep};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::types::EpicsValue;
use motor_rs::builder::{AxisSetup, MotorBuilder};
use motor_rs::devices::{device_support_factory, devices, register_axis, register_controller_port};
use motor_rs::trace::TraceUser;

fn sim(port: &str) {
    register_controller_port(&open_owner(), port, 1, ()).unwrap();
    let motor: Arc<Mutex<dyn asyn_rs::interfaces::motor::AsynMotor>> =
        Arc::new(Mutex::new(motor_rs::sim_motor::SimMotor::new()));
    let AxisSetup {
        axis, poll_loop, ..
    } = MotorBuilder::new(motor)
        .trace_user(TraceUser::Asyn {
            port: port.into(),
            addr: 0,
        })
        .poll_interval(Duration::from_millis(10))
        .build_axis();
    register_axis(axis).unwrap();
    poll_loop.start();
}

/// One iocsh line, on a thread of its own as the shell runs.
fn iocsh(db: &Arc<PvDatabase>, line: &str) {
    let (db, line) = (db.clone(), line.to_string());
    let bridge = BlockingBridge::capture();
    std::thread::spawn(move || {
        set_thread_ok_to_block(true);
        let shell = IocShell::new(db, bridge);
        for c in motor_rs::motor_util::motor_util_commands() {
            shell.register(c);
        }
        shell.execute_line_reported(&line)
    })
    .join()
    .unwrap()
    .unwrap();
}

/// Wait for `pv` to read `want`.
async fn until(db: &PvDatabase, pv: &str, want: EpicsValue) {
    for _ in 0..1000 {
        if db.get_pv(pv).unwrap() == want {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("{pv} is {:?}, not {want:?}", db.get_pv(pv).unwrap());
}

fn diff(s: &str) -> EpicsValue {
    let mut b = s.as_bytes().to_vec();
    b.push(0);
    EpicsValue::CharArray(b.into())
}

/// What `line` sends to errlog.
fn errlog_of(db: &Arc<PvDatabase>, line: &str) -> String {
    let out = Arc::new(Mutex::new(String::new()));
    let o = out.clone();
    let id = errlog_add_listener(move |m| o.lock().unwrap().push_str(m));
    iocsh(db, line);
    errlog_flush();
    epics_base_rs::runtime::log::errlog_remove_listener(id);
    out.lock().unwrap().clone()
}

#[epics_base_rs::epics_test]
async fn alldone_moving_and_allstop_follow_the_motors() {
    sim("MU1");
    sim("MU2");
    let (name, factory) = motor_rs::motor_record_factory();
    let p: HashMap<String, String> = [("P".to_string(), "MU:".to_string())].into();
    let util = format!("{}/db/motorUtil.db", motor_rs::MOTOR_DIR);
    let ioc = IocBuilder::new()
        .register_record_type(name, factory)
        .register_dynamic_device_support(&devices(), device_support_factory())
        .db_string(
            r#"
record(motor, "MU:m1") { field(DTYP, "asynMotor") field(OUT, "@asyn(MU1,0)") field(MRES, "0.001") field(VELO, "1") }
record(motor, "MU:m2") { field(DTYP, "asynMotor") field(OUT, "@asyn(MU2,0)") field(MRES, "0.001") field(VELO, "1") }
alias("MU:m1", "MU:m1a")
"#,
            &HashMap::new(),
        )
        .unwrap()
        .db_file(&util, &p)
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = Arc::clone(ioc.database());
    assert!(ioc.run().await);
    iocsh(&db, r#"motorUtilInit("MU:")"#);
    // allstop's monitor is the task's last: its first value resets it.
    db.put_record_field_from_ca("MU:allstop", "VAL", EpicsValue::Short(1))
        .await
        .unwrap();
    until(&db, "MU:allstop", EpicsValue::Enum(0)).await;
    assert_eq!(db.get_pv("MU:alldone").unwrap(), EpicsValue::Enum(1));
    assert_eq!(db.get_pv("MU:moving").unwrap(), EpicsValue::Long(0));

    // The alias is not a second motor: one move counts one.
    db.put_record_field_from_ca("MU:m1", "VAL", EpicsValue::Double(3.0))
        .await
        .unwrap();
    until(&db, "MU:moving", EpicsValue::Long(1)).await;
    until(&db, "MU:alldone", EpicsValue::Enum(0)).await;
    assert_eq!(db.get_pv("MU:movingDiff").unwrap(), diff("+MU:m1"));
    db.put_record_field_from_ca("MU:m2", "VAL", EpicsValue::Double(3.0))
        .await
        .unwrap();
    until(&db, "MU:moving", EpicsValue::Long(2)).await;
    assert_eq!(db.get_pv("MU:movingDiff").unwrap(), diff("+MU:m2"));
    assert_eq!(
        errlog_of(&db, "listMovingMotors"),
        "\nThe following motors are moving:\nMU:m1, index = 0\nMU:m2, index = 1\n"
    );
    let chids = errlog_of(&db, "printChIDlist");
    let lines: Vec<&str> = chids.lines().collect();
    assert_eq!(lines.len(), 5, "{chids}");
    for (i, (m, l)) in ["MU:m1", "MU:m2"].iter().zip(&lines).enumerate() {
        let (dmov, rest) = l
            .strip_prefix(&format!("i = {i},\tname = {m}\tchid_dmov = 0x"))
            .and_then(|r| r.split_once("\tchid_stop =                0x"))
            .unwrap_or_else(|| panic!("{l}"));
        let (stop, tail) = rest.split_once('\t').unwrap();
        assert!(u64::from_str_radix(dmov, 16).is_ok() && u64::from_str_radix(stop, 16).is_ok());
        assert_eq!(tail, format!("in_motion = 1\tindex = {i}"));
    }
    for (l, what) in lines[2..].iter().zip(["allstop", "alldone", "moving"]) {
        assert!(l.starts_with(&format!("chid_{what} = 0x")), "{l}");
    }

    // allstop stops both well short of their 3 s moves.
    db.put_record_field_from_ca("MU:allstop", "VAL", EpicsValue::Short(1))
        .await
        .unwrap();
    until(&db, "MU:moving", EpicsValue::Long(0)).await;
    until(&db, "MU:alldone", EpicsValue::Enum(1)).await;
    until(&db, "MU:allstop", EpicsValue::Enum(0)).await;
    for m in ["MU:m1", "MU:m2"] {
        assert_eq!(
            db.get_pv(format!("{m}.DMOV")).unwrap(),
            EpicsValue::Short(1)
        );
        let at = db.get_pv(format!("{m}.DRBV")).unwrap().to_f64().unwrap();
        assert!(at < 2.0, "{m} ran on to {at}");
    }
    assert!(
        [diff("-MU:m1"), diff("-MU:m2")].contains(&db.get_pv("MU:movingDiff").unwrap()),
        "{:?}",
        db.get_pv("MU:movingDiff").unwrap()
    );

    // A move that ends on its own.
    let to = db.get_pv("MU:m1.DRBV").unwrap().to_f64().unwrap() + 0.3;
    db.put_record_field_from_ca("MU:m1", "VAL", EpicsValue::Double(to))
        .await
        .unwrap();
    until(&db, "MU:moving", EpicsValue::Long(1)).await;
    assert_eq!(db.get_pv("MU:movingDiff").unwrap(), diff("+MU:m1"));
    until(&db, "MU:moving", EpicsValue::Long(0)).await;
    assert_eq!(db.get_pv("MU:alldone").unwrap(), EpicsValue::Enum(1));
    assert_eq!(db.get_pv("MU:movingDiff").unwrap(), diff("-MU:m1"));
    let at = db.get_pv("MU:m1.DRBV").unwrap().to_f64().unwrap();
    assert!((at - to).abs() < 0.001, "{at} {to}");
    assert_eq!(
        errlog_of(&db, "listMovingMotors"),
        "\nThe following motors are moving:\n"
    );
}
