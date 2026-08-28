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
//! This port's accepted-type table is static, so `{pva:…}` parses in every
//! binary — including `softioc-rs`, which is C's CA-only `softIoc` and
//! installs no `pva` link set. The link then loaded clean and, because the
//! lset boundary addresses a `PvaJson`/`Pva` link by its BARE name, it was
//! dispatched to whatever other lset happened to be registered: with a `ca`
//! link set installed the measured `{pva:{pv:"SRC"}}` link asked the CA lset
//! for `SRC` (`cached:SRC`, `open:SRC`) — a pva link silently serviced over
//! Channel Access, with nothing said at boot.
//!
//! The port cannot refuse at load the way C does: the link-set installers
//! fire at `initHookAfterCaLinkInit`, long after `dbLoadRecords`. So the
//! refusal is at link init, which is `PvDatabase::db_init_link_locality` —
//! the single owner of the post-`dbInitLink` view that both the parse cache
//! (`initialize_link_locality`) and every re-parsing consumer
//! (`record_link_fields` → `setup_cp_links`, `setup_external_link_opens`,
//! `dbcaxr`) already route through.

use epics_base_rs::server::database::{LinkSet, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::ParsedLink;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;
use std::sync::Arc;

/// Records every name it is asked about, so "was this link handed to the
/// WRONG scheme's link set" is an assertion and not an inference.
struct SpyLset {
    scheme: &'static str,
    asked: parking_lot::Mutex<Vec<String>>,
}

impl SpyLset {
    fn new(scheme: &'static str) -> Arc<Self> {
        Arc::new(Self {
            scheme,
            asked: parking_lot::Mutex::new(Vec::new()),
        })
    }
    fn asked(&self) -> Vec<String> {
        self.asked.lock().clone()
    }
}

#[async_trait::async_trait]
impl LinkSet for SpyLset {
    fn is_connected(&self, _name: &str) -> bool {
        true
    }
    async fn get_value(&self, name: &str) -> Option<EpicsValue> {
        self.asked
            .lock()
            .push(format!("{}:get:{name}", self.scheme));
        Some(EpicsValue::Long(3))
    }
    fn get_cached_value(&self, name: &str) -> Option<EpicsValue> {
        self.asked
            .lock()
            .push(format!("{}:read:{name}", self.scheme));
        Some(EpicsValue::Long(3))
    }
    async fn connect_link(&self, name: &str) {
        self.asked
            .lock()
            .push(format!("{}:open:{name}", self.scheme));
    }
}

const DB: &str = r#"
record(longin, "JSON")   { field(INP, "{pva:{pv:\"SRC\"}}") }
record(longin, "OPTS")   { field(INP, "{pva:{pv:\"SRC\",proc:true}}") }
record(longin, "PREFIX") { field(INP, "pva://SRC") }
record(longin, "CALC")   { field(INP, "{calc:{expr:\"1+1\"}}") }
record(longin, "CONST")  { field(INP, "{const:5}") }
record(longin, "CAPRE")  { field(INP, "ca://SRC") }
record(longin, "BARE")   { field(INP, "OTHER:PV") }
record(calc,   "REPARSED") { field(CALC, "A") field(INPA, "{pva:{pv:\"SRC\"}}") }
"#;

async fn build(lsets: &[(&'static str, Arc<SpyLset>)]) -> Arc<PvDatabase> {
    let db = IocBuilder::new()
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
    for rec in ["JSON", "OPTS", "PREFIX"] {
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
    let mut visited = std::collections::HashSet::new();
    let _ = db.process_record_with_links("JSON", &mut visited, 0).await;
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
    assert!(matches!(inp(&db, "PREFIX"), ParsedLink::Pva(_)));

    let mut visited = std::collections::HashSet::new();
    let _ = db.process_record_with_links("JSON", &mut visited, 0).await;
    assert_eq!(db.get_pv("JSON").unwrap(), EpicsValue::Long(3));
    assert!(
        pva.asked().iter().any(|a| a.starts_with("pva:")),
        "the pva link set services its own scheme, got {:?}",
        pva.asked()
    );
}

/// The exemptions, each for its own reason: `calc` and `const` are serviced
/// in process and need nothing installed; `ca://` and a bare non-local name
/// both parse to the same `Ca(CaLink)` as the `dbInitLink` fallthrough and
/// the `X CA` modifier, so refusing them would refuse those — and `ca` has no
/// `link()` line in base or pvxs to match against in the first place.
#[epics_macros_rs::epics_test]
async fn locally_serviced_and_ca_shaped_links_are_not_refused() {
    let db = build(&[]).await;
    assert!(matches!(inp(&db, "CALC"), ParsedLink::Calc(_)));
    assert!(matches!(inp(&db, "CONST"), ParsedLink::Constant(_)));
    assert!(matches!(inp(&db, "CAPRE"), ParsedLink::Ca(_)));
    assert!(
        matches!(inp(&db, "BARE"), ParsedLink::Ca(_)),
        "a bare non-local name still takes dbInitLink's CA fallthrough"
    );
}

/// The init-time refusal only reaches links that HAVE a parse cache. A calc
/// record's `INPA` is re-parsed from its raw text every cycle
/// (`initialize_link_locality` says so, and `record_link_fields` does not
/// enumerate it), so the scheme has to survive to the link-set boundary as
/// well: `external_pv_name` returns `pva://…` and
/// `split_external_link_name` reads it back as `LinkTarget::Scheme("pva")`.
/// Addressed by its bare name the link resolved to `LinkTarget::Any` and was
/// measured being read from the CA link set (`read:SRC`, `A` delivered 9.0).
#[epics_macros_rs::epics_test]
async fn a_reparsed_pva_link_is_not_serviced_by_another_schemes_link_set() {
    let ca = SpyLset::new("ca");
    let db = build(&[("ca", ca.clone())]).await;

    let mut visited = std::collections::HashSet::new();
    let _ = db
        .process_record_with_links("REPARSED", &mut visited, 0)
        .await;

    assert_eq!(
        ca.asked(),
        Vec::<String>::new(),
        "the CA link set must never be asked about a re-parsed pva link"
    );
    assert_eq!(db.get_pv("REPARSED").unwrap(), EpicsValue::Double(0.0));
}
