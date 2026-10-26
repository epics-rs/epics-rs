//! A CA link read with an element count (`dbCaGetLink`'s `nelements`) runs
//! `aConvert` and ignores its status (dbCa.c:507-524): the read succeeds with
//! `ntoget` elements, no LINK alarm. `getString<Type>` (dbConvert.c) stops at
//! the first string it cannot parse, so the destination keeps what it held
//! from that element on. A scalar read (`nelements` NULL) and a DB link still
//! fail. Every value below is the C oracle IOC's, on the same db.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use epics_base_rs::error::CaError;
use epics_base_rs::server::database::{LinkRef, LinkSet, ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{CalcArg, CalcLink, ParsedLink, parse_link_v2};
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue, PvString};

const DB: &str = r#"
record(stringin, "SR") { field(VAL, "5") }
record(waveform, "SW") { field(FTVL, "STRING") field(NELM, "3") }
record(ai, "AIS") { field(INP, "SR CA") }
record(waveform, "WF") { field(NELM, "4") field(FTVL, "DOUBLE") field(INP, "SR CA") }
record(waveform, "W1") { field(NELM, "1") field(FTVL, "DOUBLE") field(INP, "SR CA") }
record(waveform, "WF2") { field(NELM, "4") field(FTVL, "DOUBLE") field(INP, "SW CA") }
record(waveform, "WFL") { field(NELM, "4") field(FTVL, "LONG") field(INP, "SW CA") }
record(waveform, "WFD") { field(NELM, "4") field(FTVL, "DOUBLE") field(INP, "SW") }
record(aai, "AI") { field(NELM, "4") field(FTVL, "DOUBLE") field(INP, "SW CA") }
record(subArray, "SA") { field(MALM, "4") field(NELM, "2") field(INDX, "1") field(FTVL, "DOUBLE") field(INP, "SW CA") }
record(aao, "AO") { field(NELM, "4") field(FTVL, "DOUBLE") field(OMSL, "closed_loop") field(DOL, "SW CA") }
record(compress, "CP") { field(ALG, "Circular Buffer") field(NSAM, "4") field(INP, "SW CA") }
record(acalcout, "AC") { field(NELM, "4") field(INAA, "SR CA") field(CALC, "AA") }
record(acalcout, "AC2") { field(NELM, "4") field(INAA, "SW CA") field(CALC, "AA") }
record(aSub, "AS") { field(INPA, "SW CA") field(FTA, "DOUBLE") field(NOA, "4") field(INPB, "SR CA") field(FTB, "DOUBLE") field(NOB, "4") }
record(waveform, "SC") { field(FTVL, "STRING") field(NELM, "3") }
record(printf, "PF") { field(FMT, "%ls") field(INP0, "SC CA") field(SIZV, "40") }
"#;

const READERS: &[&str] = &[
    "AIS", "WF", "W1", "WF2", "WFL", "WFD", "AI", "SA", "AO", "CP", "AC", "AC2", "AS", "PF",
];

fn strings(items: &[&str]) -> EpicsValue {
    EpicsValue::StringArray(items.iter().map(|s| PvString::from(*s)).collect())
}

/// The C string `pv`'s chars hold.
fn text(db: &PvDatabase, pv: &str) -> String {
    match db.get_pv(pv).unwrap() {
        EpicsValue::CharArray(b) => {
            let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
            String::from_utf8_lossy(&b[..end]).into_owned()
        }
        other => panic!("{pv} is {other:?}"),
    }
}

/// Every element `pv` serves.
fn nums(db: &PvDatabase, pv: &str) -> Vec<f64> {
    match db.get_pv(pv).unwrap().get_convert(DbFieldType::Double) {
        Ok(EpicsValue::DoubleArray(a)) => a.to_vec(),
        Ok(EpicsValue::Double(x)) => vec![x],
        other => panic!("{pv} is {other:?}"),
    }
}

/// `dbpf`: the put processes the passive source, which posts it.
async fn dbpf(db: &PvDatabase, rec: &str, value: EpicsValue) {
    db.put_record_field_from_ca(rec, "VAL", value)
        .await
        .unwrap();
}

async fn process(db: &PvDatabase, rec: &str) {
    db.process_record_with_links(rec, &mut ProcStack::new())
        .await
        .or_else(CaError::unless_process_status)
        .unwrap();
}

/// The link of `rec.field` has delivered one of `want`.
async fn delivered_any(db: &PvDatabase, rec: &str, field: &str, want: &[EpicsValue]) {
    let lset = db.link_set("ca").await.expect("the ca link set");
    let link = db
        .record_link_fields(rec)
        .into_iter()
        .find_map(|(f, _, link)| (f == field).then_some(link))
        .unwrap();
    let link = link.external().expect("a CA link");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let got = lset.get_cached_value(LinkRef::new(link.name, link.open));
        if got.as_ref().is_some_and(|got| want.contains(got)) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{rec}.{field} delivers {want:?}, has {got:?}"
        );
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
}

async fn delivered(db: &PvDatabase, rec: &str, field: &str, want: &EpicsValue) {
    delivered_any(db, rec, field, std::slice::from_ref(want)).await;
}

async fn open_links(db: &PvDatabase) {
    db.setup_external_link_opens().await;
    for record in db.all_record_names() {
        for (field, _, link) in db.record_link_fields(&record) {
            let Some(link) = link.external() else {
                continue;
            };
            let deadline = Instant::now() + Duration::from_secs(10);
            while db.external_link_metadata(link).is_none() {
                assert!(Instant::now() < deadline, "{record}.{field} connects");
                epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
            }
        }
    }
}

/// `(pv, C's value)`, numbers as f64; an array is every element served.
type Want = &'static [(&'static str, &'static [f64])];

const VALID: Want = &[
    ("AIS", &[5.0]),
    ("AIS.SEVR", &[0.0]),
    ("WF", &[5.0]),
    ("WF.NORD", &[1.0]),
    ("W1", &[5.0]),
    ("WF2", &[1.0, 2.0, 3.0]),
    ("WFL", &[1.0, 2.0, 3.0]),
    ("WFD", &[1.0, 2.0, 3.0]),
    ("AI", &[1.0, 2.0, 3.0]),
    ("SA", &[2.0, 3.0]),
    ("AO", &[1.0, 2.0, 3.0]),
    ("CP", &[1.0, 2.0, 3.0]),
    ("AC", &[5.0]),
    ("AC.AA", &[5.0, 0.0, 0.0, 0.0]),
    ("AC2", &[1.0]),
    ("AC2.AA", &[1.0, 2.0, 3.0, 0.0]),
    ("AS.A", &[1.0, 2.0, 3.0]),
    ("AS.NEA", &[3.0]),
    ("AS.B", &[5.0]),
    ("AS.NEB", &[1.0]),
    ("PF.SEVR", &[0.0]),
];

/// SR = "abc", SW = ["7","abc","9"]: element 0 of SW converts, the rest of
/// each destination is what the valid round left there.
const FAILING: Want = &[
    ("AIS", &[5.0]),
    ("AIS.STAT", &[14.0]),
    ("AIS.SEVR", &[3.0]),
    ("WF", &[5.0]),
    ("WF.NORD", &[1.0]),
    ("WF.SEVR", &[0.0]),
    ("W1", &[5.0]),
    ("W1.SEVR", &[0.0]),
    ("WF2", &[7.0, 2.0, 3.0]),
    ("WF2.NORD", &[3.0]),
    ("WF2.SEVR", &[0.0]),
    ("WFL", &[7.0, 2.0, 3.0]),
    ("WFL.SEVR", &[0.0]),
    ("WFD.STAT", &[14.0]),
    ("WFD.SEVR", &[3.0]),
    ("AI", &[7.0, 2.0, 3.0]),
    ("AI.SEVR", &[0.0]),
    ("SA", &[3.0, 3.0]),
    ("SA.SEVR", &[0.0]),
    ("AO", &[7.0, 2.0, 3.0]),
    ("AO.SEVR", &[0.0]),
    ("CP", &[3.0, 7.0, 2.0, 3.0]),
    ("CP.SEVR", &[0.0]),
    ("AC", &[5.0]),
    ("AC.AA", &[5.0, 0.0, 0.0, 0.0]),
    ("AC.SEVR", &[0.0]),
    ("AC2", &[7.0]),
    ("AC2.AA", &[7.0, 2.0, 3.0, 0.0]),
    ("AC2.SEVR", &[0.0]),
    ("AS.A", &[7.0, 2.0, 3.0]),
    ("AS.NEA", &[3.0]),
    ("AS.B", &[5.0]),
    ("AS.NEB", &[1.0]),
    ("AS.SEVR", &[0.0]),
    ("PF.SEVR", &[0.0]),
];

/// Each `pv` that does not serve C's value, as a line.
fn check(db: &PvDatabase, round: &str, want: Want) -> Vec<String> {
    want.iter()
        .filter_map(|&(pv, c)| {
            let got = nums(db, pv);
            (got != c).then(|| format!("{round} {pv}: rs {got:?}, C {c:?}"))
        })
        .collect()
}

#[epics_macros_rs::epics_test]
async fn a_ca_array_read_keeps_the_destination_from_the_failed_element_on() {
    let (db, _h) = IocBuilder::new()
        .register_record_type("acalcout", || Box::new(AcalcoutRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    db.scan_control().run();
    db.process_cp_links_on_connect().await;
    open_links(&db).await;

    dbpf(&db, "SW", strings(&["1", "2", "3"])).await;
    dbpf(&db, "SC", strings(&["72"])).await;
    delivered(&db, "WF2", "INP", &strings(&["1", "2", "3"])).await;
    delivered(&db, "WF", "INP", &EpicsValue::String(PvString::from("5"))).await;
    let one = [strings(&["72"]), EpicsValue::String(PvString::from("72"))];
    delivered_any(&db, "PF", "INP0", &one).await;
    for rec in READERS {
        process(&db, rec).await;
    }
    let mut wrong = check(&db, "valid", VALID);
    // `%ls` reads `DBR_CHAR` with a count: `getStringChar` parses each
    // string, and past the failed one PF's VAL held NUL.
    let pf = text(&db, "PF");
    if pf != "H" {
        wrong.push(format!("valid PF: rs {pf:?}, C \"H\""));
    }

    dbpf(&db, "SW", strings(&["7", "abc", "9"])).await;
    dbpf(&db, "SR", EpicsValue::String(PvString::from("abc"))).await;
    dbpf(&db, "SC", strings(&["72", "abc", "33"])).await;
    delivered(&db, "WF2", "INP", &strings(&["7", "abc", "9"])).await;
    delivered(&db, "WF", "INP", &EpicsValue::String(PvString::from("abc"))).await;
    delivered(&db, "PF", "INP0", &strings(&["72", "abc", "33"])).await;
    for rec in READERS {
        process(&db, rec).await;
    }
    wrong.extend(check(&db, "failing", FAILING));
    let pf = text(&db, "PF");
    if pf != "H" {
        wrong.push(format!("failing PF: rs {pf:?}, C \"H\""));
    }
    assert!(wrong.is_empty(), "\n{}", wrong.join("\n"));
}

/// C's `dbParseLink` has no scheme rule, so `pva://XSW` is a `PV_LINK` to a
/// name no record has: a `CA_LINK` (`dbLink.c:118-130`), read as WF2 is.
/// `{ca:...}` is a `JSON_LINK` by the braces rule (`dbStaticLib.c:2280-2287`)
/// and no `CA_LINK`, so its failed element fails the read.
#[epics_macros_rs::epics_test]
async fn a_pva_url_is_a_ca_read_and_a_ca_json_link_is_not() {
    const DB: &str = r#"
record(waveform, "PU") { field(NELM, "4") field(FTVL, "DOUBLE") field(INP, "pva://XSW") }
record(waveform, "JC") { field(NELM, "4") field(FTVL, "DOUBLE") field(INP, {ca:"XSW"}) }
"#;
    let (db, _h) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    // PU's CA name is the whole text, as dbCaAddLink is handed it.
    let remote = Arc::new(Remote(Mutex::new(HashMap::from([
        ("pva://XSW", strings(&["1", "2", "3"])),
        ("XSW", strings(&["1", "2", "3"])),
    ]))));
    db.register_link_set("ca", remote.clone()).await;
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    db.scan_control().run();
    db.process_cp_links_on_connect().await;
    db.setup_external_link_opens().await;
    for rec in ["PU", "JC"] {
        process(&db, rec).await;
    }
    let mut wrong = check(
        &db,
        "valid",
        &[("PU", &[1.0, 2.0, 3.0]), ("JC", &[1.0, 2.0, 3.0])],
    );

    remote.0.lock().unwrap().extend([
        ("pva://XSW", strings(&["7", "abc", "9"])),
        ("XSW", strings(&["7", "abc", "9"])),
    ]);
    for rec in ["PU", "JC"] {
        process(&db, rec).await;
    }
    wrong.extend(check(
        &db,
        "failing",
        &[
            ("PU", &[7.0, 2.0, 3.0]),
            ("PU.SEVR", &[0.0]),
            ("JC.STAT", &[14.0]),
            ("JC.SEVR", &[3.0]),
        ],
    ));
    assert!(wrong.is_empty(), "\n{}", wrong.join("\n"));
}

/// A remote IOC serving each name in its map, connected.
struct Remote(Mutex<HashMap<&'static str, EpicsValue>>);

#[async_trait::async_trait]
impl LinkSet for Remote {
    fn is_connected(&self, link: LinkRef<'_>) -> bool {
        self.get_cached_value(link).is_some()
    }
    async fn get_value(&self, link: LinkRef<'_>) -> Option<EpicsValue> {
        self.get_cached_value(link)
    }
    fn get_cached_value(&self, link: LinkRef<'_>) -> Option<EpicsValue> {
        let name = std::str::from_utf8(link.name).ok()?;
        self.0.lock().unwrap().get(name).cloned()
    }
}

/// A DB link to a name this IOC does not serve is a CA link
/// (`dbLink.c:118-130`), so XW reads as WF2 does above. lnkCalc's child
/// read passes `&nReq` (`lnkCalc.c:542`, `:585`), so a CA child keeps
/// `arg[i]` when its string does not parse. No link text yields a CA or DB
/// child, `lnkCalc_string` refusing a string `args` element
/// (`lnkCalc.c:188-191`), so LCA's and LCD's are built.
#[epics_macros_rs::epics_test]
async fn an_external_db_link_and_a_calc_link_child_are_ca_reads() {
    const DB: &str = r#"
record(waveform, "XW") { field(NELM, "4") field(FTVL, "DOUBLE") field(INP, "XSW") }
record(ai, "LCA") {}
record(ai, "LCD") {}
"#;
    let (db, _h) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    for (rec, child) in [("LCA", "XSR CA"), ("LCD", "XSR")] {
        let calc = CalcLink {
            expr: Some("A+1".into()),
            major: None,
            minor: None,
            out: None,
            args: vec![CalcArg::Link(Box::new(parse_link_v2(child)))],
            time_source: None,
            val: Default::default(),
            prec: 15,
            units: None,
        };
        let link = Arc::new(ParsedLink::Calc(Box::new(calc)));
        let cell = db.get_record(rec).unwrap();
        cell.write().install_link_cache("INP", link).unwrap();
    }
    let remote = Arc::new(Remote(Mutex::new(HashMap::from([
        ("XSW", strings(&["1", "2", "3"])),
        ("XSR", EpicsValue::String(PvString::from("5"))),
    ]))));
    db.register_link_set("ca", remote.clone()).await;
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    db.scan_control().run();
    db.process_cp_links_on_connect().await;
    db.setup_external_link_opens().await;
    for rec in ["XW", "LCA", "LCD"] {
        process(&db, rec).await;
    }
    let mut wrong = check(
        &db,
        "valid",
        &[("XW", &[1.0, 2.0, 3.0]), ("LCA", &[6.0]), ("LCD", &[6.0])],
    );

    remote.0.lock().unwrap().extend([
        ("XSW", strings(&["7", "abc", "9"])),
        ("XSR", EpicsValue::String(PvString::from("abc"))),
    ]);
    for rec in ["XW", "LCA", "LCD"] {
        process(&db, rec).await;
    }
    wrong.extend(check(
        &db,
        "failing",
        &[
            ("XW", &[7.0, 2.0, 3.0]),
            ("XW.SEVR", &[0.0]),
            ("LCA", &[6.0]),
            ("LCA.SEVR", &[0.0]),
            ("LCD", &[6.0]),
            ("LCD.SEVR", &[0.0]),
        ],
    ));
    assert!(wrong.is_empty(), "\n{}", wrong.join("\n"));
}
