//! asyn's `printf`s go to the printing thread's stdout (`asynDriver.h`
//! includes `epicsStdio.h`), so an iocsh `> file` takes what the command's
//! own thread prints, and what a port thread prints stays on the console.
//! The expected files are C's (asyn's `test` IOC) for the same script.
#![cfg(feature = "epics")]

use asyn_rs::adapter::register_asyn_device_support;
use epics_base_rs::server::ioc_app::IocApplication;

const CHILD: &str = "ASYN_PRINTS_FOLLOW_THE_REDIRECT_SCRIPT";

/// The st.cmd, in the child only.
#[epics_base_rs::epics_test]
async fn child_script() {
    let Some(script) = std::env::var_os(CHILD) else {
        return;
    };
    register_asyn_device_support(IocApplication::new().port(0))
        .startup_script(script.to_str().unwrap())
        .run(|_| async { Ok(()) })
        .await
        .unwrap();
}

/// Runs `script` (`{d}` is the directory) and returns each named file.
fn redirected(files: &[(&str, &str)], script: &str, names: &[&str]) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    for (name, text) in files {
        std::fs::write(d.join(name), text).unwrap();
    }
    let d = d.display().to_string();
    std::fs::write(format!("{d}/st.cmd"), script.replace("{d}", &d)).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_script", "--nocapture", "--test-threads=1"])
        .env(CHILD, format!("{d}/st.cmd"))
        .current_dir(&d)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    names
        .iter()
        .map(|name| std::fs::read_to_string(format!("{d}/{name}")).unwrap())
        .collect()
}

/// asynShellCommands.c: `asynReport` prints on the shell thread, and the
/// option callbacks of a canBlock port print on its port thread.
#[test]
fn the_shell_command_lines_follow_their_thread() {
    let files = redirected(
        &[],
        "drvAsynIPPortConfigure(\"P\",\"127.0.0.1:1\",0,1,0)\n\
         drvAsynIPPortConfigure(\"C\",\"127.0.0.1:1 COM\",0,1,0)\n\
         asynReport 1 P > {d}/report.txt\n\
         asynSetOption C 0 ixon Y > {d}/set.txt\n\
         asynShowOption C 0 baud > {d}/show.txt\n",
        &["report.txt", "set.txt", "show.txt"],
    );
    let report = concat!(
        "P multiDevice:No canBlock:Yes autoConnect:No\n",
        "    enabled:Yes connected:No numberConnects 0\n",
        "    nDevices 0 nQueued 0 blocked:No\n",
        "    asynManagerLock:No synchronousLock:No\n",
        "    exceptionActive:No exceptionUsers 1 exceptionNotifys 0\n",
        "    traceMask:0x1 traceIOMask:0x0 traceInfoMask:0x1\n",
        "    Port 127.0.0.1:1: Disconnected\n",
    );
    assert_eq!(files, [report, "", ""]);
}

/// asynInterposeCom.c:834-836, :865-867: the COM layer extends the IP
/// port's asynOption, and configure's `restoreSettings` fails on a port that
/// has never connected; both on the configuring thread.
#[test]
fn the_com_interpose_lines_follow_the_redirect() {
    let files = redirected(
        &[],
        "drvAsynIPPortConfigure(\"C\",\"127.0.0.1:1 COM\",0,1,0) > {d}/c.txt\n",
        &["c.txt"],
    );
    assert_eq!(
        files,
        [
            "INFO -- asynInterposeCOM options extending and perhaps overriding those of lower interface.\n\
          WARNING -- Can't set serial port parameters: 127.0.0.1:1 COM disconnected\n"
        ]
    );
}

/// asynManager.c:2056: `registerPort`'s refusal of a taken name, on the
/// configuring thread.
#[test]
fn the_register_port_refusal_follows_the_redirect() {
    let files = redirected(
        &[],
        "drvAsynIPPortConfigure(\"P\",\"127.0.0.1:1\",0,1,0)\n\
         drvAsynIPPortConfigure(\"P\",\"127.0.0.1:1\",0,1,0) > {d}/dup.txt\n",
        &["dup.txt"],
    );
    assert_eq!(
        files,
        ["asynCommon:registerDriver P already registered\n\
          drvAsynIPPortConfigure: Can't register myself.\n"]
    );
}

/// asynPortDriver.cpp:4036-4040: the constructor's `registerPort` refusal,
/// after registerPort's own line, on the constructing thread.
#[test]
fn the_port_driver_refusal_follows_the_redirect() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dup.txt");
    let _first = asyn_rs::manager::claim_port_driver("A").unwrap();
    let second = epics_libcom_rs::runtime::stdio::redirect_stdout(
        std::fs::File::create(&path).unwrap(),
        || asyn_rs::manager::claim_port_driver("A"),
    );
    assert!(second.is_err());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "asynCommon:registerDriver A already registered\n\
         asynPortDriver:asynPortDriver ERROR: Can't register port: A\n"
    );
}

/// drvAsynIPServerPort.c:439-443: `createServerSocket`'s lines, on the
/// configuring thread.
#[test]
fn the_server_socket_lines_follow_the_redirect() {
    let files = redirected(
        &[],
        "drvAsynIPServerPortConfigure(\"S\",\"127.0.0.1:0\",1,0,0,0) > {d}/srv.txt\n",
        &["srv.txt"],
    );
    assert_eq!(files, ["serverAddr: 127.0.0.1:0\nserverPort: 0\n"]);
}

/// devAsynInt32.c:227 and the other dsets' init lines print on iocInit's
/// thread.
#[test]
fn the_dset_init_lines_follow_the_redirect() {
    let files = redirected(
        &[(
            "a.db",
            "record(ai, \"DU:BAD\") { field(DTYP, \"asynInt32\") field(INP, \"@asyn(P\") }\n",
        )],
        "drvAsynIPPortConfigure(\"P\",\"127.0.0.1:1\",0,1,0)\n\
         dbLoadRecords(\"{d}/a.db\")\n\
         iocInit > {d}/init.txt\n",
        &["init.txt"],
    );
    let lines: Vec<&str> = files[0].lines().filter(|l| !l.starts_with("##")).collect();
    assert_eq!(
        lines,
        ["DU:BAD devAsynInt32::initCommon  invalid INST_IO \
          Must be asynMask(<port> <addr> <mask> <timeout>)userParams"]
    );
}
