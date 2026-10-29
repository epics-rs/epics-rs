//! `$(MOTOR)/db` ([`motor_rs::MOTOR_DIR`]) holds copies of what
//! motorApp/Db's Makefile installs there: every `.template`, `.db` and
//! `.req`. Each database loads the records C's `dbl` lists
//! (`db_files.dbl`).

use std::collections::BTreeSet;
use std::path::Path;

use epics_base_rs::server::db_loader::parse_db;

/// The databases, in `db_files.dbl`'s order: the nth is loaded with
/// `P=Z<n>:`.
const DATABASES: [&str; 17] = [
    "asyn_auto_power.db",
    "asyn_motor.db",
    "asyn_motor_model2.db",
    "basic_asyn_motor.db",
    "basic_asyn_motor_model2.db",
    "basic_motor.db",
    "coordTrans2D.db",
    "motor.db",
    "motorUtil.db",
    "pseudoMotor.db",
    "SoftMotorEx.db",
    "softMotorTest.db",
    "sumDiff2D.db",
    "trajectoryScan.db",
    "TransPos.db",
    "profileMoveAxis.template",
    "profileMoveController.template",
];

const REQUESTS: [&str; 13] = [
    "asyn_motor_model2_positions.req",
    "asyn_motor_model2_settings.req",
    "asyn_motor_positions.req",
    "asyn_motor_settings.req",
    "basic_asyn_motor_model2_settings.req",
    "basic_asyn_motor_settings.req",
    "basic_motor_settings.req",
    "motor_positions.req",
    "motor_settings.req",
    "profileMoveAxis_settings.req",
    "profileMoveController_settings.req",
    "pseudoMotor_settings.req",
    "trajectoryScan_settings.req",
];

/// Every macro any of the databases reads but `P`, `user` and `DTYP`.
const MACROS: &str = "M=m1,R=r1:,T=t1:,MXP=mx,MYP=my,SUM=sum,DIFF=diff,SOFT=soft,\
    HARD=hard,NAXES=8,NPOINTS=100,NREADBACK=100,NPULSES=100,NELM=100,NPULSE=100,PORT=p,\
    ADDR=0,C=0,S=0,DESC=d,DIR=Pos,VELO=1,VBAS=0,ACCL=0.2,ACCU=0,BDST=0,BVEL=1,BACC=0.2,\
    MRES=0.001,RRES=1,PREC=3,EGU=mm,DHLM=10,DLLM=-10,INIT=,RSTM=NearZero,RTRY=10,FRAC=1,\
    TIMEOUT=1,DINP=x,OUT=x,RDBL=x,STOO=x,FCALC=A,RCALC=A,XDONE=x,XDRV=x,XRBV=x,XSTOP=x,\
    YDONE=x,YDRV=x,YRBV=x,YSTOP=x,M1DONE=x,M1DRV=x,M1RBV=x,M1STOP=x,M2DONE=x,M2DRV=x,\
    M2RBV=x,M2STOP=x";

fn db_dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/db"))
}

#[test]
fn the_db_dir_is_the_c_install_set() {
    let shipped: BTreeSet<String> = std::fs::read_dir(db_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    let installed: BTreeSet<String> = DATABASES
        .iter()
        .chain(&REQUESTS)
        .map(|f| f.to_string())
        .collect();
    assert_eq!(shipped, installed);
    assert_eq!(Path::new(motor_rs::MOTOR_DIR).join("db"), db_dir());
}

#[test]
fn every_database_loads_the_records_c_lists() {
    let mut c: Vec<(String, BTreeSet<String>)> = Vec::new();
    for line in include_str!("db_files.dbl").lines() {
        if line.starts_with('#') {
            continue;
        }
        let (file, record) = line.split_once(' ').unwrap();
        if c.last().is_none_or(|(f, _)| f != file) {
            c.push((file.to_owned(), BTreeSet::new()));
        }
        c.last_mut().unwrap().1.insert(record.to_owned());
    }
    let files: Vec<&str> = c.iter().map(|(f, _)| f.as_str()).collect();
    assert_eq!(files, DATABASES);
    for (i, (file, want)) in c.iter().enumerate() {
        let dtyp = match file.as_str() {
            "motor.db" | "basic_motor.db" => "MM4000",
            _ => "asynMotor",
        };
        let p = format!("Z{:02}:", i + 1);
        let macros: Vec<(String, String)> = format!("P={p},user={p},DTYP={dtyp},{MACROS}")
            .split(',')
            .map(|kv| {
                let (k, v) = kv.split_once('=').unwrap();
                (k.to_owned(), v.to_owned())
            })
            .collect();
        let text = std::fs::read_to_string(db_dir().join(file)).unwrap();
        let got: BTreeSet<String> = parse_db(&text, macros)
            .unwrap_or_else(|e| panic!("{file}: {e}"))
            .iter()
            .map(|r| r.name.to_string())
            .collect();
        assert_eq!(&got, want, "{file}");
    }
}

#[test]
fn every_request_file_includes_one_in_the_db_dir() {
    for req in REQUESTS {
        let text = std::fs::read_to_string(db_dir().join(req)).unwrap();
        for line in text.lines() {
            if let Some(rest) = line.trim_start().strip_prefix("file") {
                let name = rest.split_whitespace().next().unwrap();
                assert!(REQUESTS.contains(&name), "{req}: {line}");
            }
        }
    }
}
