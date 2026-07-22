//! External-link GET parity with C `dbCaGetLink`: a record's `ca://`/`pva://`
//! INP read serves the lset's monitor-fed cache and never opens a channel or
//! waits on the wire from the record-processing thread.
//!
//! C reference. `dbCaGetLink` (`dbCa.c:448-535`) does two things: it refuses
//! with -1 while `!pca->isConnected || !pca->hasReadAccess` (`:459-464`), and
//! otherwise copies out of `pca->pgetNative` — the buffer the CA monitor
//! callback `eventCallback` (`dbCa.c:925`) keeps fresh on the `dbCaTask`. It
//! never calls `ca_create_channel` and never calls `ca_get`. The channel was
//! opened earlier, by `dbCaAddLink` staging a `CA_CONNECT` action
//! (`dbCa.c:735-800`) that the same task services.
//!
//! The port opens lazily rather than at record init, so the open is staged on
//! the link work queue the first time a read misses. The parity property —
//! *no network suspension on the record thread* — is the same; the timing
//! differs by one scan cycle, which C also spends in LINK/INVALID whenever the
//! link has not connected yet.

// RTEMS-EXEC-MODEL-ALLOW(4): checked - these run and pass in the feature-ON suite.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use epics_base_rs::server::database::{LinkSet, PvDatabase};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::EpicsValue;

/// An lset whose cache is filled ONLY by `connect_link` — the shape of a real
/// monitor-fed resolver, where the value appears because a subscription
/// delivered it, not because someone read.
///
/// `get_value` (the network-capable read) counts its calls: the record path
/// must never reach it.
struct LazyLset {
    /// Keyed BY LINK NAME, because `pca->pgetNative` belongs to a `caLink`
    /// and not to the lset. One shared cell would mean "the value of
    /// whichever link was opened last", so opening link A would warm link
    /// B's read and B would never miss — making a per-link assertion depend
    /// on when the work owner's task happened to run.
    cache: parking_lot::Mutex<HashMap<String, EpicsValue>>,
    /// What `connect_link` publishes into the cache. `None` models a remote that
    /// stays down.
    on_open: Option<EpicsValue>,
    opens: Arc<AtomicUsize>,
    network_reads: Arc<AtomicUsize>,
}

impl LazyLset {
    fn new(
        on_open: Option<EpicsValue>,
        opens: &Arc<AtomicUsize>,
        reads: &Arc<AtomicUsize>,
    ) -> Self {
        Self {
            cache: parking_lot::Mutex::new(HashMap::new()),
            on_open,
            opens: Arc::clone(opens),
            network_reads: Arc::clone(reads),
        }
    }
}

#[async_trait::async_trait]
impl LinkSet for LazyLset {
    fn is_connected(&self, name: &str) -> bool {
        self.cache.lock().contains_key(name)
    }

    /// The network read. Opens on demand — exactly what the record path must
    /// no longer call.
    async fn get_value(&self, name: &str) -> Option<EpicsValue> {
        self.network_reads.fetch_add(1, Ordering::SeqCst);
        self.connect_link(name).await;
        self.cache.lock().get(name).cloned()
    }

    fn get_cached_value(&self, name: &str) -> Option<EpicsValue> {
        self.cache.lock().get(name).cloned()
    }

    async fn connect_link(&self, name: &str) {
        self.opens.fetch_add(1, Ordering::SeqCst);
        if let Some(v) = self.on_open.clone() {
            self.cache.lock().insert(name.to_string(), v);
        }
    }
}

async fn add_ai_with_inp(db: &PvDatabase, name: &str, inp: &str) {
    db.add_record(name, Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    let rec = db.get_record(name).expect("just added");
    let mut inst = rec.write();
    inst.put_common_field("INP", EpicsValue::String(inp.into()))
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

/// Boundary — cache miss. The read must NOT open the channel on the record
/// thread: it stages the open on the link work owner and returns nothing this
/// cycle, which is C returning -1 for a link that is not connected
/// (`dbCa.c:459-464`).
#[tokio::test]
async fn a_cache_miss_stages_the_open_instead_of_opening_inline() {
    let opens = Arc::new(AtomicUsize::new(0));
    let reads = Arc::new(AtomicUsize::new(0));
    let db = PvDatabase::new();
    db.register_link_set(
        "ca",
        Arc::new(LazyLset::new(
            Some(EpicsValue::Double(12.5)),
            &opens,
            &reads,
        )),
    )
    .await;

    add_ai_with_inp(&db, "AI_LAZY", "ca://REMOTE:IN").await;
    process(&db, "AI_LAZY").await;

    assert_eq!(
        reads.load(Ordering::SeqCst),
        0,
        "the record-processing read must never reach the network-capable \
         get_value — C dbCaGetLink only copies out of pgetNative"
    );
    assert_eq!(
        val_of(&db, "AI_LAZY").await,
        Some(0.0),
        "cycle 1 reads a cold cache, so the record keeps its prior value"
    );

    db.sync_external_link_puts().await;
    assert_eq!(
        db.external_link_opens_completed(),
        1,
        "the miss staged exactly one open, and the work owner performed it"
    );
}

/// Boundary — cache hit. Once the work owner has opened the link and the
/// monitor has published a value, the read serves it with no further opens
/// and still no network read.
#[tokio::test]
async fn a_warm_cache_is_read_without_opening_or_network() {
    let opens = Arc::new(AtomicUsize::new(0));
    let reads = Arc::new(AtomicUsize::new(0));
    let db = PvDatabase::new();
    db.register_link_set(
        "ca",
        Arc::new(LazyLset::new(
            Some(EpicsValue::Double(12.5)),
            &opens,
            &reads,
        )),
    )
    .await;

    add_ai_with_inp(&db, "AI_WARM", "ca://REMOTE:IN").await;
    process(&db, "AI_WARM").await; // stages the open
    db.sync_external_link_puts().await; // the owner performs it

    process(&db, "AI_WARM").await;
    assert_eq!(
        val_of(&db, "AI_WARM").await,
        Some(12.5),
        "cycle 2 serves the now-warm cache"
    );
    assert_eq!(
        reads.load(Ordering::SeqCst),
        0,
        "still no network read on the record thread"
    );
    assert_eq!(
        opens.load(Ordering::SeqCst),
        1,
        "a warm link is not re-opened"
    );
}

/// Boundary — a remote that never comes up. C stages ONE `CA_CONNECT` per
/// link (`dbCaAddLink`, `dbCa.c:735-800`) and libca owns every retry from
/// then on. So a record scanning against a dead remote must produce one
/// connect attempt, not one per scan.
#[tokio::test]
async fn a_permanently_cold_link_is_opened_once_not_once_per_scan() {
    let opens = Arc::new(AtomicUsize::new(0));
    let reads = Arc::new(AtomicUsize::new(0));
    let db = PvDatabase::new();
    // `on_open: None` — the open runs but the cache stays empty.
    db.register_link_set("ca", Arc::new(LazyLset::new(None, &opens, &reads)))
        .await;

    add_ai_with_inp(&db, "AI_COLD", "ca://DEAD:IN").await;
    for _ in 0..8 {
        process(&db, "AI_COLD").await;
        db.sync_external_link_puts().await;
    }

    assert_eq!(
        opens.load(Ordering::SeqCst),
        1,
        "eight scans against a dead remote must stage one open, not eight"
    );
    assert_eq!(reads.load(Ordering::SeqCst), 0);
}

/// Boundary — distinct links each get their own open. The once-only rule is
/// per link, not global; a second link must not be starved by the first.
#[tokio::test]
async fn distinct_links_are_each_opened_once() {
    let opens = Arc::new(AtomicUsize::new(0));
    let reads = Arc::new(AtomicUsize::new(0));
    let db = PvDatabase::new();
    db.register_link_set(
        "ca",
        Arc::new(LazyLset::new(Some(EpicsValue::Double(1.0)), &opens, &reads)),
    )
    .await;

    add_ai_with_inp(&db, "AI_A", "ca://A:IN").await;
    add_ai_with_inp(&db, "AI_B", "ca://B:IN").await;
    process(&db, "AI_A").await;
    process(&db, "AI_B").await;
    db.sync_external_link_puts().await;

    assert_eq!(
        db.external_link_opens_completed(),
        2,
        "each link is opened on its own account"
    );
}
