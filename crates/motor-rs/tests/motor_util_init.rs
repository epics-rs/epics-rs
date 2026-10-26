//! `motorUtilInit`'s refusals go to the console past any redirect, as
//! motorUtil.cc's libc `printf`s do, and a task that finds no motorUtil.db
//! logs what C's logs and leaves `printChIDlist` the blank `motorArray` C's
//! leaves.
//!
//! libtest captures `println!` inside the test process, so the shell runs in
//! a child copy of this binary and the parent reads the child's stdout.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::registry::open_owner;
use epics_base_rs::runtime::log::{errlog_add_listener, errlog_flush};
use epics_base_rs::runtime::task::{BlockingBridge, set_thread_ok_to_block};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::iocsh::IocShell;
use motor_rs::builder::{AxisSetup, MotorBuilder};
use motor_rs::devices::{device_support_factory, devices, register_axis, register_controller_port};
use motor_rs::trace::TraceUser;

const CHILD: &str = "MOTOR_UTIL_INIT_CHILD";

#[test]
fn refusals_stay_on_the_console() {
    if std::env::var(CHILD).is_ok() {
        return;
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "init_child", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lines: Vec<_> = stdout
        .lines()
        .filter_map(|l| l.find("motorUtil").map(|i| &l[i..]))
        .collect();
    assert_eq!(
        lines,
        [
            format!(
                "motorUtilInit: Prefix {} has more than 53 characters. Exiting",
                "P".repeat(54)
            ),
            "motorUtil already initialized. Exiting".to_string(),
        ]
    );
}

/// The child's entry: runs only when the parent sets [`CHILD`].
#[epics_base_rs::epics_test]
async fn init_child() {
    if std::env::var(CHILD).is_err() {
        return;
    }
    register_controller_port(&open_owner(), "MUI", 1, ()).unwrap();
    let motor: Arc<Mutex<dyn asyn_rs::interfaces::motor::AsynMotor>> =
        Arc::new(Mutex::new(motor_rs::sim_motor::SimMotor::new()));
    let AxisSetup {
        axis, poll_loop, ..
    } = MotorBuilder::new(motor)
        .trace_user(TraceUser::Asyn {
            port: "MUI".into(),
            addr: 0,
        })
        .build_axis();
    register_axis(axis).unwrap();
    poll_loop.start();
    let (name, factory) = motor_rs::motor_record_factory();
    let ioc = IocBuilder::new()
        .register_record_type(name, factory)
        .register_dynamic_device_support(&devices(), device_support_factory())
        .db_string(
            r#"record(motor, "MUI:m") { field(DTYP, "asynMotor") field(OUT, "@asyn(MUI,0)") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = Arc::clone(ioc.database());
    assert!(ioc.run().await);

    let log = Arc::new(Mutex::new(String::new()));
    let l = log.clone();
    errlog_add_listener(move |m| l.lock().unwrap().push_str(m));
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().display().to_string().replace('\\', "/");
    let long = "P".repeat(54);
    let most = "Q".repeat(53);
    let lines = [
        format!("motorUtilInit(\"{long}\")"),
        format!("motorUtilInit(\"{most}\")"),
        "motorUtilInit(\"X:\")".to_string(),
    ];
    let failed = format!(
        "motorUtil.cc: getChID({most}moving.VAL) error: 80\n\
         motorUtil.cc: getChID({most}alldone.VAL) error: 80\n\
         motorUtil.cc: getChID({most}movingDiff.VAL) error: 80\n\
         Failed to connect to {most}moving or {most}alldone or {most}movingDiff.\n\
         Check prefix matches Db\n"
    );
    let bridge = BlockingBridge::capture();
    let (shell_log, shell_failed) = (log.clone(), failed.clone());
    let printed = std::thread::spawn(move || {
        set_thread_ok_to_block(true);
        let shell = IocShell::new(db, bridge);
        for c in motor_rs::motor_util::motor_util_commands() {
            shell.register(c);
        }
        let printed: Vec<String> = lines
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let file = format!("{d}/{i}.txt");
                shell.execute_line(format!("{line} > {file}")).unwrap();
                std::fs::read_to_string(file).unwrap()
            })
            .collect();
        // The task logs from its own thread.
        for _ in 0..500 {
            errlog_flush();
            if shell_log.lock().unwrap().contains("Check prefix") {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(*shell_log.lock().unwrap(), shell_failed);
        shell.execute_line("printChIDlist").unwrap();
        shell.execute_line("listMovingMotors").unwrap();
        errlog_flush();
        printed
    })
    .join()
    .unwrap();
    assert_eq!(printed, ["", "", ""]);
    assert_eq!(
        log.lock().unwrap().strip_prefix(&failed).unwrap(),
        "i = 0,\tname = \tchid_dmov = (nil)\tchid_stop =                (nil)\
         \tin_motion = 0\tindex = 0\n\
         chid_allstop = (nil)\nchid_alldone = (nil)\nchid_moving = (nil)\n\
         \nThe following motors are moving:\n"
    );
}
