//! C's `mqttDriverConfigure` is silent on success (drvMqtt.cpp:737-739), and
//! the Connected PV its rs-only 5th argument creates is no reason to speak:
//! `dbl` lists it. The command prints to the process's stdout, so the IOC runs
//! in a child re-exec of this test binary whose output the parent reads.

#![cfg(feature = "ioc")]

const SCRIPT: &str = "MQ5_SCRIPT";

#[test]
fn a_connected_pv_configure_prints_only_what_dbl_lists() {
    let out = run_ioc(
        "a_connected_pv_configure_prints_only_what_dbl_lists",
        "mqttDriverConfigure(\"MQ5\", \"tcp://127.0.0.1:1\", \"mq5-client\", 0, \"MQ5:CONNECTED\")\ndbl\n",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "child exited {}: {stdout}",
        out.status
    );
    assert!(
        stdout.lines().any(|l| l == "MQ5:CONNECTED"),
        "dbl lists the Connected PV: {stdout}"
    );
    assert!(!stdout.contains("connected PV"), "{stdout}");
}

/// A Connected PV name the loader refuses is the command's error, which the
/// shell prints; nothing prints it before that.
#[test]
fn a_refused_connected_pv_name_is_reported_once() {
    let out = run_ioc(
        "a_refused_connected_pv_name_is_reported_once",
        "mqttDriverConfigure(\"MQ6\", \"tcp://127.0.0.1:1\", \"mq6-client\", 0, \"MQ6 CONNECTED\")\n",
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr.matches("mqttDriverConfigure: ").count(),
        1,
        "{stderr}"
    );
}

/// Re-exec this binary as the IOC running `script`, from inside `test`.
fn run_ioc(test: &str, script: &str) -> std::process::Output {
    if let Ok(script) = std::env::var(SCRIPT) {
        be_the_ioc(&script);
    }
    let path = std::env::temp_dir().join(format!("{test}-{}.cmd", std::process::id()));
    std::fs::write(&path, script).expect("write the child's st.cmd");
    let out =
        std::process::Command::new(std::env::current_exe().expect("this test binary's own path"))
            .args(["--exact", test, "--nocapture", "--test-threads=1"])
            .env(SCRIPT, &path)
            .output()
            .expect("run the IOC child");
    let _ = std::fs::remove_file(&path);
    out
}

fn be_the_ioc(script: &str) -> ! {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");
    let script = script.to_owned();
    let outcome = runtime.block_on(async move {
        let handle = epics_base_rs::runtime::task::runtime_handle();
        let app = epics_ca_rs::server::ioc_app::IocApplication::new();
        let app = asyn_rs::adapter::register_asyn_device_support(app);
        let app = mqtt_rs::ioc::register_mqtt_commands(app, handle);
        app.startup_script(&script)
            .run(|_config| async { Ok(()) })
            .await
    });
    drop(runtime);
    std::process::exit(if outcome.is_ok() { 0 } else { 2 })
}
