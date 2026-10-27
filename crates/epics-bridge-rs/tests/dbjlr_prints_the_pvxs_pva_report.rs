//! `dbjlr` on a pva link prints pvxs `pva_report`
//! (`ioc/pvalink_jlif.cpp:228-282`) through `epicsStdio.h`, so `> f` takes
//! it. softIocPVX 1.5.3 on these links, with `REM` served by a second IOC
//! that is stopped (SIGSTOP) across the puts and then killed, and `NOPE:Y`
//! served late by a third:
//!
//! ```text
//! L 0  '      'pva': SRC|.value CP MS conn=T'
//! L 1  '      'pva': SRC|.value CP MS Q=5 pipe=F defer=F time=F retry=F atomic=F morder=0 conn=T #disconn=0'
//! N 1  '      'pva': NOPE:X Def NMS Q=4 pipe=F defer=F time=F retry=F atomic=F morder=0 conn=F #disconn=0'
//! F 1  '      'pva': SRC NPP MSI Q=2 pipe=T defer=T time=T retry=T atomic=T morder=3 conn=T #disconn=0'
//! O 1  '      'pva': SRC PP NMS Q=4 pipe=F defer=F time=F retry=F atomic=F morder=0 conn=T #disconn=0'
//! WR 1 '      'pva': REM Def NMS Q=4 pipe=F defer=F time=F retry=F atomic=F morder=0 conn=T Put #disconn=0'
//! WR 0 '      'pva': REM Def NMS conn=T Put'
//! R 1  '      'pva': REM Def NMS Q=4 pipe=F defer=F time=F retry=F atomic=F morder=0 conn=T Put #disconn=0'
//! WT 1 '      'pva': NOPE:Y Def NMS Q=4 pipe=F defer=F time=F retry=T atomic=F morder=0 conn=F Put #disconn=0'
//! WRT 1 '      'pva': REM Def NMS Q=4 pipe=F defer=F time=F retry=T atomic=F morder=0 conn=T Put #disconn=0'
//! R 1  '      'pva': REM Def NMS Q=4 pipe=F defer=F time=F retry=F atomic=F morder=0 conn=F #disconn=1'
//! R 0  '      'pva': REM Def NMS conn=F'
//! WRT 1 '      'pva': REM Def NMS Q=4 pipe=F defer=F time=F retry=T atomic=F morder=0 conn=F #disconn=1'
//! WT 1 '      'pva': NOPE:Y Def NMS Q=4 pipe=F defer=F time=F retry=T atomic=F morder=0 conn=T #disconn=0'
//! ```
//!
//! The disconnect cancels WRT's put and its value with it; WT's put, held
//! while NOPE:Y had no server, goes out when it connects (NOPE:Y = 6).
//!
//! The IOCs end in the iocsh REPL, so each runs in a child (this binary
//! re-run on [`ioc_child`]) whose piped stdin types the commands.
#![cfg(all(feature = "pvalink", feature = "qsrv", tokio_backend, unix))]

mod common;

use common::{CHILD, Shell, signal, spawn_ioc};
use named_port::HeldPort;

/// The child half; a no-op unless the parent set [`CHILD`].
#[test]
fn ioc_child() {
    common::ioc_child();
}

const REMOTE: &str = r#"record(ai, "REM") { field(VAL, "7") }"#;

const LATE: &str = r#"record(ai, "NOPE:Y") { field(VAL, "1") }"#;

const LOCAL: &str = r#"
record(ai, "SRC") { field(VAL, "4") }
record(ai, "L") { field(INP, {pva:{pv:"SRC", field:"value", proc:"CP", sevr:"MS", Q:5}}) }
record(ai, "N") { field(INP, {pva:"NOPE:X"}) }
record(ao, "O") { field(OUT, {pva:{pv:"SRC", proc:true}}) }
record(ai, "F") { field(INP, {pva:{pv:"SRC", Q:2, pipeline:true, defer:true, time:true, retry:true, atomic:true, monorder:3, sevr:"MSI", proc:"NPP"}}) }
record(ai, "K") { field(INP, {calc:{expr:"A", args:[{pva:"SRC"}]}}) }
record(ai, "R") { field(INP, {pva:"REM"}) }
record(ao, "WR") { field(OUT, {pva:"REM"}) }
record(ao, "WRT") { field(OUT, {pva:{pv:"REM", retry:true}}) }
record(ao, "WT") { field(OUT, {pva:{pv:"NOPE:Y", retry:true}}) }
"#;

/// `dbjlr` through a [`Shell`].
trait Dbjlr {
    fn report(&mut self, rec: &str, level: u32) -> String;
    fn report_until(&mut self, rec: &str, level: u32, tail: &str) -> String;
}

impl Dbjlr for Shell<'_> {
    fn report(&mut self, rec: &str, level: u32) -> String {
        self.capture(&format!("dbjlr {rec} {level}"))
    }

    fn report_until(&mut self, rec: &str, level: u32, tail: &str) -> String {
        self.capture_until(&format!("dbjlr {rec} {level}"), tail)
    }
}

#[test]
fn dbjlr_prints_the_pvxs_pva_report() {
    if std::env::var_os(CHILD).is_some() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (remote_port, local_port, late_port) = (HeldPort::tcp(), HeldPort::tcp(), HeldPort::tcp());
    let mut remote = spawn_ioc(dir.path(), "remote", REMOTE, &remote_port, "");
    let mut local = spawn_ioc(
        dir.path(),
        "local",
        LOCAL,
        &local_port,
        &format!(
            "127.0.0.1:{} 127.0.0.1:{} 127.0.0.1:{}",
            remote_port.port, local_port.port, late_port.port
        ),
    );
    let mut sh = Shell {
        stdin: local.stdin.take().unwrap(),
        dir: dir.path(),
        tag: "r",
        n: 0,
    };
    let q4 = "Q=4 pipe=F defer=F time=F retry=F atomic=F morder=0";

    let wr = sh.report_until("WR", 1, "conn=T #disconn=0");
    let r = sh.report_until("R", 1, "conn=T #disconn=0");
    let l1 = sh.report_until("L", 1, "conn=T #disconn=0");
    let f1 = sh.report_until("F", 1, "conn=T #disconn=0");
    let l0 = sh.report("L", 0);
    let n1 = sh.report("N", 1);
    let k2 = sh.report("K", 2);
    sh.line("dbpf O 3");
    let o1 = sh.report_until("O", 1, "conn=T #disconn=0");

    signal(&remote, "-STOP");
    sh.line("dbpf WR 5");
    sh.line("dbpf WT 6");
    let wr_put = sh.report_until("WR", 1, "conn=T Put #disconn=0");
    let wr_put0 = sh.report("WR", 0);
    // R's input link and WR's output link share REM's one channel.
    let r_put = sh.report_until("R", 1, "conn=T Put #disconn=0");
    let wt_put = sh.report_until("WT", 1, "conn=F Put #disconn=0");
    signal(&remote, "-CONT");
    let wr_done = sh.report_until("WR", 1, "conn=T #disconn=0");

    signal(&remote, "-STOP");
    sh.line("dbpf WRT 8");
    let wrt_put = sh.report_until("WRT", 1, "conn=T Put #disconn=0");
    remote.kill().unwrap();
    let _ = remote.wait();
    let r_gone = sh.report_until("R", 1, "conn=F #disconn=1");
    let r0_gone = sh.report("R", 0);
    let wrt_gone = sh.report_until("WRT", 1, "conn=F #disconn=1");

    let mut late = spawn_ioc(dir.path(), "late", LATE, &late_port, "");
    let mut late_sh = Shell {
        stdin: late.stdin.take().unwrap(),
        dir: dir.path(),
        tag: "l",
        n: 0,
    };
    let wt_back = sh.report_until("WT", 1, "conn=T #disconn=0");
    let nope_y = late_sh.capture_until("dbgf NOPE:Y", "DBF_DOUBLE:         6         ");
    late_sh.line("exit");
    drop(late_sh);
    let _ = late.wait();
    sh.line("exit");
    drop(sh);
    let _ = local.wait();

    assert_eq!(
        [
            l0, l1, n1, f1, k2, o1, wr, r, wr_put, wr_put0, r_put, wt_put, wr_done, wrt_put,
            r_gone, r0_gone, wrt_gone, wt_back, nope_y
        ],
        [
            "      'pva': SRC|.value CP MS conn=T\n".to_string(),
            format!("      'pva': SRC|.value CP MS {} conn=T #disconn=0\n", q4.replace("Q=4", "Q=5")),
            format!("      'pva': NOPE:X Def NMS {q4} conn=F #disconn=0\n"),
            "      'pva': SRC NPP MSI Q=2 pipe=T defer=T time=T retry=T atomic=T morder=3 conn=T #disconn=0\n"
                .to_string(),
            // pvxs 1.5.3 never opens a pva link in a calc argument
            // (pvaOpenLink: "DBLINK* corrupt"), so it reads 0 and prints No
            // Channel; rs opens it, the defect not copied.
            format!("          'pva': SRC Def NMS {q4} conn=T #disconn=0\n"),
            format!("      'pva': SRC PP NMS {q4} conn=T #disconn=0\n"),
            format!("      'pva': REM Def NMS {q4} conn=T #disconn=0\n"),
            format!("      'pva': REM Def NMS {q4} conn=T #disconn=0\n"),
            format!("      'pva': REM Def NMS {q4} conn=T Put #disconn=0\n"),
            "      'pva': REM Def NMS conn=T Put\n".to_string(),
            format!("      'pva': REM Def NMS {q4} conn=T Put #disconn=0\n"),
            format!(
                "      'pva': NOPE:Y Def NMS {} conn=F Put #disconn=0\n",
                q4.replace("retry=F", "retry=T")
            ),
            format!("      'pva': REM Def NMS {q4} conn=T #disconn=0\n"),
            format!(
                "      'pva': REM Def NMS {} conn=T Put #disconn=0\n",
                q4.replace("retry=F", "retry=T")
            ),
            format!("      'pva': REM Def NMS {q4} conn=F #disconn=1\n"),
            "      'pva': REM Def NMS conn=F\n".to_string(),
            format!(
                "      'pva': REM Def NMS {} conn=F #disconn=1\n",
                q4.replace("retry=F", "retry=T")
            ),
            format!(
                "      'pva': NOPE:Y Def NMS {} conn=T #disconn=0\n",
                q4.replace("retry=F", "retry=T")
            ),
            "DBF_DOUBLE:         6         \n".to_string(),
        ]
    );
}
