//! Parsing a link and being able to service it are two different facts.
//!
//! C keeps them together by accident: its accepted-type registry IS the DBD
//! `link()` table, so an IOC that never loaded `pvxs7x.dbd` has no `pva`
//! entry, `dbjl_map_key` refuses the field (`dbFindLinkSup`,
//! `dbJLink.c:262-266`) and `dbLoadRecords` reports the file as failed.
//! Measured on `R7.0.10-146-g8f5015b66` with `softIoc`, a `longin` whose
//! `INP` is `{pva:{pv:"SRC"}}`:
//!
//! ```text
//! ERROR: Can't set 'IN.INP' to '{pva:{pv:"SRC"}}'  : Bad Field value
//! ERROR: Failed to load 'c8.db'
//! ...
//! INP : CONSTANT   STAT: UDF   SEVR: INVALID   UDF : 1
//! ```
//!
//! i.e. the record keeps an EMPTY link, byte for byte what a `longin` with
//! no `INP` at all shows.
//!
//! The port refuses the same field at load when no `pva` link type is
//! declared (`softioc_undeclared_json_link_type` pins that). This file is the
//! other fact: a binary that declares `pva` but has installed no `pva` link
//! set. The link used to load clean and, because the lset boundary addressed
//! a `PvaJson`/`Pva` link by its BARE name, it was dispatched to whatever other
//! lset happened to be registered: with a `ca` link set installed the
//! measured `{pva:{pv:"SRC"}}` link asked the CA lset for `SRC` (`cached:SRC`,
//! `open:SRC`) — a pva link silently serviced over Channel Access, with
//! nothing said at boot.
//!
//! The installers fire at `initHookAfterCaLinkInit`, long after
//! `dbLoadRecords`, so that refusal is at link init, which is
//! `PvDatabase::db_init_link_locality` — the single owner of the
//! post-`dbInitLink` view that both the parse cache
//! (`initialize_link_locality`) and every re-parsing consumer
//! (`record_link_fields` → `setup_cp_links`, `setup_external_link_opens`,
//! `dbcaxr`) already route through.

use epics_base_rs::server::database::{LinkRef, LinkSet, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::ParsedLink;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;
use std::sync::Arc;

/// Records every name it is asked about, so "was this link handed to the
/// WRONG scheme's link set" is an assertion and not an inference.
struct SpyLset {
    scheme: &'static str,
    connected: bool,
    asked: parking_lot::Mutex<Vec<String>>,
}

impl SpyLset {
    fn new(scheme: &'static str) -> Arc<Self> {
        Arc::new(Self {
            scheme,
            connected: true,
            asked: parking_lot::Mutex::new(Vec::new()),
        })
    }
    /// A link set whose channels never connect, as dbCa's do with no server.
    fn down(scheme: &'static str) -> Arc<Self> {
        Arc::new(Self {
            scheme,
            connected: false,
            asked: parking_lot::Mutex::new(Vec::new()),
        })
    }
    fn asked(&self) -> Vec<String> {
        self.asked.lock().clone()
    }
}

#[async_trait::async_trait]
impl LinkSet for SpyLset {
    fn is_connected(&self, _name: LinkRef<'_>) -> bool {
        self.connected
    }
    async fn get_value(&self, link: LinkRef<'_>) -> Option<EpicsValue> {
        let name = link.name;
        self.asked.lock().push(format!(
            "{}:get:{}",
            self.scheme,
            String::from_utf8_lossy(name)
        ));
        self.connected.then_some(EpicsValue::Long(3))
    }
    fn get_cached_value(&self, link: LinkRef<'_>) -> Option<EpicsValue> {
        let name = link.name;
        self.asked.lock().push(format!(
            "{}:read:{}",
            self.scheme,
            String::from_utf8_lossy(name)
        ));
        self.connected.then_some(EpicsValue::Long(3))
    }
    async fn connect_link(
        &self,
        link: LinkRef<'_>,
        _: Option<epics_base_rs::server::database::CaLinkInit>,
    ) {
        let name = link.name;
        self.asked.lock().push(format!(
            "{}:open:{}",
            self.scheme,
            String::from_utf8_lossy(name)
        ));
    }
}

const DB: &str = r#"
record(longin, "JSON")   { field(INP, "{pva:{pv:\"SRC\"}}") }
record(longin, "OPTS")   { field(INP, "{pva:{pv:\"SRC\",proc:true}}") }
record(longin, "CALC")   { field(INP, "{calc:{expr:\"1+1\"}}") }
record(longin, "CONST")  { field(INP, "{const:5}") }
record(longin, "CAMOD")  { field(INP, "SRC CA") }
record(longin, "BARE")   { field(INP, "OTHER:PV") }
record(calc,   "REPARSED") { field(CALC, "A") field(INPA, "{pva:{pv:\"SRC\"}}") }
record(longin, "PVAURL") { field(INP, "pva://SRC") }
record(longin, "CAURL")  { field(INP, "ca://SRC") }
record(calcout, "CO:PVAURL") { field(CALC, "A") field(INPA, "pva://SRC") }
record(calcout, "CO:CAURL")  { field(CALC, "A") field(INPA, "ca://SRC") }
"#;

async fn build(lsets: &[(&'static str, Arc<SpyLset>)]) -> Arc<PvDatabase> {
    let db = IocBuilder::new()
        // QSRV2 disabled: pvalink's installer installs nothing.
        .register_link_type("pva", |_db| async { Vec::new() })
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    for (scheme, l) in lsets {
        db.register_link_set(scheme, l.clone()).await;
    }
    db.initialize_link_locality().await;
    db
}

/// The post-`dbInitLink` view of `REC.INP`, i.e. what every consumer sees.
fn inp(db: &PvDatabase, rec: &str) -> ParsedLink {
    db.record_link_fields(rec)
        .into_iter()
        .find(|(f, _, _)| f == "INP")
        .map(|(_, _, p)| p)
        .unwrap_or(ParsedLink::None)
}

/// The measured defect: no `pva` link set installed, so every spelling of a
/// pva link is refused rather than loading clean.
#[epics_macros_rs::epics_test]
async fn a_pva_link_with_no_pva_link_set_is_refused() {
    let db = build(&[]).await;
    for rec in ["JSON", "OPTS"] {
        assert!(
            matches!(inp(&db, rec), ParsedLink::None),
            "{rec}.INP must be refused with no 'pva' link set, got {:?}",
            inp(&db, rec)
        );
    }
}

/// The silent re-routing that made the missing diagnostic dangerous: with a
/// `ca` link set installed and no `pva` one, the bare-name lset dispatch used
/// to hand `{pva:{pv:"SRC"}}` to Channel Access.
#[epics_macros_rs::epics_test]
async fn a_pva_link_is_never_serviced_by_another_schemes_link_set() {
    let ca = SpyLset::new("ca");
    let db = build(&[("ca", ca.clone())]).await;

    assert!(matches!(inp(&db, "JSON"), ParsedLink::None));
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    let _ = db.process_record_with_links("JSON", &mut visited).await;
    db.sync_external_link_puts().await;

    assert_eq!(
        ca.asked(),
        Vec::<String>::new(),
        "the CA link set must never be asked about a pva link"
    );
    assert_eq!(
        db.get_pv("JSON").unwrap(),
        EpicsValue::Long(0),
        "a refused link delivers nothing, exactly as C's empty link does"
    );
}

/// The gate must not over-reject: the same links with a `pva` link set
/// installed are serviced through it.
#[epics_macros_rs::epics_test]
async fn the_same_links_are_serviced_when_the_link_set_is_installed() {
    let pva = SpyLset::new("pva");
    let db = build(&[("pva", pva.clone())]).await;

    assert!(
        matches!(inp(&db, "JSON"), ParsedLink::Pva(_)),
        "got {:?}",
        inp(&db, "JSON")
    );
    assert!(matches!(inp(&db, "OPTS"), ParsedLink::PvaJson(_)));

    let mut visited = epics_base_rs::server::database::ProcStack::new();
    let _ = db.process_record_with_links("JSON", &mut visited).await;
    assert_eq!(db.get_pv("JSON").unwrap(), EpicsValue::Long(3));
    assert!(
        pva.asked().iter().any(|a| a.starts_with("pva:")),
        "the pva link set services its own scheme, got {:?}",
        pva.asked()
    );
}

/// The exemptions, each for its own reason: `calc` and `const` are serviced
/// in process and need nothing installed; a bare non-local name and the
/// `X CA` modifier both parse to the `Ca(CaLink)` of the `dbInitLink`
/// fallthrough, and `ca` has no `link()` line in base or pvxs to match
/// against in the first place.
#[epics_macros_rs::epics_test]
async fn locally_serviced_and_ca_shaped_links_are_not_refused() {
    let db = build(&[]).await;
    assert!(matches!(inp(&db, "CALC"), ParsedLink::Calc(_)));
    assert!(matches!(inp(&db, "CONST"), ParsedLink::Constant(_)));
    assert!(matches!(inp(&db, "CAMOD"), ParsedLink::Ca(_)));
    assert!(
        matches!(inp(&db, "BARE"), ParsedLink::Ca(_)),
        "a bare non-local name still takes dbInitLink's CA fallthrough"
    );
}

/// The init-time refusal only reaches links that HAVE a parse cache. A calc
/// record's `INPA` is re-parsed from its raw text every cycle
/// (`initialize_link_locality` says so, and `record_link_fields` does not
/// enumerate it), so the link set has to survive to the boundary as well:
/// `external_link` addresses it to `LinkSetTarget::Pva`. Addressed by its
/// bare name alone the link is a CA link, and was measured being read from
/// the CA link set (`read:SRC`, `A` delivered 9.0).
#[epics_macros_rs::epics_test]
async fn a_reparsed_pva_link_is_not_serviced_by_another_schemes_link_set() {
    let ca = SpyLset::new("ca");
    let db = build(&[("ca", ca.clone())]).await;

    let mut visited = epics_base_rs::server::database::ProcStack::new();
    let _ = db.process_record_with_links("REPARSED", &mut visited).await;

    assert_eq!(
        ca.asked(),
        Vec::<String>::new(),
        "the CA link set must never be asked about a re-parsed pva link"
    );
    assert_eq!(db.get_pv("REPARSED").unwrap(), EpicsValue::Double(0.0));
}

/// `dbParseLink` has no URL scheme (`dbStaticLib.c:2280-2356`): `pva://SRC`
/// and `ca://SRC` are PV names, which `dbInitLink` hands to `dbCaAddLink`
/// verbatim like any other non-local name. Measured on
/// `R7.0.10-146-g8f5015b66` `softIoc`, no server answering, after one
/// process of each record:
///
/// ```text
/// PVAURL     INP : CA_LINK pva://SRC NPP NMS   STAT: LINK  SEVR: INVALID
/// CAURL      INP : CA_LINK ca://SRC NPP NMS    STAT: LINK  SEVR: INVALID
/// CO:PVAURL  INPA: CA_LINK pva://SRC NPP NMS   INAV: Ext PV NC  STAT: LINK  SEVR: INVALID
/// CO:CAURL   INPA: CA_LINK ca://SRC NPP NMS    INAV: Ext PV NC  STAT: LINK  SEVR: INVALID
/// ```
#[epics_macros_rs::epics_test]
async fn a_scheme_spelling_is_a_ca_pv_name() {
    use epics_base_rs::server::recgbl::alarm_status::LINK_ALARM;
    use epics_base_rs::server::record::AlarmSeverity;

    let records = [
        ("PVAURL", "pva://SRC", "field INP"),
        ("CAURL", "ca://SRC", "field INP"),
        ("CO:PVAURL", "pva://SRC", "field INPA"),
        ("CO:CAURL", "ca://SRC", "field INPA"),
    ];
    let started = |ca: Arc<SpyLset>| async move {
        let db = build(&[("ca", ca)]).await;
        db.ioc_init().await;
        db.process_cp_links_on_connect().await;
        db
    };

    let db = started(SpyLset::down("ca")).await;
    for (rec, name) in [("PVAURL", "pva://SRC"), ("CAURL", "ca://SRC")] {
        match inp(&db, rec) {
            ParsedLink::Ca(link) => assert_eq!(link.pv, name, "{rec}.INP"),
            other => panic!("{rec}.INP must be a CA link to `{name}`, got {other:?}"),
        }
    }
    for (rec, _, amsg) in records {
        let mut visited = epics_base_rs::server::database::ProcStack::new();
        let _ = db.process_record_with_links(rec, &mut visited).await;
        let alarm = {
            let inst = db.get_record(rec).unwrap();
            let inst = inst.read();
            (
                inst.common.stat,
                inst.common.sevr,
                inst.common.amsg.to_string(),
            )
        };
        assert_eq!(
            alarm,
            (LINK_ALARM, AlarmSeverity::Invalid, amsg.to_string()),
            "{rec}"
        );
    }
    for rec in ["CO:PVAURL", "CO:CAURL"] {
        assert_eq!(
            db.get_pv(format!("{rec}.INAV")).unwrap().to_f64(),
            Some(0.0),
            "{rec}.INAV is Ext PV NC"
        );
    }

    // Connected, the CA link set is read for the name verbatim.
    let ca = SpyLset::new("ca");
    let db = started(ca.clone()).await;
    for (rec, name, _) in records {
        let mut visited = epics_base_rs::server::database::ProcStack::new();
        let _ = db.process_record_with_links(rec, &mut visited).await;
        assert_eq!(db.get_pv(rec).unwrap().to_f64(), Some(3.0), "{rec}");
        let asked = ca.asked();
        assert!(
            asked.contains(&format!("ca:read:{name}")),
            "{rec}: the CA link set is read for `{name}`, got {asked:?}"
        );
    }
}
