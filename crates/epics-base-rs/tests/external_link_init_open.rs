//! Open-at-init parity with C `dbInitLink` → `dbCaAddLink`.
//!
//! C reference. `dbInitLink` (`dbLink.c:95-143`) runs per record link at
//! iocInit. Any `PV_LINK` that is not resolvable as a local DB link reaches
//! `dbCaAddLinkCallbackOpt`, which stages `addAction(pca, CA_CONNECT)`
//! (`dbCa.c:389-417`) on the `dbCaTask`. That call sits *above* the
//! `dbfType` switch, so `DBF_INLINK`, `DBF_OUTLINK` and `DBF_FWDLINK` are
//! all opened, and it is independent of the `CP`/`CPP` modifier, so plain
//! links are opened exactly like CP ones. A C IOC therefore reaches its
//! first scan with every external channel already connecting.
//!
//! `PvDatabase::setup_external_link_opens` is that pass. It enumerates link
//! fields through `record_link_fields` — the same single owner
//! `setup_cp_links` uses — and stages each external link through
//! `stage_external_link_open_by_name`, i.e. the same `LinkPutQueue` owner
//! that consumes `connect_link`. Nothing here opens a channel inline.

// RTEMS-EXEC-MODEL-ALLOW(15): checked - these run and pass in the feature-ON suite.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use epics_base_rs::server::database::{LinkSet, PvDatabase};
use epics_base_rs::server::record::ParsedLink;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::types::EpicsValue;

/// An lset whose cache is filled ONLY by `connect_link`, counting the names
/// it was asked to connect. Models a monitor-fed resolver: the value appears
/// because the open created a subscription, not because anyone read.
struct CountingLset {
    /// Keyed by link name for the same reason [`LazyLset`] in
    /// `external_link_get_cached.rs` is: `pca->pgetNative` belongs to a
    /// `caLink`. A shared cell would let one link's open warm another
    /// link's read, so a per-link assertion would be reporting the
    /// scheduler.
    cache: parking_lot::Mutex<HashMap<String, EpicsValue>>,
    opened: parking_lot::Mutex<Vec<String>>,
    network_reads: Arc<AtomicUsize>,
}

impl CountingLset {
    fn new(reads: &Arc<AtomicUsize>) -> Arc<Self> {
        Arc::new(Self {
            cache: parking_lot::Mutex::new(HashMap::new()),
            opened: parking_lot::Mutex::new(Vec::new()),
            network_reads: Arc::clone(reads),
        })
    }

    fn opened_names(&self) -> Vec<String> {
        let mut v = self.opened.lock().clone();
        v.sort();
        v
    }
}

#[async_trait::async_trait]
impl LinkSet for CountingLset {
    fn is_connected(&self, name: &str) -> bool {
        self.cache.lock().contains_key(name)
    }

    async fn get_value(&self, name: &str) -> Option<EpicsValue> {
        self.network_reads.fetch_add(1, Ordering::SeqCst);
        self.connect_link(name).await;
        self.cache.lock().get(name).cloned()
    }

    fn get_cached_value(&self, name: &str) -> Option<EpicsValue> {
        self.cache.lock().get(name).cloned()
    }

    async fn connect_link(&self, name: &str) {
        self.opened.lock().push(name.to_string());
        self.cache
            .lock()
            .insert(name.to_string(), EpicsValue::Double(12.5));
    }
}

async fn add_ai(db: &PvDatabase, name: &str, inp: &str) {
    db.add_record(name, Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    let rec = db.get_record(name).expect("just added");
    let mut inst = rec.write();
    inst.put_common_field("INP", EpicsValue::String(inp.into()))
        .unwrap();
    inst.common.udf = 0;
}

async fn add_ao(db: &PvDatabase, name: &str, out: &str) {
    db.add_record(name, Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let rec = db.get_record(name).expect("just added");
    let mut inst = rec.write();
    inst.put_common_field("OUT", EpicsValue::String(out.into()))
        .unwrap();
    inst.common.udf = 0;
}

async fn process(db: &PvDatabase, name: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(name, &mut visited, 0)
        .await
        .unwrap();
}

async fn val_of(db: &PvDatabase, name: &str) -> Option<f64> {
    let rec = db.get_record(name).expect("record exists");
    let inst = rec.read();
    inst.record.val().and_then(|v| v.to_f64())
}

/// Boundary — a plain (non-CP) external INP link. `dbInitLink` does not
/// consult the `CP`/`CPP` modifier before calling `dbCaAddLink`, so this
/// link is opened at init like any other. The port must therefore stage it
/// in the iocInit pass, NOT on the first scan's cache miss: the observable
/// consequence is that scan 1 already reads a warm cache.
#[tokio::test]
async fn non_cp_external_inp_link_is_opened_at_init_not_on_first_scan() {
    let reads = Arc::new(AtomicUsize::new(0));
    let lset = CountingLset::new(&reads);
    let db = PvDatabase::new();
    db.register_link_set("ca", lset.clone()).await;

    // No `CP` / `CPP` modifier — `setup_cp_links` ignores this link entirely.
    add_ai(&db, "AI_PLAIN", "ca://REMOTE:IN").await;

    let staged = db.setup_external_link_opens().await;
    assert_eq!(staged, 1, "the plain external INP link must be staged");
    db.sync_external_link_puts().await;

    assert_eq!(
        lset.opened_names(),
        vec!["REMOTE:IN".to_string()],
        "the work owner opened the link at init, before any record scan"
    );

    // The parity payoff: no cold cycle. Pre-fix, scan 1 missed the cache,
    // staged the open, and left the record at its prior value.
    process(&db, "AI_PLAIN").await;
    assert_eq!(
        val_of(&db, "AI_PLAIN").await,
        Some(12.5),
        "the first scan of a link opened at init reads a warm cache"
    );
    assert_eq!(
        reads.load(Ordering::SeqCst),
        0,
        "the record thread must never reach the network-capable get_value"
    );
}

/// Boundary — an OUT-only external link. `dbCaAddLink` is reached from
/// `dbInitLink` above the `dbfType` switch (`dbLink.c:118-130`), so C opens
/// `DBF_OUTLINK` channels at init too; only the `pvlOptInpNative` /
/// `pvlOptFWD` flags below it are direction-specific.
#[tokio::test]
async fn out_only_external_link_is_opened_at_init() {
    let reads = Arc::new(AtomicUsize::new(0));
    let lset = CountingLset::new(&reads);
    let db = PvDatabase::new();
    db.register_link_set("ca", lset.clone()).await;

    add_ao(&db, "AO_PLAIN", "ca://REMOTE:OUT").await;

    let staged = db.setup_external_link_opens().await;
    assert_eq!(staged, 1, "an OUT-only external link must be staged");
    db.sync_external_link_puts().await;
    assert_eq!(
        lset.opened_names(),
        vec!["REMOTE:OUT".to_string()],
        "C's open-at-init is direction-agnostic"
    );
}

/// Boundary — record-specific input link fields (`INPA..`) go through the
/// same `record_link_fields` enumeration, so an external `INPA` is opened
/// at init exactly like `INP`. This is the property that would break if the
/// pass grew its own field list instead of reusing the CP pass's owner.
#[tokio::test]
async fn record_specific_input_link_fields_are_opened_at_init() {
    let reads = Arc::new(AtomicUsize::new(0));
    let lset = CountingLset::new(&reads);
    let db = PvDatabase::new();
    db.register_link_set("ca", lset.clone()).await;

    db.add_record("CALC1", Box::new(CalcRecord::new("A+B")))
        .await
        .unwrap();
    {
        let rec = db.get_record("CALC1").expect("just added");
        let mut inst = rec.write();
        inst.record
            .put_field("INPA", EpicsValue::String("ca://REMOTE:A".into()))
            .unwrap();
        inst.record
            .put_field("INPB", EpicsValue::String("ca://REMOTE:B".into()))
            .unwrap();
    }

    let staged = db.setup_external_link_opens().await;
    assert_eq!(staged, 2, "both INPA and INPB must be staged");
    db.sync_external_link_puts().await;
    assert_eq!(
        lset.opened_names(),
        vec!["REMOTE:A".to_string(), "REMOTE:B".to_string()],
    );
}

/// Boundary — a local link stages nothing. C's `dbInitLink` resolves it via
/// `dbDbInitLink` and returns before `dbCaAddLink` (`dbLink.c:118-124`);
/// a `CONSTANT` link returns even earlier (`dbLink.c:101-104`). Neither may
/// consume the link queue's one-shot open.
#[tokio::test]
async fn local_and_constant_links_stage_nothing() {
    let reads = Arc::new(AtomicUsize::new(0));
    let lset = CountingLset::new(&reads);
    let db = PvDatabase::new();
    db.register_link_set("ca", lset.clone()).await;

    db.add_record("LOCAL_TGT", Box::new(AiRecord::new(1.0)))
        .await
        .unwrap();
    add_ai(&db, "AI_LOCAL", "LOCAL_TGT").await; // plain DB link
    add_ai(&db, "AI_CONST", "3.5").await; // CONSTANT link

    let staged = db.setup_external_link_opens().await;
    assert_eq!(staged, 0, "no external link exists in this database");
    db.sync_external_link_puts().await;
    assert!(
        lset.opened_names().is_empty(),
        "a local or constant link must not open an external channel, got {:?}",
        lset.opened_names()
    );
    assert_eq!(db.external_link_opens_completed(), 0);
}

/// Boundary — a PLAIN (no `CP`/`CPP`/`CA` modifier) `Db` link whose target is
/// not a record in this IOC. C `dbInitLink` hands it to `dbDbInitLink`
/// (`dbLink.c:117-120`), whose `dbChannelCreate` cannot find the record and
/// returns `S_db_notFound` (`dbDbLink.c:94-96`); execution falls through to
/// `dbCaAddLinkCallbackOpt` (`dbLink.c:129`) and the link IS a CA link from
/// then on. So both halves must hold: the holder's parse cache is rewritten to
/// `Ca`, and the channel is opened at init like any other external link.
#[tokio::test]
async fn plain_non_local_db_link_converts_and_opens_at_init() {
    let reads = Arc::new(AtomicUsize::new(0));
    let lset = CountingLset::new(&reads);
    let db = PvDatabase::new();
    db.register_link_set("ca", lset.clone()).await;

    // No modifier at all — `setup_cp_links` never looked at this link, and
    // pre-fix nothing else converted or opened it either.
    add_ai(&db, "AI_NONLOCAL", "OTHER:PV").await;

    assert_eq!(
        db.initialize_link_locality().await,
        1,
        "the non-local plain Db link must be converted"
    );
    let inp = db
        .get_record("AI_NONLOCAL")
        .unwrap()
        .read()
        .parsed_inp
        .clone();
    match inp {
        ParsedLink::Ca(ca) => assert_eq!(ca.pv, "OTHER:PV"),
        other => panic!("expected the link to become Ca, got {other:?}"),
    }

    let staged = db.setup_external_link_opens().await;
    assert_eq!(staged, 1, "the converted link must be opened at init");
    db.sync_external_link_puts().await;
    assert_eq!(lset.opened_names(), vec!["OTHER:PV".to_string()]);

    // The parity payoff, same as the explicit `ca://` case: no cold cycle.
    process(&db, "AI_NONLOCAL").await;
    assert_eq!(val_of(&db, "AI_NONLOCAL").await, Some(12.5));
    assert_eq!(
        reads.load(Ordering::SeqCst),
        0,
        "the record thread must never reach the network-capable get_value"
    );
}

/// Boundary — a `.FIELD` target. The CA channel name C falls through with is
/// `plink->value.pv_link.pvname` verbatim (`dbLink.c:129`), i.e. the same
/// `record.FIELD` string `dbChannelCreate` just failed on — not the bare
/// record name.
#[tokio::test]
async fn plain_non_local_db_link_keeps_its_field_in_the_channel_name() {
    let reads = Arc::new(AtomicUsize::new(0));
    let lset = CountingLset::new(&reads);
    let db = PvDatabase::new();
    db.register_link_set("ca", lset.clone()).await;

    add_ai(&db, "AI_NONLOCAL_FLD", "OTHER:PV.SEVR").await;

    assert_eq!(db.initialize_link_locality().await, 1);
    db.setup_external_link_opens().await;
    db.sync_external_link_puts().await;
    assert_eq!(lset.opened_names(), vec!["OTHER:PV.SEVR".to_string()]);
}

/// Boundary — a `Db` link whose target IS local is untouched. C returns from
/// `dbInitLink` the moment `dbDbInitLink` succeeds (`dbLink.c:118-120`), never
/// reaching `dbCaAddLink`: the link keeps `dbDb_lset`, its lock-set merge, its
/// `PP` target processing and its `MS` severity inheritance.
#[tokio::test]
async fn local_db_link_is_not_converted() {
    let reads = Arc::new(AtomicUsize::new(0));
    let lset = CountingLset::new(&reads);
    let db = PvDatabase::new();
    db.register_link_set("ca", lset.clone()).await;

    db.add_record("LOCAL_SRC", Box::new(AiRecord::new(7.0)))
        .await
        .unwrap();
    add_ai(&db, "AI_LOCAL_DB", "LOCAL_SRC").await;

    assert_eq!(
        db.initialize_link_locality().await,
        0,
        "a local Db link must not be converted"
    );
    let inp = db
        .get_record("AI_LOCAL_DB")
        .unwrap()
        .read()
        .parsed_inp
        .clone();
    assert!(
        matches!(inp, ParsedLink::Db(_)),
        "a local Db link must stay a Db link, got {inp:?}"
    );
    assert_eq!(db.setup_external_link_opens().await, 0);
    db.sync_external_link_puts().await;
    assert!(lset.opened_names().is_empty());
}

/// Boundary — a CP link to a non-local target is unchanged by the new pass.
/// It was already converted (by `setup_cp_links`) and already registered as an
/// external CP trigger; moving the rewrite to the `dbInitLink` pass must keep
/// BOTH halves, because C reaches the identical `dbCaAddLink` call for a
/// CP link — it just skips `dbDbInitLink` on the way (`dbLink.c:117`).
#[tokio::test]
async fn non_local_cp_link_keeps_its_conversion_and_its_external_trigger() {
    let reads = Arc::new(AtomicUsize::new(0));
    let lset = CountingLset::new(&reads);
    let db = PvDatabase::new();
    db.register_link_set("ca", lset.clone()).await;

    add_ai(&db, "AI_CP_NONLOCAL", "OTHER:CP CP").await;

    db.initialize_link_locality().await;
    db.setup_cp_links().await;

    let inp = db
        .get_record("AI_CP_NONLOCAL")
        .unwrap()
        .read()
        .parsed_inp
        .clone();
    match inp {
        ParsedLink::Ca(ca) => assert_eq!(ca.pv, "OTHER:CP"),
        other => panic!("a non-local CP link must be Ca, got {other:?}"),
    }
    assert!(
        db.external_cp_pv_names()
            .await
            .contains(&"OTHER:CP".to_string()),
        "the external CP trigger must still be registered"
    );
}

/// Boundary — the decision is made ONCE. C sets `DBLINK_FLAG_INITIALIZED` and
/// returns early on any later `dbInitLink` (`dbLink.c:95-101`), so a record
/// that appears in the IOC *after* iocInit does NOT pull an already-converted
/// link back to a local DB link. Our commit into the parse cache has the same
/// shape: nothing re-derives it from locality afterwards.
#[tokio::test]
async fn a_record_added_after_init_does_not_un_convert_the_link() {
    let reads = Arc::new(AtomicUsize::new(0));
    let lset = CountingLset::new(&reads);
    let db = PvDatabase::new();
    db.register_link_set("ca", lset.clone()).await;

    add_ai(&db, "AI_EARLY", "LATE_SRC").await;
    assert_eq!(db.initialize_link_locality().await, 1);

    // The target shows up later — iocInit is over.
    db.add_record("LATE_SRC", Box::new(AiRecord::new(3.0)))
        .await
        .unwrap();

    let inp = db.get_record("AI_EARLY").unwrap().read().parsed_inp.clone();
    match inp {
        ParsedLink::Ca(ca) => assert_eq!(
            ca.pv, "LATE_SRC",
            "the link stays external; C never un-converts"
        ),
        other => panic!("a converted link must stay Ca, got {other:?}"),
    }
}

/// Boundary — a `Constant` link is not a `PV_LINK` and never reaches the
/// locality test: `dbInitLink` returns at `dbConstInitLink`
/// (`dbLink.c:101-104`). A numeric `INP` must not be turned into a CA channel
/// named `3.5`.
#[tokio::test]
async fn constant_link_is_not_converted() {
    let reads = Arc::new(AtomicUsize::new(0));
    let lset = CountingLset::new(&reads);
    let db = PvDatabase::new();
    db.register_link_set("ca", lset.clone()).await;

    add_ai(&db, "AI_CONST2", "3.5").await;

    assert_eq!(db.initialize_link_locality().await, 0);
    assert_eq!(db.setup_external_link_opens().await, 0);
    db.sync_external_link_puts().await;
    assert!(lset.opened_names().is_empty());
}

/// Boundary — an external `FLNK`. `dbInitLink` reaches
/// `dbCaAddLinkCallbackOpt` for `DBF_FWDLINK` exactly as for the other two
/// directions (`dbLink.c:118-136`; the `dbfType == DBF_FWDLINK` block *below*
/// the call only sets `pvlOptFWD` / warns about a non-`.PROC` target). The
/// port already forwards a live external `FLNK`
/// (`processing.rs::dispatch_external_forward_link`), so the channel must be
/// connecting before the first forward trigger rather than being staged by
/// that trigger's own refusal.
#[tokio::test]
async fn external_forward_link_is_opened_at_init() {
    let reads = Arc::new(AtomicUsize::new(0));
    let lset = CountingLset::new(&reads);
    let db = PvDatabase::new();
    db.register_link_set("ca", lset.clone()).await;

    db.add_record("AI_FWD", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    {
        let rec = db.get_record("AI_FWD").expect("just added");
        let mut inst = rec.write();
        inst.put_common_field("FLNK", EpicsValue::String("ca://REMOTE:FWD.PROC".into()))
            .unwrap();
    }

    let staged = db.setup_external_link_opens().await;
    assert_eq!(staged, 1, "an external FLNK must be staged at init");
    db.sync_external_link_puts().await;
    assert_eq!(
        lset.opened_names(),
        vec!["REMOTE:FWD.PROC".to_string()],
        "C's open-at-init covers DBF_FWDLINK"
    );
}

/// Boundary — a local `FLNK` stages nothing. C resolves it through
/// `dbDbInitLink` and returns before `dbCaAddLink` (`dbLink.c:117-120`); the
/// forward trigger is the local `dbScanPassive` path, not a link set. Widening
/// `record_link_fields` to carry `FLNK` must not turn every fanout chain in a
/// database into an external open attempt.
#[tokio::test]
async fn local_forward_link_stages_nothing() {
    let reads = Arc::new(AtomicUsize::new(0));
    let lset = CountingLset::new(&reads);
    let db = PvDatabase::new();
    db.register_link_set("ca", lset.clone()).await;

    db.add_record("FWD_TGT", Box::new(AiRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("AI_FWD_LOCAL", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    {
        let rec = db.get_record("AI_FWD_LOCAL").expect("just added");
        let mut inst = rec.write();
        inst.put_common_field("FLNK", EpicsValue::String("FWD_TGT".into()))
            .unwrap();
    }

    let staged = db.setup_external_link_opens().await;
    assert_eq!(staged, 0, "a local FLNK resolves as a DB link");
    db.sync_external_link_puts().await;
    assert!(
        lset.opened_names().is_empty(),
        "a local FLNK must not open an external channel, got {:?}",
        lset.opened_names()
    );
}

/// Boundary — an `FLNK` carries no CP/CPP policy, so widening the enumeration
/// must leave `setup_cp_links` untouched. C masks a `DBF_FWDLINK`'s modifier
/// set to `pvlOptCA` alone (`dbStaticLib.c:2390`), which is what
/// `LinkFieldType::Fwd` reproduces: `FLNK="OTHER CP"` parses as a plain NPP
/// forward link, never a CP holder edge.
#[tokio::test]
async fn forward_link_never_registers_a_cp_holder() {
    let reads = Arc::new(AtomicUsize::new(0));
    let lset = CountingLset::new(&reads);
    let db = PvDatabase::new();
    db.register_link_set("ca", lset.clone()).await;

    db.add_record("CP_SRC", Box::new(AiRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("AI_FWD_CP", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    {
        let rec = db.get_record("AI_FWD_CP").expect("just added");
        let mut inst = rec.write();
        inst.put_common_field("FLNK", EpicsValue::String("CP_SRC CP".into()))
            .unwrap();
    }

    db.setup_cp_links().await;
    assert!(
        db.get_cp_targets("CP_SRC").is_empty(),
        "a forward link's CP modifier is masked off, so no holder edge exists"
    );
}

/// Boundary — the CP path is not double-staged. `setup_cp_links` already
/// warms external CP/CPP links, and this pass re-visits the same fields.
/// Both derive the queue key from the same boundary name, and `stage_open`
/// is once-per-key and terminal (`link_put_queue.rs:346-369`), so the link
/// is connected exactly once: C stages one `CA_CONNECT` per link and libca
/// owns every retry from then on.
#[tokio::test]
async fn external_cp_link_already_warmed_is_not_staged_again() {
    let reads = Arc::new(AtomicUsize::new(0));
    let lset = CountingLset::new(&reads);
    let db = PvDatabase::new();
    db.register_link_set("ca", lset.clone()).await;

    add_ai(&db, "AI_CP", "ca://REMOTE:CP CP").await;

    db.setup_cp_links().await; // warms it through resolve_external_pv
    let staged = db.setup_external_link_opens().await;
    assert_eq!(
        staged, 0,
        "the CP pass already staged this link's open; the init pass must not \
         stage a second one"
    );
    db.sync_external_link_puts().await;

    assert_eq!(
        lset.opened_names(),
        vec!["REMOTE:CP".to_string()],
        "exactly one connect, from exactly one staged open"
    );
    assert_eq!(
        db.external_link_opens_completed(),
        1,
        "the work owner completed one open, not two"
    );
}

/// Boundary — no link set registered. The queue's open state is terminal,
/// so staging against an empty lset list would mark the link `Done` and burn
/// its one connect: a link set installed later (the `AfterCaLinkInit`
/// installers run before this pass, but a test or a late installer may not
/// have) would then never be asked to open it. The gate keeps the link
/// stageable.
#[tokio::test]
async fn no_link_set_registered_stages_nothing_and_leaves_the_link_openable() {
    let db = PvDatabase::new();
    add_ai(&db, "AI_NOLSET", "ca://REMOTE:IN").await;

    assert_eq!(
        db.setup_external_link_opens().await,
        0,
        "no lset addresses this link yet, so nothing may be staged"
    );

    // Register the lset afterwards: the link must still be stageable.
    let reads = Arc::new(AtomicUsize::new(0));
    let lset = CountingLset::new(&reads);
    db.register_link_set("ca", lset.clone()).await;
    assert_eq!(db.setup_external_link_opens().await, 1);
    db.sync_external_link_puts().await;
    assert_eq!(lset.opened_names(), vec!["REMOTE:IN".to_string()]);
}
