//! A record type's `init_record(1)` runs its own C loop over the links it
//! declares in `Record::init_link_reads`, each answered as C's `dbGetLink`
//! answers inside `init_record` (`tableRecord.c:1106-1133`): a DB link
//! delivers its target converted to the requested type, an NPP link without
//! processing it and a PP link after `dbScanPassive` processes it
//! (`dbDbLink.c:175-176`); a constant link succeeds and writes nothing; a link
//! to a name no record serves is a CA link, not connected at init, so the read
//! fails and raises LINK/INVALID (`dbLink.c:338-339`) — but only for a link
//! the record actually reads; a hardware link has no lset and fails with no
//! alarm (`dbLink.c:336-337`).
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::simm::LinkFetch;
use epics_base_rs::server::record::{FieldDesc, InitLinkReads, LinkReadAs, ProcessOutcome, Record};
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// What each record's `init_record(1)` left in `R0`/`R1`, by record.
type Seen = Arc<Mutex<Vec<(String, f64, f64)>>>;

/// table's `GetReadback` shape: `r[i] = 0` then, where the axis may be read,
/// `dbGetLink(&r0xi[i], DBR_DOUBLE, &r[i])`, stopping at the first failure.
/// `GATE` stands in for `can_RW_drive`: a gated axis reads 0 and no link.
struct Readback {
    name: String,
    links: [String; 2],
    gate: [bool; 2],
    r: [f64; 2],
    seen: Seen,
}

static FIELDS: &[FieldDesc] = &[
    FieldDesc::new("TAG", DbFieldType::String, false),
    FieldDesc::new("L0", DbFieldType::String, false),
    FieldDesc::new("L1", DbFieldType::String, false),
    FieldDesc::new("G0", DbFieldType::Short, false),
    FieldDesc::new("G1", DbFieldType::Short, false),
    FieldDesc::new("R0", DbFieldType::Double, false),
    FieldDesc::new("R1", DbFieldType::Double, false),
];

impl Record for Readback {
    fn record_type(&self) -> &'static str {
        "readback"
    }

    fn declared_fields(&self) -> &'static [FieldDesc] {
        FIELDS
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "TAG" => Some(EpicsValue::String(self.name.clone().into())),
            "L0" => Some(EpicsValue::String(self.links[0].clone().into())),
            "L1" => Some(EpicsValue::String(self.links[1].clone().into())),
            "G0" => Some(EpicsValue::Short(self.gate[0].into())),
            "G1" => Some(EpicsValue::Short(self.gate[1].into())),
            "R0" => Some(EpicsValue::Double(self.r[0])),
            "R1" => Some(EpicsValue::Double(self.r[1])),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "TAG" => self.name = value.to_string(),
            "L0" => self.links[0] = value.to_string(),
            "L1" => self.links[1] = value.to_string(),
            "G0" => self.gate[0] = value.to_f64() != Some(0.0),
            "G1" => self.gate[1] = value.to_f64() != Some(0.0),
            "R0" => self.r[0] = value.to_f64().unwrap_or(f64::NAN),
            "R1" => self.r[1] = value.to_f64().unwrap_or(f64::NAN),
            _ => {}
        }
        Ok(())
    }

    fn init_link_reads(&self) -> &'static [(&'static str, LinkReadAs)] {
        &[("L0", LinkReadAs::Double), ("L1", LinkReadAs::Double)]
    }

    fn init_record_with_links(&mut self, reads: &mut InitLinkReads) -> CaResult<()> {
        for (i, link) in ["L0", "L1"].into_iter().enumerate() {
            self.r[i] = 0.0;
            if !self.gate[i] {
                continue;
            }
            match reads.get(link) {
                LinkFetch::Value(v) => self.r[i] = v.to_f64().unwrap_or(f64::NAN),
                LinkFetch::NoData => {}
                LinkFetch::Failed => break,
            }
        }
        self.seen
            .lock()
            .unwrap()
            .push((self.name.clone(), self.r[0], self.r[1]));
        Ok(())
    }
}

#[epics_macros_rs::epics_test]
async fn init_record_with_links_runs_the_records_own_c_loop() {
    let seen: Seen = Arc::default();
    let factory = seen.clone();
    // The readers load before their sources: C reads at pass 1, after every
    // record's pass 0, so load order does not matter.
    let db = IocBuilder::new()
        .register_record_type("readback", move || {
            Box::new(Readback {
                name: String::new(),
                links: Default::default(),
                gate: [true; 2],
                r: [-1.0; 2],
                seen: factory.clone(),
            })
        })
        .db_string(
            r#"
record(readback, "DB") { field(TAG, "DB") field(L0, "LI") field(L1, "AO.HOPR") }
record(readback, "CONST") { field(TAG, "CONST") field(L0, "3") field(R0, "5") field(L1, "LI") }
record(readback, "CA") { field(TAG, "CA") field(L0, "NOSUCH") field(L1, "LI") field(R1, "1.25") }
record(readback, "PP") { field(TAG, "PP") field(L0, "CNT PP") field(G1, "0") }
record(readback, "MS") { field(TAG, "MS") field(L0, "LOWA PP MS") field(G1, "0") }
record(readback, "MSS") { field(TAG, "MSS") field(L0, "LOWB PP MSS") field(G1, "0") }
record(readback, "HW") { field(TAG, "HW") field(L0, "@dev 1") field(L1, "LI") field(R1, "2.5") }
record(readback, "GATED") { field(TAG, "GATED") field(G0, "0") field(L0, "NOSUCH") field(L1, "LI") }
record(readback, "TIEMS") { field(TAG, "TIEMS") field(L0, "HIA PP MS") field(L1, "NOSUCH") }
record(readback, "TIEMSS") { field(TAG, "TIEMSS") field(L0, "HIB PP MSS") field(L1, "NOSUCH") }
record(longin, "LI") { field(VAL, "7") }
record(calc, "CNT") { field(CALC, "VAL+1") }
record(calc, "LOWA") { field(CALC, "1") field(LOLO, "5") field(LLSV, "MAJOR") }
record(calc, "LOWB") { field(CALC, "1") field(LOLO, "5") field(LLSV, "MAJOR") }
record(calc, "HIA") { field(CALC, "1") field(HIHI, "0") field(HHSV, "INVALID") }
record(calc, "HIB") { field(CALC, "1") field(HIHI, "0") field(HHSV, "INVALID") }
record(calc, "AO") { field(CALC, "VAL+1") field(VAL, "7.5") field(HOPR, "9") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    let mut seen = seen.lock().unwrap().clone();
    seen.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        seen,
        [
            // A failed read stops the loop: R1 keeps its loaded 1.25.
            ("CA".to_string(), 0.0, 1.25),
            // A constant link writes nothing: R0 is the loop's own 0.
            ("CONST".to_string(), 0.0, 7.0),
            // A LONG source read at DBR_DOUBLE.
            ("DB".to_string(), 7.0, 9.0),
            // A gated axis reads no link.
            ("GATED".to_string(), 0.0, 7.0),
            // A hardware link has no lset: the read fails and stops the loop.
            ("HW".to_string(), 0.0, 2.5),
            ("MS".to_string(), 1.0, 0.0),
            ("MSS".to_string(), 1.0, 0.0),
            // A PP read processes its passive source first, as
            // dbDbGetValue's dbScanPassive does (dbDbLink.c:175-176).
            ("PP".to_string(), 1.0, 0.0),
            ("TIEMS".to_string(), 1.0, 0.0),
            ("TIEMSS".to_string(), 1.0, 0.0),
        ]
    );

    let alarm = |rec: &str| {
        let f = |field: &str| db.get_pv(format!("{rec}.{field}")).unwrap().to_f64();
        (f("NSTA"), f("NSEV"))
    };
    // LINK_ALARM (14) / INVALID (3), only where the failed link was read.
    assert_eq!(alarm("CA"), (Some(14.0), Some(3.0)));
    assert_eq!(alarm("GATED"), (Some(0.0), Some(0.0)));
    // ...but as `S_db_noLSET`, which `dbGetLink` answers -1 with no alarm
    // (`dbLink.c:336-337`).
    assert_eq!(alarm("HW"), (Some(0.0), Some(0.0)));
    assert_eq!(alarm("DB"), (Some(0.0), Some(0.0)));
    // A good read folds the source's LOLO/MAJOR by the link's MS flag
    // (`dbDbLink.c:228-232`): MS as LINK_ALARM, MSS with the source's status.
    assert_eq!(alarm("MS"), (Some(14.0), Some(2.0)));
    assert_eq!(alarm("MSS"), (Some(5.0), Some(2.0)));
    // An INVALID inherit and then a failed read's LINK/INVALID tie on
    // severity, so the first one taken keeps STAT and AMSG: C applies each
    // inline as its dbGetLink returns (dbDbLink.c:225-229, dbLink.c:337-338).
    let namsg = |rec: &str| db.get_pv(format!("{rec}.NAMSG")).unwrap().to_string();
    assert_eq!(alarm("TIEMS"), (Some(14.0), Some(3.0)));
    assert_eq!(namsg("TIEMS"), "");
    assert_eq!(alarm("TIEMSS"), (Some(3.0), Some(3.0)));
    assert_eq!(namsg("TIEMSS"), "");
    // An NPP read does not process: processing would have made it 8.5.
    assert_eq!(db.get_pv("AO").unwrap().to_f64(), Some(7.5));
    // Processed once, by the one PP read.
    assert_eq!(db.get_pv("CNT").unwrap().to_f64(), Some(1.0));
}

/// The PP read's `dbScanPassive` is C `processTarget` with the reader as
/// `psrc` (`dbDbLink.c:175-176`, `:457`), at init as in a cycle: a source
/// disabled on the reader's PACT does not process.
#[epics_macros_rs::epics_test]
async fn an_init_pp_read_holds_the_reader_pact() {
    let seen: Seen = Arc::default();
    let factory = seen.clone();
    let db = IocBuilder::new()
        .register_record_type("readback", move || {
            Box::new(Readback {
                name: String::new(),
                links: Default::default(),
                gate: [true, false],
                r: [-1.0; 2],
                seen: factory.clone(),
            })
        })
        .db_string(
            r#"
record(readback, "RD") { field(TAG, "RD") field(L0, "SRC PP") }
record(calc, "SRC") { field(CALC, "VAL+1") field(SDIS, "RD.PACT") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    assert_eq!(*seen.lock().unwrap(), [("RD".to_string(), 0.0, 0.0)]);
    // menuAlarmStat DISABLE.
    assert_eq!(db.get_pv("SRC.STAT").unwrap().to_f64(), Some(18.0));
    assert_eq!(db.get_pv("RD.PACT").unwrap().to_f64(), Some(0.0));
}

/// A record created outside the LOAD phase, and a `.db` merge into a live
/// record, run pass 1 through the same reads as `iocInit`'s drain: C has one
/// `init_record`, so an init path that answers "unset" is a second meaning.
#[epics_macros_rs::epics_test]
async fn the_eager_and_merge_inits_read_their_links() {
    use epics_base_rs::server::database::PvDatabase;
    use epics_base_rs::server::records::calc::CalcRecord;

    let seen: Seen = Arc::default();
    let factory = seen.clone();
    let readback = move || Readback {
        name: "R".into(),
        links: Default::default(),
        gate: [true; 2],
        r: [-1.0; 2],
        seen: factory.clone(),
    };
    let make = readback.clone();
    epics_base_rs::server::db_loader::register_record_type(
        "readback",
        Box::new(move || Box::new(make())),
    );

    let db = Arc::new(PvDatabase::new());
    for (name, calc) in [("SEVEN", "7"), ("NINE", "9")] {
        db.add_record(name, Box::new(CalcRecord::new(calc)))
            .await
            .unwrap();
    }
    let mut r = readback();
    r.links[0] = "SEVEN PP".into();
    db.add_record("R", Box::new(r)).await.unwrap();
    // Created eagerly: the PP read processed SEVEN first.
    assert_eq!(seen.lock().unwrap().pop(), Some(("R".into(), 7.0, 0.0)));

    // The record is reachable before its pass 1, as a C record is between
    // `dbLoadRecords` and `iocInit`: a put landing there is overwritten by the
    // init that follows it. PUT's OUT lands in W.R1 while W's own PP read is
    // processing PUT, and W's loop then zeroes R1.
    let mut put = epics_base_rs::server::db_loader::create_record("calcout").unwrap();
    put.put_field("CALC", EpicsValue::String("5".into()))
        .unwrap();
    db.add_loaded_record(
        "PUT",
        put,
        epics_base_rs::server::database::RecordLoad::from_common_fields(vec![(
            "OUT".into(),
            EpicsValue::String("W.R1".into()),
        )]),
    )
    .await
    .unwrap();
    let mut w = readback();
    w.name = "W".into();
    w.links[0] = "PUT PP".into();
    w.gate[1] = false;
    db.add_record("W", Box::new(w)).await.unwrap();
    assert_eq!(seen.lock().unwrap().pop(), Some(("W".into(), 5.0, 0.0)));
    assert_eq!(db.get_pv("W.R1").unwrap().to_f64(), Some(0.0));

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("merge.db");
    std::fs::write(
        &path,
        "record(readback, \"R\") { field(L0, \"NINE PP\") }\n",
    )
    .unwrap();
    let line = format!("dbLoadRecords(\"{}\")", path.display());
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let shell_db = Arc::clone(&db);
    std::thread::spawn(move || {
        epics_base_rs::runtime::task::set_thread_ok_to_block(true);
        epics_base_rs::server::iocsh::IocShell::new(shell_db, bridge).execute_line(&line)
    })
    .join()
    .expect("the shell thread")
    .expect("dbLoadRecords");
    // The merge re-ran init against the new link.
    assert_eq!(seen.lock().unwrap().pop(), Some(("R".into(), 9.0, 0.0)));
}
