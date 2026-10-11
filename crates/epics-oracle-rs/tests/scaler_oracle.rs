//! The control scaler `SC` of `scaler-c8.db`, live: C `oracleScalerIoc
//! st-scaler.cmd` (scaler 913e2e4, base 8f5015b66) against `oracle-ioc` given
//! the same st.cmd lines — `drvScalerSoftConfigure` as `--scaler-soft`, the
//! `dbLoadRecords` macro as `--macro`.
//!
//! Both are driven through the C CA tools: `camonitor -m va` on `SC.CNT`,
//! `SC.US`, `SC.SS`, `SC.S1` and `SCF`, a user count at `DLY 0` and then
//! `DLY 0.5` started by `caput SC.CNT 1` and ended by `caput c1 10000000`.
//! Each PV's posts are compared in order; the order across PVs is not, because
//! it is a property of the CA server's per-subscription queues, not of the
//! record.
//!
//! Requires the built oracle IOC (`EPICS_ORACLE_SCALER_IOC_BIN`, or the default
//! path) and fails loudly rather than skipping when it is absent.

#![cfg(all(tokio_backend, feature = "scaler"))]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use epics_oracle_rs::ioc::{CTools, RustIoc, alloc_free_port};

const DEFAULT_SCALER_IOC: &str =
    "/home/stevek/work/epics-modules/oracle-ioc/bin/linux-x86_64/oracleScalerIoc";

/// The camonitored PVs, without the prefix.
const WATCHED: [&str; 6] = ["SC.CNT", "SC.US", "SC.SS", "SC.S1", "SC.T", "SCF"];

/// `oracleScalerIoc` and the directory its `st-scaler.cmd` runs from.
fn scaler_ioc() -> (PathBuf, PathBuf) {
    let bin = PathBuf::from(
        std::env::var("EPICS_ORACLE_SCALER_IOC_BIN")
            .unwrap_or_else(|_| DEFAULT_SCALER_IOC.to_string()),
    );
    assert!(
        bin.is_file(),
        "oracleScalerIoc not found at {}; build oracle-ioc or set \
         EPICS_ORACLE_SCALER_IOC_BIN — there is no ground truth without it",
        bin.display()
    );
    let top = bin
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent());
    let dir = top.unwrap().join("iocBoot/iocOracle");
    (bin, dir)
}

/// Kills its process on drop — every child here is killed by its own pid.
struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Every line an IOC printed, on either stream.
type Log = Arc<Mutex<Vec<String>>>;

/// Read `child`'s stdout and stderr until a line contains `ready`, and return
/// that line and the log the readers keep filling. C's `iocInit` banner goes
/// to stderr, `oracle-ioc`'s port to stdout.
fn line_with(child: &mut Child, ready: &str) -> (String, Log) {
    let seen: Log = Arc::new(Mutex::new(Vec::new()));
    let (tx, rx) = mpsc::channel();
    let streams: [Box<dyn std::io::Read + Send>; 2] = [
        Box::new(child.stdout.take().unwrap()),
        Box::new(child.stderr.take().unwrap()),
    ];
    for stream in streams {
        let (sink, tx, wanted) = (Arc::clone(&seen), tx.clone(), ready.to_string());
        thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                if line.contains(&wanted) {
                    let _ = tx.send(line.clone());
                }
                sink.lock().unwrap().push(line);
            }
        });
    }
    let line = rx
        .recv_timeout(Duration::from_secs(20))
        .unwrap_or_else(|_| panic!("no {ready:?} line: {:?}", seen.lock().unwrap()));
    (line, seen)
}

/// One IOC under test, reachable at `addr` (`EPICS_CA_ADDR_LIST`).
struct Served {
    _ioc: Proc,
    addr: String,
    log: Log,
}

impl Served {
    /// Wait up to 5 s for a printed line containing `text`.
    fn printed(&self, text: &str) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if self.log.lock().unwrap().iter().any(|l| l.contains(text)) {
                return true;
            }
            thread::sleep(Duration::from_millis(50));
        }
        false
    }
}

/// What both IOCs are booted from: C runs `st` in `dir`; `oracle-ioc` gets
/// `db` and the same `drvScalerSoftConfigure` lines as `--scaler-soft`.
struct Setup {
    dir: PathBuf,
    st: PathBuf,
    db: PathBuf,
    ports: Vec<(&'static str, usize, &'static str)>,
}

impl Setup {
    /// `st-scaler.cmd` as qa runs it.
    fn stock() -> Self {
        let (_, dir) = scaler_ioc();
        Self {
            st: dir.join("st-scaler.cmd"),
            db: dir.join("scaler-c8.db"),
            dir,
            ports: vec![
                ("scalerPort", 8, "$(P=)s%d"),
                ("scalerPortC", 8, "$(P=)c%d"),
            ],
        }
    }

    /// `db` on `ports`, written with a C st.cmd under a directory of its own
    /// that is removed on drop.
    fn custom(tag: &str, db: &str, ports: Vec<(&'static str, usize, &'static str)>) -> Self {
        let (bin, _) = scaler_ioc();
        let top = bin
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent());
        let dbd = top.unwrap().join("dbd/oracleScalerIoc.dbd");
        let dir = std::env::temp_dir().join(format!("scaler-oracle-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("scaler.db");
        std::fs::write(&db_path, db).unwrap();
        let mut st = format!(
            "dbLoadDatabase(\"{}\")\noracleScalerIoc_registerRecordDeviceDriver(pdbbase)\n",
            dbd.display()
        );
        for (port, chans, template) in &ports {
            st += &format!("drvScalerSoftConfigure(\"{port}\", {chans}, \"{template}\")\n");
        }
        st += &format!(
            "dbLoadRecords(\"{}\",\"P=$(P=)\")\niocInit()\n",
            db_path.display()
        );
        let st_path = dir.join("st.cmd");
        std::fs::write(&st_path, st).unwrap();
        Self {
            dir,
            st: st_path,
            db: db_path,
            ports,
        }
    }

    fn boot_c(&self, prefix: &str) -> Served {
        let (bin, _) = scaler_ioc();
        let port = alloc_free_port().expect("free CA port");
        let mut child = Command::new(bin)
            .arg(&self.st)
            .current_dir(&self.dir)
            .env("P", prefix)
            .env("EPICS_CA_SERVER_PORT", port.to_string())
            .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
            .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1")
            .env("EPICS_CAS_AUTO_BEACON_ADDR_LIST", "NO")
            // The shell stays open on a held stdin, as under a terminal.
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn oracleScalerIoc");
        let (_, log) = line_with(&mut child, "iocRun: All initialization complete");
        Served {
            _ioc: Proc(child),
            addr: format!("127.0.0.1:{port}"),
            log,
        }
    }

    fn boot_rs(&self, prefix: &str) -> Served {
        let mut cmd = Command::new(RustIoc::binary().expect("oracle-ioc built"));
        cmd.arg("--db")
            .arg(&self.db)
            .arg("--macro")
            .arg(format!("P={prefix}"));
        for (port, chans, template) in &self.ports {
            cmd.arg("--scaler-soft")
                .arg(format!("{port},{chans},{template}"));
        }
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn oracle-ioc");
        let (line, log) = line_with(&mut child, "ORACLE_IOC_PORT");
        let port = line.rsplit(' ').next().unwrap().to_string();
        Served {
            _ioc: Proc(child),
            addr: format!("127.0.0.1:{port}"),
            log,
        }
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        if self.dir.starts_with(std::env::temp_dir()) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

fn ca(tools: &CTools, tool: &str, addr: &str) -> Command {
    let mut cmd = Command::new(tools.tool(tool));
    cmd.env("EPICS_CA_ADDR_LIST", addr)
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO");
    cmd
}

fn caput(tools: &CTools, addr: &str, pv: &str, value: &str) {
    let ok = ca(tools, "caput", addr)
        .args([pv, value])
        .stdout(Stdio::null())
        .status()
        .expect("caput runs")
        .success();
    assert!(ok, "caput {pv} {value} failed against {addr}");
}

/// Each watched PV's posts, in order, as `value stat sevr` with the timestamp
/// dropped, and the final `caget` of all five.
struct Run {
    posts: BTreeMap<String, Vec<String>>,
    /// Every post as `FIELD value…`, in camonitor's order across fields.
    stream: Vec<String>,
    after: Vec<String>,
}

/// The control-SC sequence qa drove the C oracle with.
fn drive(served: &Served, prefix: &str) -> Run {
    let tools = CTools::discover().expect("C CA tools");
    let pvs: Vec<String> = WATCHED.iter().map(|f| format!("{prefix}{f}")).collect();
    let mut monitor = ca(&tools, "camonitor", &served.addr)
        .args(["-m", "va"])
        .args(&pvs)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn camonitor");
    let out = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&out);
    let stdout = monitor.stdout.take().unwrap();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            sink.lock().unwrap().push(line);
        }
    });
    let monitor = Proc(monitor);
    // Every PV's connect post, before anything is put.
    let start = Instant::now();
    while out.lock().unwrap().len() < pvs.len() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "camonitor connected to {:?} only",
            out.lock().unwrap()
        );
        thread::sleep(Duration::from_millis(50));
    }

    for dly in ["0", "0.5"] {
        caput(&tools, &served.addr, &format!("{prefix}SC.DLY"), dly);
        thread::sleep(Duration::from_millis(300));
        caput(&tools, &served.addr, &format!("{prefix}SC.CNT"), "1");
        thread::sleep(Duration::from_millis(1500));
        caput(&tools, &served.addr, &format!("{prefix}c1"), "10000000");
        thread::sleep(Duration::from_millis(1500));
    }
    drop(monitor);

    let mut posts: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut stream = Vec::new();
    for line in out.lock().unwrap().iter() {
        let mut words = line.split_whitespace();
        let Some(pv) = words.next() else { continue };
        // `<undefined>` or `date time`.
        let rest: Vec<&str> = words.collect();
        let skip = if rest.first() == Some(&"<undefined>") {
            1
        } else {
            2
        };
        let field = pv.strip_prefix(prefix).unwrap_or(pv).to_string();
        let post = rest[skip.min(rest.len())..].join(" ");
        stream.push(format!("{field} {post}"));
        posts.entry(field).or_default().push(post);
    }

    let after = caget(served, prefix, &WATCHED);
    Run {
        posts,
        stream,
        after,
    }
}

fn both(prefix: &str) -> (Run, Run) {
    let setup = Setup::stock();
    let c = drive(&setup.boot_c(prefix), prefix);
    let rs = drive(&setup.boot_rs(prefix), prefix);
    (c, rs)
}

/// `caget -f 0` of `fields` (exact integers, not `%g`), each line
/// `FIELD value`, the prefix dropped.
fn caget(served: &Served, prefix: &str, fields: &[&str]) -> Vec<String> {
    let tools = CTools::discover().expect("C CA tools");
    let pvs: Vec<String> = fields.iter().map(|f| format!("{prefix}{f}")).collect();
    let out = ca(&tools, "caget", &served.addr)
        .args(["-f", "0"])
        .args(&pvs)
        .output()
        .expect("caget runs");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| {
            let mut w = l.split_whitespace();
            let pv = w.next().unwrap_or("");
            let pv = pv.strip_prefix(prefix).unwrap_or(pv);
            format!("{pv} {}", w.collect::<Vec<_>>().join(" "))
        })
        .collect()
}

/// `oracle-ioc` boots `scaler-c8.db` on st-scaler.cmd's soft ports, the
/// sequence leaves it where it leaves C, and the posts of the fields the
/// count does not change agree one for one.
#[test]
fn the_control_scaler_boots_and_runs_as_oracle_scaler_ioc() {
    let (c, rs) = both("scb:");
    assert_eq!(rs.after, c.after, "caget after the two counts");
    for field in ["SC.US", "SC.SS", "SCF"] {
        assert_eq!(rs.posts.get(field), c.posts.get(field), "{field}");
    }
}

/// Each field's posts after its connect post, and S1/T in one stream: CNT's `:372` Done and
/// S1's `:582` count carry the init alarm `monitor()` has not yet reset, and
/// no count-start cycle reaches `monitor()` or the FLNK (`:470`).
#[test]
fn cnt_and_s1_post_as_oracle_scaler_ioc_does() {
    let (c, rs) = both("scw:");
    let c_s1: &[&str] = &["1e+07 UDF INVALID", "0", "1e+07"];
    // CBUG-B19 (46be9a081): the first done cycle's LOG sweep carries the alarm reset.
    let rs_s1: &[&str] = &["1e+07 UDF INVALID", "1e+07", "0", "1e+07"];
    // Across fields, from the one stream: `updateCounts()` posts Sn (:582)
    // before T (:588).
    let c_s1_t: &[&str] = &[
        "SC.S1 1e+07 UDF INVALID",
        "SC.T 1 UDF INVALID",
        "SC.S1 0",
        "SC.T 0",
        "SC.S1 1e+07",
        "SC.T 1",
    ];
    let rs_s1_t: &[&str] = &[
        "SC.S1 1e+07 UDF INVALID",
        "SC.T 1 UDF INVALID",
        "SC.S1 1e+07",
        "SC.S1 0",
        "SC.T 0",
        "SC.S1 1e+07",
        "SC.T 1",
    ];
    for (side, run, s1, s1_t) in [("C", &c, c_s1, c_s1_t), ("rs", &rs, rs_s1, rs_s1_t)] {
        let expect: [(&str, &[&str]); 5] = [
            (
                "SC.CNT",
                &["Count UDF INVALID", "Done UDF INVALID", "Count", "Done"],
            ),
            ("SC.S1", s1),
            ("SCF", &["1", "2"]),
            ("SC.US", &[]),
            ("SC.SS", &[]),
        ];
        for (field, posts) in expect {
            let got: Option<Vec<&str>> = run
                .posts
                .get(field)
                .map(|p| p[1..].iter().map(String::as_str).collect());
            assert_eq!(got.as_deref(), Some(posts), "{side} {field}");
        }
        // The two connect posts precede every put.
        let got: Vec<&str> = run
            .stream
            .iter()
            .map(String::as_str)
            .filter(|l| l.starts_with("SC.S1 ") || l.starts_with("SC.T "))
            .skip(2)
            .collect();
        assert_eq!(got, s1_t, "{side} S1/T order");
    }
}

/// The control scaler with a third port whose inputs do not exist.
const MISSING_INPUT_DB: &str = r#"
record(scaler, "$(P)SC") {
    field(DTYP, "Asyn Scaler")
    field(OUT,  "@asyn(scalerPortC 0 0)")
    field(FREQ, "10000000")
    field(TP,   "1")
    field(DLY,  "0")
    field(FLNK, "$(P)SCF")
}
record(calc, "$(P)SCF") {
    field(CALC, "A+1")
    field(INPA, "$(P)SCF")
}
record(longout, "$(P)c1") {}
record(longout, "$(P)c2") {}
record(longout, "$(P)c3") {}
record(longout, "$(P)c4") {}
record(longout, "$(P)c5") {}
record(longout, "$(P)c6") {}
record(longout, "$(P)c7") {}
record(longout, "$(P)c8") {}
record(scaler, "$(P)SM") {
    field(DTYP, "Asyn Scaler")
    field(OUT,  "@asyn(scalerPortM 0 0)")
    field(FREQ, "10000000")
    field(TP,   "1")
    field(DLY,  "0")
}
"#;

/// Boot both IOCs with a third port `scalerPortM` whose inputs do not
/// exist, check each prints C's `connectPVs` message, start a count on both
/// ports, end the control one, and `caget` `fields` from each: `(C, rs)`.
fn missing_input(prefix: &str, fields: &[&str]) -> (Vec<String>, Vec<String>) {
    let setup = Setup::custom(
        prefix.trim_end_matches(':'),
        MISSING_INPUT_DB,
        vec![
            ("scalerPortC", 8, "$(P=)c%d"),
            ("scalerPortM", 8, "$(P=)nope%d"),
        ],
    );
    let message =
        format!("drvScalerSoft::connectPVs port scalerPortM can't find PV {prefix}nope1.");
    let run = |served: Served| {
        let tools = CTools::discover().expect("C CA tools");
        assert!(
            served.printed(&message),
            "no {message:?}: {:?}",
            served.log.lock().unwrap()
        );
        caput(&tools, &served.addr, &format!("{prefix}SM.CNT"), "1");
        caput(&tools, &served.addr, &format!("{prefix}SC.CNT"), "1");
        thread::sleep(Duration::from_millis(1500));
        caput(&tools, &served.addr, &format!("{prefix}c1"), "10000000");
        thread::sleep(Duration::from_millis(1500));
        caget(&served, prefix, fields)
    };
    let c = run(setup.boot_c(prefix));
    let rs = run(setup.boot_rs(prefix));
    (c, rs)
}

/// C `connectPVs` fails only the port whose input is missing: the IOC keeps
/// running, that port's scaler starts a count it never ends, and the other
/// port counts.
#[test]
fn a_port_missing_an_input_fails_alone_as_in_oracle_scaler_ioc() {
    let (c, rs) = missing_input("scm:", &["SM.CNT", "SC.CNT", "SC.S1", "SCF"]);
    assert_eq!(rs, c);
}

/// The failed port's scaler keeps its init alarm: its count never completes,
/// so no cycle reaches `monitor()`'s `recGblResetAlarms`.
#[test]
fn a_failed_port_keeps_its_scalers_init_alarm_as_in_oracle_scaler_ioc() {
    let (c, rs) = missing_input("sma:", &["SM.SEVR", "SM.STAT"]);
    assert_eq!(rs, c);
}

/// Two ports whose templates only C's `snprintf` resolves: a `%03d` width,
/// and a name longer than the 40-byte buffer, whose two channels both land
/// on the cut name. `P` must be `scz:` for the cut to fall where the record
/// name ends.
const TEMPLATE_DB: &str = r#"
record(scaler, "$(P)SZ") {
    field(DTYP, "Asyn Scaler")
    field(OUT,  "@asyn(scalerPortZ 0 0)")
    field(FREQ, "10000000")
    field(TP,   "1")
    field(DLY,  "0")
}
record(longout, "$(P)z001") {}
record(longout, "$(P)z002") {}
record(scaler, "$(P)SL") {
    field(DTYP, "Asyn Scaler")
    field(OUT,  "@asyn(scalerPortL 0 0)")
    field(FREQ, "10000000")
    field(TP,   "1")
    field(DLY,  "0")
}
record(longout, "$(P)L2345678901234567890123456789012345") {}
"#;

/// The inputs C's `epicsSnprintf(pvName, 40, ..)` names are the ones counted.
#[test]
fn templates_name_the_inputs_oracle_scaler_ioc_names() {
    let prefix = "scz:";
    let setup = Setup::custom(
        "template",
        TEMPLATE_DB,
        vec![
            ("scalerPortZ", 2, "$(P=)z%03d"),
            (
                "scalerPortL",
                2,
                "$(P=)L2345678901234567890123456789012345678_%d",
            ),
        ],
    );
    let fields = ["SZ.CNT", "SZ.S1", "SZ.S2", "SL.CNT", "SL.S1", "SL.S2"];
    let run = |served: Served| {
        let tools = CTools::discover().expect("C CA tools");
        for scaler in ["SZ", "SL"] {
            caput(&tools, &served.addr, &format!("{prefix}{scaler}.CNT"), "1");
        }
        thread::sleep(Duration::from_millis(1500));
        for input in ["z002", "z001", "L2345678901234567890123456789012345"] {
            caput(
                &tools,
                &served.addr,
                &format!("{prefix}{input}"),
                "10000000",
            );
        }
        thread::sleep(Duration::from_millis(1500));
        caget(&served, prefix, &fields)
    };
    let c = run(setup.boot_c(prefix));
    let rs = run(setup.boot_rs(prefix));
    assert_eq!(rs, c);
}

/// A port whose inputs are a `DBF_INT64` and a `DBF_DOUBLE` field, read as
/// C `dbGetField(.., DBR_LONG, ..)` reads them.
const CAST_DB: &str = r#"
record(scaler, "$(P)SX") {
    field(DTYP, "Asyn Scaler")
    field(OUT,  "@asyn(scalerPortX 0 0)")
    field(FREQ, "10000000")
    field(TP,   "1")
    field(DLY,  "0")
}
record(int64out, "$(P)x1") {}
record(ao, "$(P)x2") {}
"#;

/// Count once with `x2` at 3e9 and end the count with `x1` at 5e9, then
/// `caget` `fields` from each IOC: `(C, rs)`.
fn out_of_range_inputs(prefix: &str, fields: &[&str]) -> (Vec<String>, Vec<String>) {
    let setup = Setup::custom(
        prefix.trim_end_matches(':'),
        CAST_DB,
        vec![("scalerPortX", 2, "$(P=)x%d")],
    );
    let run = |served: Served| {
        let tools = CTools::discover().expect("C CA tools");
        caput(&tools, &served.addr, &format!("{prefix}SX.CNT"), "1");
        thread::sleep(Duration::from_millis(1500));
        caput(&tools, &served.addr, &format!("{prefix}x2"), "3000000000");
        caput(&tools, &served.addr, &format!("{prefix}x1"), "5000000000");
        thread::sleep(Duration::from_millis(1500));
        caget(&served, prefix, fields)
    };
    let c = run(setup.boot_c(prefix));
    let rs = run(setup.boot_rs(prefix));
    (c, rs)
}

/// An integer input converts as C's integer cast: 5e9 keeps its low 32 bits.
#[test]
fn an_int64_input_counts_its_low_32_bits_as_oracle_scaler_ioc_does() {
    let (c, rs) = out_of_range_inputs("scx:", &["SX.CNT", "SX.S1"]);
    assert_eq!(rs, c);
}

/// A double input above `INT_MAX` is C's out-of-range `(epicsInt32)` cast.
#[test]
#[ignore = "needs i15/c-cast-policy: c_cast::f64_to_i32 saturates on this base, where x86 C stores INT_MIN"]
fn a_double_input_above_int_max_counts_as_oracle_scaler_ioc_does() {
    let (c, rs) = out_of_range_inputs("scd:", &["SX.S2"]);
    assert_eq!(rs, c);
}
