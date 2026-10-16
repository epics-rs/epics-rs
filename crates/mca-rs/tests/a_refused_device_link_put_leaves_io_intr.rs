//! A device-link put that passes the `dsxt` gate takes an I/O Intr record off
//! its scan list and sets SCAN Passive (`dbAccess.c:1155-1158`); only the
//! success arm puts it back (`:1225-1229`). A scratch C IOC (base 8f5015b66
//! plus mca's `mcaRecord.c` and probe dsets with `get_ioint_info`: an mca one
//! on `devSoft_DSXT`, an ai one whose `add_record` refuses INP `REFUSE`, and
//! an `INST_IO` ai one with no `dsxt`, and an ai one whose `del_record`
//! refuses for INP `KEEP`) on this database:
//!
//! ```text
//! epics> scanpiol            AD AR AS AN MI
//! epics> dbpf AD.INP WF      (del_record)  AD.SCAN I/O Intr AD.PACT 0 AD.INP "KEEP NPP NMS"
//! epics> dbpf MI.INP WF      (noMod)       MI.SCAN Passive  MI.PACT 1  (del_record only)
//!                                          MI DSET (nil), DPVT kept
//! epics> dbpf AR.INP REFUSE  (add_record)  AR.SCAN Passive  AR.PACT 1
//!                                          AR DSET (nil), DPVT (nil)
//! epics> dbpf AS.INP WF                    AS.SCAN I/O Intr AS.PACT 0
//! epics> dbpf AN.INP @y      (no dsxt)     AN.SCAN I/O Intr AN.INP "@x"
//! epics> scanpiol            AD AN AS
//! epics> dbpf AR.INP WF                    AR.SCAN Passive  AR.PACT 0
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use epics_base_rs::error::{CaResult, LinkRefusal};
use epics_base_rs::runtime::sync::mpsc;
use epics_base_rs::server::device_support::DeviceSupport;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, Record, ScanType};
use epics_base_rs::types::EpicsValue;

static MCA_PROBE: [DevSup; 1] = [DevSup {
    name: "devMcaIntrProbe",
    choice: "Intr Probe",
    link_type: DbLinkType::Constant,
    dset: Dset::new(6, &[1, 2, 3, 4, 5], true),
}];

static AI_PROBES: [DevSup; 2] = [
    DevSup {
        name: "devAiIntrProbe",
        choice: "Intr Probe",
        link_type: DbLinkType::Constant,
        dset: Dset::new(6, &[1, 2, 3, 4], true),
    },
    DevSup {
        name: "devAiIntrNoDsxt",
        choice: "Intr NoDsxt",
        link_type: DbLinkType::InstIo,
        dset: Dset::new(6, &[1, 2, 3, 4], false),
    },
];

/// `add_record` calls on an mca probe: C's NOMOD put never reaches one.
static MCA_ADDS: AtomicUsize = AtomicUsize::new(0);

/// The senders the probes' I/O Intr receivers listen on, kept open.
static SENDERS: Mutex<Vec<mpsc::Sender<()>>> = Mutex::new(Vec::new());

struct Probe {
    dtyp: &'static str,
    refuse: bool,
    keep: bool,
}

impl DeviceSupport for Probe {
    fn add_record(&mut self, record_type: &str) -> Result<(), LinkRefusal> {
        if record_type == "mca" {
            MCA_ADDS.fetch_add(1, Ordering::Relaxed);
        }
        match self.refuse {
            true => Err(LinkRefusal::add_record("devAiIntrProbe")),
            false => Ok(()),
        }
    }

    fn del_record(&mut self) -> Result<(), LinkRefusal> {
        match self.keep {
            true => Err(LinkRefusal::del_record("devAiIntrProbe")),
            false => Ok(()),
        }
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        self.dtyp
    }

    fn holds_dpvt(&self) -> bool {
        true
    }

    fn io_intr_receiver(&mut self) -> Option<mpsc::Receiver<()>> {
        let (tx, rx) = mpsc::channel(1);
        SENDERS.lock().unwrap().push(tx);
        Some(rx)
    }
}

const DB: &str = r#"
record(waveform, "WF") { field(NELM, "4") field(FTVL, "LONG") }
record(mca, "MI") { field(DTYP, "Intr Probe") field(INP, "WF") field(NMAX, "4") field(SCAN, "I/O Intr") }
record(ai, "AR") { field(DTYP, "Intr Probe") field(INP, "WF") field(SCAN, "I/O Intr") }
record(ai, "AS") { field(DTYP, "Intr Probe") field(INP, "WF") field(SCAN, "I/O Intr") }
record(ai, "AD") { field(DTYP, "Intr Probe") field(INP, "KEEP") field(SCAN, "I/O Intr") }
record(ai, "AN") { field(DTYP, "Intr NoDsxt") field(INP, "@x") field(SCAN, "I/O Intr") }
"#;

fn run(shell: &IocShell, line: &str) -> String {
    let out = std::env::temp_dir().join(format!("mca-intr-{}", std::process::id()));
    shell
        .execute_line(format!("{line} > {}", out.display()))
        .unwrap();
    let text = std::fs::read_to_string(&out).unwrap();
    std::fs::remove_file(&out).unwrap();
    text.trim_end().to_string()
}

// RTEMS-EXEC-MODEL-ALLOW(1): the runtime only feeds the build and
// `BlockingBridge::capture` for the shell; no CA server is built.
#[test]
fn a_refused_device_link_put_leaves_the_record_passive() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (name, factory) = mca_rs::mca_record_factory();
    let (db, bridge) = rt.block_on(async {
        let (db, _) = IocBuilder::new()
            .register_record_type(name, factory)
            // Keyed by record type too: C's `device()` rows are.
            .register_dynamic_device_support(&[("mca", &MCA_PROBE), ("ai", &AI_PROBES)], |ctx| {
                let dtyp = match (ctx.record_type, ctx.dtyp) {
                    ("mca" | "ai", "Intr Probe") => "Intr Probe",
                    ("ai", "Intr NoDsxt") => "Intr NoDsxt",
                    _ => return None,
                };
                Some(Box::new(Probe {
                    dtyp,
                    refuse: ctx.inp == "REFUSE",
                    keep: ctx.inp == "KEEP",
                }) as Box<dyn DeviceSupport>)
            })
            .db_string(DB, &HashMap::new())
            .unwrap()
            .build()
            .await
            .unwrap();
        (db, epics_base_rs::runtime::task::BlockingBridge::capture())
    });
    let shell = IocShell::new(Arc::clone(&db), bridge);
    let io_intr = || {
        let mut names = rt.block_on(db.records_for_scan(ScanType::IoIntr));
        names.sort();
        names
    };
    let gf = |pv: &str| run(&shell, &format!("dbgf {pv}"));
    let scan = |rec: &str| gf(&format!("{rec}.SCAN"));
    let pact = |rec: &str| gf(&format!("{rec}.PACT"));
    // `dbpr <rec> 4`'s (DSET, DPVT) as set or nil.
    let pointers = |rec: &str| {
        let printed = run(&shell, &format!("dbpr {rec} 4"));
        let set = |row: &str| !printed.contains(&format!("{row}: PTR (nil)"));
        (set("DSET"), set("DPVT"))
    };
    const PASSIVE: &str = r#"DBF_STRING:         "Passive""#;
    const IO_INTR: &str = r#"DBF_STRING:         "I/O Intr""#;
    const SET: &str = "DBF_UCHAR:          1 = 0x1";
    const CLEAR: &str = "DBF_UCHAR:          0 = 0x0";
    assert_eq!(io_intr(), ["AD", "AN", "AR", "AS", "MI"]);

    run(&shell, "dbpf AD.INP WF");
    assert_eq!((scan("AD"), pact("AD")), (IO_INTR.into(), CLEAR.into()));
    assert_eq!(gf("AD.INP"), r#"DBF_STRING:         "KEEP NPP NMS""#);
    let adds = MCA_ADDS.load(Ordering::Relaxed);
    run(&shell, "dbpf MI.INP WF");
    assert_eq!((scan("MI"), pact("MI")), (PASSIVE.into(), SET.into()));
    assert_eq!(MCA_ADDS.load(Ordering::Relaxed), adds);
    assert_eq!(pointers("MI"), (false, true));
    run(&shell, "dbpf AR.INP REFUSE");
    assert_eq!((scan("AR"), pact("AR")), (PASSIVE.into(), SET.into()));
    assert_eq!(pointers("AR"), (false, false));
    run(&shell, "dbpf AS.INP WF");
    assert_eq!((scan("AS"), pact("AS")), (IO_INTR.into(), CLEAR.into()));
    assert_eq!(run(&shell, "dbpf AN.INP @y"), r#"DBF_STRING:         "@x""#);
    assert_eq!(scan("AN"), IO_INTR);
    assert_eq!(io_intr(), ["AD", "AN", "AS"]);

    run(&shell, "dbpf AR.INP WF");
    assert_eq!((scan("AR"), pact("AR")), (PASSIVE.into(), CLEAR.into()));

    // The instance's own `dbPut` entry takes the same `dsxt` refusal.
    let an = db.get_record("AN").unwrap();
    let put = an
        .write()
        .put_common_field("INP", EpicsValue::String("@z".into()));
    assert!(put.is_err(), "{put:?}");
    assert_eq!(gf("AN.INP"), r#"DBF_STRING:         "@x""#);
    drop(shell);
    std::mem::forget(rt);
}
