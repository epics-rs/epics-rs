//! C `dbCaPutLinkCallback` refuses an OUT-link put on EITHER of two
//! conditions, before anything is staged:
//!
//! ```c
//! if (!pca->isConnected || !pca->hasWriteAccess) {
//!     epicsMutexUnlock(pca->lock);
//!     return -1;
//! }
//! ```
//! (`dbCa.c:529-532`). `dbPutLink` folds that `-1` into the owning record's
//! LINK/INVALID (`dbLink.c:443-446` → `setLinkAlarm` → `recGblSetSevrMsg`),
//! so a `record(ao,"HOLD") { field(OUT,"ca://SEC:HOLD") }` pointed at a
//! server that grants READ and denies WRITE alarms on every cycle.
//!
//! The port tested only the first operand. A write-denied link is CONNECTED
//! — it keeps serving values to `dbCaGetLink` — so it passed the gate, the
//! write was staged, and the refusal happened later on the link work owner,
//! after the record cycle that issued it had already finished. The record
//! stayed NO_ALARM.
//!
//! The boundaries here are the two operands crossed with the two put
//! flavours, because C's gate is one predicate serving both and the
//! flavours fail differently when it is half-implemented: the plain put
//! loses the alarm, and the completion put ALSO resolves its wait-set as a
//! success, because the completion channel carries no status back to the
//! record (that is C's behaviour too for a POST-staging failure —
//! `dbCa.c:1175-1179` reports those through `errlogPrintf` only — which is
//! precisely why the gate has to be whole before staging).
//!
//! The rights are real, not injected: the upstream `CaServer` runs an
//! `.acf` whose WRITE rule names a HAG this client is not in, so the server
//! sends `CA_PROTO_ACCESS_RIGHTS` with the write bit clear and the
//! resolver's connection watcher caches it through `note_access_rights` —
//! C's `accessRightsCallback` (`dbCa.c:1014-1040`).

#![cfg(tokio_backend)]
#![cfg(feature = "client-core")]

use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use epics_base_rs::server::database::{LinkRef, LinkSet, PutAdmission, PvDatabase};
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::{AlarmSeverity, LinkFieldType, LinkProcessPolicy, OpenLatch};
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::calink::CaLinkResolver;
use epics_ca_rs::client::{CaClient, CaClientConfig};
use epics_ca_rs::server::CaServer;
use serial_test::serial;

/// The remote PV every test here links to.
const PV: &str = "SEC:HOLD";

/// The link to [`PV`] a test opens on its resolver directly: an INLINK, so
/// its value is monitored from the connect.
fn link() -> LinkRef<'static> {
    static OPEN: LazyLock<OpenLatch> =
        LazyLock::new(|| OpenLatch::ca(LinkProcessPolicy::NoProcess, LinkFieldType::In));
    LinkRef::new(PV.as_bytes(), &OPEN)
}

/// The value the server starts with — the OUT writes below all push
/// something else, so "the write never landed" is observable.
const REST: f64 = 1.0;

/// WRITE is granted only to the `ops` HAG; READ is granted to everyone, so
/// the link still connects and still serves values. That combination is the
/// whole point: `isConnected` is TRUE for a link C refuses to write.
const ACF: &str = r#"
HAG(ops) { opi-01.lab }
ASG(DEFAULT) {
    RULE(1, READ)
    RULE(1, WRITE) { HAG(ops) }
}
"#;

/// The name the `ops` HAG lists — a client claiming it holds both rights.
const GRANTED_HOST: &str = "opi-01.lab";
/// Any other name — READ granted, WRITE denied.
const DENIED_HOST: &str = "intruder.example";

/// Serve `PV` under [`ACF`] and return the port the server BOUND.
///
/// The port is taken by binding it (`.port(0)` → read back `udp_port()`),
/// never by probing one and handing the number on.
async fn serve_acf_pv() -> u16 {
    let dir = tempfile::tempdir().expect("temp");
    let acf_path = dir.path().join("write.acf");
    std::fs::write(&acf_path, ACF).expect("write acf");
    let server = CaServer::builder()
        .port(0)
        .pv(PV, EpicsValue::Double(REST))
        .acf_file(acf_path.to_str().unwrap())
        .expect("load acf")
        .build()
        .await
        .expect("CA server");
    let port = server.udp_port();
    tokio::spawn(async move { server.run().await });
    port
}

/// Point the ambient `EPICS_CA_*` env at `127.0.0.1:port`. Every test here
/// is `#[serial(epics_env)]` so the process-wide env is not raced.
fn pin_env(port: u16) {
    // SAFETY: tests sharing process-wide env are serialized via
    // `#[serial(epics_env)]`; no other thread reads/writes these vars
    // concurrently.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
}

/// A resolver whose client claims `host` in `CA_PROTO_HOST_NAME`, so the
/// upstream ACF decides its rights. The claim is made before any channel
/// exists, which is where libca makes it.
async fn resolver_claiming(port: u16, host: &str) -> CaLinkResolver {
    pin_env(port);
    let client = CaClient::new_with_config(CaClientConfig::default())
        .await
        .expect("CA client");
    client.set_host_name(host);
    let resolver = CaLinkResolver::with_client(Arc::new(client));
    LinkSet::connect_link(&resolver, link(), None).await;
    assert!(
        resolver
            .wait_for_link_connected(PV, fact_budget::FACT_BUDGET)
            .await,
        "READ is granted to everyone, so the link must connect whatever \
         the write right is"
    );
    resolver
}

/// The rights frame follows the create-channel reply, so poll rather than
/// racing it. Returns the admission actually observed at the deadline.
async fn admission_settling_to(
    resolver: &CaLinkResolver,
    link: LinkRef<'_>,
    want: PutAdmission,
) -> PutAdmission {
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    loop {
        let got = LinkSet::put_admission(resolver, link);
        if got == want || Instant::now() >= deadline {
            return got;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Soft-Channel `ao` with `OUT` = the CA link and `VAL` = `val`. `notify`
/// arms the put-notify wait-set, which is what selects the completion
/// flavour (C `dbPutLinkAsync`) over the plain one.
async fn add_ao(db: &PvDatabase, name: &str, val: f64, notify: bool) {
    db.add_record(name, Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.put_pv_no_process(
        format!("{name}.OUT"),
        EpicsValue::String(format!("ca://{PV}").into()),
    )
    .await
    .unwrap();
    let rec = db.get_record(name).expect("just added");
    let mut inst = rec.write();
    inst.common.udf = 0;
    inst.record
        .put_field("VAL", EpicsValue::Double(val))
        .unwrap();
    if notify {
        // Leaked on purpose: the test inspects what the OUT write did to the
        // record, and a live receiver keeps the completion `send` from
        // observing a closed channel.
        let (tx, rx) = epics_base_rs::runtime::sync::oneshot::channel();
        std::mem::forget(rx);
        inst.install_or_queue_notify(tx)
            .expect("the record is free, so the wait-set installs");
    }
}

async fn process(db: &PvDatabase, name: &str) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(name, &mut visited)
        .await
        .or_else(epics_base_rs::error::CaError::unless_process_status)
        .unwrap();
}

async fn alarm_of(db: &PvDatabase, name: &str) -> (u16, AlarmSeverity) {
    let rec = db.get_record(name).expect("record exists");
    let inst = rec.read();
    (inst.common.stat, inst.common.sevr)
}

/// A database with the write-denied resolver mounted as the `ca` link set.
async fn denied_db() -> (PvDatabase, CaLinkResolver) {
    let port = serve_acf_pv().await;
    let resolver = resolver_claiming(port, DENIED_HOST).await;
    let db = PvDatabase::new();
    db.register_link_set("ca", Arc::new(resolver.clone())).await;
    (db, resolver)
}

/// iocInit's `dbCaAddLink` pass opens `name`'s OUT link on its own channel,
/// held until it is connected and past the first refusal, so the rights
/// are known before the record processes.
async fn open_out(db: &PvDatabase, resolver: &CaLinkResolver, name: &str) {
    assert!(db.ca_link_init());
    db.setup_external_link_opens().await;
    db.process_cp_links_on_connect().await;
    assert!(
        resolver
            .wait_for_link_connected(PV, fact_budget::FACT_BUDGET)
            .await,
        "the record's link connects"
    );
    let out = db
        .record_link_fields(name)
        .into_iter()
        .find_map(|(field, _, link)| (field == "OUT").then_some(link))
        .expect("an OUT link");
    // Best-effort settle, deliberately NOT asserted: the rights frame
    // follows the create-channel reply within a millisecond or two, and the
    // claim these two tests make is about the RECORD. Asserting the gate
    // here would abort them before the record ever processed, leaving the
    // record-level half of the defect unpinned — which is how it survived.
    // `a_connected_but_write_denied_link_is_refused` asserts the gate.
    let out = LinkRef::new(PV.as_bytes(), out.external().unwrap().open);
    let _ = admission_settling_to(resolver, out, PutAdmission::Refused).await;
}

/// Operand two, alone: connected, write denied. C's gate is a disjunction,
/// so this refuses — and `is_connected` must still answer TRUE, because a
/// write-denied link keeps serving `dbCaGetLink` and the two conditions are
/// separate in C.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_connected_but_write_denied_link_is_refused() {
    let port = serve_acf_pv().await;
    let resolver = resolver_claiming(port, DENIED_HOST).await;

    assert_eq!(
        admission_settling_to(&resolver, link(), PutAdmission::Refused).await,
        PutAdmission::Refused,
        "C refuses on `!pca->hasWriteAccess` alone (dbCa.c:529-532)"
    );
    assert!(
        LinkSet::is_connected(&resolver, link()),
        "and it is refused while CONNECTED — the read half is untouched, \
         which is why testing `is_connected` alone admitted it"
    );
    assert_eq!(
        LinkSet::get_value(&resolver, link())
            .await
            .and_then(|v| v.to_f64()),
        Some(REST),
        "a write-denied link still serves its value to dbCaGetLink"
    );
}

/// The resolver's rights come from the events its watcher receives, so the
/// receiver must exist before the channel can connect. Both arms read only
/// after the connect is OVER: `wait_connected` wakes on the state change,
/// and a second `WaitConnected` is answered by the coordinator after the
/// handler that sent `AccessRightsChanged` + `Connected` has returned. The
/// receiver made with the channel holds both; one taken then — what a
/// preempted `open` used to do — holds neither, and its write right stayed
/// at the default `true`.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_receiver_taken_with_the_channel_keeps_a_connect_it_lost_the_race_to() {
    use epics_base_rs::runtime::sync::broadcast::error::TryRecvError;
    use epics_ca_rs::client::ConnectionEvent;
    let port = serve_acf_pv().await;
    pin_env(port);
    let client = CaClient::new_with_config(CaClientConfig::default())
        .await
        .expect("CA client");
    client.set_host_name(DENIED_HOST);
    let (channel, mut with_channel) = client.create_channel_with_events(PV, 0);
    for _ in 0..2 {
        channel
            .wait_connected(fact_budget::FACT_BUDGET)
            .await
            .expect("READ is granted to everyone");
    }
    let mut late = channel.connection_events();

    let mut next = async || {
        tokio::time::timeout(fact_budget::FACT_BUDGET, with_channel.recv())
            .await
            .expect("the receiver made with the channel holds the connect")
            .ok()
    };
    assert_eq!(
        next().await,
        Some(ConnectionEvent::AccessRightsChanged {
            read: true,
            write: false
        }),
        "the ACF's WRITE rule names a HAG this client is not in"
    );
    assert_eq!(next().await, Some(ConnectionEvent::Connected));
    assert_eq!(
        late.try_recv(),
        Err(TryRecvError::Empty),
        "a receiver taken after the connect holds neither event"
    );
}

/// Negative control for the same predicate: full rights are admitted, and
/// the write actually lands. Without this, "refuses" above would pass on a
/// resolver that refuses everything.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_fully_granted_link_is_still_admitted_and_written() {
    let port = serve_acf_pv().await;
    let resolver = resolver_claiming(port, GRANTED_HOST).await;

    // The rights frame arrives after connect; a link that is going to be
    // refused is refused by then, so settling on `Connected` is only
    // meaningful together with the write below actually landing.
    assert_eq!(
        admission_settling_to(&resolver, link(), PutAdmission::Connected).await,
        PutAdmission::Connected,
        "HAG(ops) grants WRITE to a client claiming that name"
    );

    use epics_base_rs::server::database::LinkPutOp;
    LinkSet::put_value(
        &resolver,
        link(),
        EpicsValue::Double(42.0),
        LinkPutOp::Plain,
    )
    .await
    .expect("an admitted write must reach the wire");

    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    loop {
        if LinkSet::get_value(&resolver, link())
            .await
            .and_then(|v| v.to_f64())
            == Some(42.0)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the admitted write never landed on the upstream PV"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Flavour one — plain put. C returns `-1` from `dbCaPutLink` and
/// `dbPutLink` raises LINK/INVALID on the writing record IN THIS CYCLE
/// (`dbLink.c:443-446`), staging nothing.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_plain_out_write_to_a_write_denied_link_alarms_the_record() {
    let (db, resolver) = denied_db().await;
    add_ao(&db, "HOLD", 5.0, false).await;
    open_out(&db, &resolver, "HOLD").await;

    process(&db, "HOLD").await;
    db.sync_external_link_puts().await;

    assert_eq!(
        alarm_of(&db, "HOLD").await,
        (alarm_status::LINK_ALARM, AlarmSeverity::Invalid),
        "C shows LINK INVALID every cycle on a write-denied CA OUT link"
    );
    assert_eq!(
        db.external_link_puts_completed(),
        0,
        "C refuses BEFORE addAction, so nothing is staged at all"
    );
    assert_eq!(
        LinkSet::get_value(&resolver, link())
            .await
            .and_then(|v| v.to_f64()),
        Some(REST),
        "and the upstream PV keeps its value"
    );
}

/// Flavour two — completion put (C `dbCaPutLinkCallback` with a callback,
/// the put-notify / blocking-put chain). The SAME predicate governs it, and
/// it is the flavour that fails silently when the predicate is half
/// implemented: the completion channel carries no status back to the
/// record, so an admitted-then-refused write resolves the wait-set as a
/// success and the chain reports done with the record still NO_ALARM.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_completion_out_write_to_a_write_denied_link_alarms_the_record() {
    let (db, resolver) = denied_db().await;
    add_ao(&db, "HOLD:CB", 5.0, true).await;
    open_out(&db, &resolver, "HOLD:CB").await;

    process(&db, "HOLD:CB").await;
    db.sync_external_link_puts().await;

    assert_eq!(
        alarm_of(&db, "HOLD:CB").await,
        (alarm_status::LINK_ALARM, AlarmSeverity::Invalid),
        "the completion flavour refuses through the same gate — a \
         put-notify chain must not report success on a denied write"
    );
    assert_eq!(
        db.external_link_puts_completed(),
        0,
        "nothing staged, so no completion was ever owed"
    );
    assert_eq!(
        LinkSet::get_value(&resolver, link())
            .await
            .and_then(|v| v.to_f64()),
        Some(REST),
        "and the upstream PV keeps its value"
    );
}
