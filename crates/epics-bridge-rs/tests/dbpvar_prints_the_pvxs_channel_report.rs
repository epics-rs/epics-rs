//! `dbpvar` prints pvxs `dbpvxr` (`ioc/pvalink.cpp:184-316`). The expected
//! text is softIocPVX 1.5.3 on the same links, with `REM` served by a second
//! IOC that is stopped (SIGSTOP) across the puts and then killed, and
//! `NOPE:Y` served late by a third.
//! `E` and `G` name never-served channels of 28 and 29 bytes: `%28s ` and
//! `%s\t` (`ioc/pvalink.cpp:244-248`).
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
record(ai, "R") { field(INP, {pva:"REM"}) }
record(ao, "WR") { field(OUT, {pva:"REM"}) }
record(ao, "WRT") { field(OUT, {pva:{pv:"REM", retry:true}}) }
record(ao, "WT") { field(OUT, {pva:{pv:"NOPE:Y", retry:true}}) }
record(ai, "E") { field(INP, {pva:"NOPE:EDGE:TWENTY:EIGHT:BYTES"}) }
record(ai, "G") { field(INP, {pva:"NOPE:LONG:TWENTY:NINE:BYTES:X"}) }
"#;

const P0: &str = r#"PVA links in all records

  4/8 channels connected used by 10 links
"#;

const P1: &str = r#"PVA links in all records

NOPE:EDGE:TWENTY:EIGHT:BYTES conn=F 0 disconnects, 0 type changes
NOPE:LONG:TWENTY:NINE:BYTES:X	conn=F 0 disconnects, 0 type changes
                      NOPE:X conn=F 0 disconnects, 0 type changes
                      NOPE:Y conn=F 0 disconnects, 0 type changes
  4/8 channels connected used by 10 links
"#;

const P2: &str = r#"PVA links in all records

NOPE:EDGE:TWENTY:EIGHT:BYTES conn=F 0 disconnects, 0 type changes
NOPE:LONG:TWENTY:NINE:BYTES:X	conn=F 0 disconnects, 0 type changes
                      NOPE:X conn=F 0 disconnects, 0 type changes
                      NOPE:Y conn=F 0 disconnects, 0 type changes
                         REM conn=T 0 disconnects, 1 type changes
                         SRC conn=T 0 disconnects, 1 type changes
                         SRC conn=T 0 disconnects, 1 type changes
                         SRC conn=T 0 disconnects, 1 type changes
  4/8 channels connected used by 10 links
"#;

const P5: &str = r#"PVA links in all records

NOPE:EDGE:TWENTY:EIGHT:BYTES conn=F 0 disconnects, 0 type changes
                              E.INP Def NMS Q=4 pipe=F defer=F time=F retry=F morder=0

NOPE:LONG:TWENTY:NINE:BYTES:X	conn=F 0 disconnects, 0 type changes
                              G.INP Def NMS Q=4 pipe=F defer=F time=F retry=F morder=0

                      NOPE:X conn=F 0 disconnects, 0 type changes
                              N.INP Def NMS Q=4 pipe=F defer=F time=F retry=F morder=0

                      NOPE:Y conn=F 0 disconnects, 0 type changes
                              WT.OUT Def NMS Q=4 pipe=F defer=F time=F retry=T morder=0

                         REM conn=T 0 disconnects, 1 type changes
                              R.INP Def NMS Q=4 pipe=F defer=F time=F retry=F morder=0
                              WR.OUT Def NMS Q=4 pipe=F defer=F time=F retry=F morder=0
                              WRT.OUT Def NMS Q=4 pipe=F defer=F time=F retry=T morder=0

                         SRC conn=T 0 disconnects, 1 type changes
                              O.OUT PP NMS Q=4 pipe=F defer=F time=F retry=F morder=0

                         SRC conn=T 0 disconnects, 1 type changes
                              L.INP CP MS Q=5 pipe=F defer=F time=F retry=F morder=0

                         SRC conn=T 0 disconnects, 1 type changes
                              F.INP NPP MSI Q=2 pipe=T defer=T time=T retry=T morder=3

  4/8 channels connected used by 10 links
"#;

const P5R: &str = r#"PVA links in record named 'R*'

                         REM conn=T 0 disconnects, 1 type changes
                              R.INP Def NMS Q=4 pipe=F defer=F time=F retry=F morder=0

  1/1 channels connected used by 1 links
"#;

const P2W: &str = r#"PVA links in record named 'W*'

                      NOPE:Y conn=F 0 disconnects, 0 type changes
                         REM conn=T 0 disconnects, 1 type changes
  1/2 channels connected used by 3 links
"#;

const PZZ: &str = r#"PVA links in record named 'ZZ'

  0/0 channels connected used by 0 links
"#;

const P1STAR: &str = r#"PVA links in all records

NOPE:EDGE:TWENTY:EIGHT:BYTES conn=F 0 disconnects, 0 type changes
NOPE:LONG:TWENTY:NINE:BYTES:X	conn=F 0 disconnects, 0 type changes
                      NOPE:X conn=F 0 disconnects, 0 type changes
                      NOPE:Y conn=F 0 disconnects, 0 type changes
  4/8 channels connected used by 10 links
"#;

const P2_PUT: &str = r#"PVA links in all records

NOPE:EDGE:TWENTY:EIGHT:BYTES conn=F 0 disconnects, 0 type changes
NOPE:LONG:TWENTY:NINE:BYTES:X	conn=F 0 disconnects, 0 type changes
                      NOPE:X conn=F 0 disconnects, 0 type changes
                      NOPE:Y conn=F 0 disconnects, 0 type changes Put
                         REM conn=T 0 disconnects, 1 type changes Put
                         SRC conn=T 0 disconnects, 1 type changes
                         SRC conn=T 0 disconnects, 1 type changes
                         SRC conn=T 0 disconnects, 1 type changes
  4/8 channels connected used by 10 links
"#;

const P2_GONE: &str = r#"PVA links in all records

NOPE:EDGE:TWENTY:EIGHT:BYTES conn=F 0 disconnects, 0 type changes
NOPE:LONG:TWENTY:NINE:BYTES:X	conn=F 0 disconnects, 0 type changes
                      NOPE:X conn=F 0 disconnects, 0 type changes
                      NOPE:Y conn=F 0 disconnects, 0 type changes Put
                         REM conn=F 1 disconnects, 1 type changes
                         SRC conn=T 0 disconnects, 1 type changes
                         SRC conn=T 0 disconnects, 1 type changes
                         SRC conn=T 0 disconnects, 1 type changes
  3/8 channels connected used by 10 links
"#;

const P5_END: &str = r#"PVA links in all records

NOPE:EDGE:TWENTY:EIGHT:BYTES conn=F 0 disconnects, 0 type changes
                              E.INP Def NMS Q=4 pipe=F defer=F time=F retry=F morder=0

NOPE:LONG:TWENTY:NINE:BYTES:X	conn=F 0 disconnects, 0 type changes
                              G.INP Def NMS Q=4 pipe=F defer=F time=F retry=F morder=0

                      NOPE:X conn=F 0 disconnects, 0 type changes
                              N.INP Def NMS Q=4 pipe=F defer=F time=F retry=F morder=0

                      NOPE:Y conn=T 0 disconnects, 1 type changes
                              WT.OUT Def NMS Q=4 pipe=F defer=F time=F retry=T morder=0

                         REM conn=F 1 disconnects, 1 type changes
                              R.INP Def NMS Q=4 pipe=F defer=F time=F retry=F morder=0
                              WR.OUT Def NMS Q=4 pipe=F defer=F time=F retry=F morder=0
                              WRT.OUT Def NMS Q=4 pipe=F defer=F time=F retry=T morder=0

                         SRC conn=T 0 disconnects, 1 type changes
                              O.OUT PP NMS Q=4 pipe=F defer=F time=F retry=F morder=0

                         SRC conn=T 0 disconnects, 1 type changes
                              L.INP CP MS Q=5 pipe=F defer=F time=F retry=F morder=0

                         SRC conn=T 0 disconnects, 1 type changes
                              F.INP NPP MSI Q=2 pipe=T defer=T time=T retry=T morder=3

  4/8 channels connected used by 10 links
"#;

fn all(level: u32) -> String {
    format!("dbpvar \"\" {level}")
}

#[test]
fn dbpvar_prints_the_pvxs_channel_report() {
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
        tag: "p",
        n: 0,
    };

    let p0 = sh.capture_until(&all(0), "  4/8 channels connected used by 10 links");
    let p1 = sh.capture(&all(1));
    let p2 = sh.capture(&all(2));
    let p5 = sh.capture(&all(5));
    let p5r = sh.capture("dbpvar \"R*\" 5");
    let p2w = sh.capture("dbpvar \"W*\" 2");
    let pzz = sh.capture("dbpvar \"ZZ\" 2");
    let p1star = sh.capture("dbpvar \"*\" 1");

    signal(&remote, "-STOP");
    sh.line("dbpf WR 5");
    sh.line("dbpf WT 6");
    sh.line("dbpf WRT 8");
    let p2_put = sh.capture_when(&all(2), |t| t.matches(" Put\n").count() == 2);
    remote.kill().unwrap();
    let _ = remote.wait();
    let p2_gone = sh.capture_when(&all(2), |t| {
        t.contains("REM conn=F 1 disconnects, 1 type changes\n")
    });

    let mut late = spawn_ioc(dir.path(), "late", LATE, &late_port, "");
    let p5_end = sh.capture_when(&all(5), |t| {
        t.contains("NOPE:Y conn=T 0 disconnects, 1 type changes\n")
    });
    let mut late_sh = Shell {
        stdin: late.stdin.take().unwrap(),
        dir: dir.path(),
        tag: "l",
        n: 0,
    };
    late_sh.line("exit");
    drop(late_sh);
    let _ = late.wait();
    sh.line("exit");
    drop(sh);
    let _ = local.wait();

    assert_eq!(
        [
            p0, p1, p2, p5, p5r, p2w, pzz, p1star, p2_put, p2_gone, p5_end
        ],
        [
            P0, P1, P2, P5, P5R, P2W, PZZ, P1STAR, P2_PUT, P2_GONE, P5_END
        ]
    );
}
