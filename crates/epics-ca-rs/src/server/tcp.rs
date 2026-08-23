// RTEMS-EXEC-MODEL-ALLOW(35): the flavored tests drive the async CA TCP server
// (tokio::net, tokio::spawn AbortHandle machinery), which needs the reactor. These run and pass in the
// feature-ON suite on the tokio driver.
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;
// These `tokio::io` traits/wrappers are used only by the async accept/read
// front-end (`handle_client`, `drain_and_flush`); the shared read helper takes
// a `tokio::io::AsyncReadExt` bound by full path. Host-only on
// `epics_embedded_target` (RTEMS or VxWorks).
#[cfg(not(epics_embedded_target))]
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufWriter};
// `tokio::net::TcpListener` (and the `socket2` keepalive setup) back only the
// async accept path; the embedded build serves TCP through `server::blocking`
// (`std::net`), so the `tokio::net` accept front-end is gated out there.
#[cfg(not(epics_embedded_target))]
use tokio::net::TcpListener;
use tokio::sync::broadcast;

/// Optional application-level idle timeout before forcibly closing a TCP
/// client. Disabled by default — OS-level TCP keepalive (set in `accept_loop`,
/// 15s idle + 5s probes) is the primary half-open detector and matches C
/// epics-base rsrv (`caservertask.c:1456` sets only `SO_KEEPALIVE`, with no
/// application-level idle timeout).
///
/// A C client receiving a continuous monitor stream may never send
/// `CA_PROTO_ECHO` (libca resets its echo timer on every received frame from
/// the server), so an inactivity timeout based purely on incoming reads
/// produces false-positive disconnects on healthy connections — the bug
/// archaeology REVIEW for this is in `archaeology/REVIEWS/`. Operators who
/// want a defensive cap (e.g., NAT environments where TCP keepalive is
/// unreliable) can set `EPICS_CAS_INACTIVITY_TMO` to a positive value;
/// values < 30 are clamped to 30 to avoid pathological short timeouts.
pub(crate) fn inactivity_timeout() -> Option<Duration> {
    static RESOLVED: std::sync::OnceLock<Option<Duration>> = std::sync::OnceLock::new();
    *RESOLVED.get_or_init(resolve_inactivity_timeout)
}

/// The uncached resolution behind [`inactivity_timeout`].
fn resolve_inactivity_timeout() -> Option<Duration> {
    crate::estdlib::env_double("EPICS_CAS_INACTIVITY_TMO")
        .ok()
        .filter(|v| *v > 0.0)
        .map(|v| crate::estdlib::duration_from_secs(v.max(30.0)))
}

/// Read into `buf` with an optional idle cap. If `cap` is `None`, the read
/// is unbounded (matches C `recv()` blocking semantics in `camsgtask.c`);
/// if `cap` is `Some(d)`, returns `Err(d)` after `d` of inactivity.
#[cfg(not(epics_embedded_target))]
async fn read_with_optional_timeout<R: tokio::io::AsyncReadExt + Unpin>(
    reader: &mut R,
    buf: &mut [u8],
    cap: Option<Duration>,
) -> Result<std::io::Result<usize>, Duration> {
    match cap {
        None => Ok(reader.read(buf).await),
        Some(d) => match tokio::time::timeout(d, reader.read(buf)).await {
            Ok(r) => Ok(r),
            Err(_) => Err(d),
        },
    }
}

/// Parse an opt-in resource cap from an env value. `None` (variable
/// unset, empty, or unparseable) means **unbounded** — C rsrv imposes
/// no per-client channel count limit in `claim_ciu_action`
/// (`camessage.c:1213-1322`) and no per-channel subscription count
/// limit in `event_add_action` (`camessage.c:1812-1920`); both refuse
/// only on genuine memory exhaustion (`casCreateChannel` /
/// `freeListCalloc` / `db_add_event` returning NULL → `ECA_ALLOCMEM`),
/// never on a fixed count. A default cap diverged from this: a single
/// legitimate client (e.g. `caget` over a 5000-PV database) creating
/// more than the cap on one circuit was refused with `ECA_ALLOCMEM`,
/// producing a hard latency cliff at the cap boundary. A present value
/// clamps to `>= 1` (a zero cap would refuse every request).
fn parse_opt_cap(raw: Option<String>) -> Option<usize> {
    raw.and_then(|s| s.parse::<usize>().ok()).map(|n| n.max(1))
}

/// Optional per-client channel cap (`EPICS_CAS_MAX_CHANNELS`).
/// Default-unbounded (C `claim_ciu_action` parity); opt-in only.
fn max_channels_per_client() -> Option<usize> {
    parse_opt_cap(epics_base_rs::runtime::env::get("EPICS_CAS_MAX_CHANNELS"))
}

/// Optional per-channel subscription cap (`EPICS_CAS_MAX_SUBS_PER_CHAN`).
/// Default-unbounded (C `event_add_action` parity); opt-in only.
fn max_subs_per_channel() -> Option<usize> {
    parse_opt_cap(epics_base_rs::runtime::env::get(
        "EPICS_CAS_MAX_SUBS_PER_CHAN",
    ))
}

#[cfg(test)]
mod cap_parse_tests {
    use super::parse_opt_cap;

    /// C rsrv parity: with the env var unset there is **no** cap, so a
    /// legitimate client can create unboundedly many channels /
    /// subscriptions on one circuit. This is the regression guard for
    /// the 4096-channel (and 100-subscription) latency cliff: the prior
    /// code returned a fixed default (`4096` / `100`) here.
    #[test]
    fn unset_env_is_unbounded() {
        assert_eq!(parse_opt_cap(None), None);
    }

    /// An unparseable / empty value is treated as "no valid cap
    /// configured" → unbounded, consistent with unset.
    #[test]
    fn unparseable_or_empty_is_unbounded() {
        assert_eq!(parse_opt_cap(Some(String::new())), None);
        assert_eq!(parse_opt_cap(Some("not-a-number".into())), None);
    }

    /// An explicit value still opts into a cap and clamps to `>= 1` so a
    /// stray `0` cannot refuse every request.
    #[test]
    fn explicit_value_caps_and_clamps_to_one() {
        assert_eq!(parse_opt_cap(Some("4096".into())), Some(4096));
        assert_eq!(parse_opt_cap(Some("1".into())), Some(1));
        assert_eq!(parse_opt_cap(Some("0".into())), Some(1));
    }
}

/// Per-socket send timeout. Without this, a client that stops
/// reading (frozen GUI, dead viewer holding the socket open) causes
/// every server `write` to block once the kernel send buffer fills,
/// stalling the whole per-client dispatcher task.
///
/// **This has no C counterpart.** `SO_SNDTIMEO` appears nowhere in epics-base,
/// and rsrv's `create_client` sets only `TCP_NODELAY`, `SO_KEEPALIVE`,
/// `SO_SNDBUF` and `SO_RCVBUF` (`caservertask.c:1444-1483`); `cas_send_bs_msg`
/// then loops on a blocking `send()` under `SEND_LOCK` with no bound at all
/// (`caserverio.c:43`), and it is the keepalive probe that eventually ends such
/// a client. So the 5 s here is this port's own choice and
/// `EPICS_CAS_SEND_TMO` its own knob — [`crate::estdlib`] classifies it as
/// exactly that, a variable outside C's `ENV_PARAM` table.
#[cfg(not(epics_embedded_target))]
fn send_timeout() -> Duration {
    static RESOLVED: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *RESOLVED.get_or_init(resolve_send_timeout)
}

/// The uncached resolution behind [`send_timeout`]. Resolving once per
/// process keeps a rejected value from re-printing its diagnostic on every
/// client connect.
#[cfg(not(epics_embedded_target))]
fn resolve_send_timeout() -> Duration {
    crate::estdlib::env_double("EPICS_CAS_SEND_TMO")
        .ok()
        .map(|v| crate::estdlib::duration_from_secs(v.max(0.1)))
        .unwrap_or(Duration::from_secs(5))
}

/// Cap on `TlsAcceptor::accept` duration. Without this
/// a peer that completes TCP but stalls during ClientHello holds a
/// connection slot until OS-level keepalive (15s/5s probes) reaps it
/// (~30s); coordinated peers can tie up listener resources. Default
/// 10 s, override via `EPICS_CAS_TLS_HANDSHAKE_TMO`. Floored at 1s.
#[cfg(feature = "experimental-rust-tls")]
fn tls_handshake_timeout() -> Duration {
    static RESOLVED: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *RESOLVED.get_or_init(|| {
        crate::estdlib::env_double("EPICS_CAS_TLS_HANDSHAKE_TMO")
            .ok()
            .map(|v| crate::estdlib::duration_from_secs(v.max(1.0)))
            .unwrap_or(Duration::from_secs(10))
    })
}

/// Connection lifecycle event broadcast by the TCP listener.
///
/// Marked `#[non_exhaustive]` so subsequent variants (e.g. per-monitor
/// events) can be added without breaking downstream `match` arms.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ServerConnectionEvent {
    /// New client connection accepted.
    Connected(SocketAddr),
    /// Client connection closed.
    Disconnected(SocketAddr),
    /// `CA_PROTO_CREATE_CHAN` succeeded for `pv_name` on `peer`. The
    /// `cid` is the client-supplied channel id from the request — pass
    /// it through to consumers so multiple channels for the same
    /// `(peer, pv_name)` pair don't collapse into one refcount slot.
    /// Used by the CA gateway to drive per-PV `Inactive` → `Active`
    /// transitions (see `ca_gateway::cache::GwPvEntry::add_subscriber`).
    ChannelCreated {
        peer: SocketAddr,
        pv_name: String,
        cid: u32,
    },
    /// `CA_PROTO_CLEAR_CHANNEL` (or implicit teardown) closed a channel
    /// for `pv_name` on `peer`. The `cid` matches the corresponding
    /// [`Self::ChannelCreated`] event one-to-one. Reverse of that event.
    ChannelCleared {
        peer: SocketAddr,
        pv_name: String,
        cid: u32,
    },
    /// `CA_PROTO_EVENT_ADD` accepted; a new subscription is live.
    /// Drives `ServerStats::subscriptions_opened_total` (PR #592's
    /// `caServerSubscriptionCount`).
    ///
    /// `mask` is the validated `DBE_*` event-select mask the client
    /// requested (`1..=255`). The CA gateway consults `mask & DBE_PROPERTY`
    /// to decide, in no-cache mode, whether to spawn the upstream property
    /// monitor for this PV — mirroring C ca-gateway gating `propMonitor()`
    /// on `needPosting() && client_mask == DBE_PROPERTY`
    /// (`gatePv.cc:1749-1752`).
    SubscriptionOpened {
        peer: SocketAddr,
        pv_name: String,
        sub_id: u32,
        mask: u16,
    },
    /// `CA_PROTO_EVENT_CANCEL` or channel teardown closed a
    /// subscription. Drives `ServerStats::subscriptions_closed_total`.
    /// Subtract from the opened counter for the live subscription
    /// count.
    SubscriptionClosed {
        peer: SocketAddr,
        pv_name: String,
        sub_id: u32,
    },
}

use super::LongStringMode;
use crate::protocol::*;
use crate::server::monitor::spawn_monitor_sender;
use crate::server::outbox::Outbox;
// The accumulator is shared with the blocking driver, but this file's only
// user of it is the async `handle_client`; host-only, same gate as that fn.
// The embedded driver reaches the same primitive from `server::blocking`.
#[cfg(not(epics_embedded_target))]
use crate::server::recv::{Admit, Gate, RecvAccumulator};
// `OutboxDrain` is consumed only by the async `drain_and_flush`; host-only.
use crate::server::frame::{FrameBuf, size_dbr_reply};
#[cfg(not(epics_embedded_target))]
use crate::server::outbox::OutboxDrain;
use epics_base_rs::error::CaResult;
// The accept backoff is shared with the blocking driver, but this file's only
// user of it is the async `accept_loop`; host-only, same gate as that fn. The
// embedded driver reaches the same primitive from `server::blocking`.
#[cfg(not(epics_embedded_target))]
use epics_base_rs::runtime::accept::AcceptBackoff;
use epics_base_rs::server::access_security::{AccessLevel, AccessSecurityConfig};
use epics_base_rs::server::database::{PvDatabase, PvEntry};
use epics_base_rs::server::pv::ProcessVariable;
use epics_base_rs::server::record::{FieldDeclaration, RecordInstance};
use epics_base_rs::types::{DbFieldType, EpicsValue, encode_dbr_into, native_type_for_dbr};

#[derive(Clone)]
pub(crate) enum ChannelTarget {
    SimplePv(Arc<ProcessVariable>),
    RecordField {
        record: Arc<parking_lot::RwLock<RecordInstance>>,
        field: String,
    },
}

/// The CA status a *database put* reports to the client.
///
/// C decides this by LAYER, not by which database error came back:
/// `write_action` (`rsrv/camessage.c:804-820`) answers a failed
/// `dbChannel_put` with `ECA_PUTFAIL` whatever the `dbStatus` was, and
/// `write_notify_reply` (`camessage.c:1417-1421`) maps every
/// `status != notifyOK` to the same `ECA_PUTFAIL`. The type errors —
/// `ECA_BADTYPE` — belong to the gates ABOVE the put (the `INVALID_DB_REQ`
/// DBR-type gate, `camessage.c:753-756`, and `caNetConvert`,
/// `camessage.c:784-790`), which run first and tear the
/// connection down; a *value* that the field's converter refuses is not a
/// wire-type error and can never be reported as one.
///
/// Constructing the reply status only through this type is what keeps that
/// true: [`CaError::to_eca_status`](epics_base_rs::error::CaError::to_eca_status) is the client-side table, where
/// `ECA_BADTYPE` is a legitimate answer, and calling it on a put reply is
/// how the port came to tell a `caput` of `32768` into a `DBF_SHORT` field
/// "The data type specified is invalid" where C says "Channel write request
/// failed".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct PutStatus(u32);

impl PutStatus {
    /// The put reached the database and was accepted.
    const OK: Self = Self(ECA_NORMAL);

    /// The status for a put the database refused.
    fn of_failure(err: &epics_base_rs::error::CaError) -> Self {
        use epics_base_rs::error::CaError;
        Self(match err {
            // C reports the channel's own write refusal from `rsrvCheckPut`,
            // above the put: measured on the C softIoc, `caput REC.STAT 3`
            // (a `special(SPC_NOMOD)` field) answers "Write access denied".
            CaError::ReadOnlyField(_) => ECA_NOWTACCESS,
            // Not an rsrv case — its database is local — but this server also
            // backs the CA gateway, whose "database" is an upstream channel.
            // A dead upstream is actionable as ECA_DISCONN, not as "Put fail".
            CaError::Disconnected | CaError::Shutdown | CaError::Io(_) => ECA_DISCONN,
            // A refusal raised BELOW this layer: the database would not take a
            // second put-callback for the record, so it answers the request
            // that drew it — the arriving one. That is a different path from
            // the CA layer's own ECA_PUTCBINPROG ([`PutNotifySlot`], which only
            // ever answers a timed-out predecessor), and turning that refusal
            // into a queued restart is the database's to do, so nothing here
            // second-guesses it.
            CaError::PutCallbackInProgress(_) => ECA_PUTCBINPROG,
            // Every other way the database can refuse a put — a value the
            // field's converter rejects, a menu string that names no choice, a
            // record-side veto — is C's `dbStatus < 0`.
            _ => ECA_PUTFAIL,
        })
    }

    /// The status of a completed put, ready for the wire.
    fn eca(self) -> u32 {
        self.0
    }
}

#[cfg(test)]
mod put_status_tests {
    //! The boundary this type exists to hold: a refusal from INSIDE the
    //! database put is `ECA_PUTFAIL` no matter which error the record layer
    //! raised, while the refusal from ABOVE it keeps its own status.
    //!
    //! Measured on the C softIoc (`record(ai,"MEAS:A") {}`):
    //! `caput MEAS:A.PHAS 32768` and `caput MEAS:A.RVAL notanumber` both print
    //! "Channel write request failed" (`ECA_PUTFAIL`), while
    //! `caput MEAS:A.STAT 3` — a `special(SPC_NOMOD)` field — prints
    //! "Write access denied" (`ECA_NOWTACCESS`).

    use super::*;
    use epics_base_rs::error::CaError;

    /// Every way the database can refuse a value is C's `dbStatus < 0`, and
    /// C answers all of them with one status. In particular none of them may
    /// answer `ECA_BADTYPE`: the wire type was already accepted by the gate
    /// above (C `caNetConvert`), so "the data type specified is invalid" is a
    /// statement the put path is not entitled to make.
    #[test]
    fn every_database_refusal_is_putfail_and_never_badtype() {
        let refusals = [
            CaError::InvalidValue("32768 overflows DBF_SHORT".into()),
            CaError::TypeMismatch("string into a numeric field".into()),
            CaError::UnsupportedType(epics_base_rs::types::DBR_STRING),
            CaError::BadField("S_db_badField".into()),
            CaError::BadChoice("S_db_badChoice".into()),
            CaError::PutDisabled("DISP".into()),
            CaError::FieldNotFound("NOSUCH".into()),
        ];
        for err in &refusals {
            let status = PutStatus::of_failure(err).eca();
            assert_ne!(
                status, ECA_BADTYPE,
                "{err:?}: the wire type passed `caNetConvert` — a value the field \
                 refuses is not a type error"
            );
            assert_eq!(
                status, ECA_PUTFAIL,
                "{err:?}: C `write_action` answers every failed dbChannel_put with \
                 ECA_PUTFAIL (camessage.c:781-789)"
            );
        }
    }

    /// The refusal C reports from `rsrvCheckPut`, above the put.
    #[test]
    fn a_field_the_channel_cannot_write_stays_nowtaccess() {
        assert_eq!(
            PutStatus::of_failure(&CaError::ReadOnlyField("STAT".into())).eca(),
            ECA_NOWTACCESS
        );
    }

    /// An accepted put is not a failure status.
    #[test]
    fn an_accepted_put_is_normal() {
        assert_eq!(PutStatus::OK.eca(), ECA_NORMAL);
    }
}

/// How long C lets a channel's previous put-callback stay busy before it
/// gives up on it — `epicsEventWaitWithTimeout(client->blockSem, 60.0)` in
/// `write_notify_action` (`camessage.c:1711`). Reaching it is the ONLY
/// condition under which this layer emits ECA_PUTCBINPROG.
const PUT_NOTIFY_BLOCK_TIMEOUT: Duration = Duration::from_secs(60);

/// Who owns one in-flight put-callback's single client reply — and, with it,
/// that put's put-log bracket.
///
/// C carries both on `pciu->pPutNotify`: `busy` says the put-callback is still
/// outstanding, and `asWritePvt` is the `asTrapWriteWithData` handle whichever
/// path answers the request must close — `camessage.c:1735-1747` lifts it out
/// of the record and calls `asTrapWriteAfter` itself. Holding them in one cell
/// is what stops a reply and its bracket from being closed by different paths:
/// [`claim`](Self::claim) hands out both together, once.
///
/// "Still busy" is the negation of "reply claimed", so it is one atomic with
/// one meaning rather than a `busy` flag and a `responded` flag that can
/// disagree.
struct PutNotifyCompletion {
    replied: AtomicBool,
    trap_guard: std::sync::Mutex<Option<epics_base_rs::server::access_security::TrapWriteGuard>>,
}

/// Proof that its holder owns a put-callback's one client reply, carrying the
/// put-log bracket that has to be closed with it. Dropping it without
/// completing the guard fires the guard's cancel AfterWrite, the balance C
/// gets from `asTrapWriteAfter` on a cancelled put.
struct PutNotifyReply(Option<epics_base_rs::server::access_security::TrapWriteGuard>);

impl PutNotifyCompletion {
    fn new(
        trap_guard: Option<epics_base_rs::server::access_security::TrapWriteGuard>,
    ) -> Arc<Self> {
        Arc::new(Self {
            replied: AtomicBool::new(false),
            trap_guard: std::sync::Mutex::new(trap_guard),
        })
    }

    /// C `pciu->pPutNotify->busy`.
    fn is_busy(&self) -> bool {
        !self.replied.load(Ordering::Acquire)
    }

    /// Take this put-callback's reply. `None` for every caller after the
    /// first, so one ioid can never draw two replies.
    fn claim(&self) -> Option<PutNotifyReply> {
        if self.replied.swap(true, Ordering::AcqRel) {
            return None;
        }
        Some(PutNotifyReply(
            self.trap_guard
                .lock()
                .expect("put-notify trap guard poisoned")
                .take(),
        ))
    }
}

/// A channel's registered in-flight `CA_PROTO_WRITE_NOTIFY` put-callback — C
/// `pciu->pPutNotify` once `busy = TRUE` (`camessage.c:1773`).
struct InFlightPutNotify {
    completion: Arc<PutNotifyCompletion>,
    /// Reply shape the overdue branch echoes: C
    /// `putNotifyErrorReply(client, &pPutNotify->msg, ECA_PUTCBINPROG)`
    /// (`camessage.c:1745`) frames it from the SAVED request, never from the
    /// one that just arrived.
    reply: WriteNotifyReply,
    /// When it was registered — `camessage.c:1712` measures its 60 s
    /// `blockSem` wait from here.
    busy_since: std::time::Instant,
}

/// Single owner of a channel's in-flight put-callback registration, and of the
/// one rule this layer has about ECA_PUTCBINPROG.
///
/// C `write_notify_action` (`camessage.c:1704-1750`) does not refuse a
/// WRITE_NOTIFY that arrives while the channel's previous put-callback is
/// still busy: it waits on `client->blockSem`, and only when that wait runs
/// out does it `dbNotifyCancel` the predecessor and send ECA_PUTCBINPROG,
/// addressed to the PREDECESSOR's saved `msg` (`camessage.c:1745` — the sole
/// site that status has in all of rsrv). The arriving request is never the one
/// refused.
///
/// So the rule here is "emit only on C's timeout path": a predecessor that has
/// settled, or that is still inside [`PUT_NOTIFY_BLOCK_TIMEOUT`], draws no
/// frame at all. Serialising concurrent put-callbacks — queueing the second on
/// the record's restart list versus refusing it — belongs to the database
/// below, so nothing here suppresses a `CaError::PutCallbackInProgress` coming
/// up from there.
///
/// Both receive loops reach this through [`serve_write_head`], which also
/// performs the registration, so neither can serialise on its own terms.
#[derive(Clone, Default)]
pub(crate) struct PutNotifySlot {
    inner: Arc<std::sync::Mutex<Option<InFlightPutNotify>>>,
}

impl PutNotifySlot {
    /// Register a freshly forked put-callback as the channel's in-flight one,
    /// replacing whatever the slot held — C overwrites the single
    /// `pciu->pPutNotify` the same way.
    fn install(&self, inflight: InFlightPutNotify) {
        *self.inner.lock().expect("put-notify slot poisoned") = Some(inflight);
    }

    /// C `write_notify_action`'s serialisation block (`camessage.c:1704-1750`),
    /// run before this WRITE_NOTIFY's first side effect. The arriving request
    /// proceeds on every arm; only a predecessor that has outstayed
    /// [`PUT_NOTIFY_BLOCK_TIMEOUT`] draws ECA_PUTCBINPROG, and it draws it for
    /// its own ioid.
    fn serialize(&self, writer: &Outbox) -> CaResult<()> {
        // Deregister once the predecessor can no longer be waited on: either it
        // settled (C's `blockSem` returns at once) or it outstayed the wait. One
        // still inside the wait stays registered and draws nothing.
        let expired = {
            let mut slot = self.inner.lock().expect("put-notify slot poisoned");
            let done = slot.as_ref().is_some_and(|reg| {
                !reg.completion.is_busy() || reg.busy_since.elapsed() >= PUT_NOTIFY_BLOCK_TIMEOUT
            });
            if done { slot.take() } else { None }
        };
        let Some(reg) = expired else { return Ok(()) };
        // A settled predecessor has already spent its reply, so `claim` says no
        // and nothing goes out; only the overdue one reaches the frame. Losing
        // the claim here is C's second `busy` re-test under `putNotifyLock`
        // (`camessage.c:1730`): a put that finished mid-decision keeps its real
        // reply.
        let Some(reply_token) = reg.completion.claim() else {
            return Ok(());
        };
        // Dropping the token un-completed closes the put-log bracket with the
        // guard's cancel AfterWrite — C `asTrapWriteAfter(asWritePvtTmp)`.
        drop(reply_token);
        send_put_notify_response(
            writer,
            reg.reply.write_type,
            reg.reply.write_count,
            ECA_PUTCBINPROG,
            reg.reply.ioid,
            ReplyContext {
                req_hdr: reg.reply.req_hdr,
                client_minor: reg.reply.client_minor,
            },
        )
    }
}

struct ChannelEntry {
    target: ChannelTarget,
    cid: u32,
    /// PV name as the client originally requested it (with any
    /// `.FIELD` suffix). Retained so the `ChannelCleared` lifecycle
    /// event can emit the same name as `ChannelCreated`.
    pv_name: String,
    /// Raw channel-filter JSON suffix the client appended after the
    /// record (epics-base 3.15.7). `None` for ordinary channels;
    /// `Some` when the client requested `REC.{"dbnd":{"d":0.5}}`
    /// etc. Validated by `try_parse_filter_chain` at
    /// `CA_PROTO_CREATE_CHAN` (a bad suffix is rejected with
    /// `CREATE_CH_FAIL` and no `ChannelEntry` is stored), then re-parsed
    /// per delivery via `ChannelEntry::filter_chain` so each subscriber
    /// and the READ path get a fresh stateful chain.
    filter_suffix: Option<String>,
    /// Single owner of this channel's in-flight `CA_PROTO_WRITE_NOTIFY`
    /// put-callback registration — C `pciu->pPutNotify`, one per channel
    /// (`rsrv/camessage.c:1704-1776`). Registered and consulted by
    /// [`serve_write_head`], so both receive loops get the same rule. See
    /// [`PutNotifySlot`]. `Arc`-backed so the completion path and a later
    /// request share it without re-borrowing `state.channels`.
    put_notify_slot: PutNotifySlot,
    /// long-string boundary conversion this channel applies on every
    /// GET/monitor delivery (and the matching native-type override set
    /// at channel-create time). `DollarChar` = client appended `$` to a
    /// `DBF_STRING` field (C `dbChannel.c:483-507`): delivered as a
    /// `DBR_CHAR[40]` array. `NativeString` = plain access to a
    /// long-string *record* field (lsi/lso VAL & OVAL, printf VAL),
    /// which C `cvt_dbaddr` presents as a scalar `DBF_STRING`. `Plain`
    /// for every other channel.
    long_string_mode: LongStringMode,
    /// Final (post-filter) element count announced to the client at
    /// `CA_PROTO_CREATE_CHAN` — the port's `dbChannelFinalElements`
    /// equivalent. Every request's wire element count is clamped to this
    /// ceiling on READ / READ_NOTIFY / EVENT_ADD so an oversized
    /// `m_count` cannot drive the reply zero-fill
    /// (`size_dbr_reply` / the steady-state monitor
    /// producer) past the channel's real capacity. Mirrors epics-base
    /// PR #934's `if (mp->m_count > dbChannelFinalElements(pciu->dbch))
    /// mp->m_count = dbChannelFinalElements(pciu->dbch);` clamp. This is
    /// the channel's true capacity (NELM), NOT the live value length
    /// (`snapshot.value.count()`, which is NORD for a partially-filled
    /// waveform) — clamping to the live length would truncate a
    /// legitimate `caget -# NELM` on a dynamic waveform.
    final_element_count: u32,
}

impl ChannelEntry {
    /// parse a FRESH channel-filter chain from this channel's
    /// stored `.{...}` suffix. A fresh chain per call is REQUIRED:
    /// stateful filters (`dbnd` last-value, `dec` counter, `sync` state)
    /// must not share state across the READ path and each monitor
    /// subscriber, nor across two subscribers on one channel.
    ///
    /// Uses the STRICT `try_parse_filter_chain`. The suffix stored on a
    /// `ChannelEntry` was already validated by the same strict parser at
    /// `CA_PROTO_CREATE_CHAN` (a `ChannelEntry` exists only if its suffix
    /// parsed — a bad suffix is rejected with `CREATE_CH_FAIL` and no
    /// entry is inserted). Parsing is deterministic, so this re-parse
    /// always succeeds; the empty fallback is unreachable, kept only so a
    /// delivery path never panics. This holds the invariant by
    /// construction — delivery never silently downgrades to an unfiltered
    /// stream the way the permissive parser would.
    ///
    /// This is the single owner of filter parsing for every delivery
    /// path — READ / READ_NOTIFY, the monitor initial snapshot, and
    /// monitor updates — so the chain is applied uniformly instead of
    /// only on the record-field `EVENT_ADD` path (the prior gap).
    fn filter_chain(&self) -> epics_base_rs::server::database::filters::FilterChain {
        match self.filter_suffix.as_deref() {
            Some(json) => epics_base_rs::server::database::filters::try_parse_filter_chain(json)
                .unwrap_or_default(),
            None => epics_base_rs::server::database::filters::FilterChain::new(),
        }
    }
}

impl SubscriptionEntry {
    /// C `pevext->msg` — the EVENT_ADD request header rsrv stores alongside
    /// the subscription and echoes back on the error path (`read_reply`
    /// `camessage.c:516-524`, `no_read_access_event` `camessage.c:455-485`).
    /// Rebuilt from the fields the entry already keeps, so it is the header
    /// the client actually sent.
    fn request_header(&self) -> CaHeader {
        let mut h = CaHeader::new(CA_PROTO_EVENT_ADD);
        h.data_type = self.data_type;
        h.cid = self.channel_sid;
        h.available = self.sub_id;
        // The client framed this request itself, so it is V49-capable by
        // construction whenever the count needs the extended form.
        h.set_payload_size(0, self.data_count, CA_MINOR_VERSION)
            .expect("the client framed this very request");
        h
    }
}

struct SubscriptionEntry {
    target: ChannelTarget,
    channel_sid: u32,
    sub_id: u32,
    data_type: u16,
    /// Original requested element count from the EVENT_ADD that
    /// installed this subscription. C `event_add_action` stores
    /// `pevext->msg.m_count`; monitor delivery and the
    /// EVENT_CANCEL ack both echo it.
    data_count: u32,
    /// Gate flipped by `reeval_access_rights` when read access is
    /// revoked / restored for `channel_sid`. While `true`, the
    /// producer task drops events at the send step (matches C
    /// `casAccessRightsCB`, `rsrv/camessage.c:1116-1124`, which
    /// calls `db_event_disable` rather than tearing the
    /// subscription down — so an ACF reload that later restores
    /// access can resume the same camonitor).
    denied: Arc<AtomicBool>,
    task: epics_base_rs::runtime::task::TaskHandle<()>,
    /// mirrors `ChannelEntry::long_string_mode`; stored here so the
    /// access-restore path and `reeval_access_rights` can apply the
    /// same boundary conversion without re-borrowing the channel entry.
    long_string_mode: LongStringMode,
}

/// Per-circuit CA server state. `pub(crate)` so the blocking
/// thread-per-client driver (`crate::server::blocking`, the RTEMS CA
/// server front-end) can construct one and drive [`dispatch_message`]
/// against it exactly as the async `handle_client` loop does — the two
/// front-ends share this state and the dispatch handlers verbatim.
pub(crate) struct ClientState {
    channels: HashMap<u32, ChannelEntry>,
    subscriptions: HashMap<u32, SubscriptionEntry>,
    channel_access: HashMap<u32, AccessLevel>,
    /// per-SID write-trap mask of the ACF rule that resolved
    /// the channel's access level. Kept parallel to `channel_access`
    /// (same key set, inserted/removed together) because the trap
    /// flag has no `CA_PROTO_ACCESS_RIGHTS` wire representation — it
    /// is consumed only by TRAPWRITE put-logging dispatch, never
    /// diffed for access-rights transition frames. Mirrors C
    /// `pasgclient->trapMask` (`asLibRoutines.c:1048`).
    channel_trap: HashMap<u32, bool>,
    next_sid: AtomicU32,
    /// Recycled SIDs from channels destroyed via CLEAR_CHANNEL —
    /// without recycling, `next_sid` would wrap after 2³² channel
    /// creations and start handing out SIDs that collide with live
    /// channels. epics-base `rsrv/camessage.c` uses
    /// `freeListItemPvt` for the same reason. We use a Vec stack
    /// (LIFO) so the most-recently-freed SID is reused first —
    /// keeps the active set's SIDs clustered near the low end.
    free_sids: Vec<u32>,
    /// This circuit's access-security host identity. See [`HostIdentity`]
    /// — the variant, not a runtime check at the write site, decides
    /// whether `CA_PROTO_HOST_NAME` may replace it.
    hostname: HostIdentity,
    username: String,
    /// Authentication method for ACF `METHOD()` clause matching.
    /// `"x509"` for mTLS-authenticated peers (epics-base PR #641);
    /// `"ca"` (or empty for backwards compat) for plaintext peers.
    /// ACF rules without a `METHOD()` clause ignore this field
    /// — the legacy `check_access_asl()` codepath continues to work.
    auth_method: String,
    /// Authority for ACF `AUTHORITY()` clause matching. mTLS peers
    /// carry their cert's *issuer* DN here so rules like
    /// `RULE(1, WRITE) { METHOD("x509") AUTHORITY("CN=ops-ca, …") }`
    /// can pin write access to certs minted by a specific CA.
    /// Empty for plaintext peers.
    auth_authority: String,
    acf: epics_base_rs::server::access_security::AcfCell,
    /// record database, for resolving ACF `INP*` links to live
    /// values when evaluating CALC-gated rules in `compute_access`.
    db: Arc<PvDatabase>,
    tcp_port: u16,
    client_minor_version: u16,
    /// C `client->evuser` (`rsrv/server.h`): this circuit's event user — the
    /// owner of `flowCtrlMode` (EVENTS_OFF/EVENTS_ON) and of the event queue
    /// every subscription on this circuit posts into (`db_init_events`).
    event_user: Arc<epics_base_rs::server::event_queue::EventUser>,
    /// One-shot flag — set when channels.len() crosses 90% of the
    /// per-client cap. Prevents log spam on every subsequent
    /// CREATE_CHAN once the warning has fired.
    channel_limit_warned: bool,
    /// Peer address as a string, retained for audit events.
    peer: String,
    /// Optional audit logger. When None the audit hot path is a single
    /// branch test and no allocation.
    audit: Option<crate::audit::AuditLogger>,
    /// Optional per-client token bucket. None disables rate limiting.
    // Set in `ClientState::new` / the async accept path, consumed only by the
    // async read loop (`handle_client`), which is gated out on `epics_embedded_target`.
    #[cfg_attr(epics_embedded_target, allow(dead_code))]
    rate_limiter: Option<crate::server::rate_limit::RateLimiter>,
    /// Consecutive denied messages — disconnect when this exceeds the
    /// configured strike threshold.
    #[cfg_attr(epics_embedded_target, allow(dead_code))]
    rate_limit_strikes: u32,
    #[cfg_attr(epics_embedded_target, allow(dead_code))]
    rate_limit_strike_threshold: u32,
    /// Capability-token verifier shared across all clients on this
    /// listener. When set, CLIENT_NAME payloads beginning with `cap:`
    /// are verified before the resolved subject is used as the ACF
    /// username.
    #[cfg(feature = "cap-tokens")]
    cap_token_verifier: Option<Arc<crate::cap_token::TokenVerifier>>,
    /// TLS channel binding (SHA-256 of the peer's leaf certificate DER)
    /// for this connection. `Some(..)` only when the peer connected
    /// over mTLS and presented a client certificate; `None` for
    /// plaintext circuits. A cap-token presented over a plaintext
    /// circuit (`None`) is rejected by `TokenVerifier::verify` —
    /// mTLS-gating, so a stolen token cannot be replayed off-channel.
    #[cfg(feature = "cap-tokens")]
    tls_channel_binding: Option<crate::cap_token::ChannelBinding>,
    /// Pending WRITE_NOTIFY completion tasks. Each entry is the channel
    /// `sid`-tagged AbortHandle of a task awaiting `put_notify_tx` for
    /// an async record write. Aborted on connection drop so a stuck
    /// async device doesn't leak the task forever, and also aborted
    /// when the owning channel is freed via `CA_PROTO_CLEAR_CHANNEL`
    /// (C parity: `clear_channel_reply` calls `rsrvFreePutNotify`
    /// per-channel — `camessage.c:1943`). The sid tag lets us drain
    /// only the channel-scoped tasks on CLEAR_CHANNEL without
    /// disturbing other channels' in-flight WRITE_NOTIFYs.
    write_notify_tasks: Vec<(u32, tokio::task::AbortHandle)>,
    /// Server-wide stats handle, cloned into each monitor task so the
    /// `subscription_events_posted` / `subscription_events_processed`
    /// counters (PCAS `caServer` parity, feeding the gateway's
    /// `serverPostRate` / `serverEventRate`) advance from the delivery
    /// layer. `None` in unit tests that drive the TCP path without a
    /// full `ServerStats` wired up.
    stats: Option<Arc<super::stats::ServerStats>>,
}

/// The access-security host identity of one CA circuit, and — by
/// construction — whether the client may still set it.
///
/// C keeps this in `client->pHostName` and decides at accept time, from
/// the global `asCheckClientIP` (`asLibRoutines.c:34`, default 0), which
/// of two things that pointer means:
///
/// * `asCheckClientIP == 0` (**C's default**) — `create_tcp_client` leaves
///   it NULL and `host_name_action` stores whatever name the client sends
///   in `CA_PROTO_HOST_NAME`, unconditionally (`camessage.c:880-903`).
///   Until then the identity is `""` (`camessage.c:1276-1281` passes
///   `pHostName ? pHostName : ""` to `asAddClient`), which matches no HAG.
///   HAG entries are host *names* in this mode, so this is the identity
///   `HOST(...)` rules are written against.
/// * `asCheckClientIP == 1` — `create_tcp_client` fills it with the peer's
///   dotted-quad IP (`caservertask.c:1425-1437`) and `host_name_action`
///   returns early without storing the claimed name
///   (`camessage.c:869-875`).
///
/// The port adds a third source with no C counterpart: an mTLS-verified
/// certificate identity, which likewise cannot be overwritten by a
/// client-supplied name.
///
/// Modelling this as two variants rather than a `String` plus an "is it
/// allowed to change?" check at the write site is what makes the illegal
/// transition unrepresentable: [`Self::claim`] is the only writer, and it
/// is a no-op on a [`Self::Pinned`] identity.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HostIdentity {
    /// C's default: the name the client claims. Empty until
    /// `CA_PROTO_HOST_NAME` arrives.
    Claimed(String),
    /// Derived from the connection itself — the peer IP under
    /// `asCheckClientIP`, or an mTLS-verified cert identity. A
    /// client-supplied name never replaces it.
    Pinned(String),
}

impl HostIdentity {
    /// The identity string ACF/HAG matching runs against.
    fn as_str(&self) -> &str {
        match self {
            Self::Claimed(h) | Self::Pinned(h) => h,
        }
    }

    /// Apply a `CA_PROTO_HOST_NAME` claim. Returns whether it was taken —
    /// `false` on a [`Self::Pinned`] identity, which is C's
    /// `host_name_action` early return under `asCheckClientIP`.
    fn claim(&mut self, name: String) -> bool {
        match self {
            Self::Claimed(h) => {
                *h = name;
                true
            }
            Self::Pinned(_) => false,
        }
    }
}

impl ClientState {
    pub(crate) fn new(
        acf: epics_base_rs::server::access_security::AcfCell,
        tcp_port: u16,
        db: Arc<PvDatabase>,
    ) -> Self {
        Self {
            channels: HashMap::new(),
            subscriptions: HashMap::new(),
            channel_access: HashMap::new(),
            channel_trap: HashMap::new(),
            next_sid: AtomicU32::new(1),
            free_sids: Vec::new(),
            hostname: HostIdentity::Claimed(String::new()),
            username: String::new(),
            auth_method: String::new(),
            auth_authority: String::new(),
            acf,
            db,
            tcp_port,
            client_minor_version: 0,
            event_user: Arc::new(epics_base_rs::server::event_queue::EventUser::new()),
            channel_limit_warned: false,
            peer: String::new(),
            audit: None,
            rate_limiter: None,
            rate_limit_strikes: 0,
            rate_limit_strike_threshold: 0,
            #[cfg(feature = "cap-tokens")]
            cap_token_verifier: None,
            #[cfg(feature = "cap-tokens")]
            tls_channel_binding: None,
            write_notify_tasks: Vec::new(),
            stats: None,
        }
    }

    /// Decide this circuit's ACF host identity + mTLS auth context, once,
    /// from the connection itself. C decides the same in `create_tcp_client`
    /// (`caservertask.c:1425-1437`). Extracted from `handle_client` so the
    /// async loop and the blocking thread-per-client driver
    /// (`crate::server::blocking`) derive identity byte-identically. See
    /// [`HostIdentity`].
    pub(crate) fn apply_connection_identity(
        &mut self,
        peer: SocketAddr,
        initial_hostname: Option<String>,
        tls_authority: Option<String>,
    ) {
        self.hostname = match initial_hostname {
            // Port extension, no C counterpart: an mTLS-verified cert identity
            // outranks both of C's modes and no client-supplied name replaces
            // it (doc/11-tls-design.md).
            Some(verified) => HostIdentity::Pinned(verified),
            // C `asCheckClientIP == 1`: the peer's address is the identity and
            // CA_PROTO_HOST_NAME is ignored.
            None if epics_base_rs::server::access_security::as_check_client_ip() => {
                HostIdentity::Pinned(peer.ip().to_string())
            }
            // C's default: NULL `pHostName` — i.e. `""` to `asAddClient` —
            // until the client claims a name over CA_PROTO_HOST_NAME.
            None => HostIdentity::Claimed(String::new()),
        };
        // PR #641: surface the mTLS authentication context to the ACF
        // check. Plaintext peers stay with empty fields — every legacy
        // rule (no METHOD/AUTHORITY clause) ignores them.
        if let Some(authority) = tls_authority {
            self.auth_method = "x509".to_string();
            self.auth_authority = authority;
        }
        self.peer = peer.to_string();
    }

    /// The negotiated CA minor protocol version of the peer. Needed by the
    /// blocking driver's receive loop, which feeds it to the shared gate
    /// (`RecvAccumulator::next_message`) and to its error replies
    /// (`send_ca_error`) from outside this module.
    pub(crate) fn client_minor_version(&self) -> u16 {
        self.client_minor_version
    }

    fn audit(&self, event: &str, pv: &str, value: &str, result: &str) {
        if let Some(ref logger) = self.audit {
            logger.log(crate::audit::AuditEvent {
                event,
                peer: &self.peer,
                user: &self.username,
                host: self.hostname.as_str(),
                pv,
                value,
                result,
            });
        }
    }

    fn alloc_sid(&mut self) -> u32 {
        // prefer recycled SIDs from CLEAR_CHANNEL'd channels.
        // Falls back to monotonic counter only when the free list is
        // empty, which prevents wraparound collisions on long-uptime
        // high-churn servers (epics-base rsrv `freeListItemPvt`
        // parity).
        if let Some(sid) = self.free_sids.pop() {
            return sid;
        }
        self.next_sid.fetch_add(1, Ordering::Relaxed)
    }

    /// Return a SID to the free list when its channel is destroyed.
    fn release_sid(&mut self, sid: u32) {
        self.free_sids.push(sid);
    }

    /// Return the type-state-wrapped access token for a
    /// SID. Op handlers MUST consult this — direct reads of the
    /// underlying `channel_access` HashMap bypass the typed gate
    /// and recreate the missed-path defects fixed in rounds 38-39.
    /// Missing SIDs map to a "denied" token so a corrupted
    /// channel-table state can never silently grant access.
    fn lookup_access(&self, sid: u32) -> crate::server::access_token::CaAccessChecked {
        use crate::server::access_token::CaAccessChecked;
        match self.channel_access.get(&sid).copied() {
            Some(level) => {
                // the trap mask is kept in a parallel map
                // populated alongside `channel_access`. A missing
                // entry means the rule carried no trap option.
                let rule_was_trap = self.channel_trap.get(&sid).copied().unwrap_or(false);
                CaAccessChecked::from_level(level, rule_was_trap)
            }
            None => CaAccessChecked::denied(),
        }
    }

    /// Compute access rights bits for a channel target, together with
    /// the write-trap mask of the ACF rule that resolved the level
    ///. The trap flag is `false` for `SimplePv`/`RecordField`
    /// targets whose access was not resolved through a `TRAPWRITE`
    /// rule — including the no-ACF permissive fallback.
    /// Resolve one ASG `INP*` link (`record` or `record.FIELD`) to its live
    /// numeric value. `None` — the `asCa.c` "channel not connected" case —
    /// marks that ONE input bad; which rules that disables is
    /// `AccessSecurityConfig`'s decision, not this server's.
    ///
    /// Caller must NOT hold a record read-guard that a link could point back
    /// to (would re-read the same lock).
    fn resolve_inp_link(&self, link: &str) -> Option<f64> {
        let (base, field) = epics_base_rs::server::access_security::inp_link_target(link);
        let rec = self.db.get_record(base)?;
        let inst = rec.read();
        inst.resolve_field(field).and_then(|v| v.to_f64())
    }

    /// This ASG's live `INP*` state, walked by the owner in `epics-base-rs`.
    fn calc_inputs(
        &self,
        cfg: &AccessSecurityConfig,
        asg_name: &str,
    ) -> epics_base_rs::server::access_security::AsgInputs {
        cfg.resolve_asg_inputs(asg_name, &|link| self.resolve_inp_link(link))
    }

    /// Evaluate `cfg`'s rules for `asg_name`, with CALC clauses gated against
    /// the resolved `INP*` values.
    ///
    /// The rule walk and the CALC truth test both belong to
    /// `AccessSecurityConfig::compute_rules`; this server only resolves the
    /// links and hands the values over. It used to carry its own evaluator,
    /// a second copy of the one in `epics-base-rs` — and the copies drifted
    /// together rather than apart: both tested `result != 0.0` where C tests
    /// the `(0.99, 1.01)` band.
    fn access_for_asg(
        &self,
        cfg: &AccessSecurityConfig,
        asg_name: &str,
        asl: u8,
    ) -> (AccessLevel, bool) {
        let calc_inputs = self.calc_inputs(cfg, asg_name);
        cfg.compute_for_name(
            asg_name,
            self.hostname.as_str(),
            &self.username,
            &[],
            asl,
            &self.auth_method,
            &self.auth_authority,
            Some(&calc_inputs),
        )
    }

    async fn compute_access(&self, target: &ChannelTarget) -> (u32, bool) {
        match target {
            ChannelTarget::SimplePv(pv) => {
                // a gateway shadow PV carries an access
                // hook routing the decision through the gateway's own
                // ACF (`.pvlist` ASG + `AccessConfig::can_read/write`)
                // for this downstream `(user, host)`. When present it
                // is authoritative for this PV — the server's own ACF
                // never saw the `.pvlist` ASG, so consulting `self.acf`
                // with a hardcoded "DEFAULT" would mis-report read
                // access. The gateway audits/traps writes in its own
                // write hook, so no TRAPWRITE rule resolves here.
                if let Some(hook) = pv.access_hook() {
                    let decision = hook(&self.username, self.hostname.as_str());
                    let bits = match (decision.read, decision.write) {
                        (_, true) => 3,
                        (true, false) => 1,
                        (false, false) => 0,
                    };
                    return (bits, false);
                }
                let policy = self.acf.load_full();
                if let Some(ref acf_cfg) = policy {
                    // Simple PVs have no per-record ASL field; treat
                    // them as ASL=0 so the most-restrictive rule
                    // applies. Matches the C IOC's behaviour for
                    // names that never went through `dbAddMember`.
                    // PR #641: pass auth method/authority so
                    // METHOD("x509") / AUTHORITY(<issuer>) rules
                    // can gate mTLS-authenticated peers. CALC
                    // rules are evaluated against resolved INP* links.
                    let (level, rule_was_trap) = self.access_for_asg(acf_cfg, "DEFAULT", 0);
                    let bits = match level {
                        AccessLevel::ReadWrite => 3,
                        AccessLevel::Read => 1,
                        AccessLevel::NoAccess => 0,
                    };
                    (bits, rule_was_trap)
                } else {
                    // No ACF attached: permissive ReadWrite, no rule
                    // resolved access, so no TRAPWRITE applies.
                    (3, false)
                }
            }
            ChannelTarget::RecordField { record, field: f } => {
                // extract is_ro/asg/asl and DROP the record
                // read-guard before resolving ACF INP* links, so a CALC
                // input pointing back at this record can't re-acquire the
                // same lock.
                let (is_ro, asg, asl) = {
                    let instance = record.read();
                    // C `rsrvCheckPut` (rsrv/camessage.c:2608-2619):
                    //
                    //     /* SPC_NOMOD fields are always unwritable */
                    //     if (dbChannelSpecial(pciu->dbch) == SPC_NOMOD) return 0;
                    //
                    // and its `0` clears the ACCESS_RIGHTS write bit at
                    // camessage.c:1154-1156. The declaration has one owner
                    // (`RecordInstance::is_no_mod`), shared with the `dbPut`
                    // gate — reading `field_list().read_only` alone here saw
                    // neither the dbCommon NOMOD set (NAME/STAT/SEVR/ACKS/…,
                    // which no record's field_list declares) nor the
                    // state-raised NOMOD of `Record::field_no_mod` (compress
                    // VAL under BALG=LIFO), so the server advertised WRITE on
                    // fields it would then refuse.
                    let is_ro = instance.is_no_mod(f.as_str());
                    (
                        is_ro,
                        instance.common.access_group().to_string(),
                        instance.common.asl,
                    )
                };
                // Read-only field-ness must AND
                // with ACF, never replace it. Pre-fix the read-only
                // branch returned `Read`(1) unconditionally — a
                // peer whose ACF resolved to `NoAccess` could still
                // READ / EVENT_ADD on every read-only field because
                // the cached access_rights skipped the ACF check
                // entirely. Now ACF runs first; the read-only flag
                // only strips the WRITE bit from the result.
                let policy = self.acf.load_full();
                let (acf_level, rule_was_trap) = if let Some(ref acf_cfg) = policy {
                    // Thread the per-record ASL so
                    // `RULE(N, …)` gates correctly. PR #641: method/
                    // authority for mTLS rules. CALC rules are
                    // evaluated against resolved INP* links.
                    self.access_for_asg(acf_cfg, &asg, asl)
                } else {
                    (AccessLevel::ReadWrite, false)
                };
                let bits = match (acf_level, is_ro) {
                    (AccessLevel::NoAccess, _) => 0,
                    (AccessLevel::Read, _) => 1,
                    (AccessLevel::ReadWrite, true) => 1,
                    (AccessLevel::ReadWrite, false) => 3,
                };
                // The trap mask reflects the rule that granted access;
                // a read-only field stripping the WRITE bit does not
                // change which rule matched.
                (bits, rule_was_trap)
            }
        }
    }
}

/// TCP listeners that are already bound and listening, one per
/// `EPICS_CAS_INTF_ADDR_LIST` entry, all sharing one port.
///
/// This type is the server's readiness guarantee: it can only be
/// produced by [`bind_tcp_listeners`], and every `CaServer`
/// construction path produces one before handing the server back. A
/// bound-and-listening socket already completes TCP handshakes in the
/// kernel, so a client that connects before `CaServer::run()` is even
/// polled is queued rather than refused — owning a `CaServer` implies
/// "listening", with no readiness handshake for the caller to get
/// wrong.
#[cfg(not(epics_embedded_target))]
#[derive(Clone)]
pub struct BoundTcp {
    listeners: Vec<(Arc<TcpListener>, std::net::Ipv4Addr)>,
    port: u16,
}

#[cfg(not(epics_embedded_target))]
impl BoundTcp {
    /// The port every listener in this set is bound to. This is the
    /// port SEARCH replies and beacons advertise — it differs from the
    /// requested port when the ephemeral fallback fired.
    pub fn port(&self) -> u16 {
        self.port
    }
}

/// Bind one TCP listener per `EPICS_CAS_INTF_ADDR_LIST` interface.
///
/// Tries the configured port first; falls back to an ephemeral port
/// (port 0) if it is already in use.
///
/// C `rsrv_init` (caservertask.c:603-712) iterates `casIntfAddrList`
/// and spawns one `CAS-TCP` accept thread per entry, all bound to the
/// same TCP port. Binding to a *specific* interface IP (vs
/// `INADDR_ANY`) and binding to a *different* specific IP on the same
/// port is allowed by POSIX; only two 0.0.0.0 binds collide. Empty
/// list → single `0.0.0.0` listener (default).
///
/// The first successful bind decides the port. All subsequent binds
/// must use that same port; if a per-interface bind fails it is logged
/// and skipped (matches C `cleanup:` / `continue;` in
/// `caservertask.c:744-749`, which frees the conf and proceeds).
#[cfg(not(epics_embedded_target))]
pub async fn bind_tcp_listeners(port: u16) -> CaResult<BoundTcp> {
    let intf_addrs: Vec<std::net::Ipv4Addr> = {
        let cfg = super::addr_list::from_env()?;
        if cfg.intf_addrs.is_empty() {
            vec![std::net::Ipv4Addr::UNSPECIFIED]
        } else {
            cfg.intf_addrs
        }
    };

    let mut listeners: Vec<(Arc<TcpListener>, std::net::Ipv4Addr)> = Vec::new();
    let mut actual_port: Option<u16> = None;
    for ip in &intf_addrs {
        let target_port = actual_port.unwrap_or(port);
        let bind_ip = std::net::IpAddr::V4(*ip);
        let listener = match TcpListener::bind((bind_ip, target_port)).await {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && actual_port.is_none() => {
                // Only fall back to ephemeral on the FIRST bind. Once
                // a port has been chosen all subsequent interfaces
                // MUST share it — otherwise a multi-NIC server would
                // advertise a different TCP port per interface in its
                // SEARCH replies (which already carry a single port).
                TcpListener::bind((bind_ip, 0)).await?
            }
            Err(e) => {
                if actual_port.is_some() {
                    // Subsequent bind on chosen port failed: log and
                    // skip (C parity, `cleanup:` path frees + continues).
                    tracing::warn!(
                        target: "epics_ca_rs::server::tcp",
                        intf = %ip,
                        port = target_port,
                        error = %e,
                        "TCP listener bind failed on this interface — skipping"
                    );
                    continue;
                }
                return Err(e.into());
            }
        };
        let chosen = listener.local_addr()?.port();
        if actual_port.is_none() {
            actual_port = Some(chosen);
        }
        listeners.push((Arc::new(listener), *ip));
    }

    match actual_port {
        Some(bound_port) => {
            announce_tcp_port(port, bound_port);
            Ok(BoundTcp {
                listeners,
                port: bound_port,
            })
        }
        None => {
            // C `cantProceed("CAS: No TCP server started\n")` at
            // `caservertask.c:752`. Every configured interface failed
            // to bind — there's nothing to serve.
            Err(epics_base_rs::error::CaError::Io(std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                "CAS: No TCP server started — all configured interfaces failed to bind",
            )))
        }
    }
}

/// What C does the moment it knows which TCP port the server actually got
/// (`caservertask.c:576-593`): tell the operator when it is not the port they
/// configured, and publish it as `RSRV_SERVER_PORT` either way.
///
/// The port did neither. A `softIoc` whose configured TCP port was taken came
/// up silently on a random one — its SEARCH replies pointed at a port nobody
/// had been told about, and any `st.cmd` that reads `RSRV_SERVER_PORT` (the
/// documented way to learn it) got nothing.
///
/// `configured == 0` is the caller asking for an ephemeral port outright
/// (`softioc-rs --port 0`); C cannot express that — `envGetInetPortConfigParam`
/// rejects any port at or below 5000 — so getting one back is what was asked
/// for, not a fallback, and C's warning has nothing to say about it.
#[cfg(not(epics_embedded_target))]
fn announce_tcp_port(configured: u16, bound: u16) {
    if configured != 0 && bound != configured {
        // C `caservertask.c:580-590`, five `errlogPrintf` lines, verbatim —
        // including the trailing comma on line 2 and the missing period on
        // line 5. Captured from the compiled `softIoc` with its configured TCP
        // port held by another process.
        let w = epics_base_rs::runtime::log::erl_warning();
        eprintln!("cas {w}: Configured TCP port was unavailable.");
        eprintln!("cas {w}: Using dynamically assigned TCP port {bound},");
        eprintln!("cas {w}: but now two or more servers share the same UDP port.");
        eprintln!("cas {w}: Depending on your IP kernel this server may not be");
        eprintln!("cas {w}: reachable with UDP unicast (a host's IP in EPICS_CA_ADDR_LIST)");
    }
    // C `caservertask.c:592`: `epicsEnvSet("RSRV_SERVER_PORT", buf)`,
    // unconditional — the variable always names the port the server ended up
    // on, which is the only reason a startup script can find a `--port 0` IOC.
    //
    // SAFETY: C's `epicsEnvSet` is the same process-wide `putenv`, called from
    // `rsrv_init` during IOC startup. This runs on the same startup path,
    // before the server has spawned the tasks that read the environment.
    unsafe { std::env::set_var("RSRV_SERVER_PORT", bound.to_string()) };
}

/// Serve CA connections on listeners already bound by
/// [`bind_tcp_listeners`].
///
/// The TCP path does NOT touch the beacon ramp. C `rsrv` sets the ramp's
/// initial period once, when `rsrv_online_notify_task` starts
/// (`online_notify.c:68` `delay = 0.02`), and restarts it in exactly one other
/// place: the `beacon_ctl == ctlPause` wait loop (`online_notify.c:126-129`).
/// A client connect or disconnect never restarts it — accepting a connection
/// is not a server state change other clients need to hear about. This port has
/// no `ctlPause` equivalent (no `iocPause` lifecycle), so the ramp's only reset
/// is its own start, and the beacon-reset signal is reachable solely through
/// [`CaServer::trigger_beacon_anomaly`](super::ca_server::CaServer::trigger_beacon_anomaly)
/// — the ca-gateway's `generateBeaconAnomaly` analogue.
#[cfg(not(epics_embedded_target))]
#[allow(clippy::too_many_arguments)]
pub async fn run_tcp_listener(
    db: Arc<PvDatabase>,
    bound: BoundTcp,
    acf: epics_base_rs::server::access_security::AcfCell,
    acf_reload_tx: broadcast::Sender<()>,
    conn_events: Option<broadcast::Sender<ServerConnectionEvent>>,
    audit: Option<crate::audit::AuditLogger>,
    drain: Arc<std::sync::atomic::AtomicBool>,
    // PR #592 dbServerStats: per-connection byte counters feed the
    // `casr` iocsh command's `bytes in=… out=…` line. Optional so unit
    // tests of the TCP path don't need a full ServerStats wired up.
    stats: Option<Arc<super::stats::ServerStats>>,
    #[cfg(feature = "experimental-rust-tls")] tls: Option<
        Arc<std::sync::RwLock<Arc<tokio_rustls::rustls::ServerConfig>>>,
    >,
    #[cfg(feature = "cap-tokens")] cap_token_verifier: Option<Arc<crate::cap_token::TokenVerifier>>,
) -> CaResult<()> {
    let BoundTcp {
        listeners,
        port: actual_port,
    } = bound;

    // One accept-loop task per bound interface. When the parent
    // `run_tcp_listener` future is dropped (CaServer shutdown via
    // `tcp_abort.abort()`), this JoinSet is dropped which aborts all
    // accept loops as a unit. First task to error wins; the rest
    // are aborted via JoinSet::Drop.
    let mut accept_tasks: tokio::task::JoinSet<CaResult<()>> = tokio::task::JoinSet::new();

    // ASG-field-change forwarder. C
    // `database/src/ioc/as/asDbLib.c:107-110,144` `asSpcAsCallback`
    // is wired by `asInitCommon` as the per-record `ASG` field
    // special callback and re-evaluates access rights for every
    // affected client on `dbPut record.ASG NEW_ASG`. Re-using the
    // existing `acf_reload_tx` broadcast is coarser than libca's
    // per-client dispatch but the downstream `oldaccess != access`
    // filter in `reeval_access_rights` keeps wire traffic bounded.
    //
    // the forwarder is spawned INTO the `accept_tasks`
    // JoinSet so it's cancelled together with the accept loops on
    // `run_tcp_listener` cancellation. Pre-fix Rust did
    // `tokio::spawn(...)` and dropped the JoinHandle, leaving the
    // task running forever (its `recv()` loop only exits on
    // `RecvError::Closed`, which the process-lifetime `OnceLock`
    // Sender can never raise). Long-running processes that restart
    // their CA server (test fixtures, fault-tolerant supervisors)
    // accumulated one zombie forwarder per restart cycle, each
    // holding a stale `acf_reload_tx_t` clone.
    {
        let mut asg_rx = epics_base_rs::server::access_security::subscribe_asg_changes();
        let acf_reload_tx_t = acf_reload_tx.clone();
        accept_tasks.spawn(async move {
            loop {
                match asg_rx.recv().await {
                    Ok(()) => {
                        let _ = acf_reload_tx_t.send(());
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        // Coalesce lagged events into one re-eval; the
                        // downstream `oldaccess != access` filter
                        // makes a single re-eval sufficient.
                        tracing::debug!(
                            target: "epics_ca_rs::server::tcp",
                            lagged = n,
                            "ASG-change notifier lagged — issuing one coalesced re-eval"
                        );
                        let _ = acf_reload_tx_t.send(());
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            Ok(())
        });
    }

    for (listener, intf) in listeners {
        let db_t = db.clone();
        let acf_t = acf.clone();
        let acf_reload_tx_t = acf_reload_tx.clone();
        let conn_events_t = conn_events.clone();
        let audit_t = audit.clone();
        let drain_t = drain.clone();
        let stats_t = stats.clone();
        #[cfg(feature = "experimental-rust-tls")]
        let tls_t = tls.clone();
        #[cfg(feature = "cap-tokens")]
        let cap_token_verifier_t = cap_token_verifier.clone();
        accept_tasks.spawn(async move {
            accept_loop(
                listener,
                intf,
                actual_port,
                db_t,
                acf_t,
                acf_reload_tx_t,
                conn_events_t,
                audit_t,
                drain_t,
                stats_t,
                #[cfg(feature = "experimental-rust-tls")]
                tls_t,
                #[cfg(feature = "cap-tokens")]
                cap_token_verifier_t,
            )
            .await
        });
    }

    // Wait for the first error (or for all loops to exit cleanly via
    // drain). On error, JoinSet::Drop aborts the surviving loops.
    while let Some(res) = accept_tasks.join_next().await {
        match res {
            Ok(Ok(())) => continue,
            Ok(Err(e)) => return Err(e),
            Err(join_err) if join_err.is_cancelled() => continue,
            Err(join_err) => {
                return Err(epics_base_rs::error::CaError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    join_err.to_string(),
                )));
            }
        }
    }
    Ok(())
}

/// Per-interface accept loop. Owned by `run_tcp_listener` via a
/// `JoinSet` — one task per `EPICS_CAS_INTF_ADDR_LIST` entry. Drains
/// when the shared `drain` flag is set; otherwise spawns a
/// `handle_client` task into the local `conn_tasks` `JoinSet` per
/// accepted connection.
///
/// `intf` is the bound interface IP; recorded on accept-error logs so
/// multi-NIC hosts can tell which listener saw the failure. The
/// `actual_port` parameter is the TCP port shared across all
/// listeners (decided in `bind_tcp_listeners`).
#[cfg(not(epics_embedded_target))]
#[allow(clippy::too_many_arguments)]
async fn accept_loop(
    listener: Arc<TcpListener>,
    intf: std::net::Ipv4Addr,
    actual_port: u16,
    db: Arc<PvDatabase>,
    acf: epics_base_rs::server::access_security::AcfCell,
    acf_reload_tx: broadcast::Sender<()>,
    conn_events: Option<broadcast::Sender<ServerConnectionEvent>>,
    audit: Option<crate::audit::AuditLogger>,
    drain: Arc<std::sync::atomic::AtomicBool>,
    stats: Option<Arc<super::stats::ServerStats>>,
    #[cfg(feature = "experimental-rust-tls")] tls: Option<
        Arc<std::sync::RwLock<Arc<tokio_rustls::rustls::ServerConfig>>>,
    >,
    #[cfg(feature = "cap-tokens")] cap_token_verifier: Option<Arc<crate::cap_token::TokenVerifier>>,
) -> CaResult<()> {
    // track per-connection tasks in a JoinSet so they're
    // aborted as a unit when this accept-loop future is dropped (e.g.
    // CaServer shutdown via tcp_abort.abort()). Without this, every
    // per-conn task ran detached and lingered until its internal
    // idle/op timeout. The select! arm on `conn_tasks.join_next()`
    // also reaps completed tasks so the set doesn't accumulate
    // finished JoinHandles.
    let mut conn_tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
    let mut backoff = AcceptBackoff::new();

    loop {
        // Drain mode: stop accepting new connections. Existing
        // connections continue to be served by their own tasks; the
        // CaServer::run() loop coordinates the grace period and the
        // ultimate exit.
        if drain.load(std::sync::atomic::Ordering::Acquire) {
            tracing::info!(intf = %intf, "TCP listener: drain mode set, exiting accept loop");
            return Ok(());
        }
        let (stream, peer) = tokio::select! {
            biased;
            // Was `res?`. A single transient failure — `ECONNABORTED` from a
            // client that resets between SYN and accept is routine on a busy
            // network — returned from the whole loop, and this interface
            // stopped accepting CA circuits for the life of the server. Same
            // primitive as the three other accept loops.
            res = listener.accept() => match res {
                Ok(accepted) => {
                    backoff.accepted();
                    accepted
                }
                Err(e) => {
                    tracing::warn!(intf = %intf, error = %e, "CA accept failed");
                    tokio::time::sleep(backoff.failed()).await;
                    continue;
                }
            },
            // Drain finished connection tasks. Returns None when the
            // set is empty — that branch resolves immediately, but
            // `biased` makes the listener arm preferred so we never
            // starve incoming accepts.
            Some(_) = conn_tasks.join_next() => continue,
        };
        // Reap finished connection tasks promptly. The select! arm on
        // `conn_tasks.join_next()` only fires when `listener.accept()`
        // is Pending, but `biased` makes the accept arm strictly
        // preferred — so under a sustained connect storm completed
        // `JoinHandle`s would accumulate in the set unbounded. A
        // non-blocking `try_join_next` drain after every accept caps
        // the set at the count of genuinely in-flight connections.
        while conn_tasks.try_join_next().is_some() {}
        if drain.load(std::sync::atomic::Ordering::Acquire) {
            tracing::info!(peer = %peer, "drain mode: rejecting new connection");
            drop(stream);
            continue;
        }
        tracing::info!(peer = %peer, intf = %intf, "CA client connected");
        metrics::counter!("ca_server_accepts_total").increment(1);
        metrics::gauge!("ca_server_clients_active").increment(1.0);
        let db = db.clone();
        let acf = acf.clone();
        if let Some(tx) = &conn_events {
            let _ = tx.send(ServerConnectionEvent::Connected(peer));
        }
        let conn_events = conn_events.clone();
        let acf_reload_rx = acf_reload_tx.subscribe();
        let audit = audit.clone();
        let stats_for_client = stats.clone();
        // Read the latest server config under the RwLock so a
        // concurrent reload_tls() takes effect for the *next* accept
        // without restarting the listener. Cheap read lock — only
        // contended against rare reload write locks.
        #[cfg(feature = "experimental-rust-tls")]
        let tls_acceptor = tls.as_ref().and_then(|slot| {
            slot.read()
                .ok()
                .map(|guard| tokio_rustls::TlsAcceptor::from(guard.clone()))
        });

        // Enable OS-level TCP keepalive on accepted socket so half-open
        // connections (e.g. NAT timeout, gateway down) are detected within
        // ~30s. Mirrors client-side keepalive in client/transport.rs.
        {
            let sock = socket2::SockRef::from(&stream);
            let keepalive = socket2::TcpKeepalive::new()
                .with_time(Duration::from_secs(15))
                .with_interval(Duration::from_secs(5));
            let _ = sock.set_keepalive(true);
            let _ = sock.set_tcp_keepalive(&keepalive);
            // SO_SNDTIMEO is set as a defence-in-depth — this port's own
            // 5 s, not C's; rsrv sets no send timeout anywhere (see
            // `send_timeout`) — but on a non-blocking tokio socket the
            // kernel does NOT
            // apply it — a stuck client where the kernel send buffer
            // fills would still leave `poll_write` Pending forever.
            //
            // This used to name the `tokio::time::timeout` wrapping
            // `dispatch_message` as "the actual stall guard". It is not one:
            // `dispatch_message` takes `&Outbox` and cannot touch the socket,
            // so no socket stall can make it late. Every server write now
            // happens in `drain_and_flush` at the bottom of the read loop,
            // which is not wrapped. **The hosted driver therefore has no stall
            // guard**, which happens to match the blocking driver and C — both
            // block in the write for as long as the peer takes. See
            // `doc/ca-stuck-reader-measurement.md`; whether to keep that or
            // restore a bound is an open decision, not an oversight to patch
            // silently.
            let _ = sock.set_write_timeout(Some(send_timeout()));
        }
        let _ = stream.set_nodelay(true);

        #[cfg(feature = "cap-tokens")]
        let cap_token_verifier_for_client = cap_token_verifier.clone();
        conn_tasks.spawn(async move {
            // TLS dispatch: when configured, wrap the accepted TCP
            // stream in a TlsAcceptor handshake. The client cert (if
            // any) is harvested afterwards for mTLS identity.
            let result: CaResult<()> = {
                #[cfg(feature = "experimental-rust-tls")]
                {
                    if let Some(acceptor) = tls_acceptor {
                        // cap the TLS handshake. A peer that
                        // completes TCP but stalls during ClientHello
                        // would otherwise hold a connection slot until
                        // OS keepalive reaps it (~30s).
                        let hs =
                            tokio::time::timeout(tls_handshake_timeout(), acceptor.accept(stream))
                                .await;
                        match hs {
                            Err(_) => {
                                tracing::warn!(peer = %peer,
                                    timeout = ?tls_handshake_timeout(),
                                    "TLS handshake timed out");
                                Err(epics_base_rs::error::CaError::Protocol(
                                    "TLS handshake timeout".into(),
                                ))
                            }
                            Ok(Ok(tls_stream)) => {
                                // Extract verified peer identity + issuer
                                // from the client certificate, if presented.
                                let leaf_cert = tls_stream
                                    .get_ref()
                                    .1
                                    .peer_certificates()
                                    .and_then(|chain| chain.first().cloned());
                                let (identity, authority) = leaf_cert
                                    .as_ref()
                                    .map(|cert| {
                                        (
                                            crate::tls::identity_from_cert(cert),
                                            crate::tls::issuer_from_cert(cert),
                                        )
                                    })
                                    .map(|(id, auth)| (Some(id), auth))
                                    .unwrap_or((None, None));
                                // TLS channel binding: SHA-256 of the
                                // peer's leaf certificate DER. Threaded
                                // into `handle_client` so cap-token
                                // verification is bound to this circuit.
                                #[cfg(feature = "cap-tokens")]
                                let tls_channel_binding = leaf_cert.as_ref().map(|cert| {
                                    crate::cap_token::ChannelBinding::from_peer_cert_der(
                                        cert.as_ref(),
                                    )
                                });
                                if let Some(ref id) = identity {
                                    tracing::info!(
                                        peer = %peer,
                                        identity = %id,
                                        authority = authority.as_deref().unwrap_or("<none>"),
                                        "mTLS identity verified"
                                    );
                                }
                                handle_client(
                                    tls_stream,
                                    peer,
                                    db,
                                    acf,
                                    acf_reload_rx,
                                    actual_port,
                                    identity,
                                    authority,
                                    audit,
                                    conn_events.clone(),
                                    stats_for_client.clone(),
                                    #[cfg(feature = "cap-tokens")]
                                    cap_token_verifier_for_client.clone(),
                                    #[cfg(feature = "cap-tokens")]
                                    tls_channel_binding,
                                )
                                .await
                            }
                            Ok(Err(e)) => {
                                tracing::warn!(peer = %peer, error = %e,
                                    "TLS handshake failed");
                                Err(epics_base_rs::error::CaError::Io(e))
                            }
                        }
                    } else {
                        handle_client(
                            stream,
                            peer,
                            db,
                            acf,
                            acf_reload_rx,
                            actual_port,
                            None,
                            None,
                            audit,
                            conn_events.clone(),
                            stats_for_client.clone(),
                            #[cfg(feature = "cap-tokens")]
                            cap_token_verifier_for_client.clone(),
                            // Plaintext circuit: no channel binding.
                            #[cfg(feature = "cap-tokens")]
                            None,
                        )
                        .await
                    }
                }
                #[cfg(not(feature = "experimental-rust-tls"))]
                {
                    handle_client(
                        stream,
                        peer,
                        db,
                        acf,
                        acf_reload_rx,
                        actual_port,
                        None,
                        None,
                        audit,
                        conn_events.clone(),
                        stats_for_client.clone(),
                        #[cfg(feature = "cap-tokens")]
                        cap_token_verifier_for_client.clone(),
                        // No TLS compiled in: never a channel binding.
                        #[cfg(feature = "cap-tokens")]
                        None,
                    )
                    .await
                }
            };
            if let Some(tx) = &conn_events {
                let _ = tx.send(ServerConnectionEvent::Disconnected(peer));
            }
            metrics::gauge!("ca_server_clients_active").decrement(1.0);
            metrics::counter!("ca_server_disconnects_total").increment(1);
            if let Err(e) = result {
                // Suppress normal disconnection errors (client closed connection)
                let is_disconnect = matches!(
                    e,
                    epics_base_rs::error::CaError::Io(ref io) if is_peer_disconnect(io.kind())
                );
                if is_disconnect {
                    tracing::debug!(peer = %peer, "client disconnected");
                } else {
                    tracing::warn!(peer = %peer, error = %e, "client handler error");
                }
            } else {
                tracing::debug!(peer = %peer, "client disconnected cleanly");
            }
        });
    }
}

/// A socket error that means the peer went away (closed or reset the
/// connection) rather than a genuine server-side failure.
///
/// Single owner of the peer-vanished classification: the client loop's
/// socket I/O break sites and the accept-loop logger must all consult
/// this predicate, so a client that disappears mid-write is uniformly a
/// *disconnect* (loop result `Ok`, audit reason "ok"), never a server
/// error. RSRV behaves the same way — a send failure terminates
/// `camsgtask` as a client disconnect, not an IOC error. Before the
/// outbox migration the spawned monitor tasks swallowed their own
/// EPIPEs; with every write centralized in the client loop, the loop
/// itself must classify them.
pub(crate) fn is_peer_disconnect(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::UnexpectedEof
    )
}

/// Drain every frame currently queued in the connection outbox into the
/// socket buffer (in arrival order) and flush once. This is the single-owner
/// drain that replaces every former out-of-band `writer.lock().await` write.
/// Returns the number of wire bytes written, for `ServerStats::bytes_out`.
///
/// It is *not* the only place server bytes reach the socket, and the comment
/// that used to say so was wrong: `handle_client` writes the unsolicited
/// `CA_PROTO_VERSION` greeting directly before the loop starts, and the
/// out-of-band monitor arm writes its first frame directly before draining the
/// rest. What is genuinely single-owner is the *policy* those three share —
/// `sock`'s innermost writer is a [`crate::server::send::RetryTransientAsync`],
/// so all three inherit C's transient-failure handling without a branch.
///
/// Batching is preserved: a whole dispatch burst (or a run of monitor
/// frames) is written back-to-back before the single `flush`, so N framed
/// replies still collapse to one TCP write.
#[cfg(not(epics_embedded_target))]
async fn drain_and_flush<W: AsyncWrite + Unpin>(
    sock: &mut BufWriter<W>,
    drain: &mut OutboxDrain,
) -> std::io::Result<u64> {
    let mut total = 0u64;
    while let Some(frame) = drain.try_next() {
        sock.write_all(&frame).await?;
        total += frame.len() as u64;
    }
    sock.flush().await?;
    Ok(total)
}

/// Handle one CA client over the supplied stream.
///
/// `initial_hostname` is the verified peer identity from the TLS
/// handshake (mTLS only). When `Some`, it takes precedence over
/// `peer.ip()` for the `state.hostname` ACF key — the
/// cryptographically authenticated identity is always more
/// trustworthy than the network address.
#[cfg(not(epics_embedded_target))]
#[allow(clippy::too_many_arguments)]
async fn handle_client<S>(
    stream: S,
    peer: SocketAddr,
    db: Arc<PvDatabase>,
    acf: epics_base_rs::server::access_security::AcfCell,
    mut acf_reload_rx: broadcast::Receiver<()>,
    tcp_port: u16,
    initial_hostname: Option<String>,
    // PR #641 — mTLS issuer DN of the peer's cert. `Some(...)` only
    // when the peer was authenticated via mTLS; gets paired with
    // `auth_method = "x509"` on the ClientState so ACF
    // METHOD()/AUTHORITY() rules can gate by issuer.
    tls_authority: Option<String>,
    audit: Option<crate::audit::AuditLogger>,
    conn_events: Option<broadcast::Sender<ServerConnectionEvent>>,
    // PR #592 dbServerStats: bytes_in/bytes_out counters. Incremented
    // post-read (per accepted UDP/TCP buffer) and at each BufWriter
    // flush (by inspecting `BufWriter::buffer().len()` before flush).
    // `None` skips all counter bookkeeping — used by the unit-test
    // dispatch fixtures that don't spin up a full CaServer.
    stats: Option<Arc<super::stats::ServerStats>>,
    #[cfg(feature = "cap-tokens")] cap_token_verifier: Option<Arc<crate::cap_token::TokenVerifier>>,
    // TLS channel binding (SHA-256 of the peer's leaf cert DER),
    // computed at the mTLS accept site. `None` for plaintext peers —
    // a cap-token presented on a `None` circuit is rejected by
    // `TokenVerifier::verify`, so the token is cryptographically
    // bound to the TLS channel it was issued for.
    #[cfg(feature = "cap-tokens")] tls_channel_binding: Option<crate::cap_token::ChannelBinding>,
) -> CaResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (reader, write_half) = tokio::io::split(stream);
    // The connection loop is the SOLE owner of the socket writer — no
    // `Arc<Mutex<..>>`, so no other task can write the socket. Bigger
    // BufWriter so a 100-PV batched response burst (~3 KB) fits without
    // auto-flushing mid-batch. The dispatch hot-path no longer writes the
    // socket at all: handlers push framed bytes into `outbox`, and this
    // loop drains that outbox into `sock` and flushes once per outer read
    // iteration — turning N small TCP writes into one. Default 8 KB was hit
    // at ~330 responses; 64 KB covers the common bulk_caget(100) case with
    // headroom for follow-on monitor events queued in the same tick.
    // The write half is wrapped before it is buffered, so `BufWriter`'s spill,
    // `write_all`, and `flush` all sit above C's transient-failure policy: an
    // `ENOBUFS` burst parks this circuit for 15 s and retries, where the bare
    // `write_all` used to return `Err` and `?` used to disconnect the client.
    // See `crate::server::send`.
    let mut sock = BufWriter::with_capacity(
        64 * 1024,
        crate::server::send::RetryTransientAsync::new(write_half),
    );
    // The single per-connection outbox. `outbox` is the cloneable producer
    // handle every emit site holds (dispatch handlers in this task, plus
    // the spawned monitor / put-notify tasks); `outbox_drain` is owned only
    // here. See `super::outbox` for the invariant this establishes.
    let (outbox, mut outbox_drain) = crate::server::outbox::channel();
    let mut state = ClientState::new(acf, tcp_port, db.clone());
    state.stats = stats.clone();
    #[cfg(feature = "cap-tokens")]
    {
        state.cap_token_verifier = cap_token_verifier;
        state.tls_channel_binding = tls_channel_binding;
    }
    // The circuit's ACF host identity + mTLS auth context are decided once,
    // here — C decides them in `create_tcp_client`
    // (`caservertask.c:1425-1437`) for the same reason. Shared with the
    // blocking thread-per-client driver (`crate::server::blocking`) so both
    // server front-ends derive identity identically. See `HostIdentity`.
    state.apply_connection_identity(peer, initial_hostname, tls_authority);
    state.audit = audit;
    let rl_cfg = crate::server::rate_limit::RateLimitConfig::from_env();
    state.rate_limiter = rl_cfg.build();
    state.rate_limit_strike_threshold = rl_cfg.strike_threshold;
    state.audit("connect", "", "", "ok");

    // C `rsrv/caservertask.c::create_tcp_client:1525` calls
    // `rsrv_version_reply(client)` immediately after `db_start_events`,
    // so the server's first wire frame on any new TCP connection is
    // an unsolicited `CA_PROTO_VERSION` (cmmd=0, count=
    // CA_MINOR_PROTOCOL_REVISION, all other fields zero). libca's
    // `tcpRecvWatchdog::messageArrivalNotify` uses every received
    // frame as a liveness beat; without this, the server's first byte
    // is delayed until the client sends its own CA_PROTO_VERSION,
    // which can drift slow handshakes toward CA_ECHO_TIMEOUT. Also
    // restores wire-trace parity with rsrv (the first byte from the
    // server matches).
    {
        // Pre-loop, single-task: the owner writes the unsolicited VERSION
        // greeting directly to its own socket buffer.
        let mut hdr = CaHeader::new(CA_PROTO_VERSION);
        hdr.count = CA_MINOR_VERSION;
        sock.write_all(&hdr.to_bytes()).await?;
        sock.flush().await?;
    }

    let mut reader = reader;

    let mut buf = vec![0u8; 8192];
    let mut accumulated = RecvAccumulator::new();
    let inactivity = inactivity_timeout();

    // CRITICAL: every exit path from the read loop — graceful EOF
    // (`break`), propagated I/O / protocol error, rate-limit disconnect,
    // send-timeout disconnect — MUST pass through the single teardown
    // block below (subscription cancel, write-notify abort,
    // SubscriptionClosed / ChannelCleared emission). Previously the
    // in-loop `return Ok(())` / `return Err(..)` sites bypassed the
    // teardown, leaking write-notify tasks and inflating consumer
    // refcounts permanently after any non-graceful disconnect.
    //
    // The loop is wrapped in a labeled block: in-loop exits use
    // `break 'client_loop <CaResult>` so control always reaches the
    // teardown, and the captured result is returned only afterwards.
    //
    // `disconnect_reason` carries the specific cause (rate_limited /
    // send_timeout / error / ok) to the single post-teardown audit
    // call — replacing the per-path `state.audit("disconnect", ..)`
    // calls that previously had to live next to each `return`.
    let mut disconnect_reason: &str = "ok";
    let loop_result: CaResult<()> = 'client_loop: {
        loop {
            // Bound read with inactivity timeout so a fully-silent half-open
            // connection eventually gets cleaned up even if OS keepalive failed.
            // Race the read against ACF reload notifications so a `reload_acf*()`
            // call promptly re-pushes CA_PROTO_ACCESS_RIGHTS for every open
            // channel — RSRV's `sendAllUpdateAS` analog.
            let n = tokio::select! {
                biased;
                reload = acf_reload_rx.recv() => {
                    match reload {
                        Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            // Lagged is fine — even one missed notification still
                            // means "rules changed", so we always recompute. A
                            // re-push failure must still pass through teardown.
                            if let Err(e) = reeval_access_rights(&mut state, &outbox).await {
                                break 'client_loop Err(e);
                            }
                            // reeval pushed the ACCESS_RIGHTS / denial frames into
                            // the outbox; drain them to the socket now so the
                            // re-evaluation reaches the client promptly (RSRV
                            // flushes these immediately).
                            if let Err(e) = drain_and_flush(&mut sock, &mut outbox_drain).await {
                                if is_peer_disconnect(e.kind()) {
                                    break 'client_loop Ok(());
                                }
                                break 'client_loop Err(e.into());
                            }
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            // Sender dropped — the server is going away.
                            break 'client_loop Ok(());
                        }
                    }
                }
                read = read_with_optional_timeout(&mut reader, &mut buf, inactivity) => {
                    match read {
                        Ok(Ok(n)) => n,
                        Ok(Err(e)) if is_peer_disconnect(e.kind()) => break 'client_loop Ok(()),
                        Ok(Err(e)) => break 'client_loop Err(e.into()),
                        Err(idle) => {
                            // Inactivity timeout — close the connection.
                            // Disabled by default (matches C rsrv); fires only
                            // when EPICS_CAS_INACTIVITY_TMO is set explicitly.
                            tracing::warn!(
                                target: "epics_ca_rs::server",
                                peer = %state.peer,
                                idle_secs = idle.as_secs(),
                                "CA server: client idle, closing"
                            );
                            break 'client_loop Ok(());
                        }
                    }
                }
                // Out-of-band frame produced by a spawned monitor / put-notify
                // task while this loop was idle on the socket read. This arm is
                // last (biased) so socket reads keep priority; under a steady
                // read stream these frames still flush via the per-burst drain
                // at the bottom of the loop, so they are never starved.
                frame = outbox_drain.recv() => {
                    match frame {
                        Some(frame) => {
                            let first = frame.len() as u64;
                            if let Err(e) = sock.write_all(&frame).await {
                                if is_peer_disconnect(e.kind()) {
                                    break 'client_loop Ok(());
                                }
                                break 'client_loop Err(e.into());
                            }
                            match drain_and_flush(&mut sock, &mut outbox_drain).await {
                                Ok(rest) => {
                                    if let Some(ref s) = stats {
                                        s.bytes_out.fetch_add(
                                            first + rest,
                                            std::sync::atomic::Ordering::Relaxed,
                                        );
                                    }
                                }
                                Err(e) if is_peer_disconnect(e.kind()) => {
                                    break 'client_loop Ok(());
                                }
                                Err(e) => break 'client_loop Err(e.into()),
                            }
                            continue;
                        }
                        // Unreachable while this loop still holds `outbox`; a
                        // `None` would mean every producer handle was dropped.
                        None => break 'client_loop Ok(()),
                    }
                }
            };
            if n == 0 {
                break 'client_loop Ok(());
            }

            // PR #592 dbServerStats: bytes_in mirrors RSRV's
            // `caServerBytes_in`. Counted on every successful read of `n`
            // wire bytes, regardless of whether the inner dispatch
            // accepts or rejects the message.
            if let Some(ref s) = stats {
                s.bytes_in
                    .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
            }

            // Chaos: optional stall + simulated read drop. Compiles to a
            // single branch when EPICS_CA_RS_CHAOS is unset.
            if crate::chaos::enabled() {
                crate::chaos::maybe_stall().await;
                if crate::chaos::should_drop_read() {
                    continue;
                }
            }

            // The single growth point. `accept` runs C's drain preamble
            // (`camessage.c:2428-2439` — bytes owed to an already-refused
            // message are discarded before any header parsing) and enforces
            // the accumulation ceiling before appending.
            match accumulated.accept(&buf[..n]) {
                Admit::Parse => {}
                Admit::Draining => continue,
                Admit::Overflow(cap) => {
                    tracing::warn!(
                        peer = %state.peer,
                        cap,
                        "CA server: client accumulated buffer exceeded the ceiling, closing"
                    );
                    break 'client_loop Ok(());
                }
            }

            // Every protocol gate between "a header parsed" and "dispatch
            // this message" belongs to `RecvAccumulator::next_message`, which
            // the blocking driver's loop calls too. Neither loop can see the
            // individual tests, reorder them, skip one, or move the parse
            // cursor itself — which is what stopped these two loops carrying
            // two hand-maintained lists that drift.
            let mut parsed_any = false;
            loop {
                let (hdr, payload) = match accumulated.next_message(state.client_minor_version) {
                    Gate::NeedMore => break,
                    Gate::Deliver { hdr, payload } => (hdr, payload),
                    Gate::Refuse(err) => {
                        // C `RSRV_OK`: the peer keeps every channel and
                        // subscription it holds; only this message is lost.
                        parsed_any = true;
                        tracing::warn!(
                            peer = %state.peer,
                            cmmd = err.hdr.cmmd,
                            status = err.status,
                            diagnostic = %err.diagnostic,
                            "CAS: message refused, circuit kept"
                        );
                        let _ = send_ca_error(
                            &outbox,
                            &err.hdr,
                            err.status,
                            0xFFFF_FFFF,
                            &err.diagnostic,
                            state.client_minor_version,
                        );
                        let _ = drain_and_flush(&mut sock, &mut outbox_drain).await;
                        continue;
                    }
                    Gate::TearDown { error, reason } => {
                        // C `RSRV_ERROR`: send what C sends, then close.
                        tracing::warn!(
                            peer = %state.peer,
                            reason = %reason,
                            "CAS: circuit torn down by the receive gate"
                        );
                        if let Some(err) = error {
                            let _ = send_ca_error(
                                &outbox,
                                &err.hdr,
                                err.status,
                                0xFFFF_FFFF,
                                &err.diagnostic,
                                state.client_minor_version,
                            );
                        }
                        let _ = drain_and_flush(&mut sock, &mut outbox_drain).await;
                        break 'client_loop Err(reason);
                    }
                };
                parsed_any = true;

                // Rate-limit gate: drop messages when the bucket is empty;
                // disconnect the client once it accumulates enough strikes.
                if let Some(ref limiter) = state.rate_limiter {
                    if limiter.try_acquire().is_err() {
                        metrics::counter!("ca_server_rate_limit_drops_total").increment(1);
                        state.rate_limit_strikes = state.rate_limit_strikes.saturating_add(1);
                        if state.rate_limit_strike_threshold > 0
                            && state.rate_limit_strikes >= state.rate_limit_strike_threshold
                        {
                            tracing::warn!(peer = %state.peer, strikes = state.rate_limit_strikes,
                            "rate limit exceeded; closing connection");
                            metrics::counter!("ca_server_rate_limit_disconnects_total")
                                .increment(1);
                            disconnect_reason = "rate_limited";
                            break 'client_loop Ok(());
                        }
                        // The cursor is already past this message — the
                        // gate advanced it when it handed the message over.
                        continue;
                    } else if state.rate_limit_strikes > 0 {
                        state.rate_limit_strikes = 0;
                    }
                }

                // Bound how long one message may spend in dispatch. On
                // timeout we drop the connection; any in-flight reply is
                // discarded.
                //
                // This does NOT bound a stuck reader, though it was written
                // to and still carried that claim until 2026-08-03.
                // `dispatch_message` takes `&Outbox` — an unbounded channel —
                // and never writes the socket, so a peer that stops reading
                // cannot make it late. The batch-flush refactor moved every
                // write to the unwrapped `drain_and_flush` below. What is
                // left bounded here is dispatch's own work: a handler that
                // blocks on the database or a link.
                match tokio::time::timeout(
                    send_timeout(),
                    dispatch_message(
                        &hdr,
                        &payload,
                        &mut state,
                        &db,
                        &outbox,
                        peer,
                        conn_events.as_ref(),
                    ),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        // Regression defence: dispatch_message no longer
                        // flushes per response (batched at the bottom of
                        // this outer loop). On a propagated dispatch
                        // error, exit-via-`?` would drop the BufWriter
                        // before the outer flush fires, so any responses
                        // queued by earlier successful handlers in this
                        // batch — or by an error-path `send_cmd_error`
                        // call inside the failing handler — would be
                        // lost. Best-effort flush before propagating so
                        // the client sees them; ignore errors here
                        // because the underlying TCP is most likely
                        // already broken (which is why dispatch failed).
                        let _ = drain_and_flush(&mut sock, &mut outbox_drain).await;
                        break 'client_loop Err(e);
                    }
                    Err(_) => {
                        // send_timeout fires — dispatch_message future is
                        // cancelled mid-flight. BufWriter may hold a
                        // partial frame (e.g., header without payload if
                        // cancellation landed between the two write_alls
                        // of a READ_NOTIFY response). Flushing here would
                        // ship the orphan header to the client and leave
                        // it parsing an incomplete frame, so we skip the
                        // flush and let BufWriter drop discard the
                        // partial bytes — same behaviour as before the
                        // batch-flush refactor.
                        tracing::warn!(
                            peer = %peer,
                            "CA server: dispatch send-timeout (stuck client?), closing"
                        );
                        disconnect_reason = "send_timeout";
                        break 'client_loop Ok(());
                    }
                }
            }

            if parsed_any {
                // Batched flush: dispatch_message pushed all responses for
                // this read iteration into the outbox without touching the
                // socket. Drain them into the BufWriter and flush once now so
                // the kernel sees a single TCP write per inbound burst. Cuts
                // e2e_bulk_get_many(100) from ~225µs → batched single write
                // (server-side throughput floor was ~2.2µs/PV due to
                // per-message flush; this collapses it to one syscall).
                //
                // The drain also picks up any monitor / put-notify frames
                // pushed by spawned tasks during this dispatch burst, so those
                // ride out on the same syscall.
                //
                // Errors here mean the TCP write stalled / peer closed —
                // surface as the read loop's normal disconnect path.
                //
                // PR #592 dbServerStats: bytes_out mirrors RSRV's
                // `caServerBytes_out`. `drain_and_flush` returns the exact
                // wire-byte count that left on this syscall. CA-over-TLS counts
                // post-decrypt plaintext since the rustls layer wraps the
                // BufWriter externally — matches what the comment on
                // ServerStats::bytes_out already documents.
                match drain_and_flush(&mut sock, &mut outbox_drain).await {
                    Ok(pending_out) => {
                        if let Some(ref s) = stats {
                            s.bytes_out
                                .fetch_add(pending_out, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    Err(e) if is_peer_disconnect(e.kind()) => break 'client_loop Ok(()),
                    Err(e) => break 'client_loop Err(e.into()),
                }
            }
        }
    };

    // Cleanup: cancel all subscriptions. PR #592 dbServerStats —
    // emit `SubscriptionClosed` for each so the running close-count
    // matches the open-count when a client disconnects without
    // explicit EVENT_CANCEL (TCP RST, network drop, panic). Without
    // this, `active_subscriptions` reports a permanent leak after
    // every ungraceful disconnect.
    let pending_subs: Vec<SubscriptionEntry> =
        state.subscriptions.drain().map(|(_, sub)| sub).collect();
    for sub in pending_subs {
        sub.task.abort();
        match &sub.target {
            ChannelTarget::SimplePv(pv) => {
                pv.remove_subscriber(sub.sub_id);
            }
            ChannelTarget::RecordField { record, .. } => {
                record.write().remove_subscriber(sub.sub_id);
            }
        }
        if let Some(tx) = &conn_events {
            let pv_name = state
                .channels
                .get(&sub.channel_sid)
                .map(|e| e.pv_name.clone())
                .unwrap_or_default();
            let _ = tx.send(ServerConnectionEvent::SubscriptionClosed {
                peer,
                pv_name,
                sub_id: sub.sub_id,
            });
        }
    }

    // Abort any in-flight WRITE_NOTIFY completion tasks. A
    // stuck async record (motor hung, asyn device unresponsive) would
    // otherwise hold the spawned task and its captured `Outbox` handle
    // forever after the client disconnects.
    for (_sid, handle) in state.write_notify_tasks.drain(..) {
        handle.abort();
    }

    // Emit a `ChannelCleared` event for every channel still open at
    // disconnect time. Without this, a client that drops without
    // sending `CA_PROTO_CLEAR_CHANNEL` (TCP RST, network drop, panic)
    // leaks its channel refcount in any consumer that uses these
    // events for refcounting (e.g. ca_gateway's per-PV `Active` →
    // `Inactive` transition). Done here so the events fire BEFORE
    // the listener emits `Disconnected(peer)`, preserving the
    // ordering invariant "clears precede disconnect".
    if let Some(tx) = &conn_events {
        for (_sid, entry) in state.channels.drain() {
            let _ = tx.send(ServerConnectionEvent::ChannelCleared {
                peer,
                pv_name: entry.pv_name,
                cid: entry.cid,
            });
        }
    }

    // Audit with the outcome the loop exited on, then return that
    // outcome. The teardown above ran unconditionally regardless of
    // whether `loop_result` is Ok or Err. An Err exit that did not set
    // a more specific reason is reported as "error".
    if loop_result.is_err() && disconnect_reason == "ok" {
        disconnect_reason = "error";
    }
    state.audit("disconnect", "", "", disconnect_reason);
    loop_result
}

pub(crate) async fn dispatch_message(
    hdr: &CaHeader,
    payload: &[u8],
    state: &mut ClientState,
    db: &Arc<PvDatabase>,
    writer: &Outbox,
    peer: SocketAddr,
    conn_events: Option<&broadcast::Sender<ServerConnectionEvent>>,
) -> CaResult<()> {
    // The "client version too old" (ECA_DEFUNCT) gate is not here: it is
    // `RecvAccumulator::next_message`'s, shared with the blocking driver.
    // C runs it at `camessage.c:2489` — before the alignment test at 2520 —
    // and it needs the receive buffer's drain bookkeeping
    // (`client->recvBytesToDrain`) to swallow the rest of the rejected
    // message. Running it here would put it after alignment, giving a
    // pre-V44 peer the V44+ peer's ECA_INTERNAL disconnect for the same
    // bytes.

    match hdr.cmmd {
        CA_PROTO_VERSION => {
            // C `tcp_version_action` (camessage.c:371-374): rejects
            // clients whose minor version < CA_MINIMUM_SUPPORTED_VERSION
            // (=4) with RSRV_ERROR, which tears the connection down.
            // Without this gate, an ancient client could complete the
            // VERSION handshake and proceed to CREATE_CHAN with a
            // wire format we no longer fully support — silently
            // diverging from C IOC behaviour.
            if !declares_supported_version(hdr) {
                tracing::warn!(
                    peer = ?peer,
                    minor = hdr.count,
                    "CAS: Ignore version from unsupported client (minor < 4); dropping"
                );
                return Err(epics_base_rs::error::CaError::Protocol(format!(
                    "unsupported CA minor version {} (matches C tcp_version_action drop)",
                    hdr.count
                )));
            }
            // C `tcp_version_action` (`rsrv/camessage.c:371-373`) drops
            // the connection (`return RSRV_ERROR`) when the client's
            // requested priority (`m_dataType`) exceeds
            // `CA_PROTO_PRIORITY_MAX` (= 99u in `caProto.h:71`). The
            // priority drives the IOC's per-client epicsThread
            // scheduling-priority assignment downstream, so a value
            // outside the legal 0..=99 range is rejected hard rather
            // than silently clamped. Pre-fix Rust accepted any
            // priority and emitted the VERSION reply normally —
            // benign on the wire but diverges from libca's expected
            // close-on-bad-priority behaviour, which a strict CAC
            // peer would notice.
            const CA_PROTO_PRIORITY_MAX: u16 = 99;
            if hdr.data_type > CA_PROTO_PRIORITY_MAX {
                tracing::warn!(
                    peer = ?peer,
                    priority = hdr.data_type,
                    "CAS: VERSION with priority > CA_PROTO_PRIORITY_MAX; dropping"
                );
                return Err(epics_base_rs::error::CaError::Protocol(format!(
                    "VERSION priority {} > {} (matches C tcp_version_action drop)",
                    hdr.data_type, CA_PROTO_PRIORITY_MAX
                )));
            }
            state.client_minor_version = hdr.count;
            // C `rsrv_version_reply` (camessage.c:2178-2180) emits VERSION
            // with all fields zero except `m_count = CA_MINOR_PROTOCOL_REVISION`.
            // The previous Rust defaults (`data_type=1, cid=1`) drifted
            // from byte-exact parity — C clients only consult `m_count`
            // (`tcpiiu.cpp::versionRespNotify`) so it was harmless in
            // practice, but a strict peer or wire trace would diverge.
            let mut resp = CaHeader::new(CA_PROTO_VERSION);
            resp.count = CA_MINOR_VERSION;
            writer.push(resp.to_bytes().to_vec());
        }

        CA_PROTO_HOST_NAME => {
            // C `camessage.c::host_name_action` (line ~795 onward)
            // rejects HOST_NAME messages that arrive after the first
            // channel has been created — once the client claims any
            // channel, the host identity is fixed for the connection.
            // Reuse the same wire response: CA_PROTO_ERROR with
            // ECA_INTERNAL and a descriptive message.
            if !state.channels.is_empty() {
                send_ca_error(
                    writer,
                    hdr,
                    ECA_INTERNAL,
                    0xFFFF_FFFF,
                    "attempts to use protocol to set host name \
                     after creating first channel ignored by server",
                    state.client_minor_version,
                )?;
                return Ok(());
            }
            // C `camessage.c:855-856`: `size = strnlen(pName, m_postsize)
            // + 1; if (size > 512 || size > m_postsize) reject`.
            // The second condition rejects payloads with no null
            // terminator within m_postsize bytes (strnlen returns
            // m_postsize, then +1 overflows). Rust's
            // `position(|&b| b == 0)` returns Some(idx) for
            // terminated names and `None` (mapped to payload.len())
            // for unterminated ones — check explicitly so we don't
            // silently accept unterminated names that C would
            // reject as "very long".
            let null_pos = payload.iter().position(|&b| b == 0);
            let end = null_pos.unwrap_or(payload.len());
            if null_pos.is_none() || end >= 512 {
                // C `host_name_action` (camessage.c:854-867): a name
                // longer than 511 bytes is a protocol violation —
                // send_err + return RSRV_ERROR (disconnect). The
                // post-claim freeze branch above returns RSRV_OK
                // (recoverable misuse), but the size cap is a
                // wire-malformation reject.
                send_ca_error(
                    writer,
                    hdr,
                    ECA_INTERNAL,
                    0xFFFF_FFFF,
                    "bad (very long) host name",
                    state.client_minor_version,
                )?;
                return Err(epics_base_rs::error::CaError::Protocol(
                    "HOST_NAME exceeds 511-byte cap (matches C host_name_action RSRV_ERROR)".into(),
                ));
            }

            // C `host_name_action` (`camessage.c:845-875`) stores the
            // client-supplied name **unconditionally** — the whole of C's
            // gating is the `asCheckClientIP` early return above it
            // (`:839-843`), which this port models as a pinned identity.
            // So: hand the claim to `HostIdentity`, which takes it only if
            // the identity is not pinned to the peer address or an
            // mTLS-verified cert.
            //
            // This is C's trust model, quirk included: rsrv believes the
            // name and leaves verification to the operator, who is expected
            // to set `asCheckClientIP=1` on an untrusted network. A HOST()
            // rule in an .acf therefore grants exactly what it grants under
            // C. Pre-R7-16 the port defaulted to the peer IP behind a
            // fictitious `EPICS_CAS_USE_HOST_NAMES` knob (no such variable
            // exists anywhere in epics-base), so a `HOST(node)` HAG that
            // granted WRITE under C granted nothing here.
            let claimed = String::from_utf8_lossy(&payload[..end]).to_string();
            if state.hostname.claim(claimed.clone()) {
                // Re-evaluate access rights for all existing channels.
                reeval_access_rights(state, writer).await?;
            } else {
                tracing::debug!(
                    peer = %peer,
                    claimed_host = %claimed,
                    identity = state.hostname.as_str(),
                    "HOST_NAME ignored: identity is pinned (asCheckClientIP or mTLS)"
                );
                state.audit("host_name", "", &claimed, "ignored_pinned");
            }
        }

        CA_PROTO_CLIENT_NAME => {
            // C `camessage.c::client_name_action` rejects CLIENT_NAME
            // after the first channel has been created (line ~898).
            if !state.channels.is_empty() {
                send_ca_error(
                    writer,
                    hdr,
                    ECA_INTERNAL,
                    0xFFFF_FFFF,
                    "attempts to use protocol to set user name \
                     after creating first channel ignored by server",
                    state.client_minor_version,
                )?;
                return Ok(());
            }
            // C `camessage.c:942-943`: same 512-byte cap as host
            // name, AND same null-termination requirement. C
            // computes `size = strnlen(pName, m_postsize) + 1`
            // then rejects on `size > m_postsize`, which catches
            // names with no null terminator within m_postsize
            // bytes. Match by treating "no null found" as a
            // reject.
            let null_pos = payload.iter().position(|&b| b == 0);
            let end = null_pos.unwrap_or(payload.len());
            if null_pos.is_none() || end >= 512 {
                // C `client_name_action` (camessage.c:941-954): same
                // 511-byte cap as host_name; send_err + RSRV_ERROR
                // (disconnect). Post-claim freeze branch returns
                // RSRV_OK; size cap returns RSRV_ERROR.
                send_ca_error(
                    writer,
                    hdr,
                    ECA_INTERNAL,
                    0xFFFF_FFFF,
                    "a very long user name was specified",
                    state.client_minor_version,
                )?;
                return Err(epics_base_rs::error::CaError::Protocol(
                    "CLIENT_NAME exceeds 511-byte cap (matches C client_name_action RSRV_ERROR)"
                        .into(),
                ));
            }
            let raw = String::from_utf8_lossy(&payload[..end]).to_string();
            // When a capability-token verifier is configured AND the
            // payload arrives in `cap:<token>` form, verify the token
            // and store the resolved subject. Unverifiable tokens are
            // logged and replaced with a fixed `unverified` sentinel
            // that ACF rules can deliberately deny. Plain (non-`cap:`)
            // usernames pass through unchanged for backwards compat.
            #[cfg(feature = "cap-tokens")]
            {
                // M1: TokenVerifier::verify expects the full `cap:`-
                // prefixed form (it strips the prefix internally).
                // The previous double-strip yielded MissingPrefix on
                // every well-formed token; cap-tokens was non-
                // functional whenever a verifier was configured.
                state.username = match (&state.cap_token_verifier, raw.starts_with("cap:")) {
                    (Some(v), true) => match v.verify(&raw, state.tls_channel_binding.as_ref()) {
                        Ok(claims) => {
                            tracing::debug!(peer = %state.peer, sub = %claims.sub,
                                "cap-token verified");
                            // propagate auth_method / authority so
                            // ACF rules of the form
                            // `RULE(1, WRITE) { METHOD("cap-token")
                            //                   AUTHORITY("ops-issuer-1") }`
                            // can scope by authenticator subsystem and
                            // issuer key id. Pre-fix only `state.username
                            // = claims.sub` was set, leaving auth_method
                            // empty (or `"x509"` if mTLS is also active),
                            // so cap-token METHOD/AUTHORITY clauses
                            // could not match a verified token.
                            state.auth_method = "cap-token".to_string();
                            state.auth_authority = claims.iss.clone();
                            claims.sub
                        }
                        Err(e) => {
                            // Do NOT fold the raw token into the username:
                            // it then lands in the ACF identity and the
                            // audit log. A structurally valid but rejected
                            // token (aud/binding/expiry mismatch) is a real
                            // bearer credential, and a garbage token is
                            // attacker-controlled bytes — neither belongs
                            // there. A fixed sentinel is enough for ACF to
                            // deny; the reason is in the warn log.
                            tracing::warn!(peer = %state.peer, error = %e,
                                "cap-token verification failed");
                            "unverified".to_string()
                        }
                    },
                    _ => raw,
                };
            }
            #[cfg(not(feature = "cap-tokens"))]
            {
                state.username = raw;
            }
            // Re-evaluate access rights for all existing channels
            reeval_access_rights(state, writer).await?;
        }

        CA_PROTO_CREATE_CHAN => {
            // Pre-CA-4.4 clients send claims with no PV name (postsize=0).
            // Silently ignore these, matching C server behavior (camessage.c:1235-1239).
            // The client will retry with v4.4+ format after receiving our VERSION.
            if hdr.actual_postsize() <= 1 {
                return Ok(());
            }
            // C `rsrv/camessage.c:1227` `claim_ciu_action` writes
            // `client->minor_version_number = mp->m_available;`
            // UNCONDITIONALLY, then rejects the channel outright if the
            // result is below 4.4 (`if (!CA_V44(...)) return RSRV_ERROR;`).
            // The protocol comment there is explicit: "The available field
            // is used (abused) here to communicate the minor version number
            // starting with CA 4.1. The field was set to zero prior to 4.1."
            // So a conformant libca client always carries its true version
            // in CREATE_CHAN `m_available` (== the VERSION `m_count` it
            // already handshook), and for such a client this upgrade-only
            // write is IDENTICAL to C's unconditional one — both leave the
            // negotiated version in place. The two differ only for a
            // non-conformant peer whose `m_available` is *lower* than the
            // version it handshook, where C downgrades-then-rejects and we
            // instead keep the handshook version; we deliberately do NOT
            // copy C's downgrade+reject for that malformed case (see the
            // 2026-07-01 R2-55 disposition). The upgrade path itself
            // (v4.4 handshake → v4.13 CREATE_CHAN) is applied here, which
            // downstream `CA_V49` extended-form framing (nElem >= 0xffff)
            // then honours; pre-fix Rust ignored `hdr.available` entirely,
            // so a peer using the upgrade pattern saw truncated large arrays.
            if (hdr.available as u16) > state.client_minor_version {
                state.client_minor_version = hdr.available as u16;
            }

            // DoS guard: refuse new channels once an opt-in per-client
            // cap is hit. Default-unbounded (`None`) — C `claim_ciu_action`
            // imposes no per-client channel count limit (see
            // `max_channels_per_client`). When no cap is configured the
            // whole block is inert, so a legitimate large-fan-out client
            // (e.g. `caget` over thousands of PVs on one circuit) is never
            // refused at a fixed boundary.
            if let Some(cap) = max_channels_per_client() {
                // Pre-warning at 90% — fired once per crossing, not once per
                // CREATE_CHAN, to avoid log spam.
                let warn_threshold = (cap * 9) / 10;
                if !state.channel_limit_warned && state.channels.len() >= warn_threshold {
                    tracing::warn!(
                        channels = state.channels.len(),
                        cap,
                        "approaching per-client channel limit (90%)"
                    );
                    metrics::counter!("ca_server_channel_limit_warnings_total").increment(1);
                    state.channel_limit_warned = true;
                }
                if state.channels.len() >= cap {
                    tracing::warn!(
                        channels = state.channels.len(),
                        cap,
                        "rejecting CREATE_CHAN: per-client channel limit reached"
                    );
                    metrics::counter!("ca_server_channel_limit_rejects_total").increment(1);
                    // C parity: `claim_ciu_action` (rsrv/camessage.c:1260-1270)
                    // routes channel-allocation failure through
                    // `send_err(mp, ECA_ALLOCMEM, …)`, NOT
                    // CREATE_CH_FAIL. CREATE_CH_FAIL is reserved for the
                    // `dbChannel_create` (PV/field not found) branch
                    // (camessage.c:1242-1251). libca
                    // `exceptionRespAction` surfaces the ECA_ALLOCMEM
                    // status to the user-level callback so the client
                    // knows "server out of resources" vs CREATE_CH_FAIL's
                    // "PV does not exist on this server" — the existing
                    // Rust path conflated the two, leading clients to
                    // remove our address from their resolution cache on
                    // a transient server saturation. Per `vsend_err`'s
                    // switch, CA_PROTO_CREATE_CHAN falls to `default`
                    // and uses `0xffffffff` for `m_cid`.
                    send_ca_error(
                        writer,
                        hdr,
                        ECA_ALLOCMEM,
                        u32::MAX,
                        "channel limit reached",
                        state.client_minor_version,
                    )?;
                    // C `claim_ciu_action` (camessage.c:1260-1270): when
                    // the server's channel-allocation pool is exhausted,
                    // send_err(ECA_ALLOCMEM) is followed by RSRV_ERROR
                    // which tears the connection down. The Rust per-
                    // client cap is the closest analogue: same root
                    // cause (this client requested more channels than
                    // the server is willing to hold) and the same
                    // ECA_ALLOCMEM wire byte. Match C by dropping the
                    // connection so a misbehaving client doesn't sit
                    // and spam CREATE_CHAN frames against a saturated
                    // cap; the next reconnect re-baselines.
                    return Err(epics_base_rs::error::CaError::Protocol(
                        "CREATE_CHAN per-client cap reached \
                         (matches C claim_ciu_action ECA_ALLOCMEM + RSRV_ERROR)"
                            .into(),
                    ));
                }
            }

            // C `claim_ciu_action` (`rsrv/camessage.c`) forces
            // `pName[mp->m_postsize - 1] = '\0'` after rejecting
            // `m_postsize <= 1`. Effect: an unterminated name of
            // exactly `postsize` non-NUL bytes is treated as a
            // `postsize - 1` byte name. Pre-fix Rust used all
            // `payload.len()` bytes on the unterminated path, so a
            // malformed peer could resolve a different name than
            // rsrv would.
            let scan_end = payload.len().saturating_sub(1).max(0);
            let end = payload[..scan_end]
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(scan_end);
            let pv_name = String::from_utf8_lossy(&payload[..end]).to_string();
            let client_cid = hdr.cid;
            // epics-base 3.15.7 channel-filter suffix
            // (`REC.{"dbnd":{"d":0.5}}`). Split the JSON suffix off
            // for the record lookup, but keep `pv_name` verbatim so
            // the audit log and `ChannelCreated`/`ChannelCleared`
            // events still surface the literal string the client
            // used. `filter_suffix` is stashed on the channel so
            // EVENT_ADD can build a `FilterChain` from it later.
            //
            // The `$` long-string modifier (C dbChannel.c:482-507) is peeled
            // there too, after the suffix and before the `record.FIELD`
            // split — including for a bare `REC$` with no explicit `.FIELD`,
            // which used to leave `$` on the record key so `find_entry_from`
            // missed the record entirely. `long_string` makes every delivery
            // path convert the value to DBR_CHAR with a NUL terminator.
            let parsed_channel =
                epics_base_rs::server::database::filters::parse_channel_name(&pv_name);
            let filter_suffix = parsed_channel.json_suffix;
            let long_string = parsed_channel.string_view;
            let record_path = parsed_channel.record_path.as_str();
            let field = parsed_channel.field.clone();

            // thread the connection peer into the search
            // resolver so the CA gateway applies host-scoped `.pvlist`
            // `DENY FROM host` admission on CREATE_CHANNEL (parity with C
            // `pvExistTest` → `gateAs::findEntry(pvname, hostname)`).
            if let Some(entry) = db.find_entry_from(record_path, Some(peer)).await {
                let sid = state.alloc_sid();

                let (dbr_type, element_count, target, long_string_mode) = match entry {
                    PvEntry::Simple(pv) => {
                        let value = pv.get();
                        // `$` long-string — C dbChannel.c:486-503 requires the
                        // field to be DBF_STRING; other types get
                        // S_dbLib_fieldNotFound (CREATE_CH_FAIL). When it is a
                        // string C overrides the channel to DBF_CHAR with
                        // `no_elements = field_size` (= 40). This must match the
                        // RecordField arm below, because the channel stores
                        // its `LongStringMode` and every delivery path runs
                        // `apply_long_string` to convert the value to CHAR[40];
                        // advertising the native DBR_STRING/1 here would
                        // mis-size the client buffer against the delivered data.
                        if long_string && !matches!(value, EpicsValue::String(_)) {
                            let mut fail = CaHeader::new(CA_PROTO_CREATE_CH_FAIL);
                            fail.cid = client_cid;
                            writer.push(fail.to_bytes().to_vec());
                            return Ok(());
                        }
                        let (dbr_type_val, element_count, mode) = if long_string {
                            (DbFieldType::Char, 40u32, LongStringMode::DollarChar)
                        } else {
                            (
                                value.dbr_type(),
                                value.count() as u32,
                                LongStringMode::Plain,
                            )
                        };
                        (
                            dbr_type_val,
                            element_count,
                            ChannelTarget::SimplePv(pv),
                            mode,
                        )
                    }
                    PvEntry::Record(rec) => {
                        let instance = rec.read();
                        // `client_field_value` = resolve_field (3-level
                        // priority) with a DBF_MENU field promoted to its
                        // DBR_ENUM form, so the channel's announced native
                        // type matches the GET/MONITOR data
                        // (`value.dbr_type()` below).
                        let value = instance.client_field_value(&field);
                        match value {
                            Some(v) => {
                                // `$` long-string — C dbChannel.c:483-507
                                // requires the field to be DBF_STRING
                                // (EpicsValue::String). Other field types get
                                // S_dbLib_fieldNotFound (CREATE_CH_FAIL parity).
                                if long_string && !matches!(v, EpicsValue::String(_)) {
                                    let mut fail = CaHeader::new(CA_PROTO_CREATE_CH_FAIL);
                                    fail.cid = client_cid;
                                    writer.push(fail.to_bytes().to_vec());
                                    return Ok(());
                                }
                                // override type and count for `$` channels.
                                // C sets `paddr->field_type = DBF_CHAR`,
                                // `paddr->dbr_field_type = DBR_CHAR`, and
                                // `paddr->no_elements = paddr->field_size` (= 40).
                                let (dbr_type_val, element_count, mode) = if long_string {
                                    (DbFieldType::Char, 40u32, LongStringMode::DollarChar)
                                } else if instance
                                    .record
                                    .long_string_fields()
                                    .contains(&field.as_str())
                                {
                                    // Long-string *record* field (lsi/lso VAL &
                                    // OVAL, printf VAL). C `cvt_dbaddr` presents
                                    // it as a scalar `DBF_STRING` with
                                    // `no_elements = 1` (lsiRecord.c:141-143,
                                    // lsoRecord.c:183-185, printfRecord.c:411-413);
                                    // the full long value is reachable only via
                                    // the `$` modifier (handled above). The record
                                    // stores the value as a CHAR-array carrier, so
                                    // every delivery path decodes it to a scalar
                                    // string with `apply_native_long_string`.
                                    (DbFieldType::String, 1u32, LongStringMode::NativeString)
                                } else if let Some(native) =
                                    instance.record.field_native_count(&field)
                                {
                                    // C `cvt_dbaddr` fixes a channel's no_elements at
                                    // the field's buffer capacity, distinct from the
                                    // current value length `get_array_info` reports
                                    // (waveform VAL→NELM serving NORD; asyn BOUT→OMAX /
                                    // BINP→IMAX serving the transferred byte count). The
                                    // client sizes its buffer to the capacity even
                                    // though a GET returns fewer elements.
                                    (v.dbr_type(), native, LongStringMode::Plain)
                                } else {
                                    (v.dbr_type(), v.count() as u32, LongStringMode::Plain)
                                };
                                (
                                    dbr_type_val,
                                    element_count,
                                    ChannelTarget::RecordField {
                                        record: rec.clone(),
                                        field: field.clone(),
                                    },
                                    mode,
                                )
                            }
                            None => {
                                // Field not found — send CREATE_CH_FAIL
                                let mut fail = CaHeader::new(CA_PROTO_CREATE_CH_FAIL);
                                fail.cid = client_cid;
                                writer.push(fail.to_bytes().to_vec());
                                return Ok(());
                            }
                        }
                    }
                };

                // Parse the channel-filter suffix STRICTLY at channel
                // creation — EPICS `dbChannelCreate()` parity. C runs
                // `chf_parse()` while building the channel; a malformed /
                // non-object suffix, an unknown filter name, or a filter
                // whose own `parse_end()` rejects its config sets `status`,
                // and at `finish:` `dbChannelCreate()` does
                // `dbChannelDelete(chan); chan = NULL` and returns NULL —
                // i.e. CREATE_CH_FAIL on the CA wire (dbChannel.c:176-179,
                // 266-279, 512-526). The earlier CA path used the
                // permissive `parse_filter_chain`, which fails OPEN to an
                // unfiltered channel on a bad suffix, so a typo in a filter
                // used to throttle / slice / synchronize exposure read the
                // raw stream where C refuses the channel.
                //
                // On success the validated chain also yields the
                // filter-FINAL element count (C `dbChannelFinalElements`):
                // a count-reshaping filter (`arr` slice) shrinks how many
                // elements the channel can ever deliver, and the client
                // must learn that count so its READ / monitor request count
                // — and buffer allocation — match the filtered payload. An
                // empty / value-gating-only chain folds to the identity, so
                // unfiltered channels keep their native count unchanged.
                let element_count = match &filter_suffix {
                    Some(json) => {
                        match epics_base_rs::server::database::filters::try_parse_filter_chain(json)
                        {
                            Ok(chain) => chain.final_element_count(element_count as usize) as u32,
                            Err(e) => {
                                tracing::debug!(
                                    pv = %pv_name,
                                    error = %e,
                                    "rejecting CREATE_CHAN: invalid channel-filter suffix",
                                );
                                let mut fail = CaHeader::new(CA_PROTO_CREATE_CH_FAIL);
                                fail.cid = client_cid;
                                writer.push(fail.to_bytes().to_vec());
                                state.audit("create_chan", &pv_name, "", "filter_parse_fail");
                                return Ok(());
                            }
                        }
                    }
                    None => element_count,
                };

                let (access, rule_was_trap) = state.compute_access(&target).await;
                let access_level = match access {
                    3 => AccessLevel::ReadWrite,
                    1 => AccessLevel::Read,
                    _ => AccessLevel::NoAccess,
                };

                state.channels.insert(
                    sid,
                    ChannelEntry {
                        target,
                        cid: client_cid,
                        pv_name: pv_name.clone(),
                        filter_suffix: filter_suffix.clone(),
                        put_notify_slot: PutNotifySlot::default(),
                        long_string_mode,
                        // The post-filter capacity computed above and
                        // announced in the CREATE_CHAN reply — the ceiling
                        // a later request's element count is clamped to.
                        final_element_count: element_count,
                    },
                );
                state.channel_access.insert(sid, access_level);
                // keep the trap-mask map in lockstep with
                // `channel_access` so `lookup_access` always finds a
                // consistent pair for this SID.
                state.channel_trap.insert(sid, rule_was_trap);

                let mut ar = CaHeader::new(CA_PROTO_ACCESS_RIGHTS);
                ar.cid = client_cid;
                ar.available = access;

                // C `claim_ciu_reply` (camessage.c:1188-1195): clients
                // whose minor version is below CA_V49 (= 9) cannot parse
                // extended-form headers. For those peers, `nElem` is
                // capped at 0xfffe so the CREATE_CHAN reply stays in
                // normal-form (16-byte) layout; V4.9+ clients receive
                // the true count via the extended header.
                let nelem = if state.client_minor_version < 9 && element_count >= 0xffff {
                    0xfffe
                } else {
                    element_count
                };
                let mut resp = CaHeader::new(CA_PROTO_CREATE_CHAN);
                resp.data_type = dbr_type as u16;
                resp.cid = client_cid;
                resp.available = sid;
                resp.set_payload_size(0, nelem, state.client_minor_version)
                    .expect("nelem is capped at 0xfffe for pre-V49 clients above");

                // Two independent, complete frames (ACCESS_RIGHTS then
                // CREATE_CHAN); push in order. FIFO drain preserves the
                // access-rights-before-create ordering C emits.
                writer.push(ar.to_bytes().to_vec());
                writer.push(resp.to_bytes_extended());

                let result = match access_level {
                    AccessLevel::NoAccess => "denied",
                    _ => "ok",
                };
                state.audit("create_chan", &pv_name, "", result);

                // Notify subscribers (e.g. ca_gateway tracking PV → client
                // attachments for `Active`/`Inactive` state transitions).
                // `cid` is included so consumers can refcount per
                // (peer, pv_name, cid) — same client opening N channels
                // to the same PV must increment N times.
                if let Some(tx) = &conn_events {
                    let _ = tx.send(ServerConnectionEvent::ChannelCreated {
                        peer,
                        pv_name: pv_name.clone(),
                        cid: client_cid,
                    });
                }
            } else {
                // PV not found — send CREATE_CH_FAIL
                let mut fail = CaHeader::new(CA_PROTO_CREATE_CH_FAIL);
                fail.cid = client_cid;
                writer.push(fail.to_bytes().to_vec());

                state.audit("create_chan", &pv_name, "", "not_found");
            }
        }

        CA_PROTO_READ | CA_PROTO_READ_NOTIFY => {
            let is_notify = hdr.cmmd == CA_PROTO_READ_NOTIFY;
            let sid = hdr.cid;
            let ioid = hdr.available;
            let requested_type = hdr.data_type;
            let mut requested_count = hdr.actual_count();

            // the two read commands differ in WHERE the
            // `INVALID_DB_REQ(m_dataType)` type check sits relative to
            // the channel lookup.
            //
            // C `read_notify_action` (`rsrv/camessage.c:703-705`) checks
            // the type BEFORE the lookup and returns RSRV_ERROR with no
            // wire frame, so READ_NOTIFY is handled here, silently.
            //
            // C `read_action` (`rsrv/camessage.c:606-621`) resolves the
            // channel FIRST (`if(!pciu){logBadId;return}` — `logBadId`
            // sends an ECA_INTERNAL "Bad Resource ID" frame with a
            // cid=0xFFFFFFFF sentinel) and only THEN, if the type is
            // invalid, sends ECA_BADTYPE carrying the channel's real cid
            // + record name. So the deprecated-READ bad-type frame must
            // be gated on the channel existing: it is emitted below,
            // after the lookup, never with a `u32::MAX` sentinel here.
            // Otherwise an unknown SID + bad type drew a spurious
            // ECA_BADTYPE where C sends the bad-SID ECA_INTERNAL frame.
            //
            // `LAST_BUFFER_TYPE = 38` (caProto.h); request types above
            // that are not encodable.
            const LAST_BUFFER_TYPE: u16 = 38;
            if is_notify && requested_type > LAST_BUFFER_TYPE {
                return Err(epics_base_rs::error::CaError::Protocol(format!(
                    "READ_NOTIFY with unsupported DBR type {} > LAST_BUFFER_TYPE \
                     (matches C read_notify_action INVALID_DB_REQ RSRV_ERROR)",
                    requested_type
                )));
            }

            let entry = match state.channels.get(&sid) {
                Some(e) => e,
                None => {
                    // C `read_action` (camessage.c:613-616) and
                    // `read_notify_action` (707-711) bad-SID:
                    // `if (!pciu) { logBadId; return RSRV_ERROR; }`.
                    // `logBadId` (camessage.c:312-325) is NOT log-only —
                    // it calls `send_err(ECA_INTERNAL, "Bad Resource ID
                    // at %s.%d")`, buffering a CA_PROTO_ERROR frame that
                    // camsgtask.c:142 flushes ("flush any queued messages
                    // before shutdown") before the disconnect. Since
                    // `MPTOPCIU` returned NULL, `vsend_err` stamps
                    // cid=0xFFFFFFFF (camessage.c:172-178). So C emits one
                    // ECA_INTERNAL frame, then drops the connection.
                    send_ca_error(
                        writer,
                        hdr,
                        ECA_INTERNAL,
                        0xFFFF_FFFF,
                        "Bad Resource ID",
                        state.client_minor_version,
                    )?;
                    return Err(epics_base_rs::error::CaError::Protocol(format!(
                        "READ on unknown SID {} (matches C read_action logBadId + RSRV_ERROR)",
                        sid
                    )));
                }
            };

            // PR #934 (epics-base) parity: clamp the wire element count to
            // the channel's final element count so an oversized `m_count`
            // cannot drive the reply zero-fill (`size_dbr_reply`)
            // past the channel's real capacity. C `read_action`
            // (`camessage.c`) / `read_notify_action`:
            // `if (mp->m_count > dbChannelFinalElements(pciu->dbch))
            //     mp->m_count = dbChannelFinalElements(pciu->dbch);`.
            // `requested_count == 0` is autosize and is preserved untouched.
            if requested_count != 0 && requested_count > entry.final_element_count {
                requested_count = entry.final_element_count;
            }

            // Deprecated READ: C `read_action` (`rsrv/camessage.c:616-619`)
            // checks `INVALID_DB_REQ` AFTER resolving the channel and
            // BEFORE the access check, sending ECA_BADTYPE with the
            // channel's real cid + record name (`vsend_err` uses
            // `pciu->cid`). READ_NOTIFY already returned above (its type
            // check is pre-lookup and silent), so reaching here with a
            // bad type means the deprecated READ command.
            // `LAST_BUFFER_TYPE = 38`.
            if requested_type > LAST_BUFFER_TYPE {
                let audit_pv = match &entry.target {
                    ChannelTarget::SimplePv(pv) => pv.name.clone(),
                    ChannelTarget::RecordField { record, field } => {
                        format!("{}.{}", record.read().name, field)
                    }
                };
                send_ca_error(
                    writer,
                    hdr,
                    ECA_BADTYPE,
                    entry.cid,
                    &audit_pv,
                    state.client_minor_version,
                )?;
                return Err(epics_base_rs::error::CaError::Protocol(format!(
                    "READ with unsupported DBR type {} > LAST_BUFFER_TYPE \
                     (matches C read_action INVALID_DB_REQ ECA_BADTYPE)",
                    requested_type
                )));
            }

            // Type-state:
            // `state.lookup_access(sid)` is the only path to the
            // access cache. `require_read()` returns a witness on
            // success and an `AccessDenied` carrying the matching
            // ECA code on failure — no `if access ==` ad-hoc
            // comparison, no missing-entry default to argue about.
            let _read_grant = match state.lookup_access(sid).require_read() {
                Ok(g) => g,
                Err(denied) => {
                    if is_notify {
                        // C `read_notify_action` →
                        // `read_reply` → `no_read_access_event`
                        // (`rsrv/camessage.c:455-485`) builds a
                        // CA_PROTO_READ_NOTIFY frame with the
                        // ORIGINAL requested count and a
                        // `dbr_size_n`-sized zero payload, abusing
                        // `m_cid` to carry the ECA status. Pre-fix
                        // Rust used `send_cmd_error` which always
                        // emits `count = 0` + zero-byte payload — a
                        // libca-style client validating callback
                        // metadata saw the wrong shape for the same
                        // no-read-access `caget` path. The helper
                        // mirrors the C wire format.
                        send_no_read_access_event(
                            writer,
                            CA_PROTO_READ_NOTIFY,
                            requested_type,
                            requested_count,
                            ioid,
                            denied.eca_code(),
                            ReplyContext {
                                req_hdr: *hdr,
                                client_minor: state.client_minor_version,
                            },
                        )?;
                    } else {
                        // C `read_action` (`rsrv/camessage.c:644-650`)
                        // sends `send_err(mp, ECA_NORDACCESS, client,
                        // RECORD_NAME(pciu->dbch))` — i.e.
                        // CA_PROTO_ERROR — for the deprecated
                        // CA_PROTO_READ on read denial. Pre-fix Rust
                        // silently returned, so a libca client saw a
                        // timeout instead of the C error callback.
                        // outer cid is `pciu->cid` per
                        // `vsend_err` (camessage.c:172-175).
                        let audit_pv = match &entry.target {
                            ChannelTarget::SimplePv(pv) => pv.name.clone(),
                            ChannelTarget::RecordField { record, field } => {
                                format!("{}.{}", record.read().name, field)
                            }
                        };
                        send_ca_error(
                            writer,
                            hdr,
                            denied.eca_code(),
                            entry.cid,
                            &audit_pv,
                            state.client_minor_version,
                        )?;
                    }
                    return Ok(());
                }
            };

            // GET path consults the target's optional read hook: a no-cache
            // CA-gateway shadow PV forwards this read to a fresh upstream
            // fetch. Item-3 sans-io split: a local record field / hookless
            // SimplePv resolves the snapshot SYNCHRONOUSLY
            // (`try_get_read_snapshot_local`) with no `.await` — the entire
            // local reply-production path (lookup → snapshot → filter →
            // `build_read_reply` → outbox push) is now reactor-free. Only a
            // gateway read hook (genuine upstream network I/O) falls through to
            // the async `get_read_snapshot`. An `Err` there is the forwarded
            // upstream get failing — surface ECA_GETFAIL to the client, the IOC
            // get-callback error C ca-gateway would propagate. READ_NOTIFY
            // carries the status in its reply frame; the deprecated READ uses
            // the CA_PROTO_ERROR channel like its read-denial path above.
            let read_result = match try_get_read_snapshot_local(&entry.target) {
                Some(snap) => Ok(snap),
                None => get_read_snapshot(&entry.target).await,
            };
            let snapshot = match read_result {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(error = %e, "ca server: read hook (no-cache get) failed");
                    if is_notify {
                        // C `read_reply` get-failure branch
                        // (`rsrv/camessage.c:548-562`): on
                        // `dbChannel_get_count() < 0` it keeps the
                        // CA_PROTO_READ_NOTIFY reply, abuses `m_cid` to
                        // carry ECA_GETFAIL, and commits a
                        // `dbr_size_n(type, count)` ZEROED body at the
                        // requested count (autosize `m_count==0` resets
                        // count to 0 and sizes `dbr_size_n(type, 0)`).
                        // Same abused-cid wire shape as the no-read-access
                        // frame, so it shares the builder below; the prior
                        // `send_cmd_error` emitted `count=0` + an empty
                        // body, which diverged from the C wire form.
                        send_no_read_access_event(
                            writer,
                            CA_PROTO_READ_NOTIFY,
                            requested_type,
                            requested_count,
                            ioid,
                            ECA_GETFAIL,
                            ReplyContext {
                                req_hdr: *hdr,
                                client_minor: state.client_minor_version,
                            },
                        )?;
                    } else {
                        let audit_pv = match &entry.target {
                            ChannelTarget::SimplePv(pv) => pv.name.clone(),
                            ChannelTarget::RecordField { record, field } => {
                                format!("{}.{}", record.read().name, field)
                            }
                        };
                        send_ca_error(
                            writer,
                            hdr,
                            ECA_GETFAIL,
                            entry.cid,
                            &audit_pv,
                            state.client_minor_version,
                        )?;
                    }
                    return Ok(());
                }
            };
            let Some(mut snapshot) = snapshot else {
                if is_notify {
                    // No snapshot (no-cache shadow PV with no upstream
                    // value): surface ECA_BADCHID through the same C
                    // `read_reply` abused-cid frame as the get-failure
                    // branch above — requested count + `dbr_size_n` zeroed
                    // body — not the prior `count=0`/empty `send_cmd_error`.
                    send_no_read_access_event(
                        writer,
                        CA_PROTO_READ_NOTIFY,
                        requested_type,
                        requested_count,
                        ioid,
                        ECA_BADCHID,
                        ReplyContext {
                            req_hdr: *hdr,
                            client_minor: state.client_minor_version,
                        },
                    )?;
                }
                return Ok(());
            };
            // run the channel filter chain on the read value
            // before DBR encoding. epics-base `dbChannelRunPreChain`
            // (db_access.c:160-167 / dbChannel.c:640-649) runs the same
            // pre-chain on a filtered read channel. `apply_to_read_value`
            // uses read context, so stream-only filters (`dec`/`sync`)
            // pass through while `arr`/`ts`/`dbnd` transform; an empty
            // chain (no suffix / malformed) is the identity. Applied
            // BEFORE the requested-count truncate so the client's `-#`
            // count caps the FILTERED result, matching C (arr slices
            // first, then the count limits).
            let read_chain = entry.filter_chain();
            if !read_chain.is_empty() {
                if let Some(v) = read_chain.apply_to_read_value(snapshot.value.clone()) {
                    snapshot.value = v;
                }
            }
            // convert String → CharArray of exactly 40 elements BEFORE the
            // requested-count clamp. C read_reply sizes the payload to
            // dbr_size_n(DBR_CHAR, request_count) after the channel reports
            // no_elements=40; the clamp must see the 40-element array so
            // `caget -# N PV.DESC$` trims to N chars (not the pre-convert
            // count of 1 that EpicsValue::String::count() returns).
            // `NativeString` is the inverse — a long-string record field
            // (printf/lsi/lso) decoded from its CHAR carrier to a scalar
            // string so plain access ships one DBR_STRING (C cvt_dbaddr).
            super::apply_long_string_mode(&mut snapshot, entry.long_string_mode);
            // Respect client's requested element count (e.g. caget -# 10)
            if requested_count > 0 && requested_count < snapshot.value.count() {
                snapshot.value.truncate(requested_count as usize);
            }

            // For DBR_STSACK_STRING populate ackt/acks from the record so
            // alarm-handler clients see the current acknowledge state.
            if requested_type == epics_base_rs::types::DBR_STSACK_STRING {
                if let ChannelTarget::RecordField { record, .. } = &entry.target {
                    let inst = record.read();
                    if let Some(EpicsValue::Short(v)) = inst.resolve_field("ACKT") {
                        snapshot.alarm.ackt = Some(v as u16);
                    }
                    if let Some(EpicsValue::Short(v)) = inst.resolve_field("ACKS") {
                        snapshot.alarm.acks = Some(v as u16);
                    }
                }
            }

            // For DBR_CLASS_NAME (38) substitute the record's recordType
            // into the response. SimplePv channels have no record-type
            // identity so they receive an empty string (which matches
            // what the C IOC does for in-process DBR_CLASS_NAME reads
            // against synthetic channels).
            if requested_type == epics_base_rs::types::DBR_CLASS_NAME {
                if let ChannelTarget::RecordField { record, .. } = &entry.target {
                    let inst = record.read();
                    snapshot.class_name = Some(inst.record.record_type().to_string());
                }
            }

            // Sans-io boundary: `snapshot` is now fully materialized — the
            // filter chain, long-string mode, and the STSACK / CLASS_NAME
            // field reads (all above) are the only I/O this command needs.
            // `build_read_reply` turns the finished snapshot and the request
            // parameters into the exact wire frame as a pure, socket-free
            // computation; the connection loop's outbox owner is the only
            // code that touches the socket. Emit by pushing the bytes.
            match build_read_reply(
                writer.pool(),
                requested_type,
                requested_count,
                is_notify,
                &snapshot,
                entry.cid,
                ioid,
                state.client_minor_version,
            ) {
                Ok(frame) => writer.push(frame),
                Err(ReadReplyError::BadType) => {
                    // C `read_action` (camessage.c:616-620) checks
                    // `INVALID_DB_REQ(m_dataType)` (type >
                    // LAST_BUFFER_TYPE = 38) BEFORE any DB lookup and
                    // returns `RSRV_ERROR` which tears the connection
                    // down. Other read-path failures (cas_copy_in_header
                    // budget, access denied, dbChannel_get, caNetConvert
                    // host-net) all return RSRV_OK and keep the
                    // connection — those don't apply here.
                    //
                    // Rust `encode_dbr` failure with `UnsupportedType`
                    // is the direct parallel of INVALID_DB_REQ —
                    // emit the error + drop the connection.
                    //
                    // C `read_notify_action`
                    // (`rsrv/camessage.c:703-705`) returns
                    // `RSRV_ERROR` on `INVALID_DB_REQ` WITHOUT
                    // emitting any wire frame — only the deprecated
                    // `read_action` (camessage.c:616-620) calls
                    // `send_err(ECA_BADTYPE)` here. Pre-fix Rust
                    // sent a CA_PROTO_READ_NOTIFY error frame for
                    // the notify path too, an extra wire frame
                    // before EOF that rsrv never produces. Mirror C:
                    // notify path is silent; only the deprecated
                    // READ path emits CA_PROTO_ERROR.
                    // outer cid is `pciu->cid`.
                    if !is_notify {
                        send_ca_error(
                            writer,
                            hdr,
                            ECA_BADTYPE,
                            entry.cid,
                            "bad READ data type",
                            state.client_minor_version,
                        )?;
                    }
                    return Err(epics_base_rs::error::CaError::Protocol(format!(
                        "READ with unsupported DBR type {} (matches C read_action RSRV_ERROR)",
                        requested_type
                    )));
                }
                Err(ReadReplyError::GetFail) => {
                    // C `read_reply`'s `dbChannel_get_count() < 0` branch
                    // (`camessage.c:544-560`) — the same wire shape the
                    // read-hook failure above emits, reached here because the
                    // conversion itself refused the field.
                    if is_notify {
                        return send_no_read_access_event(
                            writer,
                            CA_PROTO_READ_NOTIFY,
                            requested_type,
                            requested_count,
                            ioid,
                            ECA_GETFAIL,
                            ReplyContext {
                                req_hdr: *hdr,
                                client_minor: state.client_minor_version,
                            },
                        );
                    }
                    return send_ca_error(
                        writer,
                        hdr,
                        ECA_GETFAIL,
                        entry.cid,
                        "get conversion failed",
                        state.client_minor_version,
                    );
                }
                Err(ReadReplyError::Oversize) => {
                    // C client TCP parser requires 8-byte aligned postsize.
                    // C `read_action` (`camessage.c:630-639`): a reply needing
                    // the extended header for a pre-V49 client is not framed —
                    // the server answers ECA_16KARRAYCLIENT and keeps the
                    // circuit.
                    return send_16k_array_client_err(
                        writer,
                        hdr,
                        entry.cid,
                        state.client_minor_version,
                    );
                }
            }
        }

        CA_PROTO_WRITE | CA_PROTO_WRITE_NOTIFY => {
            // Thin caller: the shared wire logic (SID/type/access gates, payload
            // convert, the DB write, trap-write bracket, sync/error replies) lives
            // in `serve_write_head` — ONE copy, shared with the blocking RTEMS
            // driver. Only the async-completion handling differs per front-end:
            // here the async server spawns a task to await the record's chain and
            // send the deferred WRITE_NOTIFY reply; the blocking driver hands the
            // receiver to its event thread. Wire behavior is unchanged.
            match serve_write_head(hdr, payload, state, db, writer).await? {
                WriteHeadOutcome::Done => {}
                WriteHeadOutcome::AsyncPending(mut pending) => {
                    // Registration and the ECA_PUTCBINPROG rule both live in
                    // `serve_write_head`; all this loop owes the put-callback is
                    // the wait and the shared tail.
                    let sid = pending.sid;
                    // Clone the outbox handle for the completion task: it pushes the
                    // deferred reply into the same per-connection outbox the loop
                    // drains — it never touches the socket writer directly.
                    let outbox_c = writer.clone();
                    let join = tokio::spawn(async move {
                        // Wait indefinitely for record processing to complete,
                        // matching C rsrv.
                        let chain = (&mut pending.rx).await;
                        let _ = pending.settle(chain, &outbox_c);
                    });
                    // Track for connection-scoped cleanup: a stuck async record
                    // would otherwise pin this task and the captured `Outbox`
                    // handle forever after the client drops. Reap finished handles
                    // opportunistically; the `sid` tag also lets
                    // `CA_PROTO_CLEAR_CHANNEL` drain only the cleared channel's
                    // tasks (C `rsrvFreePutNotify`). Aborting the task drops the
                    // last reference to its completion token with the put-log
                    // bracket still inside, so the cancel AfterWrite fires there
                    // too (C `camessage.c:1650-1652`).
                    state.write_notify_tasks.retain(|(_, h)| !h.is_finished());
                    state.write_notify_tasks.push((sid, join.abort_handle()));
                }
            }
        }

        CA_PROTO_EVENT_ADD => {
            // Thin caller: the parity logic lives in `register_subscription`
            // (shared with the blocking RTEMS driver). Async mode spawns the
            // producer task and this arm records it in `state.subscriptions`.
            let sid = hdr.cid;
            let sub_id = hdr.available;
            let sub_id_in_use = state.subscriptions.contains_key(&sub_id);
            let channel_sub_count = || {
                state
                    .subscriptions
                    .values()
                    .filter(|s| s.channel_sid == sid)
                    .count()
            };
            match register_subscription(
                hdr,
                payload,
                state,
                writer,
                SubscriptionDelivery::AsyncSpawn,
                channel_sub_count,
                sub_id_in_use,
            )
            .await?
            {
                SubscriptionOutcome::Refused => {}
                SubscriptionOutcome::HandedOff(_) => {
                    unreachable!("AsyncSpawn mode never hands off the reader")
                }
                SubscriptionOutcome::Spawned(s) => {
                    state.subscriptions.insert(
                        s.sub_id,
                        SubscriptionEntry {
                            target: s.target,
                            channel_sid: s.channel_sid,
                            sub_id: s.sub_id,
                            data_type: s.data_type,
                            data_count: s.data_count,
                            denied: s.denied,
                            task: s.task,
                            long_string_mode: s.long_string_mode,
                        },
                    );
                    if let Some(tx) = conn_events {
                        let _ = tx.send(ServerConnectionEvent::SubscriptionOpened {
                            peer,
                            pv_name: s.sub_pv_name,
                            sub_id: s.sub_id,
                            mask: s.mask,
                        });
                    }
                }
            }
        }

        CA_PROTO_EVENT_CANCEL => {
            // Thin caller: the bad-SID / bad-mon-id / cancel-ACK wire logic lives
            // in `cancel_subscription_reply` (shared with the blocking driver).
            // On success this arm performs the async teardown (abort the producer
            // task, drop the subscriber, emit the close event).
            let sub_id = hdr.available;
            let sub_info = state.subscriptions.get(&sub_id).map(|s| CancelInfo {
                channel_sid: s.channel_sid,
                data_type: s.data_type,
                data_count: s.data_count,
            });
            if cancel_subscription_reply(hdr, state, writer, sub_info)? {
                if let Some(sub) = state.subscriptions.remove(&sub_id) {
                    sub.task.abort();
                    let pv_name_for_event = state
                        .channels
                        .get(&sub.channel_sid)
                        .map(|e| e.pv_name.clone())
                        .unwrap_or_default();
                    match &sub.target {
                        ChannelTarget::SimplePv(pv) => {
                            pv.remove_subscriber(sub.sub_id);
                        }
                        ChannelTarget::RecordField { record, .. } => {
                            record.write().remove_subscriber(sub.sub_id);
                        }
                    }
                    if let Some(tx) = conn_events {
                        let _ = tx.send(ServerConnectionEvent::SubscriptionClosed {
                            peer,
                            pv_name: pv_name_for_event,
                            sub_id,
                        });
                    }
                }
            }
        }

        CA_PROTO_EVENTS_OFF | CA_PROTO_EVENTS_ON => {
            // C `db_event_flow_ctrl_mode_on/off` on this circuit's event user
            // (`camessage.c:430-445`). Under EVENTS_OFF each post replaces the
            // monitor's last queued entry in place and readers suspend once the
            // queue holds no duplicates — the queue owns both rules.
            if hdr.cmmd == CA_PROTO_EVENTS_OFF {
                state.event_user.flow_ctrl_on();
            } else {
                state.event_user.flow_ctrl_off();
            }
        }

        CA_PROTO_READ_SYNC => {
            // C `read_sync_reply` (camessage.c:2107-2121): server
            // echoes the request header back with cmmd=CA_PROTO_READ_SYNC,
            // m_postsize=0, and the request's m_dataType / m_count /
            // m_cid / m_available preserved. libca client treats this
            // as ECHO (`cac.cpp:72-73`: "legacy READ_SYNC used as
            // echo with legacy server" — dispatched through
            // echoRespAction). Without the reply, a client using
            // READ_SYNC as a keepalive probe (legacy V3 / pre-V4.3
            // protocol behavior) sees no response and may trigger
            // its connection-timeout watchdog.
            //
            // The outer batched flush still fires, so any prior queued
            // responses ship along with this echo — preserving the
            // barrier semantic. Pre-fix Rust silently no-op-ed; this
            // restores wire parity.
            let mut resp = CaHeader::new(CA_PROTO_READ_SYNC);
            resp.data_type = hdr.data_type;
            resp.count = hdr.count;
            resp.cid = hdr.cid;
            resp.available = hdr.available;
            writer.push(resp.to_bytes().to_vec());
        }

        CA_PROTO_ECHO => {
            // C `tcp_echo_action` (`rsrv/camessage.c:410-425`) echoes
            // the *full* request back to the client — same m_cmmd,
            // m_postsize, m_dataType, m_count, m_cid, m_available, and
            // the m_postsize-byte payload. Real clients (libca
            // `tcpiiu::echoRequest`) issue zero-payload echos with
            // every field zero, in which case our previous
            // `CaHeader::new(CA_PROTO_ECHO).to_bytes()` happened to be
            // byte-identical to C. But a diagnostic / probe client
            // that sends ECHO with a marker payload (e.g. to measure
            // RTT or to verify the server isn't a TCP transparent
            // proxy) gets a stripped, all-zero reply from us — wire
            // divergence that breaks the documented round-trip
            // semantics.
            let mut resp = CaHeader::new(CA_PROTO_ECHO);
            // Preserve the request fields. set_payload_size handles
            // both the short and extended encodings transparently.
            resp.data_type = hdr.data_type;
            resp.set_payload_size(
                hdr.actual_postsize(),
                hdr.actual_count(),
                state.client_minor_version,
            )
            .expect("the client framed this very ECHO request");
            resp.cid = hdr.cid;
            resp.available = hdr.available;
            // Abort-safety: build header + echoed payload as ONE
            // contiguous frame and hand it to the outbox in a single
            // `push`, so a `send_timeout` cancel can never enqueue an
            // orphan header for the connection loop to write.
            let mut frame = Vec::new();
            if resp.is_extended() {
                frame.extend_from_slice(&resp.to_bytes_extended());
            } else {
                frame.extend_from_slice(&resp.to_bytes());
            }
            // Echo the payload back verbatim (truncated to the actual
            // postsize advertised by the request — `payload` here is
            // already that slice).
            frame.extend_from_slice(payload);
            writer.push(frame);
        }

        CA_PROTO_SEARCH => {
            // C `search_reply_tcp` (`camessage.c:2292-2295`) gates on the
            // SEARCH frame's own `m_count` and answers `RSRV_ERROR`, which
            // tears the circuit down. The searcher's version rides in the
            // frame because a SEARCH is not bound to the handshake that
            // opened the circuit; reading the negotiated version here served
            // an ancient searcher a v4.13 reply it cannot parse whenever some
            // other peer's handshake had raised the circuit.
            if !declares_supported_version(hdr) {
                return Err(epics_base_rs::error::CaError::Protocol(format!(
                    "TCP SEARCH declaring minor {} (< {CA_MINIMUM_SUPPORTED_VERSION}) — \
                     C search_reply_tcp RSRV_ERROR parity",
                    hdr.count
                )));
            }
            // C `search_reply_tcp` (rsrv/camessage.c:2292-2295) rejects
            // SEARCH whose `m_postsize <= 1` and silently returns
            // RSRV_OK. Mirror that here so an attacker's empty-name
            // SEARCH burst on an open TCP connection cannot drive
            // `db.has_name("")` per frame nor trigger a NOT_FOUND
            // amplification when CA_DO_REPLY is set.
            if hdr.postsize <= 1 {
                return Ok(());
            }
            // C `search_reply_tcp` forces NUL at postsize-1.
            let scan_end = payload.len().saturating_sub(1).max(0);
            let end = payload[..scan_end]
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(scan_end);
            let pv_name = String::from_utf8_lossy(&payload[..end]).to_string();

            // Thread the connection peer into the existence check just
            // like TCP CREATE_CHANNEL (`find_entry_from(.., Some(peer))`)
            // and UDP SEARCH (`has_name_from(.., Some(src))`). Without it
            // the CA gateway resolver received `peer: None` and skipped
            // host-scoped `.pvlist` `DENY FROM host` admission, so a denied
            // host's TCP SEARCH could resolve (and lazily instantiate) a
            // PV the pvlist forbids — parity with C `pvExistTest` passing
            // the client host to `gateAs::findEntry`.
            if db.has_name_from(&pv_name, Some(peer)).await {
                // C parity: `search_reply_tcp`
                // (`rsrv/camessage.c:2329-2331`) sends:
                //   m_postsize  = 0  (no payload — TCP search reply
                //                 carries no minor-version trailer,
                //                 unlike UDP)
                //   m_dataType  = ca_server_port (carries the port)
                //   m_count     = 0
                //   m_cid       = ~0U (INADDR_BROADCAST — tells client
                //                 to use TCP peer addr as server IP;
                //                 libca `tcpiiu::searchRespNotify`
                //                 explicitly checks `msg.m_cid !=
                //                 INADDR_BROADCAST` and falls back to
                //                 `this->address()` on the sentinel)
                //   m_available = client's m_available (the cid)
                //
                // The previous code wrote `m_cid = 0` (INADDR_ANY) and
                // an 8-byte minor-version payload. C libca client at
                // `tcpiiu.cpp:2209` treats anything != INADDR_BROADCAST
                // as a literal IP, so `m_cid = 0` would surface as a
                // server at 0.0.0.0:port — unroutable. With this fix
                // the reply is now byte-equivalent to the C softIoc.
                let mut resp = CaHeader::new(CA_PROTO_SEARCH);
                resp.data_type = state.tcp_port;
                resp.set_payload_size(0, 0, state.client_minor_version)
                    .expect("a zero-payload, zero-count reply is never extended");
                resp.cid = u32::MAX; // ~0U — "use TCP peer addr"
                resp.available = hdr.available;

                writer.push(resp.to_bytes().to_vec());
            } else if hdr.data_type == CA_DO_REPLY {
                // Explicit negative reply requested — send NOT_FOUND so
                // the client doesn't have to wait for a search timeout.
                //
                // C parity: `search_fail_reply` (rsrv/camessage.c:2129-2143)
                // copies the request's `m_dataType`/`m_count`/`m_cid`/
                // `m_available` verbatim into the response. The previous
                // Rust path overwrote `count` with the server's
                // CA_MINOR_VERSION and `cid` with the request's
                // `m_available` (which happens to equal `m_cid` for
                // libca search frames, but the parity intent is
                // "echo m_cid"). With this fix the reply is byte-
                // equivalent to a C softIoc fail reply.
                let mut nf = CaHeader::new(CA_PROTO_NOT_FOUND);
                nf.data_type = hdr.data_type;
                nf.count = hdr.count;
                nf.cid = hdr.cid;
                nf.available = hdr.available;
                writer.push(nf.to_bytes().to_vec());
            }
            // Otherwise silent — clients without CA_DO_REPLY treat absence
            // as "this server doesn't have it" and move on.
        }

        CA_PROTO_CLEAR_CHANNEL => {
            let sid = hdr.cid;
            let cid = hdr.available;
            // C `clear_channel_reply` (camessage.c:1937-1941)
            // disconnects on a bad SID via `logBadId` + RSRV_ERROR;
            // `logBadId` emits an ECA_INTERNAL "Bad Resource ID" frame
            // (cid=0xFFFFFFFF) flushed before the close. Channels in this
            // Rust state are per-client by construction, so the "foreign
            // channel" sub-case of the C check (`pciu->client != client`)
            // can't happen — the only failure mode is unknown SID.
            // Pre-fix Rust silently skipped without disconnecting,
            // so a probing peer could send CLEAR_CHANNEL on random
            // SIDs indefinitely.
            if !state.channels.contains_key(&sid) {
                send_ca_error(
                    writer,
                    hdr,
                    ECA_INTERNAL,
                    0xFFFF_FFFF,
                    "Bad Resource ID",
                    state.client_minor_version,
                )?;
                return Err(epics_base_rs::error::CaError::Protocol(format!(
                    "CLEAR_CHANNEL on unknown SID {} (matches C clear_channel_reply logBadId + RSRV_ERROR)",
                    sid
                )));
            }
            if let Some(entry) = state.channels.remove(&sid) {
                state.channel_access.remove(&sid);
                // drop the parallel trap-mask entry so a
                // recycled SID never inherits a stale trap flag.
                state.channel_trap.remove(&sid);
                state.release_sid(sid);
                if let Some(tx) = &conn_events {
                    let _ = tx.send(ServerConnectionEvent::ChannelCleared {
                        peer,
                        pv_name: entry.pv_name.clone(),
                        cid: entry.cid,
                    });
                }

                // C parity: `clear_channel_reply` (`camessage.c:1943`)
                // calls `rsrvFreePutNotify` to drain pending PUT_NOTIFY
                // operations for this channel. Without aborting the
                // matching tasks, a stuck async record could later
                // emit a stale WRITE_NOTIFY response carrying the
                // cleared channel's ioid — confusing the client's
                // ioid demultiplex. Drain finished handles
                // opportunistically while iterating.
                drain_write_notify_tasks_for_sid(&mut state.write_notify_tasks, sid);

                // Clean up subscriptions that belong to this channel
                let sub_ids: Vec<u32> = state
                    .subscriptions
                    .iter()
                    .filter(|(_, sub)| sub.channel_sid == sid)
                    .map(|(&id, _)| id)
                    .collect();
                for sub_id in sub_ids {
                    if let Some(sub) = state.subscriptions.remove(&sub_id) {
                        sub.task.abort();
                        match &sub.target {
                            ChannelTarget::SimplePv(pv) => {
                                pv.remove_subscriber(sub.sub_id);
                            }
                            ChannelTarget::RecordField { record, .. } => {
                                record.write().remove_subscriber(sub.sub_id);
                            }
                        }
                        if let Some(tx) = &conn_events {
                            let _ = tx.send(ServerConnectionEvent::SubscriptionClosed {
                                peer,
                                pv_name: entry.pv_name.clone(),
                                sub_id,
                            });
                        }
                    }
                }

                let mut resp = CaHeader::new(CA_PROTO_CLEAR_CHANNEL);
                resp.data_type = hdr.data_type;
                resp.count = hdr.count;
                resp.cid = sid;
                resp.available = cid;
                writer.push(resp.to_bytes().to_vec());
            }
        }

        _ => {
            // A LEGAL CA opcode with no arm above. An ILLEGAL one never
            // reaches here: `RecvAccumulator::next_message` answers those
            // with C `bad_tcp_cmd_action` (`camessage.c:342-357`) —
            // ECA_INTERNAL and a tear-down — for both drivers, so this arm
            // no longer carries a second copy of that rule. The set is
            // empty today (`every_legal_tcp_command_is_routed_by_this_driver`
            // pins it), and C's answer is the right fail-closed default for
            // a routing table that fell behind the protocol.
            let error_msg = format!("Unrouted command {}", hdr.cmmd);
            send_ca_error(
                writer,
                hdr,
                ECA_INTERNAL,
                0xFFFF_FFFF,
                &error_msg,
                state.client_minor_version,
            )?;
            return Err(epics_base_rs::error::CaError::Protocol(format!(
                "legal but unrouted TCP command {}",
                hdr.cmmd
            )));
        }
    }

    Ok(())
}
/// A wire `data_type` that a `CA_PROTO_WRITE` / `CA_PROTO_WRITE_NOTIFY` may
/// legally carry, split by what this server can do with it.
///
/// The out-of-range case is deliberately not a variant: [`Self::classify`]
/// returns `None` for it, so a value of this type only exists once the frame
/// has passed C's protocol bound and the circuit is no longer at risk.
///
/// C's bound is `LAST_BUFFER_TYPE` (= `DBR_CLASS_NAME`, 38), applied by
/// `INVALID_DB_REQ` in `write_notify_action` (`rsrv/camessage.c:1678`) and by
/// `caNetConvert` (`ca/src/client/convert.cpp:1421`) on the `write_action`
/// path. Anything above it is a protocol violation and RSRV drops the circuit;
/// anything at or below it keeps the circuit whatever the put then does.
///
/// As-built summary and the one remaining deviation: `doc/ca-compound-dbr-put.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcceptedWriteType {
    /// 0..=6 — a native DBR the payload decoder converts directly.
    Native(DbFieldType),
    /// 7..=34 — a value preceded by metadata (`DBR_STS_*`, `DBR_TIME_*`,
    /// `DBR_GR_*`, `DBR_CTRL_*`), carrying the base native type it decodes to.
    ///
    /// On `CA_PROTO_WRITE` C writes these: `dbChannel_put`
    /// (`db/db_access.c:820`) skips the metadata header and puts the `.value`
    /// member, discarding the status/severity/timestamp the client sent —
    /// which are unwritable by any path anyway (`.TIME` is `DBF_NOACCESS`,
    /// `STAT`/`SEVR`/`UTAG` are `SPC_NOMOD`). On `CA_PROTO_WRITE_NOTIFY` even
    /// C fails the put: `mapOldType` (`db_access.c:988`) maps only the native
    /// types and returns -1 for these, which `db_put_process` turns into
    /// `notifyError` → `ECA_PUTFAIL`.
    Compound,
    /// 37/38 (`DBR_STSACK_STRING`, `DBR_CLASS_NAME`) — inside C's protocol
    /// bound but outside `dbChannel_put`'s switch, so its `default:` arm
    /// returns -1 and the put fails on both opcodes.
    ///
    /// `DBR_PUT_ACKT`/`DBR_PUT_ACKS` (35/36) never reach the classifier — the
    /// alarm-acknowledge branch takes them first.
    MetadataOnly,
}

impl AcceptedWriteType {
    /// `None` for a `data_type` above C's `LAST_BUFFER_TYPE` — the caller must
    /// answer `ECA_BADTYPE` and drop, as RSRV does.
    fn classify(data_type: u16) -> Option<Self> {
        if let Ok(native) = DbFieldType::from_u16(data_type) {
            return Some(Self::Native(native.wire_carrier()));
        }
        if data_type > epics_base_rs::types::LAST_BUFFER_TYPE {
            return None;
        }
        // `native_type_for_dbr` names a base type for every code up to the
        // bound, so the upper limit — not the lookup — is what separates the
        // types `dbChannel_put` writes from the ones it drops into `default:`.
        Some(match epics_base_rs::types::native_type_for_dbr(data_type) {
            Ok(_) if data_type <= epics_base_rs::types::DBR_CTRL_DOUBLE => Self::Compound,
            _ => Self::MetadataOnly,
        })
    }

    /// The value this WRITE payload puts, in the carrier `dbChannel_put`
    /// gives it. `None` for [`Self::MetadataOnly`]: those buffers carry no
    /// value member.
    ///
    /// Both arms take the carrier from
    /// [`DbFieldType::wire_carrier`](epics_base_rs::types::DbFieldType::wire_carrier)
    /// — `classify` composes it for the native codes and `decode_dbr` for
    /// the compound ones — so the CHAR row cannot answer differently here
    /// than it does on the client's read and monitor paths.
    ///
    /// That row is where C's two maps disagree: `dbChannel_put`
    /// (`db/db_access.c:820-...`) puts `oldDBR_CHAR` — and each of its
    /// `STS_`/`TIME_`/`GR_`/`CTRL_` compounds, and `mapOldType` (`:988`) on
    /// the WRITE_NOTIFY path — as `DBR_UCHAR`, so the widening row it
    /// reaches is `putUcharLong` (`dbConvert.c`, `PUT` body
    /// `*pdst = (typeb) *psrc`) and 0xC8 widens to 200; the signed
    /// `putCharLong` is unreachable from CA.
    fn decode(self, data_type: u16, payload: &[u8], count: usize) -> Option<CaResult<EpicsValue>> {
        Some(match self {
            Self::Native(native) => EpicsValue::from_bytes_array(native, payload, count),
            // C `dbChannel_put`'s per-type arms are a header skip plus a put
            // of the base type. `decode_dbr` performs exactly that skip — one
            // owner for the compound layouts, shared with the read/monitor
            // path, and bounds-checked where C casts the struct unchecked.
            Self::Compound => epics_base_rs::types::decode_dbr(data_type, payload, count)
                .map(|snapshot| snapshot.value),
            Self::MetadataOnly => return None,
        })
    }
}

/// What the put stage does with a frame that has cleared both gates.
///
/// Both variants are *put results*, reached only after the trap-write bracket
/// is armed: C runs `asTrapWriteWithData` before `dbChannel_put` /
/// `dbProcessNotify` unconditionally (`rsrv/camessage.c:799-804`, `:1795-1802`), so
/// a put-log listener sees the attempt whether or not the buffer type has a
/// put arm. Deciding refusal *here* rather than at the type gate is what keeps
/// that bracket around it.
enum PutPlan {
    /// Hand `value` to the record. The decoded value carries its own native
    /// type — the buffer's own for a native put, the compound buffer's base
    /// type once `decode_dbr` has skipped its metadata header — so no separate
    /// type travels with it.
    Write { value: EpicsValue },
    /// `dbChannel_put` has no arm for this buffer type, so the put fails with
    /// `ECA_PUTFAIL` on either opcode. `logged_value` is what the put-log
    /// renders: C hands `asTrapWriteWithData` the converted payload, which
    /// carries a value for a compound buffer and none for the metadata-only
    /// types.
    Refuse { logged_value: Option<EpicsValue> },
}

/// The shared synchronous head of `CA_PROTO_WRITE` / `CA_PROTO_WRITE_NOTIFY`.
///
/// This is the whole wire path both front-ends run in ONE copy: the SID /
/// DBR-type / write-access gates (in the C-observable order per opcode), the
/// alarm-acknowledge (ACKT/ACKS) branch, the payload conversion, the
/// trap-write bracket, the database/PV write, and every synchronous reply or
/// error frame. It drives the write to the point C `dbProcessNotify` forks
/// sync-vs-async and returns that fork as a typed [`WriteHeadOutcome`]:
///
/// * [`WriteHeadOutcome::Done`] — fully handled; any reply/error is already
///   queued to `writer` (sync WRITE_NOTIFY reply, fire-and-forget WRITE,
///   ACKT/ACKS, and all refusal/error frames).
/// * [`WriteHeadOutcome::AsyncPending`] — a WRITE_NOTIFY whose record chain is
///   still running. The caller awaits the [`PendingWriteNotify::rx`] and, when
///   it fires, sends the deferred reply via [`finish_write_notify`]. The async
///   server spawns a task for that; the blocking RTEMS driver hands it to its
///   event thread. The message thread MUST NOT block on `rx` (C `camsgtask`
///   never blocks on the put-callback).
///
/// `state` is borrowed shared: the per-channel `put_notify_slot` is `Arc`-
/// backed, so the supersede/install of an in-flight put-callback needs no
/// `&mut`. The async server's connection-scoped bookkeeping
/// (`write_notify_tasks`, the `responded` token) stays in its arm.
pub(crate) async fn serve_write_head(
    hdr: &CaHeader,
    payload: &[u8],
    state: &ClientState,
    db: &Arc<PvDatabase>,
    writer: &Outbox,
) -> CaResult<WriteHeadOutcome> {
    let sid = hdr.cid;
    let ioid = hdr.available;
    let is_notify = hdr.cmmd == CA_PROTO_WRITE_NOTIFY;

    // DBR_PUT_ACKT (35) and DBR_PUT_ACKS (36) are alarm-acknowledge
    // writes — payload is a single u16 routed to the record's
    // ACKT/ACKS field. Handle before the regular DbFieldType
    // dispatch so we don't reject the type as unsupported.
    if hdr.data_type == epics_base_rs::types::DBR_PUT_ACKT
        || hdr.data_type == epics_base_rs::types::DBR_PUT_ACKS
    {
        let entry = match state.channels.get(&sid) {
            Some(e) => e,
            None => {
                // C `write_action` (camessage.c:747-751) +
                // `write_notify_action` (camessage.c:1672-1676):
                // `if (!pciu) { logBadId; return RSRV_ERROR; }`.
                // `logBadId` emits an ECA_INTERNAL "Bad Resource
                // ID" frame (cid=0xFFFFFFFF), flushed before the
                // disconnect — same family as the EVENT_ADD bad-SID
                // and the matching READ branch below.
                send_ca_error(
                    writer,
                    hdr,
                    ECA_INTERNAL,
                    0xFFFF_FFFF,
                    "Bad Resource ID",
                    state.client_minor_version,
                )?;
                return Err(epics_base_rs::error::CaError::Protocol(format!(
                    "WRITE (ACKT/ACKS) on unknown SID {} \
                             (matches C write_action logBadId + RSRV_ERROR)",
                    sid
                )));
            }
        };
        // epics-base #934 (4128a7c07): both write actions clamp `m_count`
        // to `dbChannelFinalElements`, then cross-check
        // `dbr_size_n(m_dataType, m_count)` against `m_postsize` — a short
        // frame is a silent RSRV_ERROR. The size check runs BEFORE the
        // access gate on the deprecated WRITE (`write_action`) and AFTER
        // it on WRITE_NOTIFY (`write_notify_action`).
        // `dbr_size_n(PUT_ACKT/ACKS, n)` is the bare u16 array; C's
        // `COUNT<=0` arm sizes one element.
        let mut ack_count = hdr.actual_count();
        if ack_count > entry.final_element_count {
            ack_count = entry.final_element_count;
        }
        let ack_size = epics_base_rs::types::dbr_buffer_size(
            hdr.data_type,
            epics_base_rs::types::DbFieldType::Short,
            ack_count.max(1) as usize,
        );
        let ack_size_check = || -> CaResult<()> {
            if ack_size > payload.len() {
                return Err(epics_base_rs::error::CaError::Protocol(format!(
                    "WRITE (ACKT/ACKS) payload {} bytes < dbr_size_n {} \
                     (matches C size > m_postsize silent RSRV_ERROR)",
                    payload.len(),
                    ack_size
                )));
            }
            Ok(())
        };
        if !is_notify {
            ack_size_check()?;
        }
        // Alarm-acknowledge PUTs travel
        // the same WRITE wire opcodes but pre-fix bypassed
        // the access_rights check that the regular WRITE
        // path performs below. ACKT/ACKS mutate alarm-handler
        // state — a `NoAccess` peer could silence alarms on
        // any record they could open. Mirror the regular
        // WRITE gate.
        // Type-state: alarm-ack PUTs go
        // through the same gate as regular WRITE. Token's
        // `require_write` returns the matching ECA code on
        // denial.
        let entry_cid = entry.cid;
        let _write_grant = match state.lookup_access(sid).require_write() {
            Ok(g) => g,
            Err(denied) => {
                if is_notify {
                    send_put_notify_response(
                        writer,
                        hdr.data_type,
                        ack_count,
                        denied.eca_code(),
                        ioid,
                        ReplyContext {
                            req_hdr: *hdr,
                            client_minor: state.client_minor_version,
                        },
                    )?;
                } else {
                    // C `write_action` (`rsrv/camessage.c:772-779`)
                    // sends `send_err(mp, ECA_NOWTACCESS, client,
                    // RECORD_NAME(pciu->dbch))` even for the no-
                    // notify WRITE. DBR_PUT_ACKT/DBR_PUT_ACKS
                    // travel the same WRITE opcodes, so this
                    // branch covers alarm-acknowledge PUTs too.
                    // outer cid is `pciu->cid`.
                    let audit_pv = match &entry.target {
                        ChannelTarget::SimplePv(pv) => pv.name.clone(),
                        ChannelTarget::RecordField { record, field } => {
                            format!("{}.{}", record.read().name, field)
                        }
                    };
                    send_ca_error(
                        writer,
                        hdr,
                        denied.eca_code(),
                        entry_cid,
                        &audit_pv,
                        state.client_minor_version,
                    )?;
                }
                return Ok(WriteHeadOutcome::Done);
            }
        };
        // WRITE_NOTIFY runs the size cross-check here — after the access
        // gate — matching C `write_notify_action`'s order (a denied peer
        // gets ECA_NOWTACCESS; the size check never runs for it).
        if is_notify {
            ack_size_check()?;
        }
        // The size gate guarantees the u16 is present; pre-fix a missing
        // value silently defaulted to 0.
        let value_u16 = u16::from_be_bytes([payload[0], payload[1]]);
        // C dispatches alarm acknowledgement on the DBR *request type*
        // inside `dbPut` (`dbAccess.c:1331-1335`), ABOVE the SPC_NOMOD
        // gate that refuses an ordinary put to ACKT/ACKS — the client
        // acknowledges through its normal (VAL) channel, and the field
        // the channel names only feeds the DISP gate. Routing this as a
        // field put to "ACKT"/"ACKS" would now be refused with
        // S_db_noMod, exactly as `caput REC.ACKS 2` is.
        let ack = if hdr.data_type == epics_base_rs::types::DBR_PUT_ACKT {
            epics_base_rs::server::record::AlarmAck::Transient
        } else {
            epics_base_rs::server::record::AlarmAck::Severity
        };
        // DBR_PUT_ACKT/ACKS WRITE_NOTIFY travels C
        // `write_notify_action`, so it meets the same per-channel
        // put-callback serialisation as a regular WRITE_NOTIFY, and
        // it meets it *before* the alarm-ack side effect
        // (`camessage.c:1704-1750`). The alarm-ack completes
        // synchronously, so it never registers an entry of its own.
        // The deprecated fire-and-forget CA_PROTO_WRITE path is not
        // serialised in C.
        if is_notify {
            entry.put_notify_slot.serialize(writer)?;
        }
        let result = match &entry.target {
            ChannelTarget::RecordField { record, field } => {
                let name = record.read().name.clone();
                // Alarm-ack puts are immediate in C even for the
                // notify variant — `rsrv/camessage.c` writes ACKT/
                // ACKS via `dbPutField` and replies straight away,
                // never building a putNotify. Neither mode here
                // awaits a completion receiver, so park nothing.
                db.put_alarm_ack_from_ca(&name, field, ack, value_u16).await
            }
            ChannelTarget::SimplePv(_) => Err(epics_base_rs::error::CaError::Protocol(
                "PUT_ACKT/PUT_ACKS only valid on record-backed channels".to_string(),
            )),
        };
        if is_notify {
            let eca = match &result {
                Ok(()) => PutStatus::OK,
                Err(e) => PutStatus::of_failure(e),
            };
            send_put_notify_response(
                writer,
                hdr.data_type,
                ack_count,
                eca.eca(),
                ioid,
                ReplyContext {
                    req_hdr: *hdr,
                    client_minor: state.client_minor_version,
                },
            )?;
        } else if let Err(e) = &result {
            // deprecated CA_PROTO_WRITE for DBR_PUT_ACKT/
            // DBR_PUT_ACKS must surface put failure via
            // CA_PROTO_ERROR per C `write_action`
            // (`rsrv/camessage.c:781-789`). Pre-fix the
            // non-notify alarm-ack path silently swallowed
            // record-side write errors so the libca peer never
            // saw the failure.
            let audit_pv = match &entry.target {
                ChannelTarget::SimplePv(pv) => pv.name.clone(),
                ChannelTarget::RecordField { record, field } => {
                    format!("{}.{}", record.read().name, field)
                }
            };
            let eca = PutStatus::of_failure(e);
            send_ca_error(
                writer,
                hdr,
                eca.eca(),
                entry_cid,
                &audit_pv,
                state.client_minor_version,
            )?;
        }
        return Ok(WriteHeadOutcome::Done);
    }

    // C `write_action` (`rsrv/camessage.c:747-751`) and
    // `write_notify_action` (`camessage.c:1672-1676`) call
    // `MPTOPCIU(mp)` BEFORE any DBR-type check, so a bad SID
    // path goes through `logBadId` + RSRV_ERROR — emitting the
    // ECA_INTERNAL "Bad Resource ID" frame (cid=0xFFFFFFFF)
    // regardless of whether the type is also invalid. Pre-fix
    // Rust ran the type check first and emitted an ECA_BADTYPE
    // error frame for the SID+type combo where rsrv sends the
    // bad-SID ECA_INTERNAL frame instead. Reorder to match C.
    let entry = match state.channels.get(&sid) {
        Some(e) => e,
        None => {
            // Same C logBadId + RSRV_ERROR family as the
            // ACKT/ACKS branch above and the READ branch: an
            // ECA_INTERNAL "Bad Resource ID" frame (cid=0xFFFFFFFF)
            // is buffered then flushed ahead of the disconnect.
            send_ca_error(
                writer,
                hdr,
                ECA_INTERNAL,
                0xFFFF_FFFF,
                "Bad Resource ID",
                state.client_minor_version,
            )?;
            return Err(epics_base_rs::error::CaError::Protocol(format!(
                "WRITE on unknown SID {} (matches C write_action logBadId + RSRV_ERROR)",
                sid
            )));
        }
    };
    // channel-scoped CA_PROTO_ERROR replies must echo
    // `pciu->cid` (the CLIENT cid the libca peer allocated),
    // not the server-side SID we received in `hdr.cid`. C
    // `vsend_err` (`rsrv/camessage.c:160-170`) looks up the
    // `channel_in_use` and uses its `cid` field for the outer
    // error header. Captured here as a Copy so the error sites
    // below can use it after the `entry` borrow ends.
    let entry_cid = entry.cid;
    // The #934 count clamp's bound, captured as a Copy for the same
    // borrow-lifetime reason as `entry_cid` above.
    let final_element_count = entry.final_element_count;
    // Clone the per-channel put-callback slot (Arc-backed) so the
    // supersede gate and the async-completion install below use it
    // without holding the `entry` borrow across them.
    let put_notify_slot = entry.put_notify_slot.clone();

    // Resolve the audit-friendly PV name once. Cheap when audit
    // is off because state.audit() is a single None check.
    let audit_pv = match &entry.target {
        ChannelTarget::SimplePv(pv) => pv.name.clone(),
        ChannelTarget::RecordField { record, field } => {
            format!("{}.{}", record.read().name, field)
        }
    };

    // Post-#934 (epics-base 4128a7c07) BOTH opcodes gate the TYPE first,
    // then clamp `m_count` to `dbChannelFinalElements`, then cross-check
    // `dbr_size_n(m_dataType, m_count)` against `m_postsize`; only the
    // reply shapes and the size-vs-access order still differ:
    //
    // * `write_notify_action`: type (`putNotifyErrorReply` ECA_BADTYPE →
    //   RSRV_ERROR/drop), clamp, access (ECA_NOWTACCESS → RSRV_OK/keep),
    //   size cross-check (silent RSRV_ERROR/drop).
    // * `write_action`: type (`log_header` only — SILENT RSRV_ERROR/drop;
    //   #934 moved this ahead of the access gate, which pre-#934 ran
    //   first with no standalone type check), clamp, size cross-check
    //   (silent RSRV_ERROR/drop), access (ECA_NOWTACCESS → RSRV_OK/keep).
    //
    // The observable both-fail case therefore inverted with #934: a
    // deprecated WRITE carrying a bad type to a channel the peer cannot
    // write is now a silent drop, not ECA_NOWTACCESS + keep.
    //
    // A type-state WRITE gate: `lookup_access` is the only path to the
    // cache; the witness ensures the matching ECA code reaches the wire.
    let wire_type = match AcceptedWriteType::classify(hdr.data_type) {
        Some(t) => t,
        None => {
            // A peer sending a DBR above `LAST_BUFFER_TYPE` has a
            // corrupted dispatcher or is probing, so C drops. A compound
            // type is BELOW that bound and therefore never reaches here.
            if is_notify {
                // C `putNotifyErrorReply` (camessage.c:1513-1533)
                // preserves `m_dataType`/`m_count` from the request —
                // the count is pre-clamp here, as in C.
                send_put_notify_response(
                    writer,
                    hdr.data_type,
                    hdr.actual_count(),
                    ECA_BADTYPE,
                    ioid,
                    ReplyContext {
                        req_hdr: *hdr,
                        client_minor: state.client_minor_version,
                    },
                )?;
            }
            // `write_action` sends nothing: C logs "bad put data type"
            // and returns RSRV_ERROR without a frame.
            return Err(epics_base_rs::error::CaError::Protocol(format!(
                "{} with unsupported DBR type {} (matches C INVALID_DB_REQ RSRV_ERROR)",
                if is_notify { "WRITE_NOTIFY" } else { "WRITE" },
                hdr.data_type
            )));
        }
    };

    // #934 count clamp. C mutates `mp->m_count` in place, so the size
    // cross-check, the payload decode, the DB put and every later reply
    // echo all see the clamped count.
    let mut write_count = hdr.actual_count();
    if write_count > final_element_count {
        write_count = final_element_count;
    }

    // `dbr_size_n(m_dataType, m_count)` vs `m_postsize` (`payload` is
    // the postsize-long buffer). C's `COUNT<=0` arm sizes one element.
    //
    // Scalar DBR_STRING is exempt: libca frames it as
    // `CA_MESSAGE_ALIGN(strlen + 1)` (comQueSend.cpp:332-341), not the
    // 40-byte `dbr_size[DBR_STRING]`, so #934 as merged drops every
    // default-mode `caput` (upstream regression #943; the exemption is
    // PR #944's fix: require a NUL within `m_postsize` instead).
    let size_check = || -> CaResult<()> {
        if hdr.data_type == epics_base_rs::types::DBR_STRING && write_count == 1 {
            if !payload.contains(&0) {
                return Err(epics_base_rs::error::CaError::Protocol(format!(
                    "WRITE scalar string payload {} bytes with no NUL terminator \
                     (matches C epicsStrnLen >= m_postsize silent RSRV_ERROR, PR #944)",
                    payload.len()
                )));
            }
            return Ok(());
        }
        let native = epics_base_rs::types::native_type_for_dbr(hdr.data_type)?;
        let size = epics_base_rs::types::dbr_buffer_size(
            hdr.data_type,
            native,
            write_count.max(1) as usize,
        );
        if size > payload.len() {
            return Err(epics_base_rs::error::CaError::Protocol(format!(
                "WRITE payload {} bytes < dbr_size_n {} for type {} count {} \
                 (matches C size > m_postsize silent RSRV_ERROR)",
                payload.len(),
                size,
                hdr.data_type,
                write_count
            )));
        }
        Ok(())
    };

    let write_grant = if is_notify {
        let write_grant = match state.lookup_access(sid).require_write() {
            Ok(g) => g,
            Err(denied) => {
                // route through the refinement helper so
                // large-array put-callbacks refused by ACF carry
                // the extended-form count instead of the u16
                // marker. The echoed type is the REQUEST's
                // (`putNotifyErrorReply` preserves `m_dataType`,
                // camessage.c:1513-1533); the count is the CLAMPED
                // one — C reads the mutated `mp->m_count`.
                send_put_notify_response(
                    writer,
                    hdr.data_type,
                    write_count,
                    denied.eca_code(),
                    ioid,
                    ReplyContext {
                        req_hdr: *hdr,
                        client_minor: state.client_minor_version,
                    },
                )?;
                state.audit("caput", &audit_pv, "", "denied");
                return Ok(WriteHeadOutcome::Done);
            }
        };
        size_check()?;
        write_grant
    } else {
        size_check()?;
        // C `write_action` emits `send_err(mp, ECA_NOWTACCESS, ...)` and
        // returns RSRV_OK (keep). Without surfacing this the Rust server
        // dropped denied PROTO_WRITEs silently — libca's
        // `cac::exception` never fired, so a `caput` from a read-only
        // peer looked like it had succeeded even though the value never
        // reached the DB.
        match state.lookup_access(sid).require_write() {
            Ok(g) => g,
            Err(denied) => {
                send_ca_error(
                    writer,
                    hdr,
                    denied.eca_code(),
                    entry_cid,
                    &audit_pv,
                    state.client_minor_version,
                )?;
                state.audit("caput", &audit_pv, "", "denied");
                return Ok(WriteHeadOutcome::Done);
            }
        }
    };

    // the write-trap mask of the ACF rule that
    // authorised this write. C `asTrapWriteWithData`
    // (`rsrv/camessage.c:799-802`) consults
    // `pasgclient->trapMask` so a `NOTRAPWRITE` rule — or a
    // rule with no trap option — is not reported to
    // put-logging listeners. Pre-fix Rust hard-coded
    // `rule_was_trap: true` for every accepted write.
    let rule_was_trap = write_grant.rule_was_trap();

    // Serialise against this channel's previous put-callback here — after
    // the SID/type/access checks and *before* any side effect (payload
    // conversion, trap-write `BeforeWrite` dispatch, the database/PV write,
    // or the async device kickoff). C `write_notify_action` reaches the
    // same boundary, after `rsrvCheckPut` and before `caNetConvert` /
    // `asTrapWriteWithData` / `dbProcessNotify`. This request proceeds on
    // every arm; only a predecessor past C's `blockSem` deadline is
    // cancelled and answered ECA_PUTCBINPROG, for its own ioid. The
    // deprecated fire-and-forget CA_PROTO_WRITE path is not serialised in
    // C, so it is left untouched.
    if is_notify {
        put_notify_slot.serialize(writer)?;
    }

    // The CLAMPED full 32-bit count flows to the decode, the put and the
    // reply echo (pre-fix the echo used `hdr.count`, which is the 0
    // marker for extended requests and therefore lost the count on large
    // array put-callbacks; then the raw wire count, which #934 clamps).
    let count = write_count as usize;

    // Whatever the buffer type carries, only the value reaches the
    // record. The metadata a compound put brings along is discarded —
    // as it is in C, and as it must be: `.TIME` is `DBF_NOACCESS` and
    // `STAT`/`SEVR`/`UTAG` are `SPC_NOMOD`, so no client can set them
    // through any path.
    //
    // The refusals below land HERE, at the put, rather than at the type
    // gate — which is why those gates kept their C-observable positions:
    // a compound WRITE_NOTIFY to a channel the peer cannot write must
    // still report ECA_NOWTACCESS from the access gate, must still
    // supersede the channel's in-flight put-callback (both done above),
    // and must still be bracketed by the trap-write pair below, as C's
    // unconditional `asTrapWriteWithData` does. ECA_PUTFAIL is C's answer
    // on either opcode — `send_err(mp, ECA_PUTFAIL, ...)` + RSRV_OK
    // (`camessage.c:812-820`) and `notifyError` → `ECA_PUTFAIL`
    // (`camessage.c:1417-1419`) — and neither drops the circuit.

    // A malformed payload is a protocol violation on every buffer type: C
    // sizes the frame against `dbr_size_n` of the WIRE type and returns
    // RSRV_ERROR before it looks at the put (`camessage.c:768-770`,
    // `:1700-1702`), and `caNetConvert` failure does the same. One closure so
    // that drop rule cannot fork between the native and compound decoders.
    let value_or_drop = |decoded: CaResult<EpicsValue>| -> CaResult<EpicsValue> {
        if decoded.is_ok() {
            return decoded;
        }
        if is_notify {
            // Same `putNotifyErrorReply` shape; count post-clamp, as C's
            // mutated `mp->m_count` is at this point.
            send_put_notify_response(
                writer,
                hdr.data_type,
                write_count,
                ECA_BADTYPE,
                ioid,
                ReplyContext {
                    req_hdr: *hdr,
                    client_minor: state.client_minor_version,
                },
            )?;
        } else {
            send_ca_error(
                writer,
                hdr,
                ECA_BADTYPE,
                entry_cid,
                "bad WRITE payload bytes",
                state.client_minor_version,
            )?;
        }
        Err(epics_base_rs::error::CaError::Protocol(format!(
            "WRITE payload conversion failed for type {} count {} (matches C caNetConvert RSRV_ERROR)",
            hdr.data_type, count
        )))
    };

    let plan = match wire_type.decode(hdr.data_type, payload, count) {
        // C's `default:` arm on either opcode; these buffers carry no value
        // member for the put-log to render.
        None => PutPlan::Refuse { logged_value: None },
        Some(decoded) => {
            let value = value_or_drop(decoded)?;
            if is_notify && wire_type == AcceptedWriteType::Compound {
                // `mapOldType` (`db_access.c:988`) maps only the native
                // types, so this one reaches `notifyError`.
                PutPlan::Refuse {
                    logged_value: Some(value),
                }
            } else {
                PutPlan::Write { value }
            }
        }
    };

    // Stringify the value once for the audit log; skipped when
    // audit is off. Use the truncated renderer so a malicious
    // peer can't pin the dispatch task on `format!`-ing a
    // peer-controlled array of millions of elements.
    //
    // TRAPWRITE listeners also need a string form. We
    // render once when *either* audit or a trap-write listener
    // is registered; the truncated form is cheap and lets
    // listeners avoid touching the raw `EpicsValue`.
    let trap_listeners_active = epics_base_rs::server::access_security::has_trap_write_listeners();
    let display_value = if state.audit.is_some() || trap_listeners_active {
        match &plan {
            PutPlan::Write { value, .. } => value.display_truncated(64),
            PutPlan::Refuse { logged_value } => logged_value
                .as_ref()
                .map(|v| v.display_truncated(64))
                .unwrap_or_default(),
        }
    } else {
        String::new()
    };

    // One RAII guard owns this put's BeforeWrite/AfterWrite
    // pair. `begin` fires BeforeWrite now; the matching
    // AfterWrite fires from `complete` (the synchronous paths
    // and the async completion task, once the real status is
    // known) or — if neither runs because the put was aborted
    // first, a superseding WRITE_NOTIFY or a client teardown
    // calling `abort` on the completion task — from the guard's
    // Drop. This makes the C invariant hold by construction:
    // every `asTrapWriteWithData` is matched by exactly one
    // `asTrapWriteAfter` on all rsrv exit paths — completion
    // (`camessage.c:1431`), still-busy teardown
    // (`rsrvFreePutNotify`, :1620), and supersede-cancel
    // (`write_notify_action`, :1700). Pre-fix the AfterWrite was
    // an explicit dispatch that the abort paths skipped, leaving
    // a BeforeWrite with no match in the put-log.
    //
    // BeforeWrite still sits here (not inside each write arm):
    // C `asTrapWriteWithData` (`camessage.c:799-802`) fires
    // before `dbChannel_put`, and narrowing the bracket into the
    // match arms would not remove the pre-storage over-log
    // (RecordField pre-rejections happen inside the called
    // function) without a deeper refactor.
    let mut trap_guard = trap_listeners_active.then(|| {
        epics_base_rs::server::access_security::TrapWriteGuard::begin(
            epics_base_rs::server::access_security::TrapWriteFields {
                pv_name: audit_pv.clone(),
                user: state.username.clone(),
                host: state.hostname.as_str().to_string(),
                peer: state.peer.clone(),
                value_str: display_value.clone(),
                // C `asTrapWriteWithData` (`camessage.c:799-802`) logs
                // `mp->m_dataType` — the type the client SENT, which for a
                // compound put differs from the base type the record took.
                dbr_type: hdr.data_type,
                no_elements: write_count,
                event_id: epics_base_rs::server::access_security::next_trap_write_event_id(),
                rule_was_trap,
                // C carries no status on the cancelled-put
                // `asTrapWriteAfter`; this Rust enrichment marks
                // the supersede / teardown tail so listeners can
                // tell it from a clean completion.
                cancel_status: "cancel".to_string(),
            },
        )
    });

    // The completion outcome of the synchronous head of this write —
    // `Sync` (reply inline) vs `Async(handle)` (spawn a completion task
    // that replies when the record's chain settles). A simple PV and a
    // fire-and-forget `CA_PROTO_WRITE` are always synchronous.
    //
    // `audit_result` is decided alongside the put rather than re-derived from
    // its error, so the reason C had for failing a refused buffer type — no
    // `dbChannel_put` arm, as opposed to a value the field rejected — survives
    // into both the audit line and the trap-write AfterWrite status.
    use epics_base_rs::server::record::ProcessCompletion;
    let (write_result, audit_result): (CaResult<ProcessCompletion>, &str) = match plan {
        PutPlan::Refuse { .. } => (
            Err(epics_base_rs::error::CaError::UnsupportedType(
                hdr.data_type,
            )),
            "dbr-type-not-puttable",
        ),
        PutPlan::Write { value: new_value } => {
            let result: CaResult<ProcessCompletion> = match &entry.target {
                ChannelTarget::SimplePv(pv) => {
                    if let Some(hook) = pv.write_hook() {
                        let ctx = epics_base_rs::server::pv::WriteContext {
                            user: state.username.clone(),
                            host: state.hostname.as_str().to_string(),
                            peer: state.peer.clone(),
                        };
                        hook(new_value, ctx).await.map(|()| ProcessCompletion::Sync)
                    } else {
                        pv.set(new_value);
                        Ok(ProcessCompletion::Sync)
                    }
                }
                ChannelTarget::RecordField { record, field } => {
                    let name = record.read().name.clone();
                    if is_notify {
                        db.put_record_field_from_ca(&name, field, new_value).await
                    } else {
                        // C `write_action` (`rsrv/camessage.c:781-789`)
                        // routes CA_PROTO_WRITE through `dbPutField` —
                        // no putNotify is ever built. Parking a wait-set
                        // whose receiver this fire-and-forget arm drops
                        // would occupy the record's notify slot until
                        // any async processing it starts settles (a
                        // motor's whole motion), failing every
                        // legitimate WRITE_NOTIFY on the record with
                        // ECA_PUTCBINPROG in the meantime.
                        db.put_record_field_from_ca_no_notify(&name, field, new_value)
                            .await
                            .map(|()| ProcessCompletion::Sync)
                    }
                }
            };
            let tag = if result.is_ok() { "ok" } else { "fail" };
            (result, tag)
        }
    };

    state.audit("caput", &audit_pv, &display_value, audit_result);

    // SYNCHRONOUS write paths (no async record completion
    // pending): fire AfterWrite now with the known status via
    // the guard. The async path instead hands the guard to the
    // completion task so AfterWrite reflects real device-side
    // completion timing (C `write_notify_reply:1400`).
    let needs_async_after = is_notify && matches!(&write_result, Ok(pc) if pc.is_async());
    if !needs_async_after {
        if let Some(guard) = &mut trap_guard {
            guard.complete(audit_result);
        }
    }

    // C `write_action` (`rsrv/camessage.c:812-820`):
    // even the deprecated fire-and-forget `CA_PROTO_WRITE`
    // surfaces a failed `dbChannel_put` to the client via
    // `send_err(mp, ECA_PUTFAIL, ...)`. Pre-fix Rust dropped
    // the failure silently for the non-notify path, so a
    // `caput` against a read-only-by-rule field that bypassed
    // earlier access checks (e.g. record-side `PutDisabled`)
    // looked successful to the libca peer even though the
    // value never reached the DB. is_notify already replies
    // via WRITE_NOTIFY below.
    if !is_notify {
        if let Err(e) = &write_result {
            let eca = PutStatus::of_failure(e);
            send_ca_error(
                writer,
                hdr,
                eca.eca(),
                entry_cid,
                &audit_pv,
                state.client_minor_version,
            )?;
        }
    }

    // CA_PROTO_WRITE (cmd=4) is fire-and-forget — no response. A
    // WRITE_NOTIFY replies inline when the write settled synchronously;
    // when the record's process cycle went async it hands the completion
    // receiver back to the caller. C `write_notify_action` never blocks
    // the message thread on the put-callback — completion is delivered
    // later (`dbNotify.c` callDone on a background thread) — so the
    // caller decides HOW to await it (async server: a spawned task;
    // blocking driver: its event thread).
    if is_notify {
        let eca_status = match &write_result {
            Ok(_) => PutStatus::OK,
            Err(e) => PutStatus::of_failure(e),
        }
        .eca();
        // `Async(rx)` ⟹ the record's chain is still running: return the
        // handle. `Sync` and any `Err` ⟹ reply inline now (an error
        // carries no completion to await).
        let completion_rx = write_result.ok().and_then(ProcessCompletion::into_handle);
        if let Some(rx) = completion_rx {
            let reply = WriteNotifyReply {
                // C `write_notify_reply` (`camessage.c:1417-1434`) frames the
                // completion from the saved request header, so the echoed type
                // is always the one the client SENT — the same value as the
                // native type for a native put, and the only available answer
                // for the compound and refused ones.
                write_type: hdr.data_type,
                write_count,
                ioid,
                req_hdr: *hdr,
                client_minor: state.client_minor_version,
            };
            // The trap-write bracket moves into the completion token rather
            // than travelling with the receiver, so that whichever path
            // answers this ioid closes it — C keeps `asWritePvt` on
            // `pciu->pPutNotify` for the same reason.
            let completion = PutNotifyCompletion::new(trap_guard.take());
            // Register HERE, in the head both receive loops run, not in the
            // caller: an unregistered channel silently opts out of the
            // serialisation above, which is exactly how the two loops came to
            // answer a concurrent put-callback differently.
            put_notify_slot.install(InFlightPutNotify {
                completion: completion.clone(),
                reply,
                busy_since: std::time::Instant::now(),
            });
            return Ok(WriteHeadOutcome::AsyncPending(PendingWriteNotify {
                rx,
                eca_status,
                reply,
                completion,
                sid,
            }));
        }
        // Synchronous completion — respond immediately. Same echoed type as
        // the deferred reply above: C frames both from the request header.
        send_put_notify_response(
            writer,
            hdr.data_type,
            write_count,
            eca_status,
            ioid,
            ReplyContext {
                req_hdr: *hdr,
                client_minor: state.client_minor_version,
            },
        )?;
    }
    Ok(WriteHeadOutcome::Done)
}

/// Everything the deferred WRITE_NOTIFY completion reply echoes: the framed
/// type/count, the request ioid, and the request header + negotiated version
/// (for extended-form promotion). One value so the shared completion tail and
/// the in-flight-slot install pass a single descriptor rather than five loose
/// scalars. `Copy` (like the loose scalars it replaces) so it can be captured
/// by an `async move` completion task and still be read by the in-flight-slot
/// install afterwards.
#[derive(Clone, Copy)]
pub(crate) struct WriteNotifyReply {
    pub write_type: u16,
    pub write_count: u32,
    pub ioid: u32,
    pub req_hdr: CaHeader,
    pub client_minor: u16,
}

/// Send a WRITE_NOTIFY completion reply and fire the deferred trap-write
/// AfterWrite — the single shared tail both the async server's completion
/// task and the blocking driver's event thread run once the record's chain
/// settles. `final_status` is the put's real completion status
/// (`eca_status` on success, `ECA_PUTFAIL` if the completion sender was
/// dropped). Keeping this in one place keeps the deferred-reply bytes and the
/// put-log AfterWrite status identical across both front-ends.
pub(crate) fn finish_write_notify(
    trap_guard: &mut Option<epics_base_rs::server::access_security::TrapWriteGuard>,
    final_status: u32,
    reply: &WriteNotifyReply,
    writer: &Outbox,
) -> CaResult<()> {
    // AfterWrite at real device-side completion. `status` carries "ok" for
    // ECA_NORMAL or the ECA-code form otherwise so listeners can filter
    // failed puts. `complete` disarms the guard so its later Drop is a no-op.
    if let Some(guard) = trap_guard {
        let status_s = if final_status == ECA_NORMAL {
            "ok".to_string()
        } else {
            format!("eca:0x{:04x}", final_status)
        };
        guard.complete(&status_s);
    }
    send_put_notify_response(
        writer,
        reply.write_type,
        reply.write_count,
        final_status,
        reply.ioid,
        ReplyContext {
            req_hdr: reply.req_hdr,
            client_minor: reply.client_minor,
        },
    )
}

/// The sync-vs-async fork [`serve_write_head`] returns (C `dbProcessNotify`).
pub(crate) enum WriteHeadOutcome {
    /// Fully handled; any reply/error is already queued to the outbox.
    Done,
    /// A WRITE_NOTIFY whose record chain is still running; the caller awaits
    /// the receiver and replies via [`finish_write_notify`].
    AsyncPending(PendingWriteNotify),
}

/// One in-flight async `CA_PROTO_WRITE_NOTIFY` handed back by
/// [`serve_write_head`] for the caller to await. The channel registration is
/// already done by the head, so all a receive loop owes this value is
/// [`settle`](Self::settle) once `rx` reports. `sid` is the async server's
/// connection-scoped task tracking (`CA_PROTO_CLEAR_CHANNEL` drains by it);
/// the blocking driver ignores it and awaits `rx` on its event thread.
pub(crate) struct PendingWriteNotify {
    pub rx: tokio::sync::oneshot::Receiver<()>,
    pub eca_status: u32,
    pub reply: WriteNotifyReply,
    completion: Arc<PutNotifyCompletion>,
    pub sid: u32,
}

impl PendingWriteNotify {
    /// Answer this put-callback now that its record chain has reported — the
    /// ONE completion tail both receive loops run, so neither can decide on
    /// its own whether a reply is still owed.
    ///
    /// `chain` is what the completion receiver yielded: `Err` means the sender
    /// was dropped without firing (processing aborted), which surfaces as
    /// ECA_PUTFAIL so the client never sees a false success — C rsrv.
    pub(crate) fn settle(
        &mut self,
        chain: Result<(), tokio::sync::oneshot::error::RecvError>,
        writer: &Outbox,
    ) -> CaResult<()> {
        // Losing the claim means a predecessor-timeout supersede already
        // answered this ioid with ECA_PUTCBINPROG and closed its put-log
        // bracket with it. One ioid, one reply.
        let Some(mut owned) = self.completion.claim() else {
            return Ok(());
        };
        let final_status = match chain {
            Ok(()) => self.eca_status,
            Err(_) => ECA_PUTFAIL,
        };
        finish_write_notify(&mut owned.0, final_status, &self.reply, writer)
    }
}

fn get_full_snapshot(target: &ChannelTarget) -> Option<epics_base_rs::server::snapshot::Snapshot> {
    match target {
        ChannelTarget::SimplePv(pv) => Some(pv.snapshot()),
        ChannelTarget::RecordField { record, field } => record.read().snapshot_for_field(field),
    }
}

/// Snapshot for a one-shot client GET (`CA_PROTO_READ` /
/// `CA_PROTO_READ_NOTIFY`).
///
/// Distinct from [`get_full_snapshot`] (used for monitor initial events
/// and access-rights re-posts) so only the *client GET* path consults a
/// PV's optional [`ReadHook`](epics_base_rs::server::pv::ReadHook): the
/// CA gateway's no-cache mode installs that hook to forward each
/// downstream read to a fresh upstream fetch. For a PV without a read
/// hook — every record-backed and cached PV — this is exactly
/// `get_full_snapshot` wrapped in `Ok`. The `Err` propagates an upstream
/// get failure so the GET handler can answer `ECA_GETFAIL`, matching C
/// ca-gateway forwarding the read to the IOC under `-no_cache`
/// (`gateVc.cc:1361-1369`).
async fn get_read_snapshot(
    target: &ChannelTarget,
) -> Result<Option<epics_base_rs::server::snapshot::Snapshot>, epics_base_rs::error::CaError> {
    match target {
        ChannelTarget::SimplePv(pv) => pv.read_snapshot().await.map(Some),
        ChannelTarget::RecordField { record, field } => Ok(record.read().snapshot_for_field(field)),
    }
}

/// How [`register_subscription`] wires delivery of a freshly-registered
/// subscription. The async TCP server spawns a producer task per subscription;
/// the blocking (RTEMS) driver has no async runtime, so it takes the raw
/// [`EventReader`](epics_base_rs::server::event_queue::EventReader) and multiplexes every subscription on one event thread.
#[derive(Clone, Copy)]
pub(crate) enum SubscriptionDelivery {
    /// C `event_add_action` async path: spawn the producer task.
    AsyncSpawn,
    /// Blocking driver: return the reader; the caller drives delivery.
    HandOff,
}

/// The result of [`register_subscription`].
pub(crate) enum SubscriptionOutcome {
    /// A refusal frame (CA_PROTO_ERROR) was already queued; the caller returns
    /// without registering anything. C's admission-failure branches.
    Refused,
    /// [`SubscriptionDelivery::AsyncSpawn`]: the producer task is running; the
    /// caller records it in its subscription map.
    Spawned(SpawnedSubscription),
    /// [`SubscriptionDelivery::HandOff`]: the live reader plus the metadata the
    /// caller needs to frame deliveries; no task was spawned.
    HandedOff(RegisteredSubscription),
}

/// Everything the async caller needs to record a spawned subscription and emit
/// its `SubscriptionOpened` event.
pub(crate) struct SpawnedSubscription {
    pub task: epics_base_rs::runtime::task::TaskHandle<()>,
    pub target: ChannelTarget,
    pub channel_sid: u32,
    pub sub_id: u32,
    pub data_type: u16,
    pub data_count: u32,
    pub denied: Arc<AtomicBool>,
    pub long_string_mode: LongStringMode,
    pub sub_pv_name: String,
    pub mask: u16,
}

/// A registered-but-not-spawned subscription handed to the blocking driver's
/// event thread. Carries the live [`EventReader`](epics_base_rs::server::event_queue::EventReader) plus everything
/// [`run_event_task`] needs to frame deliveries byte-identically to the async
/// producer.
pub(crate) struct RegisteredSubscription {
    pub reader: epics_base_rs::server::event_queue::EventReader,
    pub target: ChannelTarget,
    pub channel_sid: u32,
    pub sub_id: u32,
    pub data_type: u16,
    pub data_count: u32,
    pub denied: Arc<AtomicBool>,
    pub long_string_mode: LongStringMode,
    pub client_minor: u16,
    pub stats: Option<Arc<super::stats::ServerStats>>,
}

/// The subscription metadata [`cancel_subscription_reply`] needs, looked up by
/// the caller in its own registry (`state.subscriptions` for async,
/// the blocking-side map for the RTEMS driver).
pub(crate) struct CancelInfo {
    pub channel_sid: u32,
    pub data_type: u16,
    pub data_count: u32,
}

/// Validate and register a CA subscription (C `event_add_action`,
/// `rsrv/camessage.c:1812-1920`): the caps / dedup / DBR-type / mask /
/// PR-#934 count-clamp / access / filter-chain parity logic plus the
/// initial `db_post_single_event` snapshot, extracted so the async TCP
/// dispatch and the blocking (RTEMS) driver share ONE copy. It performs
/// no `state.subscriptions` mutation and emits no connection events — the
/// caller owns those. In [`SubscriptionDelivery::AsyncSpawn`] mode it spawns
/// the monitor producer task and returns [`SubscriptionOutcome::Spawned`];
/// in [`SubscriptionDelivery::HandOff`] mode it returns the live
/// [`EventReader`](epics_base_rs::server::event_queue::EventReader) as [`SubscriptionOutcome::HandedOff`] before spawning, so
/// the blocking driver's single event thread can multiplex it (the driver
/// has no async runtime to spawn onto). `channel_sub_count` and
/// `sub_id_in_use` are read from the caller's own subscription registry so
/// the per-channel cap and duplicate-sub-id refusal consult the right map.
pub(crate) async fn register_subscription(
    hdr: &CaHeader,
    payload: &[u8],
    state: &ClientState,
    writer: &Outbox,
    mode: SubscriptionDelivery,
    channel_sub_count: impl Fn() -> usize,
    sub_id_in_use: bool,
) -> CaResult<SubscriptionOutcome> {
    let sid = hdr.cid;
    let sub_id = hdr.available;
    let requested_type = hdr.data_type;
    // store the request's element count so each monitor
    // delivery and the EVENT_CANCEL ack can echo it (matches
    // C `event_add_action` capturing `pevext->msg` for later
    // `read_reply` / `event_cancel_reply` use).
    let mut requested_count = hdr.actual_count();

    // DoS guard: cap subscriptions per channel. Default-unbounded
    // (`None`) — C `event_add_action` imposes no per-channel
    // subscription count limit (see `max_subs_per_channel`). The
    // O(n) count is only paid when an opt-in cap is configured.
    if let Some(cap) = max_subs_per_channel() {
        let subs_for_channel = channel_sub_count();
        if subs_for_channel >= cap {
            // C `event_add_action` sends admission
            // failures through `send_err(ECA_ALLOCMEM, ...)`
            // i.e. CA_PROTO_ERROR — libca's
            // `cac::eventRespAction` returns immediately for
            // zero-payload EVENT_ADD because that shape is the
            // historical cancel-confirmation no-op. Pre-fix
            // Rust used `send_cmd_error` which emits zero-
            // payload EVENT_ADD, so a libca client treated the
            // refusal as a cancel ack and waited forever for
            // monitor updates that never arrived. Use
            // CA_PROTO_ERROR so the exception path fires.
            let entry_cid = state.channels.get(&sid).map(|e| e.cid).unwrap_or(u32::MAX);
            send_ca_error(
                writer,
                hdr,
                ECA_ALLOCMEM,
                entry_cid,
                "EVENT_ADD refused: per-channel subscription cap",
                state.client_minor_version,
            )?;
            return Ok(SubscriptionOutcome::Refused);
        }
    }

    let native_type = match native_type_for_dbr(requested_type) {
        Ok(t) => t,
        Err(_) => {
            // C `event_add_action` (camessage.c:1819-1821):
            // `INVALID_DB_REQ` (data_type > LAST_BUFFER_TYPE = 38)
            // returns RSRV_ERROR with NO error reply — the
            // connection just drops. Unlike WRITE / READ where
            // C emits CA_PROTO_ERROR + drops, EVENT_ADD is
            // silent. Match that wire shape: no send, just
            // disconnect. Clients see EOF without an ECA hint;
            // this matches C IOC behaviour exactly.
            return Err(epics_base_rs::error::CaError::Protocol(format!(
                "EVENT_ADD with unsupported DBR type {} (matches C event_add_action silent drop)",
                requested_type
            )));
        }
    };

    // C `event_add_action` (epics-base #934, 4128a7c07):
    // `INVALID_DB_REQ(m_dataType) || m_postsize < sizeof(*pmi)` is ONE
    // silent pre-lookup gate — a truncated `struct mon_info` (16 bytes:
    // three f32 dead-band fields, u16 mask, u16 pad) is RSRV_ERROR with
    // no reply frame, exactly like the bad-type arm above. Pre-fix a
    // short payload was accepted with a DBE_VALUE|DBE_ALARM default
    // mask C never applies.
    if payload.len() < 16 {
        return Err(epics_base_rs::error::CaError::Protocol(format!(
            "EVENT_ADD with truncated mon_info payload ({} < 16 bytes; matches C event_add_action silent drop)",
            payload.len()
        )));
    }
    let mask = u16::from_be_bytes([payload[12], payload[13]]);
    let entry = match state.channels.get(&sid) {
        Some(e) => e,
        None => {
            // C `event_add_action` (camessage.c:1823-1827):
            // `logBadId` + RSRV_ERROR on missing channel.
            // `logBadId` emits an ECA_INTERNAL "Bad Resource ID"
            // frame (cid=0xFFFFFFFF) before the disconnect — the
            // genuinely-silent EVENT_ADD path is only the
            // pre-lookup INVALID_DB_REQ (bad-TYPE) branch above,
            // which returns RSRV_ERROR with no send. This MUST run
            // before the mask==0 ALLOCMEM check below: in C the
            // missing-channel branch precedes the `db_add_event`
            // NULL (select==0) path, so an unknown SID draws the
            // ECA_INTERNAL frame regardless of mask.
            send_ca_error(
                writer,
                hdr,
                ECA_INTERNAL,
                0xFFFF_FFFF,
                "Bad Resource ID",
                state.client_minor_version,
            )?;
            return Err(epics_base_rs::error::CaError::Protocol(format!(
                "EVENT_ADD on unknown SID {} (matches C event_add_action logBadId + RSRV_ERROR)",
                sid
            )));
        }
    };

    // PR #934 (epics-base) parity: clamp the wire element count to
    // the channel's final element count BEFORE it is stored on the
    // subscription (`SubscriptionEntry::data_count`), so neither the
    // initial snapshot (`size_dbr_reply`) nor every
    // steady-state monitor delivery (the producer in `monitor.rs`)
    // can zero-fill past the channel's real capacity. C
    // `event_add_action`:
    // `if (mp->m_count > dbChannelFinalElements(pciu->dbch))
    //     mp->m_count = dbChannelFinalElements(pciu->dbch);`.
    // `requested_count == 0` is autosize and is preserved untouched.
    if requested_count != 0 && requested_count > entry.final_element_count {
        requested_count = entry.final_element_count;
    }

    // C `db_add_event` (dbEvent.c:437-439) returns NULL when
    // `select == 0 || select > UCHAR_MAX`, which propagates as
    // ECA_ALLOCMEM + disconnect (`camessage.c:1866-1877`). A zero mask
    // installs a subscription that never triggers; a mask above
    // UCHAR_MAX (the CA wire mask is a `u16`, so 256..=65535 is
    // reachable) is not a valid event select. Reject both immediately.
    // This is the `db_add_event` NULL path, which in C only runs for
    // a *valid* channel (after the missing-channel check above), so
    // `entry.cid` is always known here — no `u32::MAX` fallback.
    if mask == 0 || mask > u16::from(u8::MAX) {
        let entry_cid = entry.cid;
        send_ca_error(
            writer,
            hdr,
            ECA_ALLOCMEM,
            entry_cid,
            &format!("EVENT_ADD invalid mask {mask}: must be 1..={}", u8::MAX),
            state.client_minor_version,
        )?;
        return Err(epics_base_rs::error::CaError::Protocol(
                    "EVENT_ADD invalid mask (matches C db_add_event select==0 || select>UCHAR_MAX + RSRV_ERROR)".into(),
                ));
    }
    // Captured up front so the SubscriptionOpened event we
    // emit after a successful insert below doesn't have to
    // re-borrow `state.channels` (the insert path mutates
    // `state.subscriptions` so the entry borrow has to be
    // released before then).
    let sub_pv_name = entry.pv_name.clone();
    let long_string_mode = entry.long_string_mode;

    // EVENT_ADD must also consult the
    // channel's access_rights. A NoAccess peer mounting a
    // subscription would receive every value update —
    // identical leak to the `subscribe_raw` ACF
    // bypass on the PVA side. C IOC's `event_add_NoAccess`
    // returns ECA_NORDACCESS for the same reason.
    // Type-state EVENT_ADD gate. This closed the
    // missing per-op check; the typed `require_read` shape
    // is the path every future MONITOR-class op should
    // mirror.
    // C `event_add_action` (`rsrv/camessage.c:1812-1920`)
    // installs the event unconditionally and conditionally
    // enables it via `db_event_enable` only when
    // `asCheckGet(pciu->asClientPVT)` allows reads; on no-read
    // access the subscription stays installed but disabled
    // and the initial event is `no_read_access_event`. Pre-fix
    // Rust returned `ECA_NORDACCESS` here without installing —
    // a subscription opened while denied was permanently
    // absent, so a later ACF reload that granted access could
    // not re-arm anything. Capture access as a flag and let
    // the install path below populate the `denied` gate so
    // `reeval_access_rights` can flip it later (Bug 4 parity).
    let access_denied = state.lookup_access(sid).require_read().is_err();

    // Refuse a duplicate sub_id on the same connection. Without
    // this, two EVENT_ADDs with identical sub_id leave both
    // subscribers attached to the producer (push without
    // dedup); EVENT_CANCEL strips both at once via retain, but
    // until then every event delivery emits two wire frames —
    // archived data + dashboard counts duplicated.
    if sub_id_in_use {
        tracing::warn!(
            sub_id,
            "EVENT_ADD refused: sub_id already in use on this connection"
        );
        // use CA_PROTO_ERROR (libca exception path)
        // instead of zero-payload EVENT_ADD which
        // `cac::eventRespAction` treats as a cancel-ack
        // no-op. The libca peer
        // otherwise silently swallows the refusal and
        // waits forever for monitor updates.
        send_ca_error(
            writer,
            hdr,
            ECA_BADMONID,
            entry.cid,
            "duplicate sub_id",
            state.client_minor_version,
        )?;
        return Ok(SubscriptionOutcome::Refused);
    }
    {
        match &entry.target {
            ChannelTarget::SimplePv(pv) => {
                let rx_opt = pv.add_subscriber_on(&state.event_user, sub_id, native_type, mask);
                let Some(rx) = rx_opt else {
                    // per-PV subscriber cap reached.
                    // Previously dropped silently
                    // (let the client time out). Now sends
                    // ECA_ALLOCMEM so the client surfaces the
                    // refusal immediately and can fall back to
                    // a different transport, retry strategy,
                    // or operator alert. Mirrors the
                    // already-existing per-channel-cap response
                    // a few lines above — same ECA code, same
                    // shape.
                    tracing::warn!(
                        pv = %pv.name,
                        sub_id,
                        "EVENT_ADD refused: PV subscriber cap reached"
                    );
                    // CA_PROTO_ERROR for the
                    // admission failure (see comment above
                    // on the per-channel cap branch).
                    send_ca_error(
                        writer,
                        hdr,
                        ECA_ALLOCMEM,
                        entry.cid,
                        "EVENT_ADD refused: per-PV subscriber cap",
                        state.client_minor_version,
                    )?;
                    return Ok(SubscriptionOutcome::Refused);
                };

                // attach the channel filter chain to the
                // just-added SimplePv subscriber so update delivery
                // (`ProcessVariable::notify_subscribers`) runs the
                // SAME chain as a record-field monitor. Pre-fix a
                // `SimplePv` monitor on a `.{...}` channel always
                // used the empty default chain — the filter suffix
                // was ignored entirely. Symmetric with the
                // record-field `attach_filter_to_last_subscriber`
                // path below; both source the chain from the single
                // `ChannelEntry::filter_chain` owner.
                pv.attach_filters_to_subscriber(sub_id, entry.filter_chain());

                let denied = Arc::new(AtomicBool::new(access_denied));
                // initial event is the snapshot when read
                // access is granted, `no_read_access_event` when
                // denied (C `event_add_action` → `read_reply`
                // routes denial through `no_read_access_event`,
                // `rsrv/camessage.c:534-540`).
                if access_denied {
                    // an autosize (`count == 0`) request
                    // must be normalised to the target's live
                    // element count before sizing the zero-
                    // filled denial payload. C `read_reply`
                    // (`camessage.c:509-514`) maps `m_count==0`
                    // to `paddr->no_elements`; the denial frame
                    // must match so it carries a nonzero DBR
                    // body. A zero-payload `CA_PROTO_EVENT_ADD`
                    // is indistinguishable from the historical
                    // cancel-ack no-op and is silently dropped
                    // by the client before the `ECA_NORDACCESS`
                    // status is read (`cac.cpp` eventRespAction
                    // returns on `m_postsize == 0`).
                    // C calls `db_post_single_event`
                    // unconditionally at monitor creation
                    // (`camessage.c:1907`, BEFORE the access
                    // check at 1858), so even the initial
                    // DENIED post runs through the event-context
                    // pre-chain; the ECA_NORDACCESS frame is
                    // gated by it (`db_queue_event_log` fires
                    // only `if(pLog)`). Skip the frame when the
                    // chain drops the post — the subscription is
                    // still registered below.
                    let snap = pv.snapshot();
                    if entry
                        .filter_chain()
                        .apply_to_event_value(snap.value.clone())
                        .is_some()
                    {
                        let denied_count =
                            no_read_access_count(requested_count, snap.value.count());
                        send_no_read_access_event(
                            writer,
                            CA_PROTO_EVENT_ADD,
                            requested_type,
                            denied_count,
                            sub_id,
                            ECA_NORDACCESS,
                            ReplyContext {
                                req_hdr: *hdr,
                                client_minor: state.client_minor_version,
                            },
                        )?;
                    }
                } else {
                    let mut snap = pv.snapshot();
                    // the initial monitor event is a
                    // CA monitor single-event post (C
                    // `db_post_single_event` →
                    // `db_create_event_log` with
                    // `dbfl_context_event`), NOT a one-shot
                    // read. Run the EVENT-context chain
                    // (`dec`/`sync` DO decimate/gate, unlike
                    // `READ`) on a fresh throwaway chain so the
                    // subscriber's attached chain state stays
                    // isolated (`dbnd` baseline / `dec`
                    // counter). `None` means the chain dropped
                    // the post (C `db_queue_event_log` fires
                    // only `if(pLog)`), so send no initial
                    // frame — never fall back to the unfiltered
                    // value.
                    let init_chain = entry.filter_chain();
                    match init_chain.apply_to_event_value(snap.value.clone()) {
                        Some(v) => {
                            snap.value = v;
                            // long-string boundary conversion
                            // (`$` → CHAR[40], or native record field
                            // → scalar DBR_STRING); no-op otherwise.
                            super::apply_long_string_mode(&mut snap, long_string_mode);
                            // the initial event honours
                            // the EVENT_ADD request count for
                            // BOTH directions —
                            // `send_monitor_snapshot` now pads
                            // when `requested_count` exceeds
                            // the live element count and
                            // truncates when it is smaller, via
                            // `size_dbr_reply` (C
                            // `read_reply` parity). The
                            // producer task already
                            // pads/truncates future updates
                            // through the same helper, so the
                            // initial frame and later frames
                            // now share one shape.
                            send_monitor_snapshot(
                                writer,
                                sub_id,
                                requested_type,
                                requested_count,
                                &snap,
                                ReplyContext {
                                    req_hdr: *hdr,
                                    client_minor: state.client_minor_version,
                                },
                            )?;
                            // Initial subscription value — C posts
                            // it via `db_post_single_event` at
                            // monitor creation (`camessage.c:1907`),
                            // so it counts as one posted and one
                            // processed subscription event (PCAS
                            // parity). Future updates flow through
                            // the monitor task below.
                            if let Some(ref s) = state.stats {
                                s.subscription_events_posted
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                s.subscription_events_processed
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                        None => {}
                    }
                }

                if let SubscriptionDelivery::HandOff = mode {
                    return Ok(SubscriptionOutcome::HandedOff(RegisteredSubscription {
                        reader: rx,
                        target: ChannelTarget::SimplePv(pv.clone()),
                        channel_sid: sid,
                        sub_id,
                        data_type: requested_type,
                        data_count: requested_count,
                        denied: denied.clone(),
                        long_string_mode,
                        client_minor: state.client_minor_version,
                        stats: state.stats.clone(),
                    }));
                }
                let task = spawn_monitor_sender(
                    sub_id,
                    requested_type,
                    requested_count,
                    writer.clone(),
                    rx,
                    denied.clone(),
                    long_string_mode,
                    state.stats.clone(),
                    ReplyContext {
                        req_hdr: *hdr,
                        client_minor: state.client_minor_version,
                    },
                );

                Ok(SubscriptionOutcome::Spawned(SpawnedSubscription {
                    task,
                    target: ChannelTarget::SimplePv(pv.clone()),
                    channel_sid: sid,
                    sub_id,
                    data_type: requested_type,
                    data_count: requested_count,
                    denied,
                    long_string_mode,
                    sub_pv_name: sub_pv_name.clone(),
                    mask,
                }))
            }
            ChannelTarget::RecordField { record, field } => {
                // Guarded segment: register the record-field subscriber,
                // attach its filter chain, and snapshot the initial (or
                // denial) event. The data guard is released at the block
                // close before the writer awaits below (parking_lot guards
                // are `!Send`); `None` means the subscriber cap was reached.
                let registered = 'registered: {
                    let mut instance = record.write();
                    let Some(rx) = instance.add_subscriber_on(
                        &state.event_user,
                        field,
                        sub_id,
                        native_type,
                        mask,
                    ) else {
                        // record-field subscriber cap reached.
                        // Symmetric with the SimplePv path; send
                        // ECA_ALLOCMEM so the client surfaces the
                        // refusal instead of timing out silently.
                        tracing::warn!(
                            record = %instance.name,
                            field = %field,
                            sub_id,
                            "EVENT_ADD refused: record-field subscriber cap reached"
                        );
                        break 'registered None;
                    };

                    // epics-base 3.15.7 channel filter — attach the
                    // chain (parsed via the single
                    // `ChannelEntry::filter_chain` owner, the same one
                    // the READ path and the SimplePv monitor now use)
                    // to the just-registered subscriber. The parser is
                    // permissive: malformed JSON or unknown filters
                    // degrade gracefully to an empty chain with a
                    // tracing::warn!, so an empty chain is a no-op loop.
                    for filt in entry.filter_chain().iter() {
                        instance.attach_filter_to_last_subscriber(field, filt.clone());
                    }

                    // snapshot when read access granted,
                    // no_read_access_event when denied. Drop the
                    // instance write lock before await on the
                    // writer so the producer task can pick it up.
                    //
                    // even on the denied path we must read
                    // the field's live element count under the
                    // lock, so an autosize (`count == 0`) denial
                    // frame can be sized to a nonzero DBR body
                    // instead of the zero-payload cancel-ack shape.
                    let initial_snap = if access_denied {
                        None
                    } else {
                        instance.snapshot_for_field(field).and_then(|mut snap| {
                            if requested_type == epics_base_rs::types::DBR_CLASS_NAME {
                                snap.class_name = Some(instance.record.record_type().to_string());
                            }
                            // the initial record-field monitor
                            // event is an EVENT-context single-event
                            // post (see the SimplePv branch) — run the
                            // event-context chain (`dec`/`sync` apply)
                            // on a fresh throwaway chain so the
                            // subscriber's attached chain state stays
                            // isolated. `None` means the chain dropped
                            // the post (C `db_queue_event_log` fires
                            // only `if(pLog)`); fold it through so the
                            // send below is skipped — never fall back
                            // to the unfiltered value.
                            let init_chain = entry.filter_chain();
                            match init_chain.apply_to_event_value(snap.value.clone()) {
                                Some(v) => {
                                    snap.value = v;
                                    // long-string boundary conversion
                                    // (`$` → CHAR[40], or native record
                                    // field → scalar DBR_STRING).
                                    super::apply_long_string_mode(&mut snap, long_string_mode);
                                    Some(snap)
                                }
                                None => None,
                            }
                        })
                    };
                    // Derive the field's element
                    // count for the autosize-denial frame AND run
                    // the event-context chain under the lock (the
                    // value is needed for both). `snapshot_for_field`
                    // is the same accessor the granted path uses, so
                    // the denial count matches what a granted monitor
                    // on the same field would carry. C
                    // `event_add_action` calls `db_post_single_event`
                    // unconditionally (`camessage.c:1907`, before the
                    // access check), so the DENIED initial post is
                    // gated by the event-context chain too:
                    // `Some(count)` => send the ECA_NORDACCESS frame,
                    // `None` => the chain dropped the post, send
                    // nothing. A missing field snapshot keeps the
                    // prior count=1 fallback (no value to filter).
                    let denied_event_count = if access_denied {
                        match instance.snapshot_for_field(field) {
                            Some(snap) => {
                                let count = snap.value.count();
                                entry
                                    .filter_chain()
                                    .apply_to_event_value(snap.value)
                                    .map(|_| count)
                            }
                            None => Some(1),
                        }
                    } else {
                        None
                    };
                    Some((rx, initial_snap, denied_event_count))
                };
                // Release the data guard, then handle the refusal (send the
                // ECA_ALLOCMEM error and return) or bind the registered
                // subscriber for the writer awaits below.
                let (rx, initial_snap, denied_event_count) = match registered {
                    None => {
                        // CA_PROTO_ERROR for admission failure (libca's
                        // eventRespAction treats zero-payload EVENT_ADD as a
                        // cancel ack, so the prior send_cmd_error path lost).
                        send_ca_error(
                            writer,
                            hdr,
                            ECA_ALLOCMEM,
                            entry.cid,
                            "EVENT_ADD refused: record-field subscriber cap",
                            state.client_minor_version,
                        )?;
                        return Ok(SubscriptionOutcome::Refused);
                    }
                    Some(t) => t,
                };
                if access_denied {
                    // normalise autosize before sizing
                    // the zero-filled denial payload. See the
                    // SimplePv branch above for the C
                    // `read_reply` (`camessage.c:507-509`)
                    // parity rationale.
                    if let Some(field_count) = denied_event_count {
                        let denied_count = no_read_access_count(requested_count, field_count);
                        send_no_read_access_event(
                            writer,
                            CA_PROTO_EVENT_ADD,
                            requested_type,
                            denied_count,
                            sub_id,
                            ECA_NORDACCESS,
                            ReplyContext {
                                req_hdr: *hdr,
                                client_minor: state.client_minor_version,
                            },
                        )?;
                    }
                } else if let Some(snap) = initial_snap {
                    // initial event honours the
                    // EVENT_ADD request count in both
                    // directions — `send_monitor_snapshot`
                    // pads an over-requested count and
                    // truncates an under-requested one via
                    // `size_dbr_reply`.
                    send_monitor_snapshot(
                        writer,
                        sub_id,
                        requested_type,
                        requested_count,
                        &snap,
                        ReplyContext {
                            req_hdr: *hdr,
                            client_minor: state.client_minor_version,
                        },
                    )?;
                    // Initial subscription value posted and
                    // processed (C `db_post_single_event` at
                    // monitor creation, `camessage.c:1907`); PCAS
                    // parity. Later updates flow through the
                    // monitor task below.
                    if let Some(ref s) = state.stats {
                        s.subscription_events_posted
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        s.subscription_events_processed
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }

                // Clone the outbox handle for the RecordField monitor
                // task: it pushes framed EVENT_ADD deliveries into the
                // per-connection outbox, never touching the socket.
                let denied = Arc::new(AtomicBool::new(access_denied));
                if let SubscriptionDelivery::HandOff = mode {
                    return Ok(SubscriptionOutcome::HandedOff(RegisteredSubscription {
                        reader: rx,
                        target: ChannelTarget::RecordField {
                            record: record.clone(),
                            field: field.clone(),
                        },
                        channel_sid: sid,
                        sub_id,
                        data_type: requested_type,
                        data_count: requested_count,
                        denied: denied.clone(),
                        long_string_mode,
                        client_minor: state.client_minor_version,
                        stats: state.stats.clone(),
                    }));
                }
                let outbox_clone = writer.clone();
                let record_for_task = record.clone();
                let denied_for_task = denied.clone();
                let stats_for_task = state.stats.clone();
                // C stores the EVENT_ADD request (`pevext->msg`) with
                // the subscription: every later delivery is framed for
                // this client's negotiated version, and echoes this
                // header if the frame turns out to be unbuildable.
                let client_minor = state.client_minor_version;
                let req_hdr = *hdr;
                let task = epics_base_rs::runtime::task::spawn(async move {
                    let mut reader = rx;
                    loop {
                        // C `event_read` on this circuit's queue — the
                        // same single owner `spawn_monitor_sender` uses,
                        // so a pause means the same thing on both monitor
                        // paths: suspend only while `flowCtrlMode &&
                        // nDuplicates == 0`, otherwise drain. A post that
                        // arrived while the ring was short of room
                        // replaced this monitor's LAST queued entry in
                        // place, so the earlier distinct entries are
                        // still queued and each goes out as its own
                        // frame.
                        let Some(mut event) = reader.recv().await else {
                            break;
                        };
                        // One subscription update committed for
                        // delivery this cycle (post-coalesce).
                        // PCAS `subscriptionEventsPosted` parity —
                        // counted before the read-access gate so a
                        // suppressed delivery reads as
                        // posted-but-not-processed, the same
                        // `serverPostRate` > `serverEventRate`
                        // divergence the gateway expects.
                        if let Some(ref s) = stats_for_task {
                            s.subscription_events_posted.fetch_add(1, Ordering::Relaxed);
                        }
                        // C `casAccessRightsCB`
                        // (`rsrv/camessage.c:1116-1124`)
                        // suppresses delivery via
                        // `db_event_disable` while read access
                        // is denied, without tearing the
                        // subscription down. Producer task
                        // stays alive so a later re-enable
                        // resumes the same camonitor; drop
                        // the event here while denied.
                        if denied_for_task.load(Ordering::Acquire) {
                            continue;
                        }
                        // CA-268 monitor parity: populate
                        // class_name on every emitted event so
                        // a `ca_create_subscription` against
                        // DBR_CLASS_NAME sees the record_type
                        // string instead of an empty 40-byte
                        // pad.
                        if requested_type == epics_base_rs::types::DBR_CLASS_NAME {
                            std::sync::Arc::make_mut(&mut event.snapshot).class_name =
                                Some(record_for_task.read().record.record_type().to_string());
                        }
                        // long-string boundary conversion before
                        // encoding: `$` → CHAR[40]+NUL, or a native
                        // record field → scalar DBR_STRING. Both this and
                        // the CLASS_NAME override are per-subscription
                        // rewrites of a snapshot every other subscriber
                        // shares, so they take the copy-on-write path —
                        // and only when they have something to write, so a
                        // `Plain` channel (the common case) never copies.
                        if long_string_mode != crate::server::LongStringMode::Plain {
                            super::apply_long_string_mode(
                                std::sync::Arc::make_mut(&mut event.snapshot),
                                long_string_mode,
                            );
                        }
                        // Header space reserved up front, payload encoded
                        // straight into it — see `server::frame` for the C
                        // `cas_copy_in_header` shape this ports.
                        let mut frame = FrameBuf::acquire(outbox_clone.pool(), 0);
                        match encode_dbr_into(frame.dst(), requested_type, &event.snapshot) {
                            Ok(()) => {}
                            Err(epics_base_rs::error::CaError::GetConvertFailed(_)) => {
                                // C `read_reply` serves EVENT_ADD from the same
                                // body as READ_NOTIFY, so a refused conversion
                                // is a zeroed ECA_GETFAIL update
                                // (`camessage.c:544-560`), not a dropped
                                // subscription. The client keeps the monitor
                                // and sees the status on each update.
                                let _ = send_no_read_access_event(
                                    &outbox_clone,
                                    CA_PROTO_EVENT_ADD,
                                    requested_type,
                                    requested_count,
                                    sub_id,
                                    ECA_GETFAIL,
                                    ReplyContext {
                                        req_hdr,
                                        client_minor,
                                    },
                                );
                                continue;
                            }
                            Err(_) => break,
                        }
                        // CA-268: see GET path note — fixed 1.
                        //
                        // Pre-fix Rust framed every update at the
                        // live `snapshot.value.count()`, so an
                        // EVENT_ADD with explicit `count=1` on a
                        // waveform received the full N-element
                        // waveform on every update instead of just
                        // one element. `size_dbr_reply` owns the
                        // request-count rule.
                        let element_count = size_dbr_reply(
                            &mut frame,
                            requested_type,
                            event.snapshot.value.count() as u32,
                            requested_count,
                        );
                        frame.align_payload();

                        let mut ev = CaHeader::new(CA_PROTO_EVENT_ADD);
                        // C client TCP parser requires 8-byte aligned postsize.
                        // C `read_reply` (`camessage.c:515-524`): a pre-V49
                        // client that cannot parse the extended header gets
                        // ECA_16KARRAYCLIENT instead of a de-syncing frame.
                        if ev
                            .set_payload_size(frame.payload_len(), element_count, client_minor)
                            .is_err()
                        {
                            let _ = send_16k_array_client_err(
                                &outbox_clone,
                                &req_hdr,
                                req_hdr.cid,
                                client_minor,
                            );
                            continue;
                        }
                        ev.data_type = requested_type;
                        ev.cid = 1; // ECA_NORMAL
                        ev.available = sub_id;

                        // Back-pressure, taken once this event is known to
                        // become a frame — after `recv`, so an idle producer
                        // holds nothing (`super::outbox` invariant). With the
                        // outbox full this parks the producer and later posts
                        // coalesce in the ring instead of queueing.
                        let credit = outbox_clone.reserve().await;
                        // Abort-safety: this monitor task can be
                        // `task.abort()`ed mid-flight by EVENT_CANCEL /
                        // CLEAR_CHANNEL / disconnect cleanup. `seal`
                        // yields the whole EVENT_ADD frame as ONE
                        // contiguous buffer, handed to the outbox with a
                        // single synchronous `push_with`, so an abort can
                        // only land at a frame boundary — the connection
                        // loop never observes a partial frame, and an abort
                        // at the `reserve` above strands no credit.
                        outbox_clone.push_with(frame.seal(&ev), credit);
                        // Frame handed to the outbox — PCAS
                        // `subscriptionEventsProcessed` parity
                        // (gateway `serverEventRate`).
                        if let Some(ref s) = stats_for_task {
                            s.subscription_events_processed
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });

                Ok(SubscriptionOutcome::Spawned(SpawnedSubscription {
                    task,
                    target: ChannelTarget::RecordField {
                        record: record.clone(),
                        field: field.clone(),
                    },
                    channel_sid: sid,
                    sub_id,
                    data_type: requested_type,
                    data_count: requested_count,
                    denied,
                    long_string_mode,
                    sub_pv_name: sub_pv_name.clone(),
                    mask,
                }))
            }
        }
    }
}

/// Shared EVENT_CANCEL wire logic (C `event_cancel_reply`,
/// `rsrv/camessage.c:2035-2102`): validates the addressed channel and monitor
/// id and queues the cancel acknowledgement. Returns `Ok(true)` when the
/// subscription is valid and the ACK is queued — the caller then performs its
/// (driver-specific) teardown. Bad SID / bad mon-id already pushed the
/// CA_PROTO_ERROR frame and return `Err` (C `RSRV_ERROR` → the circuit ends).
/// `sub_info` is the addressed subscription as seen in the caller's registry.
pub(crate) fn cancel_subscription_reply(
    hdr: &CaHeader,
    state: &ClientState,
    writer: &Outbox,
    sub_info: Option<CancelInfo>,
) -> CaResult<bool> {
    let sub_id = hdr.available;
    let req_channel_sid = hdr.cid;
    // C `event_cancel_reply` resolves the channel first (`MPTOPCIU`); an unknown
    // channel id draws `logBadId` (ECA_INTERNAL "Bad Resource ID") + RSRV_ERROR.
    let (entry_cid, entry_pv_name) = match state.channels.get(&req_channel_sid) {
        Some(entry) => (entry.cid, entry.pv_name.clone()),
        None => {
            send_ca_error(
                writer,
                hdr,
                ECA_INTERNAL,
                0xFFFF_FFFF,
                "Bad Resource ID",
                state.client_minor_version,
            )?;
            return Err(epics_base_rs::error::CaError::Protocol(format!(
                "EVENT_CANCEL on unknown SID {} (matches C event_cancel_reply \
                 logBadId + RSRV_ERROR)",
                req_channel_sid
            )));
        }
    };
    // C walks the channel's eventq for a matching sub-id; a sub-id that is
    // unknown OR bound to a different channel is "not on this channel's eventq"
    // and draws ECA_BADMONID + RSRV_ERROR.
    let channel_matches = sub_info
        .as_ref()
        .is_some_and(|i| i.channel_sid == req_channel_sid);
    if !channel_matches {
        tracing::debug!(
            sub_id,
            sid = req_channel_sid,
            "EVENT_CANCEL channel-mismatch (sub belongs to different channel); ECA_BADMONID"
        );
        send_ca_error(
            writer,
            hdr,
            ECA_BADMONID,
            entry_cid,
            &entry_pv_name,
            state.client_minor_version,
        )?;
        return Err(epics_base_rs::error::CaError::Protocol(format!(
            "EVENT_CANCEL sub-id {} channel-mismatch (requested sid {}; \
             matches C event_cancel_reply 'not on this channel's eventq' RSRV_ERROR)",
            sub_id, req_channel_sid
        )));
    }
    let info = sub_info.expect("channel_matches ⟹ sub_info is Some");
    // C `event_cancel_reply` (`camessage.c:2089-2091`): echo the stored
    // EVENT_ADD request — m_dataType / m_count / m_cid (the SID) / m_available —
    // with a zero payload, in the extended form when the count needs it.
    let mut resp = CaHeader::new(CA_PROTO_EVENT_ADD);
    resp.data_type = info.data_type;
    resp.set_payload_size(0, info.data_count, state.client_minor_version)
        .expect("the client framed this very EVENT_ADD count");
    resp.cid = info.channel_sid;
    resp.available = sub_id;
    writer.push(resp.to_bytes_extended());
    Ok(true)
}

/// One subscription's delivery context for the blocking driver's event thread —
/// the fields [`run_event_task`] needs to frame updates byte-identically to the
/// async producer (`spawn_monitor_sender` / the record-field producer loop).
pub(crate) struct MonitorDelivery {
    pub reader: epics_base_rs::server::event_queue::EventReader,
    pub target: ChannelTarget,
    pub sub_id: u32,
    pub data_type: u16,
    pub data_count: u32,
    pub denied: Arc<AtomicBool>,
    pub long_string_mode: LongStringMode,
    pub req_hdr: CaHeader,
    pub client_minor: u16,
    pub stats: Option<Arc<super::stats::ServerStats>>,
}

/// Control messages from the blocking dispatch thread to its single event
/// thread. Mirrors C: `event_add` hands the new subscription to the client's
/// one `event_task`, `event_cancel` removes it. Dropping the sender is the
/// disconnect signal (C `db_close_events`).
pub(crate) enum EventTaskControl {
    Add(Box<MonitorDelivery>),
    Cancel(u32),
    /// A WRITE_NOTIFY whose record chain went async ([`serve_write_head`]
    /// returned [`WriteHeadOutcome::AsyncPending`]). The event thread awaits its
    /// completion receiver alongside the monitor readers and sends the deferred
    /// reply under the send lock — so the message thread never blocks on the
    /// put-callback (C `camsgtask`) and there is ONE owner of async socket
    /// writes (the event thread), never a third writer.
    WriteComplete(Box<PendingWriteNotify>),
}

/// What one `select` cycle of [`run_event_task`] resolved to.
enum EventStep {
    /// Subscription `idx` produced an event (`None` = its producer is gone).
    /// Boxed: a `MonitorEvent` dwarfs the `Control` variant, so the box keeps
    /// `EventStep` small.
    Delivered(usize, Option<Box<epics_base_rs::server::pv::MonitorEvent>>),
    /// A control message (`None` = the dispatch thread dropped the sender).
    Control(Option<EventTaskControl>),
    /// Pending write-completion `idx` fired (`Ok` = the record's chain settled,
    /// `Err` = the completion sender was dropped → ECA_PUTFAIL).
    WriteDone(usize, Result<(), tokio::sync::oneshot::error::RecvError>),
}

/// Await the next event across every active subscription, every pending
/// write-completion, and the control channel, in one `select`. C's `event_task`
/// blocks on one semaphore per `event_user`; here the per-subscription
/// [`EventReader`](epics_base_rs::server::event_queue::EventReader)s and the WRITE_NOTIFY completion oneshots are multiplexed with
/// `select_all`, and the control channel lets the dispatch thread add/cancel
/// subscriptions, hand over write completions, and signal shutdown.
/// `EventReader::recv` is cancel-safe and an unfired oneshot polled by `&mut`
/// retains its state, so the losing futures are simply dropped and rebuilt next
/// cycle. An empty collection is stood in for by a never-ready `pending()` so the
/// `select` shape stays uniform.
async fn next_event_step(
    subs: &mut [MonitorDelivery],
    pending_writes: &mut [PendingWriteNotify],
    control: &mut tokio::sync::mpsc::UnboundedReceiver<EventTaskControl>,
) -> EventStep {
    use futures_util::future::{Either, pending, select, select_all};

    let readers = async {
        if subs.is_empty() {
            pending::<(Option<Box<epics_base_rs::server::pv::MonitorEvent>>, usize)>().await
        } else {
            let recvs: Vec<_> = subs.iter_mut().map(|s| Box::pin(s.reader.recv())).collect();
            let (event, idx, _rest) = select_all(recvs).await;
            (event.map(Box::new), idx)
        }
    };
    let writes = async {
        if pending_writes.is_empty() {
            pending::<(Result<(), tokio::sync::oneshot::error::RecvError>, usize)>().await
        } else {
            let rxs: Vec<_> = pending_writes
                .iter_mut()
                .map(|p| Box::pin(&mut p.rx))
                .collect();
            let (res, idx, _rest) = select_all(rxs).await;
            (res, idx)
        }
    };
    let ctrl = control.recv();
    tokio::pin!(readers, writes, ctrl);

    match select(select(readers, writes), ctrl).await {
        Either::Left((Either::Left(((event, idx), _writes)), _ctrl)) => {
            EventStep::Delivered(idx, event)
        }
        Either::Left((Either::Right(((res, idx), _readers)), _ctrl)) => {
            EventStep::WriteDone(idx, res)
        }
        Either::Right((ctrl_msg, _)) => EventStep::Control(ctrl_msg),
    }
}

/// The blocking (RTEMS) driver's monitor event thread — the analogue of C
/// `dbEvent.c` `event_task` (`~876`): ONE second thread per client that blocks
/// on the client's monitor event queue and, when `db_post_events` posts an
/// update, frames it and writes it to the client's TCP socket under the same
/// send lock the dispatch thread holds (C `client->lock`, `server.h:221`).
/// `db_post_events` stays enqueue-only; this thread is the sole consumer.
///
/// It multiplexes every subscription's [`EventReader`](epics_base_rs::server::event_queue::EventReader) (added via the control
/// channel by the dispatch thread on EVENT_ADD, removed on EVENT_CANCEL) and
/// frames each delivery with [`super::monitor::send_event`] — the same builder
/// the async `spawn_monitor_sender` uses — so a blocking-driver monitor update
/// is byte-identical to the async server's. The record-field DBR_CLASS_NAME
/// override (C `event_add_action` populates `record_type` per event) is applied
/// here for parity with the async record-field producer loop.
///
/// It is ALSO the single owner of async WRITE_NOTIFY completion writes: the
/// dispatch thread, forbidden from blocking on a put-callback (C `camsgtask`),
/// hands each [`PendingWriteNotify`] here via [`EventTaskControl::WriteComplete`];
/// this thread awaits the completion oneshot in the same `select` and, when it
/// fires, sends the deferred reply via [`PendingWriteNotify::settle`] under the
/// send lock. So there is no third socket writer — monitors and write
/// completions share this one owner.
///
/// `write_frame` writes one complete frame under the send lock; on its first
/// error (peer gone) the thread exits. It returns when the control sender is
/// dropped (clean disconnect) so the dispatch thread can join it.
pub(crate) async fn run_event_task<W>(
    mut control: tokio::sync::mpsc::UnboundedReceiver<EventTaskControl>,
    mut write_frame: W,
) where
    W: FnMut(&[u8]) -> std::io::Result<()>,
{
    let mut subs: Vec<MonitorDelivery> = Vec::new();
    // WRITE_NOTIFYs whose record chain went async, awaiting completion.
    let mut pending_writes: Vec<PendingWriteNotify> = Vec::new();
    // Frames are built into a private outbox (reusing the shared `send_event` /
    // `send_put_notify_response` builders) then drained to the socket under the
    // send lock.
    let (ev_outbox, mut ev_drain) = crate::server::outbox::channel();
    loop {
        match next_event_step(&mut subs, &mut pending_writes, &mut control).await {
            // Dispatch thread dropped the control sender: clean disconnect.
            EventStep::Control(None) => break,
            EventStep::Control(Some(EventTaskControl::Add(d))) => subs.push(*d),
            EventStep::Control(Some(EventTaskControl::Cancel(id))) => {
                subs.retain(|s| s.sub_id != id)
            }
            EventStep::Control(Some(EventTaskControl::WriteComplete(p))) => pending_writes.push(*p),
            // Producer gone (channel cleared / subscription dropped): drop it.
            EventStep::Delivered(idx, None) => {
                subs.remove(idx);
            }
            EventStep::Delivered(idx, Some(mut event)) => {
                // Frame the event with an immutable borrow of the subscription,
                // then release it before mutating `subs`.
                let (encode_ok, deliver) = {
                    let d = &subs[idx];
                    // C `db_event_get_field`: one subscription update committed
                    // this cycle (PCAS `subscriptionEventsPosted`), counted
                    // before the read-access gate.
                    if let Some(ref s) = d.stats {
                        s.subscription_events_posted.fetch_add(1, Ordering::Relaxed);
                    }
                    if d.denied.load(Ordering::Acquire) {
                        // C `casAccessRightsCB` suppresses delivery while read
                        // access is denied, without tearing the sub down.
                        (true, false)
                    } else {
                        // CA-268: a record-field DBR_CLASS_NAME monitor carries
                        // the record's type string (C `event_add_action`); the
                        // async record-field producer sets it per event.
                        if d.data_type == epics_base_rs::types::DBR_CLASS_NAME {
                            if let ChannelTarget::RecordField { record, .. } = &d.target {
                                std::sync::Arc::make_mut(&mut event.snapshot).class_name =
                                    Some(record.read().record.record_type().to_string());
                            }
                        }
                        let ok = super::monitor::send_event(
                            d.data_type,
                            d.data_count,
                            d.sub_id,
                            &event,
                            &ev_outbox,
                            // `ev_outbox` is this task's private one-delivery
                            // scratch buffer, drained to the socket under the
                            // send lock immediately below, so the socket — not
                            // credit — is already this producer's bound.
                            crate::server::outbox::Credit::none(),
                            d.long_string_mode,
                            ReplyContext {
                                req_hdr: d.req_hdr,
                                client_minor: d.client_minor,
                            },
                        )
                        .is_ok();
                        (ok, ok)
                    }
                };
                if !encode_ok {
                    // C: an unencodable field ends this monitor's producer loop.
                    subs.remove(idx);
                    continue;
                }
                if deliver {
                    let mut peer_gone = false;
                    while let Some(frame) = ev_drain.try_next() {
                        if write_frame(&frame).is_err() {
                            peer_gone = true;
                            break;
                        }
                    }
                    if peer_gone {
                        break;
                    }
                    // Frame written (PCAS `subscriptionEventsProcessed`).
                    if let Some(ref s) = subs[idx].stats {
                        s.subscription_events_processed
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            EventStep::WriteDone(idx, res) => {
                // A deferred WRITE_NOTIFY's record chain settled. Send its
                // completion reply under the send lock through the SAME shared
                // tail the async server's completion task runs
                // (`PendingWriteNotify::settle`), so the deferred-reply bytes and
                // the decision about whether a reply is still owed are one copy,
                // not two.
                let mut p = pending_writes.remove(idx);
                if p.settle(res, &ev_outbox).is_err() {
                    // Encoding the reply failed (16k-array boundary): the reply
                    // is unshippable, drop this completion and keep serving.
                    continue;
                }
                let mut peer_gone = false;
                while let Some(frame) = ev_drain.try_next() {
                    if write_frame(&frame).is_err() {
                        peer_gone = true;
                        break;
                    }
                }
                if peer_gone {
                    break;
                }
            }
        }
    }
}

/// Synchronous fast path for the one-shot GET snapshot, mirroring
/// [`get_read_snapshot`] but sans-io.
///
/// The outer `Option` answers "was the read resolvable synchronously?":
/// - `Some(inner)` — a fully local read (a record field always; a `SimplePv`
///   with no read hook, i.e. every cached / non-gateway PV). No `.await`, no
///   reactor. `inner` is the `Option<Snapshot>` the async path also yields:
///   `Some(snap)` for a value, `None` for an absent record field.
/// - `None` — a gateway `-no_cache` [`ReadHook`](epics_base_rs::server::pv::ReadHook) is installed, whose upstream
///   GET is genuine network I/O; the caller must fall back to the async
///   [`get_read_snapshot`]. Only `SimplePv` can reach this.
///
/// This is the item-3 sans-io boundary: local-record READ reply production
/// never touches this crate's async runtime, while the gateway-upstream read
/// stays async and separated.
fn try_get_read_snapshot_local(
    target: &ChannelTarget,
) -> Option<Option<epics_base_rs::server::snapshot::Snapshot>> {
    match target {
        ChannelTarget::RecordField { record, field } => {
            Some(record.read().snapshot_for_field(field))
        }
        // `read_snapshot_local` is `None` exactly when a read hook is
        // installed — the async upstream-GET signal — so it maps straight to
        // this fn's "not sync-resolvable" `None`; a hookless `SimplePv` always
        // yields a snapshot, so its `Some(snap)` maps to `Some(Some(snap))`
        // and never collides with the async-signal `None`.
        ChannelTarget::SimplePv(pv) => pv.read_snapshot_local().map(Some),
    }
}

/// Send an initial / access-restore monitor snapshot as a
/// `CA_PROTO_EVENT_ADD` frame.
///
/// `requested_count` is the element count from the originating
/// `CA_PROTO_EVENT_ADD` request; the encoded DBR payload is sized by
/// `size_dbr_reply`, exactly as the READ path and the steady-state
/// monitor producer are. Without that the first monitor frame (and
/// the access-restore frame) was framed at `snapshot.value.count()`,
/// so a client requesting more elements than the PV currently holds
/// saw a count/size discontinuity between the initial frame and
/// later padded updates.
fn send_monitor_snapshot(
    writer: &Outbox,
    sub_id: u32,
    data_type: u16,
    requested_count: u32,
    snapshot: &epics_base_rs::server::snapshot::Snapshot,
    reply: ReplyContext,
) -> CaResult<()> {
    let (request_hdr, client_minor) = (&reply.req_hdr, reply.client_minor);
    // Header space reserved up front, payload encoded straight into it — see
    // `server::frame` for the C `cas_copy_in_header` shape this ports.
    let mut frame = FrameBuf::acquire(writer.pool(), 0);
    match encode_dbr_into(frame.dst(), data_type, snapshot) {
        Ok(()) => {}
        // Same C branch as the steady-state producer: the initial monitor
        // update carries ECA_GETFAIL with a zeroed body rather than failing
        // the subscription (`camessage.c:545-561`).
        Err(epics_base_rs::error::CaError::GetConvertFailed(_)) => {
            return send_no_read_access_event(
                writer,
                CA_PROTO_EVENT_ADD,
                data_type,
                requested_count,
                sub_id,
                ECA_GETFAIL,
                reply,
            );
        }
        Err(e) => return Err(e),
    }
    // Size the payload to the request count *before* the 8-byte alignment
    // resize, so the header count and the payload shape agree.
    let element_count = size_dbr_reply(
        &mut frame,
        data_type,
        snapshot.value.count() as u32,
        requested_count,
    );
    frame.align_payload();

    let mut resp = CaHeader::new(CA_PROTO_EVENT_ADD);
    // C client TCP parser requires 8-byte aligned postsize
    if resp
        .set_payload_size(frame.payload_len(), element_count, client_minor)
        .is_err()
    {
        return send_16k_array_client_err(writer, request_hdr, request_hdr.cid, client_minor);
    }
    resp.data_type = data_type;
    resp.cid = 1; // ECA_NORMAL
    resp.available = sub_id;

    // Abort-safety: `seal` yields header + payload as ONE contiguous frame,
    // handed to the outbox with a single `push`, so a cancel (send_timeout
    // / task abort) can only land at a frame boundary — the connection loop
    // never observes a partial frame.
    writer.push(frame.seal(&resp));
    Ok(())
}

/// Re-evaluate and re-send CA_PROTO_ACCESS_RIGHTS for all open channels.
/// Called when hostname or username changes (e.g. ACF reload).
///
/// Tracks the *transition* per channel because the C behaviour is
/// asymmetric: a read-access loss must push a single
/// `no_read_access_event` frame and silence subsequent deliveries,
/// while a read-access gain must re-enable deliveries and push one
/// current snapshot. C `casAccessRightsCB`
/// (`rsrv/camessage.c:1070-1138`) walks the channel's `eventq` and
/// calls `db_event_disable` / `db_event_enable` plus
/// `db_post_single_event` — the subscription itself is never
/// removed. Pre-fix Rust permanently destroyed the subscription
/// on a NoAccess transition (`state.subscriptions.remove +
/// task.abort`), so a later ACF reload that restored read access
/// left an orphaned camonitor: the C-equivalent re-arm never
/// happened, and the subscriber's callback receiver went silent
/// until the client noticed and re-subscribed manually.
async fn reeval_access_rights(state: &mut ClientState, writer: &Outbox) -> CaResult<()> {
    if state.channels.is_empty() {
        return Ok(());
    }
    let chan_info: Vec<(u32, u32, ChannelTarget)> = state
        .channels
        .iter()
        .map(|(&sid, entry)| (sid, entry.cid, entry.target.clone()))
        .collect();

    // (sid, old_level, new_level) — old defaults to NoAccess for a
    // sid the access cache has not seen before (parity with the
    // pre-fix `insert`-without-comparison behaviour for freshly
    // created channels).
    //
    // C `libcom/src/as/asLibRoutines.c:1047-1051` fires
    // `pclient->pcallback(... asClientCOAR)` (the COAR callback
    // that calls `casAccessRightsCB` → `access_rights_reply`)
    // ONLY when `oldaccess != access`. An ACF reload that leaves
    // every channel at the same level emits zero ACCESS_RIGHTS
    // frames in C. Pre-fix Rust unconditionally pushed a frame per
    // channel, generating an O(N) burst per connection on routine
    // reloads (typo fix, new UAG that doesn't intersect, etc.).
    // Mirror C: only emit on actual transition.
    let mut transitions: Vec<(u32, AccessLevel, AccessLevel)> = Vec::new();
    for (sid, cid, target) in chan_info {
        let (new_access, new_rule_was_trap) = state.compute_access(&target).await;
        let new_level = match new_access {
            3 => AccessLevel::ReadWrite,
            1 => AccessLevel::Read,
            _ => AccessLevel::NoAccess,
        };
        let old_level = state
            .channel_access
            .insert(sid, new_level)
            .unwrap_or(AccessLevel::NoAccess);
        // an ACF reload can change which rule grants
        // access (e.g. a new TRAPWRITE rule), so the trap mask
        // must be refreshed alongside the level.
        state.channel_trap.insert(sid, new_rule_was_trap);
        if old_level == new_level {
            continue;
        }
        transitions.push((sid, old_level, new_level));
        // Each CA_PROTO_ACCESS_RIGHTS frame is a complete header; push it
        // into the outbox for the connection loop to drain and flush.
        let mut ar = CaHeader::new(CA_PROTO_ACCESS_RIGHTS);
        ar.cid = cid;
        ar.available = new_access;
        writer.push(ar.to_bytes().to_vec());
    }

    fn has_read(level: AccessLevel) -> bool {
        matches!(level, AccessLevel::ReadWrite | AccessLevel::Read)
    }

    for (sid, old_level, new_level) in transitions {
        let old_read = has_read(old_level);
        let new_read = has_read(new_level);
        if old_read == new_read {
            continue;
        }
        let affected: Vec<u32> = state
            .subscriptions
            .iter()
            .filter(|(_, s)| s.channel_sid == sid)
            .map(|(&id, _)| id)
            .collect();
        if affected.is_empty() {
            continue;
        }
        if !new_read {
            // Read access REVOKED. C path: db_post_single_event
            // (which emits `no_read_access_event` — ECA_NORDACCESS
            // in m_cid plus a `dbr_size_n(type, count)` zero-filled
            // payload sized from the stored EVENT_ADD request) then
            // db_event_disable. Pre-fix Rust sent a header-only
            // frame; use `send_no_read_access_event`
            // so the wire frame matches C byte-for-byte (the stored
            // request count drives the zero-fill).
            for sub_id in &affected {
                let (data_type, sub_id_v, data_count, target, req_hdr) = {
                    let Some(sub) = state.subscriptions.get(sub_id) else {
                        continue;
                    };
                    sub.denied.store(true, Ordering::Release);
                    (
                        sub.data_type,
                        sub.sub_id,
                        sub.data_count,
                        sub.target.clone(),
                        sub.request_header(),
                    )
                };
                // C posts the access-revoked event through
                // `db_post_single_event` (event-context pre-chain)
                // BEFORE `db_event_disable` (`camessage.c:1121-1123`);
                // the ECA_NORDACCESS frame is only sent when the chain
                // passes the post (`db_queue_event_log` fires only
                // `if(pLog)`). Run a fresh event-context chain on the
                // current value and skip the frame when it drops — the
                // `denied` gate is already set, so the producer stays
                // disabled either way. The denial frame is zero-filled,
                // so only the chain's pass/drop decision matters, not
                // the filtered value.
                let snap = get_full_snapshot(&target);
                let dropped_by_filter = match (state.channels.get(&sid), &snap) {
                    (Some(entry), Some(snap)) => entry
                        .filter_chain()
                        .apply_to_event_value(snap.value.clone())
                        .is_none(),
                    _ => false,
                };
                if dropped_by_filter {
                    continue;
                }
                // an autosize (`data_count == 0`) subscription
                // revoked here must also be normalised to the live
                // element count, otherwise the access-revoked
                // notification is the same zero-payload
                // `CA_PROTO_EVENT_ADD` the client drops as a
                // cancel-ack. Same C `read_reply` autosize parity
                // (`camessage.c:509-514`) as the initial EVENT_ADD
                // denial path.
                let denied_count = if data_count == 0 {
                    let actual = snap.as_ref().map(|snap| snap.value.count()).unwrap_or(1);
                    no_read_access_count(data_count, actual)
                } else {
                    data_count
                };
                send_no_read_access_event(
                    writer,
                    CA_PROTO_EVENT_ADD,
                    data_type,
                    denied_count,
                    sub_id_v,
                    ECA_NORDACCESS,
                    ReplyContext {
                        req_hdr,
                        client_minor: state.client_minor_version,
                    },
                )?;
            }
            // Denial frames pushed to the outbox; the connection loop drains
            // and flushes them (no writer to flush here).
        } else {
            // Read access RESTORED. C path: db_event_enable then
            // db_post_single_event. Clear the gate so the producer
            // task resumes deliveries, and emit one snapshot of
            // the current value so the subscriber sees a fresh
            // event the moment access comes back (rather than
            // waiting for the next natural update).
            //
            // the restore snapshot honours the stored
            // EVENT_ADD request count in BOTH directions.
            // `send_monitor_snapshot` pads when the request asked
            // for more elements than the PV currently holds and
            // truncates when it asked for fewer, via
            // `size_dbr_reply` — so the access-restore
            // frame matches the request shape and later padded
            // updates. C `read_reply` always honours the stored
            // request count; pre-fix Rust framed the restore event
            // at the live `snapshot.value.count()`, only truncating
            // and never padding.
            for sub_id in &affected {
                let (target, data_type, data_count, sub_id_val, sub_long_string_mode, req_hdr) = {
                    let Some(sub) = state.subscriptions.get(sub_id) else {
                        continue;
                    };
                    sub.denied.store(false, Ordering::Release);
                    (
                        sub.target.clone(),
                        sub.data_type,
                        sub.data_count,
                        sub.sub_id,
                        sub.long_string_mode,
                        sub.request_header(),
                    )
                };
                if let Some(mut snap) = get_full_snapshot(&target) {
                    // C enables the event (`db_event_enable`)
                    // THEN posts the current value through the
                    // event-context pre-chain
                    // (`db_post_single_event`, `camessage.c:1117-1119`);
                    // the restore frame is sent only when the chain
                    // passes (`db_queue_event_log` fires only
                    // `if(pLog)`) and carries the FILTERED value. The
                    // `denied` gate is already cleared above so future
                    // natural updates resume; a fresh event-context
                    // chain per subscriber keeps `dec`/`sync`/`dbnd`
                    // state isolated. `None` => send no restore frame.
                    if let Some(entry) = state.channels.get(&sid) {
                        match entry
                            .filter_chain()
                            .apply_to_event_value(snap.value.clone())
                        {
                            Some(v) => snap.value = v,
                            None => continue,
                        }
                    }
                    // long-string boundary conversion (`$` → CHAR[40], or
                    // native record field → scalar DBR_STRING).
                    super::apply_long_string_mode(&mut snap, sub_long_string_mode);
                    send_monitor_snapshot(
                        writer,
                        sub_id_val,
                        data_type,
                        data_count,
                        &snap,
                        ReplyContext {
                            req_hdr,
                            client_minor: state.client_minor_version,
                        },
                    )?;
                    // Access-restore post — C `db_event_enable` then
                    // `db_post_single_event` (`camessage.c:1117-1119`):
                    // one posted and one processed subscription event,
                    // same PCAS accounting as the initial value.
                    if let Some(ref s) = state.stats {
                        s.subscription_events_posted.fetch_add(1, Ordering::Relaxed);
                        s.subscription_events_processed
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
    }
    Ok(())
}

/// CA_PROTO_WRITE_NOTIFY reply with extended-form
/// count support. C `putNotifyErrorReply` / `write_notify_reply`
/// (`rsrv/camessage.c:1513-1533` / `1438-1441`) call
/// `cas_copy_in_header` with `mp->m_count` / `msgtmp.m_count` from
/// `caHdrLargeArray`, which is the decoded 32-bit count for
/// extended requests and re-emits in extended form when needed.
/// Pre-fix Rust set `resp.count = hdr.count as u16` and serialised
/// with `to_bytes()`, so a `ca_array_put_callback()` on a
/// `>= 0xFFFF`-element array received a normal-form Rust reply
/// with `count = 0` (the extended marker) where rsrv preserves
/// the count with an extended header.
fn send_put_notify_response(
    writer: &Outbox,
    data_type: u16,
    count: u32,
    eca_status: u32,
    ioid: u32,
    reply: ReplyContext,
) -> CaResult<()> {
    let (request_hdr, client_minor) = (&reply.req_hdr, reply.client_minor);
    let mut resp = CaHeader::new(CA_PROTO_WRITE_NOTIFY);
    resp.data_type = data_type;
    // postsize = 0 (WRITE_NOTIFY replies have no payload);
    // set_payload_size promotes to extended form when count >= 0xFFFF.
    if resp.set_payload_size(0, count, client_minor).is_err() {
        return send_16k_array_client_err(writer, request_hdr, request_hdr.cid, client_minor);
    }
    resp.cid = eca_status;
    resp.available = ioid;
    writer.push(resp.to_bytes_extended());
    Ok(())
}

/// normalise an EVENT_ADD request count for a no-read-access
/// denial frame. C `read_reply` (`rsrv/camessage.c:509-514`) treats a
/// zero element count as autosize and substitutes `paddr->no_elements`
/// — the target's live element count. The `no_read_access_event`
/// denial path must do the same, otherwise a `count == 0` monitor on
/// a plain DBR type (`DBR_DOUBLE`, …) produces a zero-payload
/// `CA_PROTO_EVENT_ADD`. That shape is the historical
/// subscription-cancel-confirmation no-op: the CA client drops it
/// before reading the `ECA_NORDACCESS` status (C `cac.cpp`
/// eventRespAction returns on `m_postsize == 0`; this port's
/// `client/transport.rs` mirrors that), so the denied monitor would
/// silently appear to hang.
///
/// A non-zero request count is returned unchanged (explicit counts
/// are already framed at the requested shape). `actual_count` is the
/// target's live element count, used only for the autosize case.
fn no_read_access_count(requested_count: u32, actual_count: u32) -> u32 {
    if requested_count == 0 {
        // Autosize: at least one element so the denial frame carries
        // a nonzero DBR body. A target reporting zero live elements
        // still gets a single-element zero-filled payload.
        actual_count.max(1)
    } else {
        requested_count
    }
}

/// Send a `no_read_access_event`-shaped reply: same wire frame as the
/// original READ_NOTIFY / EVENT_ADD command, with `m_cid` carrying the
/// ECA status and a `dbr_buffer_size`-sized zero payload. C
/// `no_read_access_event` (`rsrv/camessage.c:455-485`) and `read_reply`
/// (`camessage.c:540-557`) use this shape for READ_NOTIFY denials and
/// dbChannel_get failures — preserving the requested count and DBR
/// type so libca-style clients see the correct callback metadata even
/// on the error path.
///
/// callers on the EVENT_ADD denial path must pass a `count`
/// already normalised through [`no_read_access_count`] so an autosize
/// (`count == 0`) request does not produce a zero-payload frame.
fn send_no_read_access_event(
    writer: &Outbox,
    cmd: u16,
    data_type: u16,
    count: u32,
    available: u32,
    eca_status: u32,
    reply: ReplyContext,
) -> CaResult<()> {
    let (request_hdr, client_minor) = (&reply.req_hdr, reply.client_minor);
    let native = epics_base_rs::types::native_type_for_dbr(data_type)
        .unwrap_or(epics_base_rs::types::DbFieldType::Char);
    let payload_size = epics_base_rs::types::dbr_buffer_size(data_type, native, count as usize);
    let padded_size = align8(payload_size);
    let mut hdr = CaHeader::new(cmd);
    if hdr
        .set_payload_size(padded_size, count, client_minor)
        .is_err()
    {
        return send_16k_array_client_err(writer, request_hdr, request_hdr.cid, client_minor);
    }
    hdr.data_type = data_type;
    hdr.cid = eca_status;
    hdr.available = available;
    let hdr_bytes = hdr.to_bytes_extended();
    // Build header + zero payload as one contiguous frame so a
    // task abort can only land at a frame boundary (same abort-
    // safety invariant as `send_event` / `send_monitor_snapshot`).
    let mut frame = Vec::with_capacity(hdr_bytes.len() + padded_size);
    frame.extend_from_slice(&hdr_bytes);
    frame.resize(frame.len() + padded_size, 0);
    writer.push(frame);
    Ok(())
}

/// Why a READ / READ_NOTIFY reply could not be framed. The two variants
/// mirror C's two framing-time failures; the async dispatch handler maps
/// each to its C-parity wire response (see the `build_read_reply` call site).
#[derive(Debug)]
enum ReadReplyError {
    /// `encode_dbr` rejected the requested DBR type — the direct parallel of
    /// C `read_action`'s `INVALID_DB_REQ(m_dataType)` (`camessage.c:616-620`).
    /// The deprecated READ answers `ECA_BADTYPE`; READ_NOTIFY stays silent.
    BadType,
    /// The framed payload needs the extended (24-byte) header but the client
    /// is pre-V49 and cannot parse it — C answers `ECA_16KARRAYCLIENT` and
    /// keeps the circuit (`camessage.c:630-639`).
    Oversize,
    /// `dbChannel_get`'s conversion returned a non-zero status — C
    /// `read_reply` answers with a zeroed payload and `m_cid = ECA_GETFAIL`
    /// (`camessage.c:544-560`), keeping the circuit. Distinct from
    /// [`Self::BadType`], which is a request the server can never serve and
    /// which tears the connection down.
    GetFail,
}

/// Sans-io core of the READ / READ_NOTIFY reply: turn a fully-materialized
/// snapshot plus the request parameters into the exact wire frame (extended
/// header + 8-aligned DBR payload), with no socket, database, or async.
///
/// The async dispatch handler is responsible for every I/O step first —
/// channel lookup, access checks, fetching the snapshot and applying the
/// filter chain, long-string mode, and STSACK / CLASS_NAME field reads. Once
/// the snapshot is finished, the wire bytes are a pure function of it and of
/// (`requested_type`, `requested_count`, `is_notify`, `cid`, `ioid`,
/// `client_minor`), which is exactly this function. That makes the READ
/// reply byte-production testable against a hand-built snapshot with no
/// socket in the loop.
// The send buffer joins seven request parameters that are all genuinely
// independent; grouping them into a single-use struct to satisfy the lint would
// hide the sans-io signature this function exists to expose.
#[allow(clippy::too_many_arguments)]
fn build_read_reply(
    pool: &std::sync::Arc<crate::server::frame::FramePool>,
    requested_type: u16,
    requested_count: u32,
    is_notify: bool,
    snapshot: &epics_base_rs::server::snapshot::Snapshot,
    cid: u32,
    ioid: u32,
    client_minor: u16,
) -> Result<crate::server::frame::PooledFrame, ReadReplyError> {
    // Header space reserved up front, payload encoded straight into it — see
    // `server::frame` for the C `cas_copy_in_header` shape this ports.
    let mut frame = FrameBuf::acquire(pool, 0);
    match encode_dbr_into(frame.dst(), requested_type, snapshot) {
        Ok(()) => {}
        Err(epics_base_rs::error::CaError::GetConvertFailed(_)) => {
            return Err(ReadReplyError::GetFail);
        }
        Err(_) => return Err(ReadReplyError::BadType),
    }
    // C `read_reply` (`rsrv/camessage.c:507-571`) keeps
    // the request count in the header and zero-fills the
    // payload when fewer elements are returned than requested
    // (`autosize = mp->m_count == 0` is the exception:
    // request count 0 means "all available"; otherwise the
    // response carries the requested count and pads with
    // zeros). Pre-fix Rust dropped the requested count on
    // a short array, so a `ca_array_get_callback(type,
    // count > native, ...)` saw a shorter response from
    // Rust than from rsrv.
    let actual_count = snapshot.value.count() as u32;
    // ORDER MATTERS: the deprecated-READ count==0 branch MUST
    // precede the DBR_CLASS_NAME normalization. C `read_action`
    // (rsrv/camessage.c:622-645) sizes EVERY type — including
    // DBR_CLASS_NAME — with `dbr_size_n(type, m_count)` and writes
    // the header count as `m_count` VERBATIM, with no class-name
    // special case. `dbr_size_n(DBR_CLASS_NAME, 0) = dbr_size[38]
    // - dbr_value_size[38] = 40 - 40 = 0`, so a deprecated READ of
    // DBR_CLASS_NAME at count==0 ships count=0 and a 0-byte
    // payload. Only the READ_NOTIFY / EVENT_ADD path (`read_reply`)
    // forces the fixed 40-byte class string at count 1 (CA-268).
    let element_count = if !is_notify && requested_count == 0 {
        // The deprecated synchronous CA_PROTO_READ (cmd 3) does
        // NOT treat m_count==0 as autosize. C `read_action`
        // (rsrv/camessage.c:622-645) sizes the reply with
        // `dbr_size_n(type, m_count)`, writes the header count as
        // `m_count`, and calls `dbChannel_get(.., m_count, ..)`
        // — all VERBATIM. Only `read_reply` (READ_NOTIFY /
        // EVENT_ADD, camessage.c:509-514) interprets m_count==0 as
        // "all available elements". So a deprecated READ with
        // count==0 must ship count=0 and a value-less payload of
        // `dbr_size_n(type, 0)` bytes == the type's metadata only
        // (0 bytes for a plain DBR type; the STS/TIME/GR/CTRL
        // header for a compound type, since `dbr_size_n(t,0)` ==
        // `dbr_size[t] - dbr_value_size[t]`). Pre-fix Rust shared
        // the autosize path for both opcodes and returned the full
        // native array with count=actual_native_count, diverging
        // from rsrv on both the wire count field and the payload
        // length.
        match epics_base_rs::types::native_type_for_dbr(requested_type) {
            Ok(native) => {
                // `dbr_buffer_size(.., 0)` equals `dbr_size_n(t, 0)`
                // for every type EXCEPT DBR_CLASS_NAME, where it
                // reports the fixed 40-byte string (the count>=1
                // framing size) rather than `dbr_size_n(38,0)=0`.
                // Use 0 for CLASS_NAME to match C's value-less
                // count==0 payload.
                let meta_size = if requested_type == epics_base_rs::types::DBR_CLASS_NAME {
                    0
                } else {
                    epics_base_rs::types::dbr_buffer_size(requested_type, native, 0)
                };
                if frame.payload_len() > meta_size {
                    frame.truncate_payload(meta_size);
                }
                0
            }
            // Unreachable for a type that already encoded above
            // (<= LAST_BUFFER_TYPE). If it ever weren't, fall back
            // to the autosize sizing so the header count still
            // matches the payload rather than shipping count=0
            // with a value-bearing body.
            // Unreachable for a type that already encoded above
            // (<= LAST_BUFFER_TYPE), and unreachable for
            // DBR_CLASS_NAME (whose native lookup succeeds). If it
            // ever weren't, fall through to the shared sizing so the
            // header count still matches the payload rather than
            // shipping count=0 with a value-bearing body.
            Err(_) => size_dbr_reply(&mut frame, requested_type, actual_count, requested_count),
        }
    } else {
        // Every other count — including DBR_CLASS_NAME's fixed single
        // element — is the shared `read_reply` sizing. Only the
        // deprecated-READ count==0 case above deviates from it.
        size_dbr_reply(&mut frame, requested_type, actual_count, requested_count)
    };
    // Deprecated CA_PROTO_READ (cmd 3) contracts a scalar
    // DBR_STRING payload to its NUL-terminated length before the
    // 8-byte alignment. C `read_action` (rsrv/camessage.c:666-680)
    // recomputes `payloadSize = epicsStrnLen(pStr, 40) + 1` for
    // `DBR_STRING && m_count == 1` when a NUL is found within the
    // 40-byte slot (otherwise it force-terminates byte 39 and keeps
    // the full 40), then `cas_commit_msg` (caserverio.c:350-365)
    // aligns the shortened size to 8 and rewrites m_postsize while
    // leaving the header count at 1. So `"OK"` commits an 8-byte
    // payload, not the fixed 40-byte slot. READ_NOTIFY / EVENT_ADD
    // never run this branch — C `read_reply` keeps the full slot —
    // so gate on `!is_notify`. `element_count == 1` is the scalar
    // case; arrays / count!=1 keep their full per-element slots.
    if !is_notify && requested_type == epics_base_rs::types::DBR_STRING && element_count == 1 {
        // epicsStrnLen(pStr, 40): the first NUL index, capped at the
        // 40-byte slot. Trim to value + its NUL only when a NUL
        // exists within the slot; the encoder always NUL-bounds a
        // scalar string at <= 39 chars (value.rs to_bytes), so the
        // no-NUL else-branch C guards against cannot arise here and
        // the full 40-byte slot is kept untouched if it ever did.
        let slot = frame.payload_len().min(40);
        if let Some(nul) = frame.payload()[..slot].iter().position(|&b| b == 0) {
            frame.truncate_payload(nul + 1);
        }
    }
    frame.align_payload();

    // For deprecated CA_PROTO_READ (cmd=3), the response carries
    // the *client-side* CID (`pciu->cid` in C `read_action`
    // — `camessage.c:631-632` passes `pciu->cid`, NOT
    // `pciu->sid`, to `cas_copy_in_header`). Modern libca's
    // `readRespAction` demuxes by ioid (`m_available`) and
    // ignores `m_cid` for READ responses, but pre-3.14 clients
    // and stricter wire validators (Wireshark CA dissector,
    // packet-level fuzzers) cross-check the field. Notify
    // clients (cmd=15) get ECA_NORMAL since READ_NOTIFY's cid
    // slot carries status, not the channel CID.
    let mut resp = if is_notify {
        let mut r = CaHeader::new(CA_PROTO_READ_NOTIFY);
        r.cid = ECA_NORMAL;
        r
    } else {
        let mut r = CaHeader::new(CA_PROTO_READ);
        r.cid = cid;
        r
    };
    // C client TCP parser requires 8-byte aligned postsize.
    // C `read_action` (`camessage.c:625-631`): a reply needing the
    // extended header for a pre-V49 client is not framed — the server
    // answers ECA_16KARRAYCLIENT and keeps the circuit.
    if resp
        .set_payload_size(frame.payload_len(), element_count, client_minor)
        .is_err()
    {
        return Err(ReadReplyError::Oversize);
    }
    resp.data_type = requested_type;
    resp.available = ioid;

    // Abort-safety: `seal` yields the whole READ/READ_NOTIFY frame as ONE
    // contiguous buffer so the caller can hand it to the outbox in a
    // single `push`. A `send_timeout` cancel can only land at a frame
    // boundary — a partial frame can never be enqueued, so it can never
    // reach the connection loop's socket writer and mis-frame the
    // following messages. Same shape as the monitor path (`monitor.rs`).
    Ok(frame.seal(&resp))
}

/// Send a CA_PROTO_ERROR response with the original header echoed
/// into the payload and an error message.
///
/// Layout follows C `vsend_err` (`rsrv/camessage.c:149-255`):
///   * outer `m_cid` carries the *channel client cid* (i.e. the
///     client-side identifier of the channel the error relates to),
///     or `0xFFFFFFFF` for commands that aren't channel-scoped.
///   * outer `m_available` carries the ECA status code.
///   * payload is the original request header followed by a
///     NUL-terminated diagnostic string.
///
/// The previous implementation put the ECA status in `m_cid` and left
/// `m_available` zero, so libca's `exceptionRespAction`
/// (`cac.cpp:1118`) — which reads the status from `hdr.m_available` —
/// would surface every server-emitted CA_PROTO_ERROR as ECA_NORMAL
/// (status 0), silently masking the failure.
/// C `vsend_err` (rsrv/camessage.c:157,225-233) allocates a fixed
/// 512-byte buffer for the entire reply (outer header + echoed
/// request header + diagnostic + NUL), and `epicsVsnprintf` truncates
/// the formatted diagnostic if it would overflow. Mirror that bound
/// so a buggy caller (or future translated message catalog) can't
/// ship a CA_PROTO_ERROR whose payload exceeds the libca per-server
/// recv buffer or the extended-header threshold. 480 = 512 −
/// 2*sizeof(caHdr) matches the diagnostic budget C grants
/// `epicsVsnprintf`.
const CA_PROTO_ERROR_MAX_DIAG_LEN: usize = 480;

/// The two facts every reply needs about the request it answers, and they
/// are never meaningful apart: the request header C keeps for the reply
/// (`pevext->msg` for a subscription, the in-flight request for a
/// one-shot) and the peer's minor protocol version, which decides whether
/// the reply may use the extended (24-byte) header at all
/// (`caserverio.c:266-270` — a pre-V49 peer gets ECA_16KARRAYCLIENT
/// instead).
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReplyContext {
    pub(crate) req_hdr: CaHeader,
    pub(crate) client_minor: u16,
}

/// On `CA_PROTO_CLEAR_CHANNEL`, abort any pending WRITE_NOTIFY
/// completion task whose owning channel `sid` is being freed (C
/// parity: `clear_channel_reply` calls `rsrvFreePutNotify` per
/// channel — `camessage.c:1943`). Finished handles are reaped
/// opportunistically while iterating so the per-connection Vec stays
/// bounded across many WRITE_NOTIFYs over a long-lived connection.
/// Pure transformation extracted so the drain semantics are unit-
/// testable without standing up a full server + async record.
fn drain_write_notify_tasks_for_sid(tasks: &mut Vec<(u32, tokio::task::AbortHandle)>, sid: u32) {
    let mut keep = Vec::with_capacity(tasks.len());
    let mut to_abort = Vec::new();
    for (task_sid, h) in tasks.drain(..) {
        if h.is_finished() {
            continue;
        }
        if task_sid == sid {
            to_abort.push(h);
        } else {
            keep.push((task_sid, h));
        }
    }
    *tasks = keep;
    for h in to_abort {
        h.abort();
    }
}

/// Truncate `message` to at most `CA_PROTO_ERROR_MAX_DIAG_LEN` bytes
/// on a char boundary (so the resulting `&str` slice is always valid
/// UTF-8). `pad_string` appends the NUL terminator and 8-aligns.
fn truncate_diag(message: &str) -> &str {
    if message.len() <= CA_PROTO_ERROR_MAX_DIAG_LEN {
        return message;
    }
    let mut end = CA_PROTO_ERROR_MAX_DIAG_LEN;
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    &message[..end]
}

/// C `read_reply` (`camessage.c:515-524`) and `read_action`
/// (`camessage.c:625-631`): when `cas_copy_in_header` refuses to frame a reply
/// because the client is pre-CA_V49 and the payload/count needs the 24-byte
/// extended header (`caserverio.c:266-270` returns `ECA_16KARRAYCLIENT`), the
/// server does NOT put the frame on the wire. It answers CA_PROTO_ERROR with
/// that status, echoing the request header, and keeps the circuit.
pub(crate) fn send_16k_array_client_err(
    writer: &Outbox,
    request_hdr: &CaHeader,
    chan_cid: u32,
    client_minor: u16,
) -> CaResult<()> {
    send_ca_error(
        writer,
        request_hdr,
        ECA_16KARRAYCLIENT,
        chan_cid,
        "server unable to load read (or subscription update) response into \
         protocol buffer: client protocol revision does not support transfers \
         exceeding 16k bytes",
        client_minor,
    )
}

/// Push a `CA_PROTO_ERROR` reply into the connection outbox. The connection
/// loop is the sole owner that drains the outbox to the socket, so this
/// only builds one framed message and enqueues it — see
/// [`build_ca_error_frame`]. Synchronous: enqueueing is a pure buffer
/// push with no I/O, so callers do not `.await` it.
pub(crate) fn send_ca_error(
    writer: &Outbox,
    original_hdr: &CaHeader,
    eca_status: u32,
    chan_cid: u32,
    message: &str,
    client_minor: u16,
) -> CaResult<()> {
    writer.push(build_ca_error_frame(
        original_hdr,
        eca_status,
        chan_cid,
        message,
        client_minor,
    ));
    Ok(())
}

/// Build the contiguous wire bytes of a `CA_PROTO_ERROR` reply
/// (response header + echoed request header + diagnostic string). Pure and
/// socket-free — the connection loop writes the returned frame.
pub(crate) fn build_ca_error_frame(
    original_hdr: &CaHeader,
    eca_status: u32,
    chan_cid: u32,
    message: &str,
    client_minor: u16,
) -> Vec<u8> {
    let error_msg_bytes = pad_string(truncate_diag(message));
    // C `vsend_err` (`rsrv/camessage.c:210-235`) picks the echo's form from
    // the request's ACTUAL size and count — `(m_postsize >= 0xffff ||
    // m_count >= 0xffff) && CA_V49(minor)` — never from the form the request
    // arrived in. Those are different questions in both directions: a
    // normal-form request may carry `m_count == 0xffff`, and an extended
    // request may declare a size and count that both fit in 16 bits.
    //
    // `set_payload_size` is that rule's owner already, which is why the echo
    // is re-derived through it rather than asking the parsed header whether
    // it happened to carry an annex.
    let mut echo = *original_hdr;
    let orig_bytes = match echo.set_payload_size(
        original_hdr.actual_postsize(),
        original_hdr.actual_count(),
        client_minor,
    ) {
        Ok(()) => echo.to_bytes_extended(),
        // A pre-V49 peer has no code to parse the 8-byte annex, so C does not
        // withhold the reply: it keeps the 16-byte form and truncates the two
        // fields to `u16` (`htons((ca_uint16_t) curp->m_postsize)`,
        // `camessage.c:226-235`).
        Err(ExtendedHeaderUnsupported) => {
            echo.postsize = original_hdr.actual_postsize() as u16;
            echo.count = original_hdr.actual_count() as u16;
            echo.extended_postsize = None;
            echo.extended_count = None;
            echo.to_bytes().to_vec()
        }
    };
    // payload_size must use the echo header's ACTUAL byte length (16 or 24
    // for extended), not the constant CaHeader::SIZE=16.
    let payload_size = orig_bytes.len() + error_msg_bytes.len();

    let mut resp = CaHeader::new(CA_PROTO_ERROR);
    // A CA_PROTO_ERROR body is a header echo plus a bounded diagnostic
    // string, so it can never reach 0xffff; `set_payload_size` cannot fail
    // here for any peer version.
    resp.set_payload_size(payload_size, 0, client_minor)
        .expect("CA_PROTO_ERROR payload is bounded well below 0xffff");
    resp.cid = chan_cid;
    resp.available = eca_status;

    // Abort-safety: a CA_PROTO_ERROR reply is response-header +
    // echoed-request-header + diagnostic string. Build all three as ONE
    // contiguous frame and issue a single `write_all` so a `send_timeout`
    // cancel cannot leave a partial frame (orphan header) in the shared
    // BufWriter and mis-frame every following message.
    //
    // For a V49 client the echoed request header is emitted in extended form
    // when the original request used the extended layout: a 16-byte header
    // with `m_postsize = 0xffff` plus an 8-byte annex carrying the full
    // 32-bit postsize / count. libca `cac::exceptionRespAction`
    // (`modules/ca/src/client/cac.cpp:1097-1107`) parses the annex first when
    // it sees the 0xffff marker, then walks the diag string from the
    // post-annex offset — so an extended READ/WRITE error round-trips
    // byte-for-byte with libca. `orig_bytes` (computed above) already carries
    // the version-correct 16- or 24-byte form.
    let resp_bytes = resp.to_bytes_extended();
    let mut frame = Vec::with_capacity(resp_bytes.len() + orig_bytes.len() + error_msg_bytes.len());
    frame.extend_from_slice(&resp_bytes);
    frame.extend_from_slice(&orig_bytes);
    frame.extend_from_slice(&error_msg_bytes);
    frame
}

#[cfg(test)]
mod write_notify_drain_tests {
    use super::drain_write_notify_tasks_for_sid;

    /// Spawn a long-running task (sleep-loop) and return its abort
    /// handle. The handle's `is_finished()` flips to true once `abort()`
    /// has fired AND the runtime has processed the cancellation. We
    /// poll for that transition in the test below — drop-flag
    /// approaches were timing-sensitive on saturated CI runners.
    fn spawn_pending() -> tokio::task::AbortHandle {
        tokio::spawn(async {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .abort_handle()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drains_only_matching_sid() {
        let h_a = spawn_pending();
        let h_b = spawn_pending();
        let h_c = spawn_pending();
        let h_a_probe = h_a.clone();
        let h_b_probe = h_b.clone();
        let h_c_probe = h_c.clone();
        let mut tasks = vec![(10u32, h_a), (20u32, h_b), (10u32, h_c)];

        drain_write_notify_tasks_for_sid(&mut tasks, 10);

        // sid=20 entry survives
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].0, 20);

        // Wait up to 2s (generous for saturated CI) for the aborted
        // tasks to actually finish. The sid=20 task must still be
        // running (no abort fired against it).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if h_a_probe.is_finished() && h_c_probe.is_finished() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(h_a_probe.is_finished(), "sid=10 task #1 must be aborted");
        assert!(h_c_probe.is_finished(), "sid=10 task #3 must be aborted");
        assert!(!h_b_probe.is_finished(), "sid=20 task must survive");

        // Cleanup the surviving task so we don't leak.
        h_b_probe.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reaps_finished_handles_during_drain() {
        // A handle whose future already completed should be removed
        // from the Vec regardless of whether its sid matches — this
        // is the opportunistic-reap behaviour the long-lived
        // connection relies on.
        let done = tokio::spawn(async {}).abort_handle();
        for _ in 0..200 {
            if done.is_finished() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(done.is_finished(), "spawned no-op task should complete");

        let live = spawn_pending();
        let live_probe = live.clone();
        let mut tasks = vec![(99u32, done), (5u32, live)];
        drain_write_notify_tasks_for_sid(&mut tasks, 1234);
        assert_eq!(tasks.len(), 1, "finished handle was not reaped");
        assert_eq!(tasks[0].0, 5);

        // Cleanup the still-live task.
        live_probe.abort();
    }
}

#[cfg(test)]
mod put_notify_serialize_tests {
    //! The boundaries of the ONE rule this layer has about ECA_PUTCBINPROG:
    //! a channel's registered put-callback draws the status only once it has
    //! outstayed C's `blockSem` wait, and it draws it for its own ioid
    //! (`putNotifyErrorReply(client, &pPutNotify->msg, ECA_PUTCBINPROG)`,
    //! `camessage.c:1745`). One case per boundary — registered vs not,
    //! inside the wait vs past it, still busy vs already answered.
    use super::{
        ECA_PUTCBINPROG, InFlightPutNotify, PUT_NOTIFY_BLOCK_TIMEOUT, PutNotifyCompletion,
        PutNotifySlot, WriteNotifyReply,
    };
    use crate::server::outbox::{Outbox, OutboxDrain};
    use std::time::Instant;

    fn live_outbox() -> (Outbox, OutboxDrain) {
        crate::server::outbox::channel()
    }

    /// Every frame pushed to the outbox, in push order (one push == one
    /// complete frame).
    fn drain_frames(drain: &mut OutboxDrain) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        while let Some(f) = drain.try_next() {
            frames.push(f.to_vec());
        }
        frames
    }

    fn reply_shape(ioid: u32) -> WriteNotifyReply {
        WriteNotifyReply {
            write_type: epics_base_rs::types::DBR_LONG,
            write_count: 1,
            ioid,
            req_hdr: crate::protocol::CaHeader::new(crate::protocol::CA_PROTO_WRITE_NOTIFY),
            client_minor: crate::protocol::CA_MINOR_VERSION,
        }
    }

    /// A slot holding one registered put-callback, registered `age` ago.
    fn slot_registered_for(ioid: u32, age: std::time::Duration) -> PutNotifySlot {
        let slot = PutNotifySlot::default();
        slot.install(InFlightPutNotify {
            completion: PutNotifyCompletion::new(None),
            reply: reply_shape(ioid),
            busy_since: Instant::now() - age,
        });
        slot
    }

    /// The ECA status and ioid a WRITE_NOTIFY-shaped reply frame carries:
    /// param1 at `[8..12]`, param2 at `[12..16]`.
    fn status_and_ioid(frame: &[u8]) -> (u32, u32) {
        (
            u32::from_be_bytes([frame[8], frame[9], frame[10], frame[11]]),
            u32::from_be_bytes([frame[12], frame[13], frame[14], frame[15]]),
        )
    }

    /// Boundary: nothing registered. C reaches the `else` arm and allocates a
    /// fresh `pPutNotify` without sending anything.
    #[test]
    fn an_unregistered_channel_draws_no_frame() {
        let (outbox, mut drain) = live_outbox();
        PutNotifySlot::default().serialize(&outbox).unwrap();
        assert!(drain_frames(&mut drain).is_empty());
    }

    /// Boundary: registered, still busy, still inside the wait. This is the
    /// arm the port used to answer with ECA_PUTCBINPROG on every arrival; C
    /// sends nothing until `epicsEventWaitWithTimeout` actually times out.
    #[test]
    fn a_predecessor_inside_the_wait_draws_no_frame() {
        let (outbox, mut drain) = live_outbox();
        let slot = slot_registered_for(0x1234, PUT_NOTIFY_BLOCK_TIMEOUT / 2);
        slot.serialize(&outbox).unwrap();
        assert!(
            drain_frames(&mut drain).is_empty(),
            "C sends nothing while the blockSem wait is still running"
        );
    }

    /// ...and it stays registered, so it is still the predecessor a later
    /// request measures against rather than being silently forgotten.
    #[test]
    fn a_predecessor_inside_the_wait_stays_registered() {
        let (outbox, mut drain) = live_outbox();
        let slot = slot_registered_for(0x1234, PUT_NOTIFY_BLOCK_TIMEOUT / 2);
        slot.serialize(&outbox).unwrap();
        slot.serialize(&outbox).unwrap();
        assert!(drain_frames(&mut drain).is_empty());
        assert!(
            slot.inner.lock().unwrap().is_some(),
            "a predecessor still inside the wait is not deregistered"
        );
    }

    /// Boundary: registered and past the wait. C cancels it and answers the
    /// SAVED request — `&pPutNotify->msg`, not the arriving one.
    #[test]
    fn an_overdue_predecessor_draws_putcbinprog_for_its_own_ioid() {
        let (outbox, mut drain) = live_outbox();
        let slot = slot_registered_for(0x1234, PUT_NOTIFY_BLOCK_TIMEOUT);
        slot.serialize(&outbox).unwrap();

        let frames = drain_frames(&mut drain);
        assert_eq!(frames.len(), 1, "exactly one reply");
        assert_eq!(
            status_and_ioid(&frames[0]),
            (ECA_PUTCBINPROG, 0x1234),
            "the timed-out predecessor's ioid hears ECA_PUTCBINPROG"
        );
        assert!(
            slot.inner.lock().unwrap().is_none(),
            "and it is deregistered, so it cannot be answered twice"
        );
    }

    /// Boundary: registered, past the wait, but already answered by its own
    /// completion. C re-tests `busy` under `putNotifyLock` before replying
    /// (`camessage.c:1730`); a put that finished keeps its real reply.
    #[test]
    fn an_already_answered_predecessor_draws_no_second_frame() {
        let (outbox, mut drain) = live_outbox();
        let completion = PutNotifyCompletion::new(None);
        assert!(completion.claim().is_some(), "the completion path replies");
        let slot = PutNotifySlot::default();
        slot.install(InFlightPutNotify {
            completion,
            reply: reply_shape(0x55),
            busy_since: Instant::now() - PUT_NOTIFY_BLOCK_TIMEOUT,
        });

        slot.serialize(&outbox).unwrap();
        assert!(
            drain_frames(&mut drain).is_empty(),
            "no second reply for an already-answered ioid"
        );
    }

    /// A settled predecessor is deregistered on the way past, so the slot
    /// tracks "may still be busy" and nothing else.
    #[test]
    fn a_settled_predecessor_is_deregistered() {
        let (outbox, _drain) = live_outbox();
        let completion = PutNotifyCompletion::new(None);
        completion.claim();
        let slot = PutNotifySlot::default();
        slot.install(InFlightPutNotify {
            completion,
            reply: reply_shape(0x55),
            busy_since: Instant::now(),
        });
        slot.serialize(&outbox).unwrap();
        assert!(slot.inner.lock().unwrap().is_none());
    }

    /// `claim` hands out the reply and the put-log bracket together, once —
    /// the property that keeps a reply and its `asTrapWriteAfter` from being
    /// closed by different paths.
    #[test]
    fn claim_grants_a_single_reply_owner() {
        let completion = PutNotifyCompletion::new(None);
        assert!(completion.is_busy(), "a fresh put-callback is busy");
        assert!(completion.claim().is_some(), "the first caller owns it");
        assert!(completion.claim().is_none(), "every later caller does not");
        assert!(!completion.is_busy(), "and it is no longer busy");
    }

    /// Cancelling an overdue predecessor fires its cancel AfterWrite, so the
    /// put-log carries a balanced Before/After — C `asTrapWriteAfter` on the
    /// cancelled put (`camessage.c:1744`). The bracket rides in the
    /// completion token, so it goes wherever the reply goes.
    #[test]
    fn an_overdue_predecessor_closes_its_put_log_bracket() {
        use epics_base_rs::server::access_security::{
            TrapWriteFields, TrapWriteGuard, TrapWriteOp, next_trap_write_event_id,
            register_trap_write_listener,
        };
        use std::sync::{Arc, Mutex};

        // The listener registry is process-global; filter to this test's pv.
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let _handle = register_trap_write_listener(Arc::new(move |msg| {
            if msg.pv_name == "overdue:trap" {
                sink.lock()
                    .unwrap()
                    .push((msg.op, msg.status.map(str::to_owned)));
            }
        }));

        let guard = TrapWriteGuard::begin(TrapWriteFields {
            pv_name: "overdue:trap".to_string(),
            user: "u".to_string(),
            host: "h".to_string(),
            peer: "h:5064".to_string(),
            value_str: "1".to_string(),
            dbr_type: epics_base_rs::types::DBR_LONG,
            no_elements: 1,
            event_id: next_trap_write_event_id(),
            rule_was_trap: true,
            cancel_status: "cancel".to_string(),
        });
        let slot = PutNotifySlot::default();
        slot.install(InFlightPutNotify {
            completion: PutNotifyCompletion::new(Some(guard)),
            reply: reply_shape(0x77),
            busy_since: Instant::now() - PUT_NOTIFY_BLOCK_TIMEOUT,
        });

        slot.serialize(&live_outbox().0).unwrap();

        assert_eq!(
            events.lock().unwrap().clone(),
            vec![
                (TrapWriteOp::BeforeWrite, None),
                (TrapWriteOp::AfterWrite, Some("cancel".to_string())),
            ]
        );
    }
}

#[cfg(test)]
mod truncate_diag_tests {
    use super::{CA_PROTO_ERROR_MAX_DIAG_LEN, truncate_diag};

    #[test]
    fn passes_through_short_message() {
        let s = "channel limit reached";
        assert_eq!(truncate_diag(s), s);
    }

    #[test]
    fn truncates_at_exact_limit() {
        let s = "x".repeat(CA_PROTO_ERROR_MAX_DIAG_LEN);
        assert_eq!(truncate_diag(&s).len(), CA_PROTO_ERROR_MAX_DIAG_LEN);
    }

    #[test]
    fn truncates_oversize_to_limit() {
        let s = "x".repeat(CA_PROTO_ERROR_MAX_DIAG_LEN + 100);
        let out = truncate_diag(&s);
        assert_eq!(out.len(), CA_PROTO_ERROR_MAX_DIAG_LEN);
        assert!(out.chars().all(|c| c == 'x'));
    }

    #[test]
    fn truncation_lands_on_utf8_char_boundary() {
        // Construct a string that crosses the 480-byte cap inside
        // a multi-byte UTF-8 sequence: 'é' (U+00E9) is 2 bytes in
        // UTF-8. Padding with 479 'a's puts the first byte of 'é'
        // exactly at byte 479 — within the limit — and the second
        // at byte 480 — past it. Naive byte slicing would split it
        // and panic. `truncate_diag` must back off to the previous
        // char boundary (byte 479).
        let mut s = "a".repeat(479);
        s.push('é');
        assert_eq!(s.len(), 481);
        let out = truncate_diag(&s);
        assert!(out.is_char_boundary(out.len()));
        assert!(out.len() <= CA_PROTO_ERROR_MAX_DIAG_LEN);
        // Standard library guarantees: the returned &str is valid
        // UTF-8 (otherwise this method-call would panic).
        let _ = out.to_owned();
    }
}

#[cfg(test)]
mod multi_nic_listener_tests {
    //! C-parity regression: `run_tcp_listener` must honour every entry
    //! in `EPICS_CAS_INTF_ADDR_LIST`, not just the first.
    //!
    //! C `rsrv_init` (caservertask.c:603-712) iterates `casIntfAddrList`
    //! and spawns one accept thread per entry, all on the same TCP
    //! port. The previous Rust implementation bound only the first
    //! interface, so a server configured with `INTF_ADDR_LIST="A B"`
    //! silently dropped TCP accepts on interface B.
    use super::*;
    use epics_base_rs::server::database::PvDatabase;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;
    use tokio::net::TcpStream;
    use tokio::sync::broadcast;

    /// Bind an ephemeral listener set and spawn `run_tcp_listener` on it
    /// against a per-test database; return the (port, abort-handle).
    /// Honours whatever EPICS_CAS_INTF_ADDR_LIST is currently set in the
    /// process env (caller manages it).
    async fn start_listener() -> (u16, tokio::task::JoinHandle<()>) {
        let db = Arc::new(PvDatabase::new());
        let acf = epics_base_rs::server::access_security::new_acf_cell(None);
        let (acf_reload_tx, _) = broadcast::channel::<()>(4);
        let drain = Arc::new(AtomicBool::new(false));
        let bound = bind_tcp_listeners(0).await.expect("listener bound");
        let port = bound.port();
        let handle = tokio::spawn(async move {
            let _ = run_tcp_listener(
                db,
                bound,
                acf,
                acf_reload_tx,
                None,
                None,
                drain,
                None,
                #[cfg(feature = "experimental-rust-tls")]
                None,
                #[cfg(feature = "cap-tokens")]
                None,
            )
            .await;
        });
        (port, handle)
    }

    /// R18-22, the half a stderr capture cannot see: C publishes the port the
    /// server ACTUALLY bound as `RSRV_SERVER_PORT` (`caservertask.c:592`),
    /// unconditionally — which is the only way a startup script can find an
    /// IOC that was given an ephemeral port. The port never set it, so
    /// `RSRV_SERVER_PORT` was unset in every Rust IOC that ever ran.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial]
    async fn the_bound_tcp_port_is_published_as_rsrv_server_port() {
        // SAFETY: gated by `serial_test::serial`.
        unsafe { std::env::remove_var("RSRV_SERVER_PORT") };
        let bound = bind_tcp_listeners(0).await.expect("listener bound");
        assert_eq!(
            std::env::var("RSRV_SERVER_PORT").ok(),
            Some(bound.port().to_string())
        );
    }

    /// Confirm `INTF_ADDR_LIST=127.0.0.1` results in a listener that
    /// accepts on 127.0.0.1. This is the "single specific IP" path
    /// which already worked before — the test guards against a
    /// regression in the refactor.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial]
    async fn single_specific_intf_binds_and_accepts() {
        let saved = std::env::var("EPICS_CAS_INTF_ADDR_LIST").ok();
        // SAFETY: gated by `serial_test::serial`; restored before return.
        unsafe { std::env::set_var("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1") };

        let (port, listener_task) = start_listener().await;
        // Connect — TCP handshake completes only if the listener bound
        // to 127.0.0.1 and is accepting.
        let stream = tokio::time::timeout(
            Duration::from_secs(2),
            TcpStream::connect(("127.0.0.1", port)),
        )
        .await
        .expect("connect within timeout")
        .expect("connect succeeded");
        drop(stream);

        listener_task.abort();
        let _ = listener_task.await;

        // SAFETY: same `serial_test::serial` scope.
        unsafe {
            match saved {
                Some(v) => std::env::set_var("EPICS_CAS_INTF_ADDR_LIST", v),
                None => std::env::remove_var("EPICS_CAS_INTF_ADDR_LIST"),
            }
        }
    }

    /// Two-entry `INTF_ADDR_LIST`: the first valid interface decides
    /// the port; the second must bind on the same port. Use
    /// `127.0.0.1` for both — POSIX rejects two identical
    /// (addr,port) binds, so the second bind on the same loopback IP
    /// fails. The contract is that a failed *subsequent* bind is
    /// logged-and-skipped (matching C `cleanup: continue;`), and the
    /// listener as a whole still serves the first interface.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial]
    async fn duplicate_intf_subsequent_skipped_not_fatal() {
        let saved = std::env::var("EPICS_CAS_INTF_ADDR_LIST").ok();
        // SAFETY: gated by `serial_test::serial`.
        unsafe { std::env::set_var("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1 127.0.0.1") };

        let (port, listener_task) = start_listener().await;
        // First listener still accepts.
        let stream = tokio::time::timeout(
            Duration::from_secs(2),
            TcpStream::connect(("127.0.0.1", port)),
        )
        .await
        .expect("connect within timeout")
        .expect("connect succeeded — first listener serves");
        drop(stream);

        listener_task.abort();
        let _ = listener_task.await;

        // SAFETY: same scope.
        unsafe {
            match saved {
                Some(v) => std::env::set_var("EPICS_CAS_INTF_ADDR_LIST", v),
                None => std::env::remove_var("EPICS_CAS_INTF_ADDR_LIST"),
            }
        }
    }

    /// Empty list → falls back to single 0.0.0.0 bind (default).
    /// Asserts the empty-list path didn't regress.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial]
    async fn empty_intf_list_binds_wildcard() {
        let saved = std::env::var("EPICS_CAS_INTF_ADDR_LIST").ok();
        // SAFETY: gated by `serial_test::serial`.
        unsafe { std::env::remove_var("EPICS_CAS_INTF_ADDR_LIST") };

        let (port, listener_task) = start_listener().await;
        // 0.0.0.0 binds accept connections on every local IP including
        // 127.0.0.1.
        let stream = tokio::time::timeout(
            Duration::from_secs(2),
            TcpStream::connect(("127.0.0.1", port)),
        )
        .await
        .expect("connect within timeout")
        .expect("connect succeeded");
        drop(stream);

        listener_task.abort();
        let _ = listener_task.await;

        // SAFETY: same scope.
        unsafe {
            if let Some(v) = saved {
                std::env::set_var("EPICS_CAS_INTF_ADDR_LIST", v);
            }
        }
    }
}

#[cfg(test)]
mod pre_v49_peer_tests {
    //! R6-18 — the server must never speak the extended (24-byte) CA
    //! header to a peer that has not announced CA_V49, in either
    //! direction:
    //!
    //! * receive: `m_postsize == 0xffff` from a pre-V49 peer is NOT an
    //!   extended marker. C reads it as a plain header with a 65,535-byte
    //!   body (`camessage.c:2483-2486` else-branch), so `msgsize = 65551` fails
    //!   the `msgsize & 0x7` test at `camessage.c:2520` →
    //!   ECA_INTERNAL "CAS: Missaligned protocol rejected" + disconnect.
    //! * send: an error echo for a pre-V49 peer is the 16-byte form
    //!   (`vsend_err`, `camessage.c:225-233`).
    use super::single_write_all_framing_tests::{drain_frames, live_outbox};
    use super::*;
    use epics_base_rs::server::database::PvDatabase;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// The ACF-reload sender must outlive the handler: `handle_client`
    /// treats a closed reload channel as "server shutting down" and exits
    /// its read loop, so a dropped sender would end the session before the
    /// first byte is parsed. Hand it back to the caller.
    async fn spawn_handler() -> (
        tokio::io::DuplexStream,
        broadcast::Sender<()>,
        tokio::task::JoinHandle<CaResult<()>>,
    ) {
        let db = Arc::new(PvDatabase::new());
        let acf = epics_base_rs::server::access_security::new_acf_cell(None);
        let (acf_reload_tx, acf_reload_rx) = broadcast::channel::<()>(4);
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let peer: SocketAddr = "127.0.0.1:55987".parse().unwrap();
        let handle = tokio::spawn(async move {
            handle_client(
                server_io,
                peer,
                db,
                acf,
                acf_reload_rx,
                5064,
                None,
                None,
                None,
                None,
                None,
                #[cfg(feature = "cap-tokens")]
                None,
                #[cfg(feature = "cap-tokens")]
                None,
            )
            .await
        });
        (client_io, acf_reload_tx, handle)
    }

    /// Read from `client` until a `CA_PROTO_ERROR` frame appears, and
    /// return its ECA status. The server emits an unsolicited VERSION
    /// greeting on connect plus a VERSION reply to ours, so the error
    /// frame is never at a fixed offset — walk the header chain.
    async fn read_until_error(
        client: &mut tokio::io::DuplexStream,
        peer_minor: u16,
    ) -> Option<(u32, String)> {
        let mut rx: Vec<u8> = Vec::new();
        let mut buf = vec![0u8; 512];
        loop {
            let mut offset = 0;
            while offset + CaHeader::SIZE <= rx.len() {
                let (hdr, consumed) =
                    CaHeader::from_bytes_for_peer(&rx[offset..], CA_MINOR_VERSION)
                        .expect("server frames parse");
                if hdr.cmmd == CA_PROTO_ERROR {
                    let body_start = offset + consumed;
                    let body_end = body_start + hdr.actual_postsize();
                    if body_end > rx.len() {
                        break; // body still in flight
                    }
                    // Body = echoed request header (16 or 24 bytes) + the
                    // NUL-terminated diagnostic string.
                    let body = &rx[body_start..body_end];
                    // The echoed request header is 24 bytes only for a V49
                    // peer (`vsend_err`, camessage.c:211-233) — a pre-V49
                    // echo is 16 bytes even when its truncated postsize
                    // happens to read 0xffff, which is precisely why the
                    // real client keys this off its own version, not off
                    // the bytes.
                    let echo_len = if crate::protocol::ca_v49(peer_minor) && body.len() >= 24 {
                        24
                    } else {
                        16
                    };
                    let diag = String::from_utf8_lossy(&body[echo_len..])
                        .trim_end_matches('\0')
                        .to_string();
                    return Some((hdr.available, diag));
                }
                let msg_len = consumed + hdr.actual_postsize();
                if offset + msg_len > rx.len() {
                    break;
                }
                offset += msg_len;
            }
            let n = match tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf)).await
            {
                Ok(Ok(0)) | Err(_) => return None,
                Ok(Ok(n)) => n,
                Ok(Err(_)) => return None,
            };
            rx.extend_from_slice(&buf[..n]);
        }
    }

    /// A peer that identifies as CA minor 8 (V44+, so it is not the
    /// "too old" ECA_DEFUNCT case, but pre-V49) sends a header with
    /// `m_postsize == 0xffff`. C never treats that as an extended
    /// marker for this peer, so it becomes a 65,535-byte body →
    /// misaligned → ECA_INTERNAL + disconnect. Pre-fix Rust parsed the
    /// annex unconditionally and happily accepted the frame.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pre_v49_extended_marker_rejected_as_misaligned() {
        let (mut client, _acf_reload_tx, handle) = spawn_handler().await;

        let mut ver = CaHeader::new(CA_PROTO_VERSION);
        ver.count = 8; // V44 <= 8 < V49
        client
            .write_all(&ver.to_bytes())
            .await
            .expect("write version");

        // Extended-form READ_NOTIFY, exactly as a V49 peer would frame a
        // 200,000-element read. All 24 bytes are present, so the only
        // thing that can reject it is the version gate.
        let mut hdr = CaHeader::new(CA_PROTO_READ_NOTIFY);
        hdr.set_payload_size(0, 200_000, CA_MINOR_VERSION)
            .expect("frame it as a V49 peer would");
        client
            .write_all(&hdr.to_bytes_extended())
            .await
            .expect("write extended header");
        client.flush().await.expect("flush");

        let (eca, diag) = read_until_error(&mut client, 8)
            .await
            .expect("server must reply CA_PROTO_ERROR");
        assert_eq!(
            diag, "CAS: Missaligned protocol rejected",
            "C reads the pre-V49 0xffff postsize as a 65,551-byte message and \
             rejects it at the alignment test (camessage.c:2520)"
        );
        assert_eq!(
            eca, ECA_INTERNAL,
            "C sends ECA_INTERNAL for a misaligned message"
        );

        let res = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("handler exits")
            .expect("join ok");
        assert!(
            res.is_err(),
            "C disconnects the client after the misaligned rejection"
        );
    }

    /// A V49 peer sending the SAME bytes is served normally — the
    /// rejection above must be attributable to the version gate, not to
    /// the frame itself. The read targets a non-existent channel, so the
    /// reply is ECA_BADCHID / a channel error, never ECA_INTERNAL.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn v49_peer_extended_marker_accepted() {
        let (mut client, _acf_reload_tx, handle) = spawn_handler().await;

        let mut ver = CaHeader::new(CA_PROTO_VERSION);
        ver.count = CA_MINOR_VERSION;
        client
            .write_all(&ver.to_bytes())
            .await
            .expect("write version");

        let mut hdr = CaHeader::new(CA_PROTO_READ_NOTIFY);
        hdr.set_payload_size(0, 200_000, CA_MINOR_VERSION)
            .expect("V49 peer accepts the extended header");
        client
            .write_all(&hdr.to_bytes_extended())
            .await
            .expect("write extended header");
        client.flush().await.expect("flush");

        // The read targets a channel that was never created, so C answers
        // with its bad-resource-id error and drops the circuit either way.
        // What must differ is WHY: the V49 peer's header is parsed as an
        // extended header, never rejected as a misaligned plain one.
        let (_eca, diag) = read_until_error(&mut client, CA_MINOR_VERSION)
            .await
            .expect("server replies with an error for the unknown sid");
        assert_ne!(
            diag, "CAS: Missaligned protocol rejected",
            "a V49 peer's extended header must be parsed, not rejected as misaligned"
        );
        drop(client);
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    /// C `vsend_err` (`camessage.c:211-233`) echoes the request header in
    /// extended form only when `CA_V49(minor)`. For a pre-V49 peer the
    /// echo is the plain 16-byte header — a 24-byte echo would de-sync a
    /// client with no annex parser.
    #[epics_macros_rs::epics_test]
    async fn pre_v49_error_echo_is_sixteen_bytes() {
        let (outbox, mut drain) = live_outbox();
        let mut original = CaHeader::new(CA_PROTO_READ_NOTIFY);
        original
            .set_payload_size(0, 0x1_0000, CA_MINOR_VERSION)
            .expect("extended original");
        assert!(original.is_extended());

        send_ca_error(
            &outbox,
            &original,
            ECA_INTERNAL,
            0xFFFF_FFFF,
            "boom",
            8, // pre-V49 peer
        )
        .expect("send_ca_error succeeds");

        let frames = drain_frames(&mut drain);
        let frame = &frames[0];
        let diag = pad_string("boom");
        assert_eq!(
            frame.len(),
            CaHeader::SIZE + 16 + diag.len(),
            "pre-V49 echo must be the 16-byte header form"
        );
        let echo_postsize = u16::from_be_bytes([frame[18], frame[19]]);
        assert_eq!(
            echo_postsize, 0,
            "the echoed 16-bit postsize is the truncated original \
             (C: htons((ca_uint16_t) curp->m_postsize))"
        );
    }

    /// The same error to a V49 peer keeps the 24-byte extended echo.
    #[epics_macros_rs::epics_test]
    async fn v49_error_echo_is_twenty_four_bytes() {
        let (outbox, mut drain) = live_outbox();
        let mut original = CaHeader::new(CA_PROTO_READ_NOTIFY);
        original
            .set_payload_size(0, 0x1_0000, CA_MINOR_VERSION)
            .expect("extended original");

        send_ca_error(
            &outbox,
            &original,
            ECA_INTERNAL,
            0xFFFF_FFFF,
            "boom",
            CA_MINOR_VERSION,
        )
        .expect("send_ca_error succeeds");

        let frames = drain_frames(&mut drain);
        let frame = &frames[0];
        let diag = pad_string("boom");
        assert_eq!(
            frame.len(),
            CaHeader::SIZE + 24 + diag.len(),
            "V49 echo carries the 8-byte annex"
        );
    }

    /// C `vsend_err` chooses the echo's form from the request's real size
    /// and count, and its condition is an OR over BOTH
    /// (`camessage.c:210-211`). A normal-form request carrying
    /// `m_count == 0xffff` therefore gets the 24-byte echo, even though it
    /// arrived in 16 bytes with no annex of its own.
    ///
    /// The port asked the parsed header whether it had arrived extended, so
    /// this request was echoed in 16 bytes: eight bytes shorter than a C
    /// softIoc's answer to the identical frame, with the diagnostic starting
    /// at a different offset.
    #[epics_macros_rs::epics_test]
    async fn a_max_count_request_in_normal_form_is_echoed_extended() {
        let (outbox, mut drain) = live_outbox();
        // Normal form on the wire: no annex, `m_count` at its 16-bit ceiling.
        let mut original = CaHeader::new(CA_PROTO_EVENT_ADD);
        original.count = 0xFFFF;
        assert!(!original.is_extended(), "the request arrived in 16 bytes");

        send_ca_error(
            &outbox,
            &original,
            ECA_INTERNAL,
            0xFFFF_FFFF,
            "boom",
            CA_MINOR_VERSION,
        )
        .expect("send_ca_error succeeds");

        let frames = drain_frames(&mut drain);
        let frame = &frames[0];
        let diag = pad_string("boom");
        assert_eq!(
            frame.len(),
            CaHeader::SIZE + 24 + diag.len(),
            "m_count >= 0xffff alone selects the extended echo"
        );
        assert_eq!(
            u16::from_be_bytes([frame[18], frame[19]]),
            0xFFFF,
            "the echo carries the extended marker C writes"
        );
        assert_eq!(
            u16::from_be_bytes([frame[22], frame[23]]),
            0,
            "C zeroes the 16-bit m_count when the annex carries the real one"
        );
        assert_eq!(
            u32::from_be_bytes([frame[36], frame[37], frame[38], frame[39]]),
            0xFFFF,
            "the annex carries the request's real count"
        );
    }

    /// The converse, from the same OR: an extended request whose real size
    /// and count both fit in 16 bits is echoed in the SHORT form, because C
    /// reads the values and not the layout they arrived in.
    #[epics_macros_rs::epics_test]
    async fn an_extended_request_with_small_values_is_echoed_short() {
        let (outbox, mut drain) = live_outbox();
        // Extended on the wire, but neither value needs the annex.
        let mut original = CaHeader::new(CA_PROTO_READ_NOTIFY);
        original.postsize = 0xFFFF;
        original.extended_postsize = Some(8);
        original.extended_count = Some(3);
        assert!(original.is_extended(), "the request arrived in 24 bytes");

        send_ca_error(
            &outbox,
            &original,
            ECA_INTERNAL,
            0xFFFF_FFFF,
            "boom",
            CA_MINOR_VERSION,
        )
        .expect("send_ca_error succeeds");

        let frames = drain_frames(&mut drain);
        let frame = &frames[0];
        let diag = pad_string("boom");
        assert_eq!(
            frame.len(),
            CaHeader::SIZE + 16 + diag.len(),
            "neither value reaches 0xffff, so C echoes the 16-byte form"
        );
        assert_eq!(
            u16::from_be_bytes([frame[18], frame[19]]),
            8,
            "the echoed postsize is the request's real one, not the marker"
        );
        assert_eq!(
            u16::from_be_bytes([frame[22], frame[23]]),
            3,
            "the echoed count is the request's real one"
        );
    }

    /// A response too large for the 16-bit header cannot be framed for a
    /// pre-V49 peer. C `read_reply` / `read_action` answer with
    /// `send_err(ECA_16KARRAYCLIENT)` (`camessage.c:630-639`) rather than
    /// emitting an extended header the peer cannot parse.
    #[epics_macros_rs::epics_test]
    async fn oversize_reply_to_pre_v49_peer_is_eca_16karrayclient() {
        use epics_base_rs::server::snapshot::Snapshot;
        use epics_base_rs::types::{DBR_LONG, EpicsValue};

        let (outbox, mut drain) = live_outbox();
        // 20,000 LONG elements = 80,000 payload bytes → needs the
        // extended header (>= 0xffff).
        let values: Vec<i32> = vec![7; 20_000];
        let snapshot = Snapshot::new(
            EpicsValue::LongArray(values),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );
        let req = CaHeader::new(CA_PROTO_READ_NOTIFY);

        send_monitor_snapshot(
            &outbox,
            9,
            DBR_LONG,
            20_000,
            &snapshot,
            ReplyContext {
                req_hdr: req,
                client_minor: 8,
            },
        )
        .expect("the error reply itself must succeed");

        let frames = drain_frames(&mut drain);
        let frame = &frames[0];
        let cmmd = u16::from_be_bytes([frame[0], frame[1]]);
        let eca = u32::from_be_bytes([frame[12], frame[13], frame[14], frame[15]]);
        assert_eq!(cmmd, CA_PROTO_ERROR, "pre-V49 peer gets an error, not data");
        assert_eq!(
            eca, ECA_16KARRAYCLIENT,
            "C answers an unframeable large array with ECA_16KARRAYCLIENT"
        );
    }
}

#[cfg(test)]
mod extended_header_split_tests {
    //! C-parity regression: a TCP segment that ends in the middle of an
    //! extended-form header (16..24 bytes, `m_postsize == 0xffff`) must
    //! make the framing loop *wait* for the rest of the header, not
    //! disconnect the client. C `rsrv/camessage.c:2466-2469` does
    //! `status = RSRV_OK; break;` for this partial-header case.
    use super::*;
    use epics_base_rs::server::database::PvDatabase;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;

    /// Feed exactly 20 bytes of an extended-form header (a 16-byte base
    /// header with `postsize == 0xFFFF`, plus 4 of the 8 extended
    /// bytes) and assert `handle_client` does NOT return early with an
    /// error: it must block awaiting the remaining 4 bytes. Pre-fix,
    /// `from_bytes_extended` returned `Err("extended header
    /// incomplete")` and the `?` closed the connection.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn partial_extended_header_waits_not_disconnects() {
        let db = Arc::new(PvDatabase::new());
        let acf = epics_base_rs::server::access_security::new_acf_cell(None);
        let (_acf_reload_tx, acf_reload_rx) = broadcast::channel::<()>(4);

        let (client_io, server_io) = tokio::io::duplex(256);
        let peer: SocketAddr = "127.0.0.1:55123".parse().unwrap();

        let handle = tokio::spawn(async move {
            handle_client(
                server_io,
                peer,
                db,
                acf,
                acf_reload_rx,
                5064,
                None,
                None,
                None,
                None,
                None,
                #[cfg(feature = "cap-tokens")]
                None,
                #[cfg(feature = "cap-tokens")]
                None,
            )
            .await
        });

        // Build a CA_PROTO_READ_NOTIFY header in extended form:
        // postsize=0xFFFF marks extended; write only 20 of the 24
        // header bytes so the framing loop sees a partial ext header.
        let mut hdr = CaHeader::new(CA_PROTO_READ_NOTIFY);
        hdr.postsize = 0xFFFF;
        let base = hdr.to_bytes();
        let mut prefix = base.to_vec();
        // 4 of the 8 extended bytes (extended postsize = 0).
        prefix.extend_from_slice(&[0u8, 0, 0, 0]);
        assert_eq!(prefix.len(), 20);

        let mut client = client_io;
        // Identify as V49 first: C only parses the extended annex when
        // `CA_V49(client->minor_version_number)` (`camessage.c:2464`), so
        // the partial-annex wait is reachable only for a V49 peer. A
        // pre-V49 peer sending the same bytes is rejected — see
        // `pre_v49_extended_marker_rejected_as_misaligned`.
        let mut ver = CaHeader::new(CA_PROTO_VERSION);
        ver.count = CA_MINOR_VERSION;
        client
            .write_all(&ver.to_bytes())
            .await
            .expect("write version");
        client.write_all(&prefix).await.expect("write prefix");
        client.flush().await.expect("flush prefix");

        // The handler must still be running — it is waiting for the
        // remaining 4 bytes, not disconnected with an error.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !handle.is_finished(),
            "handle_client returned on a partial extended header — \
             must wait for more bytes (C camessage.c RSRV_OK; break)"
        );

        // Close the write half: a clean EOF on a partial frame must
        // resolve to Ok(()), never an Err.
        drop(client);
        let res = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("handle_client completes after EOF")
            .expect("join ok");
        assert!(
            res.is_ok(),
            "clean EOF after partial extended header must be Ok, got {res:?}"
        );
    }
}

#[cfg(test)]
mod host_identity_tests {
    //! R7-16: the ACF host identity of a circuit.
    //!
    //! C rsrv's default (`asCheckClientIP == 0`, `asLibRoutines.c:34`) is to
    //! store the hostname the client claims over `CA_PROTO_HOST_NAME`
    //! unconditionally (`camessage.c:880-903`) and match HAGs against that
    //! name (`asLibRoutines.c:1223`). The port instead keyed ACF on the peer
    //! IP unless a fictitious `EPICS_CAS_USE_HOST_NAMES=YES` was set — a
    //! variable that does not exist anywhere in epics-base — so a
    //! `HOST(node)` rule that granted WRITE under C granted nothing here.
    use super::*;

    #[test]
    fn claimed_identity_takes_the_client_supplied_name() {
        // C's default path: `create_tcp_client` leaves pHostName NULL, so
        // the identity is "" until the client claims one.
        let mut id = HostIdentity::Claimed(String::new());
        assert_eq!(
            id.as_str(),
            "",
            "no claim yet — C passes \"\" to asAddClient"
        );

        assert!(id.claim("opi-01.lab".into()), "a claimed identity takes it");
        assert_eq!(
            id.as_str(),
            "opi-01.lab",
            "C stores the client-supplied name unconditionally (camessage.c:880-903)"
        );
    }

    #[test]
    fn pinned_identity_ignores_the_client_supplied_name() {
        // C's `asCheckClientIP == 1` path: the peer IP is the identity and
        // `host_name_action` returns early without storing the claim
        // (camessage.c:869-875). The port's mTLS identity is pinned the
        // same way.
        let mut id = HostIdentity::Pinned("10.0.0.7".into());
        assert!(
            !id.claim("privileged-console".into()),
            "a pinned identity refuses the claim"
        );
        assert_eq!(
            id.as_str(),
            "10.0.0.7",
            "a client cannot spoof its way past a pinned identity"
        );
    }

    /// The connection-setup decision, made where C makes it
    /// (`create_tcp_client`, `caservertask.c:1425-1437`). Reproduces the
    /// same three-way match `handle_client` runs, pinning the mapping from
    /// (mTLS identity, asCheckClientIP) to variant.
    #[test]
    fn identity_source_is_decided_once_at_connection_setup() {
        use epics_base_rs::server::access_security::{as_check_client_ip, set_as_check_client_ip};

        let peer: SocketAddr = "192.168.4.9:44321".parse().unwrap();
        let decide = |verified: Option<String>| match verified {
            Some(v) => HostIdentity::Pinned(v),
            None if as_check_client_ip() => HostIdentity::Pinned(peer.ip().to_string()),
            None => HostIdentity::Claimed(String::new()),
        };

        // C default: claimable, empty until CA_PROTO_HOST_NAME.
        set_as_check_client_ip(false);
        assert_eq!(decide(None), HostIdentity::Claimed(String::new()));

        // asCheckClientIP=1: pinned to the peer's dotted-quad address.
        set_as_check_client_ip(true);
        assert_eq!(
            decide(None),
            HostIdentity::Pinned("192.168.4.9".into()),
            "C fills pHostName with the peer IP (caservertask.c:1432)"
        );

        // An mTLS-verified identity outranks both modes.
        assert_eq!(
            decide(Some("CN=alice".into())),
            HostIdentity::Pinned("CN=alice".into())
        );
        set_as_check_client_ip(false);
        assert_eq!(
            decide(Some("CN=alice".into())),
            HostIdentity::Pinned("CN=alice".into()),
            "the cert identity is pinned regardless of asCheckClientIP"
        );
    }
}

#[cfg(test)]
mod oversize_request_tests {
    //! R7-18: an oversize inbound request must NOT tear the circuit down.
    //!
    //! C `camessage.c:2539-2556` answers a message it cannot buffer with
    //! `send_err(ECA_TOLARGE)`, sets `recvBytesToDrain` to skip the body,
    //! and returns `RSRV_OK` — the circuit and every channel and
    //! subscription on it survive. The port used to `break 'client_loop
    //! Err(..)` here, so one oversize array caput destroyed the whole
    //! client. Server-side sibling of the client-side R6-21 fix.
    use super::*;
    use epics_base_rs::server::database::PvDatabase;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// End-to-end: an oversize extended-form request draws ECA_TOLARGE,
    /// its body is drained, and the circuit keeps serving — the ECHO that
    /// follows the oversize body is answered.
    ///
    /// The bound is only a *bound* when `EPICS_CA_AUTO_ARRAY_BYTES` is off:
    /// that is the sole configuration in which C's server allocates a fixed
    /// `rsrvSizeofLargeBufTCP` buffer and answers ECA_TOLARGE past it
    /// (`caservertask.c:534-538`, `camessage.c:2539-2556`). With it ON — the
    /// compiled default — C grows the buffer to whatever the peer announced and
    /// refuses nothing, so an oversize request is not a thing that exists.
    ///
    /// This test used to set `EPICS_CA_MAX_ARRAY_BYTES` **alone** and expect a
    /// refusal, which pinned the defect it was written under: that variable was
    /// also standing in as the port's DoS cap, so it bounded a path C does not
    /// bound with it. It sets both parameters now, and takes the ceiling from
    /// [`crate::protocol::max_frame_body_bytes`] rather than assuming the raw
    /// env value is the ceiling — C floors it at `MAX_TCP` and adds the 24-byte
    /// extended header, so a 4 KiB request really yields a 16 KiB buffer.
    ///
    /// nextest runs each test in its own process, so the env mutation is
    /// contained.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn oversize_request_draws_eca_tolarge_and_keeps_the_circuit() {
        // SAFETY: nextest gives each test its own process; no other thread
        // in it reads the environment concurrently at this point.
        unsafe {
            std::env::set_var("EPICS_CA_AUTO_ARRAY_BYTES", "NO");
            std::env::set_var("EPICS_CA_MAX_ARRAY_BYTES", "4096");
        }
        let ceiling = crate::protocol::max_frame_body_bytes();
        assert_eq!(
            ceiling,
            crate::protocol::MAX_TCP,
            "C rounds a sub-MAX_TCP EPICS_CA_MAX_ARRAY_BYTES up to MAX_TCP"
        );

        let db = Arc::new(PvDatabase::new());
        let acf = epics_base_rs::server::access_security::new_acf_cell(None);
        let (_acf_reload_tx, acf_reload_rx) = broadcast::channel::<()>(4);
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let peer: SocketAddr = "127.0.0.1:55124".parse().unwrap();

        let handle = tokio::spawn(async move {
            handle_client(
                server_io,
                peer,
                db,
                acf,
                acf_reload_rx,
                5064,
                None,
                None,
                None,
                None,
                None,
                #[cfg(feature = "cap-tokens")]
                None,
                #[cfg(feature = "cap-tokens")]
                None,
            )
            .await
        });

        let mut client = client_io;

        // Identify as V49 — C only reads the extended annex for a V49 peer.
        let mut ver = CaHeader::new(CA_PROTO_VERSION);
        ver.count = CA_MINOR_VERSION;
        client.write_all(&ver.to_bytes()).await.unwrap();

        // Extended-form WRITE with a body one element past the ceiling.
        let ext_post: u32 = (ceiling + 8) as u32;
        let mut hdr = CaHeader::new(CA_PROTO_WRITE);
        hdr.postsize = 0xFFFF;
        let mut frame = hdr.to_bytes().to_vec();
        frame.extend_from_slice(&ext_post.to_be_bytes());
        frame.extend_from_slice(&1u32.to_be_bytes()); // extended count
        client.write_all(&frame).await.unwrap();
        client.flush().await.unwrap();

        // Read one whole CA frame (header + declared payload) so the next
        // read starts on a frame boundary.
        async fn read_frame(c: &mut tokio::io::DuplexStream) -> [u8; 16] {
            let mut hdr = [0u8; 16];
            tokio::time::timeout(Duration::from_secs(2), c.read_exact(&mut hdr))
                .await
                .expect("server must answer, not hang")
                .expect("server must answer, not close the circuit");
            let postsize = u16::from_be_bytes([hdr[2], hdr[3]]) as usize;
            if postsize > 0 {
                let mut body = vec![0u8; postsize];
                c.read_exact(&mut body).await.expect("frame body");
            }
            hdr
        }

        // Skip the VERSION handshake frames, then the ECA_TOLARGE error
        // must come back on the wire — not an EOF.
        let reply = loop {
            let f = read_frame(&mut client).await;
            if u16::from_be_bytes([f[0], f[1]]) != CA_PROTO_VERSION {
                break f;
            }
        };
        assert_eq!(
            u16::from_be_bytes([reply[0], reply[1]]),
            CA_PROTO_ERROR,
            "oversize request must draw a CA_PROTO_ERROR"
        );
        assert_eq!(
            u32::from_be_bytes([reply[12], reply[13], reply[14], reply[15]]),
            ECA_TOLARGE,
            "C answers an unbufferable request with ECA_TOLARGE (camessage.c:2544)"
        );

        // The circuit must still be up: pre-fix this had already been torn
        // down with a Protocol error.
        assert!(
            !handle.is_finished(),
            "an oversize request must not close the circuit (C keeps serving)"
        );

        // Ship the oversize body — the server drains it — then an ECHO.
        // The ECHO is only answered if the drain consumed exactly the body.
        client
            .write_all(&vec![0u8; ext_post as usize])
            .await
            .unwrap();
        let echo = CaHeader::new(CA_PROTO_ECHO);
        client.write_all(&echo.to_bytes()).await.unwrap();
        client.flush().await.unwrap();

        let echo_reply = read_frame(&mut client).await;
        assert_eq!(
            u16::from_be_bytes([echo_reply[0], echo_reply[1]]),
            CA_PROTO_ECHO,
            "the message after a drained oversize body must be parsed normally — \
             the drain must consume the body exactly, no more and no less"
        );

        handle.abort();
    }
}

#[cfg(test)]
mod non_graceful_disconnect_teardown_tests {
    //! CRITICAL regression: every exit path out of `handle_client`'s
    //! read loop — not just the graceful `break` — must run the single
    //! teardown block (subscription cancel + `SubscriptionClosed`
    //! emission + write-notify abort + `ChannelCleared` emission).
    //!
    //! Before the fix, an in-loop `return Err(..)` (misaligned payload,
    //! payload-too-large, dispatch error, send-timeout, rate-limit
    //! disconnect, batched-flush error) bypassed the teardown. A client
    //! that established a subscription and then disconnected
    //! non-gracefully (TCP RST, malformed frame) would leave its
    //! `SubscriptionClosed` event unfired forever — inflating consumer
    //! refcounts that key off these events (e.g. ca_gateway).
    use super::*;
    use epics_base_rs::server::database::PvDatabase;
    use epics_base_rs::types::EpicsValue;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Build a CA_PROTO_VERSION request (minor version 13).
    pub(super) fn version_frame() -> Vec<u8> {
        let mut h = CaHeader::new(CA_PROTO_VERSION);
        h.count = CA_MINOR_VERSION;
        h.to_bytes().to_vec()
    }

    /// Build a CA_PROTO_CREATE_CHAN request for `pv_name` with the
    /// given client cid. Payload is the 8-aligned, NUL-terminated name.
    pub(super) fn create_chan_frame(cid: u32, pv_name: &str) -> Vec<u8> {
        let name = pad_string(pv_name);
        let mut h = CaHeader::new(CA_PROTO_CREATE_CHAN);
        h.cid = cid;
        h.available = CA_MINOR_VERSION as u32;
        h.set_payload_size(name.len(), 0, crate::protocol::CA_MINOR_VERSION)
            .expect("modern peer accepts the extended header");
        let mut frame = h.to_bytes().to_vec();
        frame.extend_from_slice(&name);
        frame
    }

    /// Build a CA_PROTO_EVENT_ADD request: subscribe `sub_id` on `sid`.
    /// Payload is the 16-byte monitor request (low/high/to f32 + mask).
    pub(super) fn event_add_frame(sid: u32, sub_id: u32) -> Vec<u8> {
        let mut h = CaHeader::new(CA_PROTO_EVENT_ADD);
        h.data_type = epics_base_rs::types::DBR_TIME_DOUBLE;
        h.count = 1;
        h.cid = sid;
        h.available = sub_id;
        h.set_payload_size(16, 1, crate::protocol::CA_MINOR_VERSION)
            .expect("modern peer accepts the extended header");
        let mut frame = h.to_bytes().to_vec();
        frame.extend_from_slice(&0f32.to_be_bytes());
        frame.extend_from_slice(&0f32.to_be_bytes());
        frame.extend_from_slice(&0f32.to_be_bytes());
        frame.extend_from_slice(&3u16.to_be_bytes()); // mask: value+alarm
        frame.extend_from_slice(&0u16.to_be_bytes()); // pad
        frame
    }

    /// Drain `rx` for up to `timeout`, returning the first event that
    /// satisfies `pred`, or `None` on timeout.
    pub(super) async fn await_event(
        rx: &mut broadcast::Receiver<ServerConnectionEvent>,
        timeout: Duration,
        mut pred: impl FnMut(&ServerConnectionEvent) -> bool,
    ) -> Option<ServerConnectionEvent> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(ev)) => {
                    if pred(&ev) {
                        return Some(ev);
                    }
                }
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => return None,
            }
        }
    }

    /// Read from `client` until a CA_PROTO_CREATE_CHAN response frame is
    /// seen, then return its server-assigned sid (`m_available`). The
    /// EVENT_ADD request must address the channel by this sid, not by
    /// the client cid.
    pub(super) async fn read_create_chan_sid<R: tokio::io::AsyncRead + Unpin>(
        client: &mut R,
        timeout: Duration,
    ) -> u32 {
        let mut acc: Vec<u8> = Vec::new();
        let mut buf = [0u8; 512];
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for CREATE_CHAN response"
            );
            let n = tokio::time::timeout(remaining, client.read(&mut buf))
                .await
                .expect("read within timeout")
                .expect("read ok");
            assert!(n > 0, "server closed before CREATE_CHAN response");
            acc.extend_from_slice(&buf[..n]);
            let mut offset = 0;
            while offset + CaHeader::SIZE <= acc.len() {
                let (hdr, hdr_size) = CaHeader::from_bytes_extended(&acc[offset..])
                    .expect("server response header parses");
                let msg_len = hdr_size + hdr.actual_postsize();
                if offset + msg_len > acc.len() {
                    break;
                }
                if hdr.cmmd == CA_PROTO_CREATE_CHAN {
                    return hdr.available;
                }
                offset += msg_len;
            }
        }
    }

    /// Read frames until a CREATE_CHAN (success) or CREATE_CH_FAIL response
    /// is seen, then return that header so the caller can distinguish the
    /// two outcomes and inspect the advertised type/count.
    pub(super) async fn read_create_chan_result<R: tokio::io::AsyncRead + Unpin>(
        client: &mut R,
        timeout: Duration,
    ) -> CaHeader {
        let mut acc: Vec<u8> = Vec::new();
        let mut buf = [0u8; 512];
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for CREATE_CHAN result"
            );
            let n = tokio::time::timeout(remaining, client.read(&mut buf))
                .await
                .expect("read within timeout")
                .expect("read ok");
            assert!(n > 0, "server closed before CREATE_CHAN result");
            acc.extend_from_slice(&buf[..n]);
            let mut offset = 0;
            while offset + CaHeader::SIZE <= acc.len() {
                let (hdr, hdr_size) = CaHeader::from_bytes_extended(&acc[offset..])
                    .expect("server response header parses");
                let msg_len = hdr_size + hdr.actual_postsize();
                if offset + msg_len > acc.len() {
                    break;
                }
                if hdr.cmmd == CA_PROTO_CREATE_CHAN || hdr.cmmd == CA_PROTO_CREATE_CH_FAIL {
                    return hdr;
                }
                offset += msg_len;
            }
        }
    }

    /// Regression: a record-level `$` long-string channel — `REC$` with no
    /// explicit `.FIELD` — must resolve and create as a CHAR[40] channel.
    /// C dbChannel.c:482-507 strips the `$` modifier AFTER the record/field
    /// name lookup, so it applies to the default VAL field. The port
    /// previously stripped `$` only from an explicit `.FIELD`, leaving it on
    /// the record key so `find_entry_from` missed the record and returned
    /// CREATE_CH_FAIL where C serves the field's long string.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn record_level_dollar_creates_long_string_channel() {
        use epics_base_rs::server::records::stringin::StringinRecord;

        let db = Arc::new(PvDatabase::new());
        db.add_record("longstr:rec", Box::new(StringinRecord::new("hello")))
            .await
            .expect("add stringin record");
        let acf = epics_base_rs::server::access_security::new_acf_cell(None);
        let (_acf_reload_tx, acf_reload_rx) = broadcast::channel::<()>(4);
        let (conn_tx, _conn_rx) = broadcast::channel::<ServerConnectionEvent>(64);

        let (client_io, server_io) = tokio::io::duplex(4096);
        let peer: SocketAddr = "127.0.0.1:55223".parse().unwrap();
        let handle = tokio::spawn(async move {
            handle_client(
                server_io,
                peer,
                db,
                acf,
                acf_reload_rx,
                5064,
                None,
                None,
                None,
                Some(conn_tx),
                None,
                #[cfg(feature = "cap-tokens")]
                None,
                #[cfg(feature = "cap-tokens")]
                None,
            )
            .await
        });

        let mut client = client_io;
        client.write_all(&version_frame()).await.expect("version");
        // Record-level `$`: no explicit `.FIELD`.
        client
            .write_all(&create_chan_frame(0xAA, "longstr:rec$"))
            .await
            .expect("create_chan");
        client.flush().await.expect("flush create_chan");

        let hdr = read_create_chan_result(&mut client, Duration::from_secs(3)).await;
        assert_eq!(
            hdr.cmmd, CA_PROTO_CREATE_CHAN,
            "record-level `REC$` must create the channel, not CREATE_CH_FAIL"
        );
        // A `$` long string is served as CHAR[40] (DbFieldType::Char == 4).
        assert_eq!(hdr.data_type, 4, "long-string channel advertises DBR_CHAR");
        assert_eq!(hdr.count, 40, "long-string channel advertises 40 elements");

        drop(client);
        handle.abort();
    }

    /// Regression: a `$` long-string channel on a string SimplePv must
    /// advertise CHAR[40], not the native DBR_STRING/1. The channel stores
    /// `long_string` and every delivery path runs `apply_long_string` to
    /// emit CHAR[40], so the SimplePv arm must override the advertised
    /// type/count to match — like the RecordField arm and C
    /// dbChannel.c:486-492.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn simple_pv_dollar_advertises_char_array() {
        let db = Arc::new(PvDatabase::new());
        db.add_pv("longstr:simple", EpicsValue::String("hi".into()))
            .await
            .expect("add string pv");
        let acf = epics_base_rs::server::access_security::new_acf_cell(None);
        let (_acf_reload_tx, acf_reload_rx) = broadcast::channel::<()>(4);
        let (conn_tx, _conn_rx) = broadcast::channel::<ServerConnectionEvent>(64);

        let (client_io, server_io) = tokio::io::duplex(4096);
        let peer: SocketAddr = "127.0.0.1:55224".parse().unwrap();
        let handle = tokio::spawn(async move {
            handle_client(
                server_io,
                peer,
                db,
                acf,
                acf_reload_rx,
                5064,
                None,
                None,
                None,
                Some(conn_tx),
                None,
                #[cfg(feature = "cap-tokens")]
                None,
                #[cfg(feature = "cap-tokens")]
                None,
            )
            .await
        });

        let mut client = client_io;
        client.write_all(&version_frame()).await.expect("version");
        client
            .write_all(&create_chan_frame(0xA1, "longstr:simple$"))
            .await
            .expect("create_chan");
        client.flush().await.expect("flush create_chan");

        let hdr = read_create_chan_result(&mut client, Duration::from_secs(3)).await;
        assert_eq!(
            hdr.cmmd, CA_PROTO_CREATE_CHAN,
            "string SimplePv `$` must create the channel"
        );
        assert_eq!(hdr.data_type, 4, "long-string SimplePv advertises DBR_CHAR");
        assert_eq!(hdr.count, 40, "long-string SimplePv advertises 40 elements");

        drop(client);
        handle.abort();
    }

    /// Regression: a `$` long-string channel on a non-string SimplePv must
    /// be rejected with CREATE_CH_FAIL — C dbChannel.c:500-502 returns
    /// S_dbLib_fieldNotFound for a `$` modifier on a non-DBF_STRING field,
    /// matching the RecordField arm.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn simple_pv_dollar_on_non_string_fails() {
        let db = Arc::new(PvDatabase::new());
        db.add_pv("num:simple", EpicsValue::Double(1.0))
            .await
            .expect("add double pv");
        let acf = epics_base_rs::server::access_security::new_acf_cell(None);
        let (_acf_reload_tx, acf_reload_rx) = broadcast::channel::<()>(4);
        let (conn_tx, _conn_rx) = broadcast::channel::<ServerConnectionEvent>(64);

        let (client_io, server_io) = tokio::io::duplex(4096);
        let peer: SocketAddr = "127.0.0.1:55225".parse().unwrap();
        let handle = tokio::spawn(async move {
            handle_client(
                server_io,
                peer,
                db,
                acf,
                acf_reload_rx,
                5064,
                None,
                None,
                None,
                Some(conn_tx),
                None,
                #[cfg(feature = "cap-tokens")]
                None,
                #[cfg(feature = "cap-tokens")]
                None,
            )
            .await
        });

        let mut client = client_io;
        client.write_all(&version_frame()).await.expect("version");
        client
            .write_all(&create_chan_frame(0xA2, "num:simple$"))
            .await
            .expect("create_chan");
        client.flush().await.expect("flush create_chan");

        let hdr = read_create_chan_result(&mut client, Duration::from_secs(3)).await;
        assert_eq!(
            hdr.cmmd, CA_PROTO_CREATE_CH_FAIL,
            "`$` on a non-string SimplePv must be rejected with CREATE_CH_FAIL"
        );

        drop(client);
        handle.abort();
    }

    /// A client that opens a subscription and then sends a misaligned
    /// frame (postsize not 8-aligned) MUST still have its
    /// `SubscriptionClosed` event emitted. The misaligned frame drives
    /// the `break 'client_loop Err(..)` path that previously bypassed
    /// the teardown block.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn misaligned_frame_after_subscribe_still_emits_subscription_closed() {
        let db = Arc::new(PvDatabase::new());
        db.add_pv("teardown:test:pv", EpicsValue::Double(1.0))
            .await
            .expect("add pv");
        let acf = epics_base_rs::server::access_security::new_acf_cell(None);
        let (_acf_reload_tx, acf_reload_rx) = broadcast::channel::<()>(4);
        let (conn_tx, mut conn_rx) = broadcast::channel::<ServerConnectionEvent>(64);

        let (client_io, server_io) = tokio::io::duplex(4096);
        let peer: SocketAddr = "127.0.0.1:55222".parse().unwrap();

        let handle = tokio::spawn(async move {
            handle_client(
                server_io,
                peer,
                db,
                acf,
                acf_reload_rx,
                5064,
                None,
                None,
                None,
                Some(conn_tx),
                None,
                #[cfg(feature = "cap-tokens")]
                None,
                #[cfg(feature = "cap-tokens")]
                None,
            )
            .await
        });

        let mut client = client_io;
        // Establish the channel first; read back the server-assigned sid.
        client.write_all(&version_frame()).await.expect("version");
        client
            .write_all(&create_chan_frame(0xAA, "teardown:test:pv"))
            .await
            .expect("create_chan");
        client.flush().await.expect("flush create_chan");
        let sid = read_create_chan_sid(&mut client, Duration::from_secs(3)).await;
        // Now subscribe, addressing the channel by its server sid.
        client
            .write_all(&event_add_frame(sid, 0xBB))
            .await
            .expect("event_add");
        client.flush().await.expect("flush event_add");

        // The server must accept the subscription before we test the
        // teardown — otherwise the test would pass vacuously.
        let opened = await_event(&mut conn_rx, Duration::from_secs(3), |ev| {
            matches!(ev, ServerConnectionEvent::SubscriptionOpened { .. })
        })
        .await;
        assert!(
            matches!(opened, Some(ServerConnectionEvent::SubscriptionOpened { sub_id, .. }) if sub_id == 0xBB),
            "subscription must open before the disconnect test (got {opened:?})"
        );

        // Now send a definitively malformed frame: a header whose
        // postsize is not 8-byte aligned. The server rejects it with
        // ECA_INTERNAL and `break 'client_loop Err(..)` — a NON-graceful
        // exit. Before the fix this `return Err` bypassed the teardown.
        let mut bad = CaHeader::new(CA_PROTO_READ_NOTIFY);
        bad.postsize = 5; // not a multiple of 8 — misaligned
        client
            .write_all(&bad.to_bytes())
            .await
            .expect("misaligned frame");
        client.flush().await.expect("flush misaligned");

        // The teardown MUST emit SubscriptionClosed for sub_id 0xBB even
        // though the connection ended via the error path.
        let closed = await_event(&mut conn_rx, Duration::from_secs(3), |ev| {
            matches!(ev, ServerConnectionEvent::SubscriptionClosed { .. })
        })
        .await;
        assert!(
            matches!(closed, Some(ServerConnectionEvent::SubscriptionClosed { sub_id, .. }) if sub_id == 0xBB),
            "SubscriptionClosed must fire on a non-graceful (error-path) \
             disconnect — teardown was bypassed (got {closed:?})"
        );

        // The handler returns Err for the misaligned-frame path.
        let res = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("handle_client completes")
            .expect("join ok");
        assert!(
            res.is_err(),
            "misaligned frame must close the connection with Err, got {res:?}"
        );
        drop(client);
    }

    /// Control case: a graceful EOF disconnect must ALSO emit
    /// `SubscriptionClosed` (the path that always worked) — guards
    /// against the restructure regressing the `break` path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn graceful_eof_after_subscribe_emits_subscription_closed() {
        let db = Arc::new(PvDatabase::new());
        db.add_pv("teardown:test:pv2", EpicsValue::Double(1.0))
            .await
            .expect("add pv");
        let acf = epics_base_rs::server::access_security::new_acf_cell(None);
        let (_acf_reload_tx, acf_reload_rx) = broadcast::channel::<()>(4);
        let (conn_tx, mut conn_rx) = broadcast::channel::<ServerConnectionEvent>(64);

        let (client_io, server_io) = tokio::io::duplex(4096);
        let peer: SocketAddr = "127.0.0.1:55223".parse().unwrap();

        let handle = tokio::spawn(async move {
            handle_client(
                server_io,
                peer,
                db,
                acf,
                acf_reload_rx,
                5064,
                None,
                None,
                None,
                Some(conn_tx),
                None,
                #[cfg(feature = "cap-tokens")]
                None,
                #[cfg(feature = "cap-tokens")]
                None,
            )
            .await
        });

        let mut client = client_io;
        client.write_all(&version_frame()).await.expect("version");
        client
            .write_all(&create_chan_frame(0xCC, "teardown:test:pv2"))
            .await
            .expect("create_chan");
        client.flush().await.expect("flush create_chan");
        let sid = read_create_chan_sid(&mut client, Duration::from_secs(3)).await;
        client
            .write_all(&event_add_frame(sid, 0xDD))
            .await
            .expect("event_add");
        client.flush().await.expect("flush event_add");

        let opened = await_event(&mut conn_rx, Duration::from_secs(3), |ev| {
            matches!(ev, ServerConnectionEvent::SubscriptionOpened { .. })
        })
        .await;
        assert!(opened.is_some(), "subscription must open");

        // Graceful close: drop the write half → server reads EOF →
        // `break 'client_loop Ok(())`.
        drop(client);

        let closed = await_event(&mut conn_rx, Duration::from_secs(3), |ev| {
            matches!(ev, ServerConnectionEvent::SubscriptionClosed { .. })
        })
        .await;
        assert!(
            matches!(closed, Some(ServerConnectionEvent::SubscriptionClosed { sub_id, .. }) if sub_id == 0xDD),
            "SubscriptionClosed must fire on graceful EOF too (got {closed:?})"
        );

        let res = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("handle_client completes")
            .expect("join ok");
        assert!(res.is_ok(), "graceful EOF must be Ok, got {res:?}");
    }
}

#[cfg(test)]
mod write_gate_order_tests {
    //! Both the deprecated `CA_PROTO_WRITE` (cmd 4) and
    //! `CA_PROTO_WRITE_NOTIFY` (cmd 19) run their DBR-type gate BEFORE
    //! their write-access gate. What differs is what the peer is told when
    //! the type fails, and that is observable when BOTH gates would fail:
    //!
    //! * `write_action` (rsrv/camessage.c:753-756): TYPE first, and a bad
    //!   type is `log_header` + RSRV_ERROR with no frame at all — the
    //!   access gate at `camessage.c:772-775` is never reached, so the peer
    //!   sees silence and a dropped connection.
    //! * `write_notify_action` (rsrv/camessage.c:1678-1682): TYPE first
    //!   too, but it answers `putNotifyErrorReply(ECA_BADTYPE)` before
    //!   RSRV_ERROR, so the peer sees the status and then the drop.
    //!
    //! Neither opcode answers ECA_NOWTACCESS in that case. These two tests
    //! pin each opcode's outcome.
    use super::non_graceful_disconnect_teardown_tests::{
        create_chan_frame, read_create_chan_sid, version_frame,
    };
    use super::*;
    use epics_base_rs::server::access_security::parse_acf;
    use epics_base_rs::server::database::PvDatabase;
    use epics_base_rs::types::EpicsValue;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// An unsupported DBR type code: far above `LAST_BUFFER_TYPE`, and
    /// neither `DBR_PUT_ACKT` (35) nor `DBR_PUT_ACKS` (36) — so it falls
    /// through to the regular `DbFieldType::from_u16` gate and fails it.
    const BAD_DBR_TYPE: u16 = 9999;

    /// Build a write frame (`cmmd` = `CA_PROTO_WRITE` or
    /// `CA_PROTO_WRITE_NOTIFY`) addressed to `sid`, carrying `data_type`
    /// and an 8-byte (one DBR_DOUBLE) payload. `count == 1`.
    fn write_frame(cmmd: u16, sid: u32, ioid: u32, data_type: u16) -> Vec<u8> {
        let mut h = CaHeader::new(cmmd);
        h.data_type = data_type;
        h.count = 1;
        h.cid = sid;
        h.available = ioid;
        h.set_payload_size(8, 1, crate::protocol::CA_MINOR_VERSION)
            .expect("modern peer accepts the extended header");
        let mut frame = h.to_bytes().to_vec();
        frame.extend_from_slice(&0f64.to_be_bytes());
        frame
    }

    /// Read frames from `client` until one whose command is `want_cmmd`
    /// is seen; return it. Times out (panics) otherwise.
    async fn read_frame_of_cmmd<R: tokio::io::AsyncRead + Unpin>(
        client: &mut R,
        want_cmmd: u16,
        timeout: Duration,
    ) -> CaHeader {
        let mut acc: Vec<u8> = Vec::new();
        let mut buf = [0u8; 512];
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for cmmd {want_cmmd}"
            );
            let n = tokio::time::timeout(remaining, client.read(&mut buf))
                .await
                .expect("read within timeout")
                .expect("read ok");
            assert!(n > 0, "server closed before cmmd {want_cmmd}");
            acc.extend_from_slice(&buf[..n]);
            let mut offset = 0;
            while offset + CaHeader::SIZE <= acc.len() {
                let (hdr, hdr_size) =
                    CaHeader::from_bytes_extended(&acc[offset..]).expect("response header parses");
                let msg_len = hdr_size + hdr.actual_postsize();
                if offset + msg_len > acc.len() {
                    break;
                }
                if hdr.cmmd == want_cmmd {
                    return hdr;
                }
                offset += msg_len;
            }
        }
    }

    /// Spawn `handle_client` over a duplex pair with a read-only ACF
    /// (`RULE(0, READ)` — unconditional, write denied to every peer) and
    /// a single double SimplePv `caput:ro`. Returns the client half, the
    /// join handle, and the ACF-reload sender — the caller MUST keep the
    /// sender alive for the test's duration: the read loop polls
    /// `acf_reload_rx.recv()` biased-first, and a dropped sender resolves
    /// to `Closed`, which makes `handle_client` exit `Ok(())` before it
    /// ever reads a client frame.
    async fn spawn_read_only_server(
        peer_port: u16,
    ) -> (
        tokio::io::DuplexStream,
        tokio::task::JoinHandle<CaResult<()>>,
        broadcast::Sender<()>,
    ) {
        let db = Arc::new(PvDatabase::new());
        db.add_pv("caput:ro", EpicsValue::Double(0.0))
            .await
            .expect("add pv");
        let cfg = parse_acf("ASG(DEFAULT) { RULE(0, READ) }").expect("parse acf");
        let acf = epics_base_rs::server::access_security::new_acf_cell(Some(cfg));
        let (acf_reload_tx, acf_reload_rx) = broadcast::channel::<()>(4);

        let (client_io, server_io) = tokio::io::duplex(4096);
        let peer: SocketAddr = format!("127.0.0.1:{peer_port}").parse().unwrap();
        let handle = tokio::spawn(async move {
            handle_client(
                server_io,
                peer,
                db,
                acf,
                acf_reload_rx,
                5064,
                None,
                None,
                None,
                None,
                None,
                #[cfg(feature = "cap-tokens")]
                None,
                #[cfg(feature = "cap-tokens")]
                None,
            )
            .await
        });
        (client_io, handle, acf_reload_tx)
    }

    /// Drive version + create-chan and return the server-assigned sid.
    async fn handshake(client: &mut tokio::io::DuplexStream) -> u32 {
        client.write_all(&version_frame()).await.expect("version");
        client
            .write_all(&create_chan_frame(0xC1, "caput:ro"))
            .await
            .expect("create_chan");
        client.flush().await.expect("flush create_chan");
        read_create_chan_sid(client, Duration::from_secs(3)).await
    }

    /// Deprecated `CA_PROTO_WRITE`, access-denied AND bad type: post-#934
    /// (epics-base 4128a7c07) C `write_action` checks the TYPE first —
    /// `INVALID_DB_REQ` is `log_header` + RSRV_ERROR with NO frame, so
    /// the access gate (and its ECA_NOWTACCESS reply) is never reached.
    /// Pre-#934 C ran access first and replied ECA_NOWTACCESS + keep,
    /// which the pre-fix code mirrored.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn deprecated_write_bad_type_drops_silently_before_access() {
        let (mut client, mut handle, _acf_reload_tx) = spawn_read_only_server(55330).await;
        let sid = handshake(&mut client).await;

        client
            .write_all(&write_frame(CA_PROTO_WRITE, sid, 0x42, BAD_DBR_TYPE))
            .await
            .expect("write");
        client.flush().await.expect("flush write");

        // RSRV_ERROR with no reply — the connection drops without a frame.
        let res = tokio::time::timeout(Duration::from_secs(2), &mut handle)
            .await
            .expect("handle_client completes after WRITE bad type")
            .expect("join ok");
        assert!(
            res.is_err(),
            "deprecated WRITE bad type must DROP the connection \
             (C write_action INVALID_DB_REQ RSRV_ERROR), got {res:?}"
        );
        let mut rest = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut client, &mut rest)
            .await
            .expect("read to EOF");
        assert!(
            rest.is_empty(),
            "C write_action sends nothing before this RSRV_ERROR, got {} bytes",
            rest.len()
        );

        drop(client);
    }

    /// `CA_PROTO_WRITE_NOTIFY`, same access-denied + bad type: C
    /// `write_notify_action` checks the TYPE FIRST → ECA_BADTYPE +
    /// RSRV_ERROR (drop). The error rides a `CA_PROTO_WRITE_NOTIFY` reply
    /// (`m_cid` = ECA status). This pins the opposite ordering so a future
    /// "unify the two opcodes" refactor cannot silently re-invert either.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn write_notify_bad_type_reports_badtype_and_drops_conn() {
        let (mut client, handle, _acf_reload_tx) = spawn_read_only_server(55331).await;
        let sid = handshake(&mut client).await;

        client
            .write_all(&write_frame(CA_PROTO_WRITE_NOTIFY, sid, 0x43, BAD_DBR_TYPE))
            .await
            .expect("write_notify");
        client.flush().await.expect("flush write_notify");

        let reply =
            read_frame_of_cmmd(&mut client, CA_PROTO_WRITE_NOTIFY, Duration::from_secs(3)).await;
        // send_put_notify_response carries the ECA status in `m_cid`.
        assert_eq!(
            reply.cid, ECA_BADTYPE,
            "WRITE_NOTIFY must report the type error first (ECA_BADTYPE), \
             not access (ECA_NOWTACCESS={ECA_NOWTACCESS}); got {}",
            reply.cid
        );

        // RSRV_ERROR after ECA_BADTYPE — the connection must drop.
        let res = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("handle_client completes after WRITE_NOTIFY bad type")
            .expect("join ok");
        assert!(
            res.is_err(),
            "WRITE_NOTIFY bad type must DROP the connection (C write_notify_action RSRV_ERROR), \
             got {res:?}"
        );

        drop(client);
    }
}

#[cfg(test)]
mod deprecated_read_autosize_tests {
    //! The deprecated synchronous CA_PROTO_READ (cmd 3) does NOT
    //! autosize a zero element count. C `read_action`
    //! (rsrv/camessage.c:626-653) sizes the reply with
    //! `dbr_size_n(type, m_count)`, writes the header count as `m_count`,
    //! and calls `dbChannel_get(.., m_count, ..)` — all verbatim. Only
    //! `read_reply` (READ_NOTIFY / EVENT_ADD, camessage.c:509-514) treats
    //! `m_count == 0` as "all available elements". So a deprecated READ
    //! with count==0 against a 3-element waveform must reply count=0 with
    //! a value-less payload, while a READ_NOTIFY with count==0 autosizes
    //! to the full 3-element array.
    use super::non_graceful_disconnect_teardown_tests::{
        create_chan_frame, read_create_chan_sid, version_frame,
    };
    use super::*;
    use epics_base_rs::server::database::PvDatabase;
    use epics_base_rs::types::{DBR_DOUBLE, EpicsValue};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Build a READ / READ_NOTIFY request for `sid` at `data_type` with
    /// the given element `count` (0 means "autosize" only on the NOTIFY
    /// opcode). No payload.
    fn read_request(cmmd: u16, sid: u32, ioid: u32, data_type: u16, count: u32) -> Vec<u8> {
        let mut h = CaHeader::new(cmmd);
        h.data_type = data_type;
        h.cid = sid;
        h.available = ioid;
        h.set_payload_size(0, count, crate::protocol::CA_MINOR_VERSION)
            .expect("modern peer accepts the extended header");
        h.to_bytes_extended().to_vec()
    }

    /// Read frames until one whose command is `want_cmmd` arrives; return
    /// its header. Panics on timeout / EOF.
    async fn read_frame_of_cmmd<R: tokio::io::AsyncRead + Unpin>(
        client: &mut R,
        want_cmmd: u16,
        timeout: Duration,
    ) -> CaHeader {
        let mut acc: Vec<u8> = Vec::new();
        let mut buf = [0u8; 512];
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for cmmd {want_cmmd}"
            );
            let n = tokio::time::timeout(remaining, client.read(&mut buf))
                .await
                .expect("read within timeout")
                .expect("read ok");
            assert!(n > 0, "server closed before cmmd {want_cmmd}");
            acc.extend_from_slice(&buf[..n]);
            let mut offset = 0;
            while offset + CaHeader::SIZE <= acc.len() {
                let (hdr, hdr_size) =
                    CaHeader::from_bytes_extended(&acc[offset..]).expect("response header parses");
                let msg_len = hdr_size + hdr.actual_postsize();
                if offset + msg_len > acc.len() {
                    break;
                }
                if hdr.cmmd == want_cmmd {
                    return hdr;
                }
                offset += msg_len;
            }
        }
    }

    /// Spawn `handle_client` over a duplex pair with a permissive (no
    /// ACF) server holding one waveform SimplePv `rd:arr` of `elems`.
    /// Returns the client half, the join handle, and the ACF-reload
    /// sender — the caller MUST keep the sender alive (a dropped sender
    /// makes the biased-first `acf_reload_rx.recv()` resolve `Closed`,
    /// exiting `handle_client` before it reads a client frame).
    async fn spawn_array_server(
        peer_port: u16,
        elems: Vec<f64>,
    ) -> (
        tokio::io::DuplexStream,
        tokio::task::JoinHandle<CaResult<()>>,
        broadcast::Sender<()>,
    ) {
        let db = Arc::new(PvDatabase::new());
        db.add_pv("rd:arr", EpicsValue::DoubleArray(elems))
            .await
            .expect("add array pv");
        let acf = epics_base_rs::server::access_security::new_acf_cell(None);
        let (acf_reload_tx, acf_reload_rx) = broadcast::channel::<()>(4);

        let (client_io, server_io) = tokio::io::duplex(4096);
        let peer: SocketAddr = format!("127.0.0.1:{peer_port}").parse().unwrap();
        let handle = tokio::spawn(async move {
            handle_client(
                server_io,
                peer,
                db,
                acf,
                acf_reload_rx,
                5064,
                None,
                None,
                None,
                None,
                None,
                #[cfg(feature = "cap-tokens")]
                None,
                #[cfg(feature = "cap-tokens")]
                None,
            )
            .await
        });
        (client_io, handle, acf_reload_tx)
    }

    async fn handshake(client: &mut tokio::io::DuplexStream) -> u32 {
        client.write_all(&version_frame()).await.expect("version");
        client
            .write_all(&create_chan_frame(0xD1, "rd:arr"))
            .await
            .expect("create_chan");
        client.flush().await.expect("flush create_chan");
        read_create_chan_sid(client, Duration::from_secs(3)).await
    }

    /// Deprecated CA_PROTO_READ (cmd 3), count==0 against a 3-element
    /// DBR_DOUBLE waveform: C ships count=0 + a value-less (0-byte for a
    /// plain DBR type) payload. Pre-fix Rust autosized to count=3 + 24
    /// payload bytes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn deprecated_read_count0_ships_verbatim_zero() {
        let (mut client, handle, _acf_reload_tx) =
            spawn_array_server(55340, vec![1.0, 2.0, 3.0]).await;
        let sid = handshake(&mut client).await;

        client
            .write_all(&read_request(CA_PROTO_READ, sid, 0x11, DBR_DOUBLE, 0))
            .await
            .expect("read");
        client.flush().await.expect("flush read");

        let resp = read_frame_of_cmmd(&mut client, CA_PROTO_READ, Duration::from_secs(3)).await;
        assert_eq!(
            resp.actual_count(),
            0,
            "deprecated READ must echo m_count==0 verbatim (no autosize); got {}",
            resp.actual_count()
        );
        assert_eq!(
            resp.actual_postsize(),
            0,
            "deprecated READ count==0 of a plain DBR_DOUBLE ships a value-less \
             (dbr_size_n(type,0)==0) payload; got {} bytes",
            resp.actual_postsize()
        );

        drop(client);
        handle.abort();
    }

    /// Contrast: CA_PROTO_READ_NOTIFY (cmd 15), count==0 against the same
    /// waveform DOES autosize to the full 3-element array (C `read_reply`,
    /// camessage.c:507-509). Pins that the fix did not touch the NOTIFY
    /// autosize path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_notify_count0_autosizes_to_full_array() {
        let (mut client, handle, _acf_reload_tx) =
            spawn_array_server(55341, vec![1.0, 2.0, 3.0]).await;
        let sid = handshake(&mut client).await;

        client
            .write_all(&read_request(
                CA_PROTO_READ_NOTIFY,
                sid,
                0x12,
                DBR_DOUBLE,
                0,
            ))
            .await
            .expect("read_notify");
        client.flush().await.expect("flush read_notify");

        let resp =
            read_frame_of_cmmd(&mut client, CA_PROTO_READ_NOTIFY, Duration::from_secs(3)).await;
        assert_eq!(
            resp.actual_count(),
            3,
            "READ_NOTIFY count==0 must autosize to the full native array; got {}",
            resp.actual_count()
        );
        assert_eq!(
            resp.actual_postsize(),
            24,
            "READ_NOTIFY count==0 ships all 3 DBR_DOUBLE elements (3*8=24 bytes); got {}",
            resp.actual_postsize()
        );

        drop(client);
        handle.abort();
    }

    /// Build a CA_PROTO_EVENT_ADD subscribing `sub_id` on `sid` with an
    /// explicit element `count` (the stock `event_add_frame` pins count=1).
    fn event_add_request_count(sid: u32, sub_id: u32, count: u32) -> Vec<u8> {
        let mut h = CaHeader::new(CA_PROTO_EVENT_ADD);
        h.data_type = DBR_DOUBLE;
        h.cid = sid;
        h.available = sub_id;
        h.set_payload_size(16, count, crate::protocol::CA_MINOR_VERSION)
            .expect("modern peer accepts the extended header");
        let mut frame = h.to_bytes_extended().to_vec();
        frame.extend_from_slice(&0f32.to_be_bytes());
        frame.extend_from_slice(&0f32.to_be_bytes());
        frame.extend_from_slice(&0f32.to_be_bytes());
        frame.extend_from_slice(&3u16.to_be_bytes()); // mask: value+alarm
        frame.extend_from_slice(&0u16.to_be_bytes()); // pad
        frame
    }

    /// Build a fire-and-forget CA_PROTO_WRITE of `values` so a test can
    /// seed a waveform's NORD below its NELM before reading it back.
    fn write_doubles_request(sid: u32, values: &[f64]) -> Vec<u8> {
        let mut h = CaHeader::new(CA_PROTO_WRITE);
        h.data_type = DBR_DOUBLE;
        h.cid = sid;
        h.available = 0;
        let mut body = Vec::with_capacity(values.len() * 8);
        for v in values {
            body.extend_from_slice(&v.to_be_bytes());
        }
        h.set_payload_size(
            body.len(),
            values.len() as u32,
            crate::protocol::CA_MINOR_VERSION,
        )
        .expect("modern peer accepts the extended header");
        let mut frame = h.to_bytes_extended().to_vec();
        frame.extend_from_slice(&body);
        frame
    }

    /// Spawn `handle_client` over a duplex pair holding one waveform record
    /// `wf:rec` of buffer capacity `nelm` (NELM). A partial write leaves
    /// NORD < NELM, so this fixture separates the two — the clamp ceiling
    /// must be NELM, never the live NORD.
    async fn spawn_waveform_server(
        peer_port: u16,
        nelm: i32,
    ) -> (
        tokio::io::DuplexStream,
        tokio::task::JoinHandle<CaResult<()>>,
        broadcast::Sender<()>,
    ) {
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;
        let db = Arc::new(PvDatabase::new());
        db.add_record(
            "wf:rec",
            Box::new(WaveformRecord::new(nelm, DbFieldType::Double)),
        )
        .await
        .expect("add waveform record");
        let acf = epics_base_rs::server::access_security::new_acf_cell(None);
        let (acf_reload_tx, acf_reload_rx) = broadcast::channel::<()>(4);
        let (client_io, server_io) = tokio::io::duplex(4096);
        let peer: SocketAddr = format!("127.0.0.1:{peer_port}").parse().unwrap();
        let handle = tokio::spawn(async move {
            handle_client(
                server_io,
                peer,
                db,
                acf,
                acf_reload_rx,
                5064,
                None,
                None,
                None,
                None,
                None,
                #[cfg(feature = "cap-tokens")]
                None,
                #[cfg(feature = "cap-tokens")]
                None,
            )
            .await
        });
        (client_io, handle, acf_reload_tx)
    }

    /// SECURITY — epics-base PR #934 (Item 2) parity. An oversized wire
    /// element count on READ_NOTIFY must clamp to the channel's final
    /// element count, never drive the reply zero-fill
    /// (`size_dbr_reply`) to a `count * element_size`
    /// allocation. Pre-fix an extended-header count of 0xFFFFFFFF on a
    /// DBR_DOUBLE channel sized the reply to ~34 GB and the `Vec` zero-fill
    /// aborted the whole process — a remote, unauthenticated DoS.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_notify_oversized_count_clamps_to_channel_capacity() {
        let (mut client, handle, _acf_reload_tx) =
            spawn_array_server(55342, vec![1.0, 2.0, 3.0]).await;
        let sid = handshake(&mut client).await;

        client
            .write_all(&read_request(
                CA_PROTO_READ_NOTIFY,
                sid,
                0x21,
                DBR_DOUBLE,
                0xFFFF_FFFF,
            ))
            .await
            .expect("read_notify");
        client.flush().await.expect("flush read_notify");

        let resp =
            read_frame_of_cmmd(&mut client, CA_PROTO_READ_NOTIFY, Duration::from_secs(3)).await;
        assert_eq!(
            resp.actual_count(),
            3,
            "0xFFFFFFFF must clamp to the 3-element channel capacity, not the wire count; got {}",
            resp.actual_count()
        );
        assert_eq!(
            resp.actual_postsize(),
            24,
            "clamped reply ships exactly 3 DBR_DOUBLE elements (24 bytes), not a count-sized \
             buffer; got {}",
            resp.actual_postsize()
        );

        drop(client);
        handle.abort();
    }

    /// SECURITY — same PR #934 clamp on the EVENT_ADD path. The count is
    /// clamped BEFORE it is stored on the subscription, so both the initial
    /// snapshot and every steady-state monitor delivery (the producer in
    /// `monitor.rs`, a second copy of the unbounded zero-fill) are bounded.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn event_add_oversized_count_clamps_to_channel_capacity() {
        let (mut client, handle, _acf_reload_tx) =
            spawn_array_server(55343, vec![1.0, 2.0, 3.0]).await;
        let sid = handshake(&mut client).await;

        client
            .write_all(&event_add_request_count(sid, 0x31, 0xFFFF_FFFF))
            .await
            .expect("event_add");
        client.flush().await.expect("flush event_add");

        let resp =
            read_frame_of_cmmd(&mut client, CA_PROTO_EVENT_ADD, Duration::from_secs(3)).await;
        assert_eq!(
            resp.actual_count(),
            3,
            "EVENT_ADD 0xFFFFFFFF must clamp to the 3-element capacity; got {}",
            resp.actual_count()
        );
        assert_eq!(
            resp.actual_postsize(),
            24,
            "clamped monitor frame ships 3 DBR_DOUBLE elements (24 bytes); got {}",
            resp.actual_postsize()
        );

        drop(client);
        handle.abort();
    }

    /// SECURITY + parity boundary — the clamp ceiling is the channel's
    /// final element count (NELM, buffer capacity), NOT the live value
    /// length (NORD). A dynamic waveform (NELM=8) written to only 3
    /// elements (NORD=3) must still frame a `caget -# 8` at 8 padded
    /// elements — a NORD ceiling would wrongly truncate it to 3 — while an
    /// oversized 0xFFFFFFFF request clamps to 8, not 3.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_notify_count_clamps_to_waveform_nelm_not_nord() {
        let (mut client, handle, _acf_reload_tx) = spawn_waveform_server(55344, 8).await;
        client.write_all(&version_frame()).await.expect("version");
        client
            .write_all(&create_chan_frame(0xE1, "wf:rec"))
            .await
            .expect("create_chan");
        client.flush().await.expect("flush create_chan");
        let sid = read_create_chan_sid(&mut client, Duration::from_secs(3)).await;

        // Seed NORD=3 (< NELM=8) with a fire-and-forget WRITE.
        client
            .write_all(&write_doubles_request(sid, &[1.0, 2.0, 3.0]))
            .await
            .expect("write");
        client.flush().await.expect("flush write");

        // A request AT the NELM ceiling (8 > NORD 3) pads up to 8 — proving
        // the ceiling is NELM, not NORD (which would truncate to 3).
        client
            .write_all(&read_request(
                CA_PROTO_READ_NOTIFY,
                sid,
                0x41,
                DBR_DOUBLE,
                8,
            ))
            .await
            .expect("read at nelm");
        client.flush().await.expect("flush read at nelm");
        let at_nelm =
            read_frame_of_cmmd(&mut client, CA_PROTO_READ_NOTIFY, Duration::from_secs(3)).await;
        assert_eq!(
            at_nelm.actual_count(),
            8,
            "count=8 (NELM) must pad up from NORD=3, not clamp to 3; got {}",
            at_nelm.actual_count()
        );
        assert_eq!(
            at_nelm.actual_postsize(),
            64,
            "8 DBR_DOUBLE elements = 64 bytes; got {}",
            at_nelm.actual_postsize()
        );

        // An oversized request clamps to NELM (8), never NORD (3).
        client
            .write_all(&read_request(
                CA_PROTO_READ_NOTIFY,
                sid,
                0x42,
                DBR_DOUBLE,
                0xFFFF_FFFF,
            ))
            .await
            .expect("read oversized");
        client.flush().await.expect("flush read oversized");
        let oversized =
            read_frame_of_cmmd(&mut client, CA_PROTO_READ_NOTIFY, Duration::from_secs(3)).await;
        assert_eq!(
            oversized.actual_count(),
            8,
            "0xFFFFFFFF must clamp to NELM=8, not NORD=3; got {}",
            oversized.actual_count()
        );
        assert_eq!(
            oversized.actual_postsize(),
            64,
            "clamped to 8 DBR_DOUBLE elements = 64 bytes; got {}",
            oversized.actual_postsize()
        );

        drop(client);
        handle.abort();
    }
}

#[cfg(test)]
mod single_write_all_framing_tests {
    //! BUG 4: GET/READ_NOTIFY, introspection (`send_monitor_snapshot`)
    //! and CA_PROTO_ERROR (`send_ca_error`) replies must reach the wire as
    //! ONE contiguous frame. A split across two writes lets a `send_timeout`
    //! cancel land between header and payload, leaving an orphan header that
    //! mis-frames every following message. A true cancel-race is
    //! non-deterministic; this asserts the structural property that makes
    //! the race impossible: exactly one pushed frame per reply. The outbox
    //! makes this stronger than the old shared-`BufWriter` invariant — each
    //! emit site builds one `Vec<u8>` and `push`es it as an atomic unit, so
    //! "one push == one frame" holds by construction, not by discipline.
    use super::*;
    use crate::server::outbox::{Outbox, OutboxDrain};

    /// Build a live outbox + its drain for a test emit site.
    pub(super) fn live_outbox() -> (Outbox, OutboxDrain) {
        crate::server::outbox::channel()
    }

    /// Drain every frame pushed to the outbox, in push order. Each push is
    /// one complete contiguous frame, so `frames.len()` is exactly the
    /// former `write_all` count this module asserts on.
    pub(super) fn drain_frames(drain: &mut OutboxDrain) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        while let Some(f) = drain.try_next() {
            frames.push(f.to_vec());
        }
        frames
    }

    /// `send_ca_error` builds response-header + echoed-request-header +
    /// diagnostic string. All three must leave in a single `write_all`.
    #[epics_macros_rs::epics_test]
    async fn send_ca_error_writes_single_frame() {
        let (outbox, mut drain) = live_outbox();
        let original = CaHeader::new(CA_PROTO_READ_NOTIFY);

        send_ca_error(
            &outbox,
            &original,
            ECA_INTERNAL,
            0xFFFF_FFFF,
            "CAS: Missaligned protocol rejected",
            crate::protocol::CA_MINOR_VERSION,
        )
        .expect("send_ca_error succeeds");

        let batches = drain_frames(&mut drain);
        assert_eq!(
            batches.len(),
            1,
            "send_ca_error must issue exactly one write_all (got {} batches: {:?})",
            batches.len(),
            batches.iter().map(|b| b.len()).collect::<Vec<_>>(),
        );
        // The one batch must be the complete frame: response header +
        // 16-byte echoed request header + padded diagnostic string.
        let frame = &batches[0];
        let payload_size = u16::from_be_bytes([frame[2], frame[3]]) as usize;
        assert_eq!(
            16 + payload_size,
            frame.len(),
            "CA_PROTO_ERROR header-declared size must match the contiguous frame",
        );
    }

    /// READ_NOTIFY get-failure / no-snapshot wire shape: C `read_reply`
    /// `status < 0` branch (`rsrv/camessage.c:548-562`) keeps the
    /// CA_PROTO_READ_NOTIFY reply at the requested count, abuses `m_cid`
    /// to carry the ECA status, and commits a `dbr_size_n(type, count)`
    /// ZEROED body. The pre-fix `send_cmd_error` shipped `count=0` + an
    /// empty body; this locks the corrected `send_no_read_access_event`
    /// shape the get-failure and no-snapshot paths now use.
    #[epics_macros_rs::epics_test]
    async fn read_notify_get_failure_frame_keeps_count_and_zero_body() {
        let (outbox, mut drain) = live_outbox();
        let requested_count = 3u32;
        // DBR_TIME_DOUBLE = compound type with 16-byte metadata, so the
        // get-failure body is non-empty even though every byte is zero —
        // exactly where `count=0`/empty diverged from C.
        send_no_read_access_event(
            &outbox,
            CA_PROTO_READ_NOTIFY,
            epics_base_rs::types::DBR_TIME_DOUBLE,
            requested_count,
            0x4242_4242, // ioid echoed into m_available
            ECA_GETFAIL,
            ReplyContext {
                req_hdr: CaHeader::new(CA_PROTO_READ_NOTIFY),
                client_minor: crate::protocol::CA_MINOR_VERSION,
            },
        )
        .expect("send_no_read_access_event succeeds");

        let batches = drain_frames(&mut drain);
        assert_eq!(batches.len(), 1, "one contiguous write_all");
        let frame = &batches[0];
        let hdr = CaHeader::from_bytes(&frame[..16]).expect("parse READ_NOTIFY header");
        assert_eq!(hdr.cmmd, CA_PROTO_READ_NOTIFY, "stays a READ_NOTIFY reply");
        assert_eq!(hdr.data_type, epics_base_rs::types::DBR_TIME_DOUBLE);
        assert_eq!(
            hdr.actual_count(),
            requested_count,
            "C preserves the requested count, not 0"
        );
        assert_eq!(hdr.cid, ECA_GETFAIL, "m_cid abused to carry the ECA status");
        assert_eq!(hdr.available, 0x4242_4242);

        // body = dbr_size_n(type, count), 8-aligned, all zero.
        let native =
            epics_base_rs::types::native_type_for_dbr(epics_base_rs::types::DBR_TIME_DOUBLE)
                .expect("native type");
        let body_size = epics_base_rs::types::dbr_buffer_size(
            epics_base_rs::types::DBR_TIME_DOUBLE,
            native,
            requested_count as usize,
        );
        let padded = align8(body_size);
        assert!(
            padded > 0,
            "compound DBR get-failure body must be non-empty"
        );
        assert_eq!(
            hdr.actual_postsize(),
            padded,
            "m_postsize is dbr_size_n(type, count), not 0"
        );
        assert_eq!(
            frame.len(),
            16 + padded,
            "single frame = header + zero body"
        );
        assert!(
            frame[16..].iter().all(|&b| b == 0),
            "the get-failure body is entirely zero-filled"
        );
    }

    /// Regression: when the original request used an extended
    /// 24-byte header, the outer CA_PROTO_ERROR reply must declare
    /// `m_postsize = 24 + diag_len`, not `16 + diag_len`.
    #[epics_macros_rs::epics_test]
    async fn send_ca_error_extended_original_declares_correct_payload_size() {
        let (outbox, mut drain) = live_outbox();
        // Build an extended original header: set_payload_size triggers
        // extended form when count >= 0xFFFF.
        let mut original = CaHeader::new(CA_PROTO_READ_NOTIFY);
        original
            .set_payload_size(0, 0x1_0000, crate::protocol::CA_MINOR_VERSION)
            .expect("V49 peer accepts the extended header"); // count >= 0xFFFF → extended (24 bytes)
        assert!(
            original.is_extended(),
            "test requires an extended original header"
        );

        send_ca_error(
            &outbox,
            &original,
            ECA_INTERNAL,
            0xFFFF_FFFF,
            "Regression test",
            crate::protocol::CA_MINOR_VERSION,
        )
        .expect("send_ca_error succeeds");

        let batches = drain_frames(&mut drain);
        assert_eq!(batches.len(), 1, "must issue exactly one write_all");
        let frame = &batches[0];
        // Outer CA_PROTO_ERROR response header is normal form (payload < 0xFFFF);
        // m_postsize lives at bytes [2..4].
        let declared = u16::from_be_bytes([frame[2], frame[3]]) as usize;
        assert_eq!(
            16 + declared,
            frame.len(),
            "declared m_postsize must account for the full payload (echo hdr + diag)"
        );
        // With an extended original, the echoed header contributes 24 bytes.
        // Any diagnostic shorter than 0xFFFF − 24 keeps the outer header normal.
        // Echo header occupies frame bytes [16..40]; those 24 bytes must round-trip
        // the extended marker (postsize field = 0xFFFF at echo-hdr offset 2..4).
        let echo_postsize = u16::from_be_bytes([frame[18], frame[19]]);
        assert_eq!(
            echo_postsize, 0xFFFF,
            "echoed request header must preserve the extended marker (m_postsize = 0xFFFF)"
        );
    }

    /// `send_monitor_snapshot` (the introspection EVENT_ADD reply) must
    /// emit header + padded payload as a single `write_all`.
    #[epics_macros_rs::epics_test]
    async fn send_monitor_snapshot_writes_single_frame() {
        use epics_base_rs::server::snapshot::Snapshot;
        use epics_base_rs::types::{DBR_LONG, EpicsValue};

        let (outbox, mut drain) = live_outbox();
        let snapshot = Snapshot::new(
            EpicsValue::Long(123),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );

        // requested_count 0 = autosize: frame the live element count.
        send_monitor_snapshot(
            &outbox,
            9,
            DBR_LONG,
            0,
            &snapshot,
            ReplyContext {
                req_hdr: CaHeader::new(CA_PROTO_EVENT_ADD),
                client_minor: crate::protocol::CA_MINOR_VERSION,
            },
        )
        .expect("send_monitor_snapshot succeeds");

        let batches = drain_frames(&mut drain);
        assert_eq!(
            batches.len(),
            1,
            "send_monitor_snapshot must issue exactly one write_all (got {} batches: {:?})",
            batches.len(),
            batches.iter().map(|b| b.len()).collect::<Vec<_>>(),
        );
    }

    /// an initial monitor snapshot for an EVENT_ADD whose
    /// request count exceeds the live element count must be framed at
    /// the requested count with a zero-padded payload — the same
    /// shape the READ path and later monitor updates use. Pre-fix the
    /// initial frame was framed at `snapshot.value.count()`, so a
    /// client saw a count/size discontinuity inside one subscription.
    #[epics_macros_rs::epics_test]
    async fn ex_r9_initial_snapshot_pads_over_requested_count() {
        use epics_base_rs::server::snapshot::Snapshot;
        use epics_base_rs::types::{DBR_LONG, DbFieldType, EpicsValue};

        // Live PV holds 3 LONG elements; the client requested 8.
        let snapshot = Snapshot::new(
            EpicsValue::LongArray(vec![10, 20, 30]),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );
        let requested_count = 8u32;

        let (outbox, mut drain) = live_outbox();
        send_monitor_snapshot(
            &outbox,
            9,
            DBR_LONG,
            requested_count,
            &snapshot,
            ReplyContext {
                req_hdr: CaHeader::new(CA_PROTO_EVENT_ADD),
                client_minor: crate::protocol::CA_MINOR_VERSION,
            },
        )
        .expect("send_monitor_snapshot succeeds");

        let batches = drain_frames(&mut drain);
        assert_eq!(batches.len(), 1, "exactly one contiguous frame");
        let frame = &batches[0];

        // Standard 16-byte CA header: count 8 and the resulting
        // payload both fit under the 0xFFFF extended-form threshold.
        let postsize = u16::from_be_bytes([frame[2], frame[3]]) as usize;
        let count = u16::from_be_bytes([frame[6], frame[7]]) as u32;
        assert_eq!(
            count, requested_count,
            "the initial monitor frame must carry the REQUESTED \
             element count (8), not the live count (3)"
        );

        // DBR_LONG is a plain type (no metadata); the payload must
        // hold the requested element count of value bytes, zero-
        // padded for the elements the PV does not have.
        let elem = DbFieldType::Long.element_size();
        let value_bytes = requested_count as usize * elem;
        assert!(
            postsize >= value_bytes,
            "payload ({postsize}) must be padded to at least the \
             requested {requested_count} elements ({value_bytes} bytes)"
        );
        // The three live elements come first, then zero padding.
        let body = &frame[16..16 + postsize];
        assert_eq!(&body[0..4], &10i32.to_be_bytes(), "element 0 preserved");
        assert_eq!(&body[8..12], &30i32.to_be_bytes(), "element 2 preserved");
        assert!(
            body[3 * elem..value_bytes].iter().all(|&b| b == 0),
            "over-requested elements must be zero-filled"
        );
    }

    /// a request count SMALLER than the live element count
    /// still truncates — `send_monitor_snapshot` must own both
    /// directions of the count contract.
    #[epics_macros_rs::epics_test]
    async fn ex_r9_initial_snapshot_truncates_under_requested_count() {
        use epics_base_rs::server::snapshot::Snapshot;
        use epics_base_rs::types::{DBR_LONG, EpicsValue};

        let snapshot = Snapshot::new(
            EpicsValue::LongArray(vec![1, 2, 3, 4, 5]),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );
        let (outbox, mut drain) = live_outbox();
        send_monitor_snapshot(
            &outbox,
            9,
            DBR_LONG,
            2,
            &snapshot,
            ReplyContext {
                req_hdr: CaHeader::new(CA_PROTO_EVENT_ADD),
                client_minor: crate::protocol::CA_MINOR_VERSION,
            },
        )
        .expect("send_monitor_snapshot succeeds");

        let frames = drain_frames(&mut drain);
        let frame = &frames[0];
        let count = u16::from_be_bytes([frame[6], frame[7]]) as u32;
        assert_eq!(count, 2, "under-requested count must truncate to 2");
    }

    /// `requested_count == 0` is autosize — the frame keeps the
    /// live element count, unchanged behaviour.
    #[epics_macros_rs::epics_test]
    async fn ex_r9_autosize_keeps_live_count() {
        use epics_base_rs::server::snapshot::Snapshot;
        use epics_base_rs::types::{DBR_LONG, EpicsValue};

        let snapshot = Snapshot::new(
            EpicsValue::LongArray(vec![7, 8, 9, 10]),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );
        let (outbox, mut drain) = live_outbox();
        send_monitor_snapshot(
            &outbox,
            9,
            DBR_LONG,
            0,
            &snapshot,
            ReplyContext {
                req_hdr: CaHeader::new(CA_PROTO_EVENT_ADD),
                client_minor: crate::protocol::CA_MINOR_VERSION,
            },
        )
        .expect("send_monitor_snapshot succeeds");

        let frames = drain_frames(&mut drain);
        let frame = &frames[0];
        let count = u16::from_be_bytes([frame[6], frame[7]]) as u32;
        assert_eq!(count, 4, "autosize (count==0) keeps the live count");
    }
}

#[cfg(test)]
mod ex_r6_no_read_access_count_tests {
    //! an autosize (`count == 0`) no-read-access EVENT_ADD
    //! denial must be sized to a nonzero DBR body. A zero-payload
    //! `CA_PROTO_EVENT_ADD` is the historical subscription-cancel
    //! confirmation no-op; the CA client drops it before reading the
    //! `ECA_NORDACCESS` status, so a denied autosize monitor would
    //! silently appear to hang.
    use super::no_read_access_count;
    use epics_base_rs::types::{DbFieldType, dbr_buffer_size};

    /// Autosize (`requested_count == 0`) must normalise to the
    /// target's live element count — mirrors C `read_reply`
    /// substituting `paddr->no_elements` (`camessage.c:509-514`).
    #[test]
    fn ex_r6_autosize_normalises_to_actual_count() {
        assert_eq!(no_read_access_count(0, 7), 7);
        // A scalar (1 element) autosize denial still gets a body.
        assert_eq!(no_read_access_count(0, 1), 1);
        // A target reporting zero live elements is floored at one so
        // the frame is never zero-payload.
        assert_eq!(no_read_access_count(0, 0), 1);
    }

    /// An explicit non-zero request count is framed unchanged — the
    /// caller already asked for a definite shape.
    #[test]
    fn ex_r6_explicit_count_passes_through() {
        assert_eq!(no_read_access_count(3, 7), 3);
        assert_eq!(no_read_access_count(1, 100), 1);
    }

    /// The defect proof: with the pre-fix raw `count == 0`, the
    /// `dbr_buffer_size` of a plain DBR type (`DBR_DOUBLE`) is zero,
    /// producing the cancel-ack-shaped frame. After normalisation the
    /// payload is strictly positive, so the client's status-error
    /// path runs. `DBR_DOUBLE == 6` is a plain (non-STS) type, so its
    /// metadata size is zero — the value bytes are the whole payload.
    #[test]
    fn ex_r6_normalised_count_yields_nonzero_plain_dbr_payload() {
        const DBR_DOUBLE: u16 = 6;
        // Pre-fix shape: raw autosize count 0 → zero-payload frame
        // (indistinguishable from an EVENT_CANCEL ack).
        assert_eq!(
            dbr_buffer_size(DBR_DOUBLE, DbFieldType::Double, 0),
            0,
            "regression baseline: a raw count==0 plain-DBR denial is \
             zero-payload — the cancel-ack shape later normalised"
        );
        // After normalisation the denial frame carries a real
        // DBR body, so the client sees the ECA_NORDACCESS status.
        let normalised = no_read_access_count(0, 4) as usize;
        let payload = dbr_buffer_size(DBR_DOUBLE, DbFieldType::Double, normalised);
        assert!(
            payload > 0,
            "a normalised autosize denial must have a nonzero \
             DBR payload so the client does not drop it as a cancel-ack"
        );
        assert_eq!(payload, 4 * DbFieldType::Double.element_size());
    }
}

#[cfg(test)]
mod bfr7_event_context_filter_tests {
    //! a CA monitor initial single-event post
    //! (`db_post_single_event`, `rsrv/camessage.c:1907`) runs the
    //! channel filter chain in EVENT context (`db_create_event_log` →
    //! `dbfl_context_event`), NOT in one-shot READ context. `dec`/`sync`
    //! therefore decimate/gate the initial event, and a chain that drops
    //! the post sends NO initial `EVENT_ADD` frame (C
    //! `db_queue_event_log` fires only `if(pLog)`) — never an unfiltered
    //! fallback.
    //!
    //! These drive the full `handle_client` EVENT_ADD path; the
    //! deterministic context split (read bypasses `sync`, event gates it)
    //! is also unit-proven in
    //! `epics_base_rs::server::database::filters` tests.
    use super::non_graceful_disconnect_teardown_tests::{
        await_event, create_chan_frame, event_add_frame, read_create_chan_sid, version_frame,
    };
    use super::*;
    use epics_base_rs::server::database::PvDatabase;
    use epics_base_rs::types::EpicsValue;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Read frames from `client` for `window`, returning the
    /// `(cmd, postsize)` of every COMPLETE CA frame observed. Used to
    /// assert presence/absence of an initial `CA_PROTO_EVENT_ADD` data
    /// frame.
    async fn collect_frames<R: tokio::io::AsyncRead + Unpin>(
        client: &mut R,
        window: Duration,
    ) -> Vec<(u16, usize)> {
        let mut acc: Vec<u8> = Vec::new();
        let mut frames: Vec<(u16, usize)> = Vec::new();
        let mut buf = [0u8; 512];
        let deadline = tokio::time::Instant::now() + window;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, client.read(&mut buf)).await {
                Ok(Ok(0)) => break, // EOF
                Ok(Ok(n)) => {
                    acc.extend_from_slice(&buf[..n]);
                    let mut offset = 0;
                    while offset + CaHeader::SIZE <= acc.len() {
                        let Ok((hdr, hdr_size)) = CaHeader::from_bytes_extended(&acc[offset..])
                        else {
                            break;
                        };
                        let msg_len = hdr_size + hdr.actual_postsize();
                        if offset + msg_len > acc.len() {
                            break;
                        }
                        frames.push((hdr.cmmd, hdr.actual_postsize()));
                        offset += msg_len;
                    }
                    acc.drain(0..offset);
                }
                Ok(Err(_)) | Err(_) => break,
            }
        }
        frames
    }

    /// Spawn `handle_client` over a duplex socket; returns the client
    /// half, the join handle, the connection-event receiver, and the
    /// ACF-reload sender. The caller MUST keep the sender alive for the
    /// connection's lifetime — dropping it closes the ACF-reload
    /// broadcast, which `handle_client` treats as a shutdown signal.
    fn spawn_server(
        db: Arc<PvDatabase>,
        port: u16,
    ) -> (
        tokio::io::DuplexStream,
        tokio::task::JoinHandle<CaResult<()>>,
        broadcast::Receiver<ServerConnectionEvent>,
        broadcast::Sender<()>,
    ) {
        let acf = epics_base_rs::server::access_security::new_acf_cell(None);
        let (acf_reload_tx, acf_reload_rx) = broadcast::channel::<()>(4);
        let (conn_tx, conn_rx) = broadcast::channel::<ServerConnectionEvent>(64);
        let (client_io, server_io) = tokio::io::duplex(4096);
        let peer: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let handle = tokio::spawn(async move {
            handle_client(
                server_io,
                peer,
                db,
                acf,
                acf_reload_rx,
                5064,
                None,
                None,
                None,
                Some(conn_tx),
                None,
                #[cfg(feature = "cap-tokens")]
                None,
                #[cfg(feature = "cap-tokens")]
                None,
            )
            .await
        });
        (client_io, handle, conn_rx, acf_reload_tx)
    }

    /// Open a subscription on `pv_name` and return every frame the
    /// server emits in the `window` after the subscription opens.
    /// Asserts the subscription actually opened so a missing initial
    /// frame is never a vacuous pass.
    async fn subscribe_and_collect(pv_name: &str, port: u16) -> Vec<(u16, usize)> {
        subscribe_then_put(pv_name, port, &[]).await
    }

    /// [`subscribe_and_collect`] plus values written to the PV once the
    /// subscription is open, so a test can observe what the filter chain
    /// does to the monitor stream and not only to the initial post.
    async fn subscribe_then_put(pv_name: &str, port: u16, then_put: &[f64]) -> Vec<(u16, usize)> {
        let db = Arc::new(PvDatabase::new());
        db.add_pv("bfr7:pv", EpicsValue::Double(42.0))
            .await
            .expect("add pv");
        let put_db = Arc::clone(&db);
        let (mut client, handle, mut conn_rx, _acf_reload_tx) = spawn_server(db, port);

        client.write_all(&version_frame()).await.expect("version");
        client
            .write_all(&create_chan_frame(0xA1, pv_name))
            .await
            .expect("create_chan");
        client.flush().await.expect("flush create_chan");
        let sid = read_create_chan_sid(&mut client, Duration::from_secs(3)).await;

        client
            .write_all(&event_add_frame(sid, 0xB1))
            .await
            .expect("event_add");
        client.flush().await.expect("flush event_add");

        let opened = await_event(&mut conn_rx, Duration::from_secs(3), |ev| {
            matches!(ev, ServerConnectionEvent::SubscriptionOpened { .. })
        })
        .await;
        assert!(
            matches!(opened, Some(ServerConnectionEvent::SubscriptionOpened { sub_id, .. }) if sub_id == 0xB1),
            "subscription must open (else the no-initial-frame assertion is vacuous): got {opened:?}"
        );

        for v in then_put {
            put_db
                .put_pv_and_post("bfr7:pv", EpicsValue::Double(*v))
                .await
                .expect("put bfr7:pv");
        }

        let frames = collect_frames(&mut client, Duration::from_millis(700)).await;
        drop(client);
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
        frames
    }

    /// Regression: a `sync` filter gating `while` a never-set
    /// state drops the initial monitor post in EVENT context, so the
    /// server sends NO `CA_PROTO_EVENT_ADD` data frame. Pre-fix the
    /// initial post ran in READ context (`apply_to_read_value`), where
    /// `sync` is bypassed, so an initial frame WAS sent — this test
    /// fails against that behaviour.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn event_context_sync_gate_suppresses_initial_event() {
        // `sync.c`'s `parse_ok` resolves the state with `dbStateFind`, so
        // the state has to be declared for the channel to open at all; it
        // is simply never set.
        epics_base_rs::server::database::filters::db_state_registry()
            .get_or_create("BFR7:NEVERSET");
        let pv = r#"bfr7:pv.{"sync":{"while":"BFR7:NEVERSET"}}"#;
        let frames = subscribe_and_collect(pv, 55301).await;
        let event_adds: Vec<_> = frames
            .iter()
            .filter(|(cmd, _)| *cmd == CA_PROTO_EVENT_ADD)
            .collect();
        assert!(
            event_adds.is_empty(),
            "the event-context `sync` gate must suppress the \
             initial EVENT_ADD post — no fallback to the unfiltered \
             value (got {event_adds:?})"
        );
    }

    /// Control: an unfiltered channel still sends exactly one initial
    /// `CA_PROTO_EVENT_ADD` data frame — proving the harness/timing is
    /// sound and the suppression above is the filter's doing, not a
    /// dropped read.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn plain_channel_sends_initial_event() {
        let frames = subscribe_and_collect("bfr7:pv", 55302).await;
        let event_adds = frames
            .iter()
            .filter(|(cmd, _)| *cmd == CA_PROTO_EVENT_ADD)
            .count();
        assert!(
            event_adds >= 1,
            "control: an unfiltered channel must send an initial \
             EVENT_ADD frame (got frames {frames:?})"
        );
    }

    /// The monitor stream runs through the decimator: C forwards window
    /// slot 0 and drops the rest (`decimate.c:63`, `if (i++ == 0)`), so
    /// with `n=2` the first of two value changes is sent and the second is
    /// not. The initial post carries its own chain (see
    /// `ChannelEntry::filter_chain`) and takes slot 0 of that one, so three
    /// posts reach the client as two frames.
    ///
    /// `offset` is deliberately absent: `decimate.c`'s opts table defines
    /// `n` alone, so the port's old `offset` extension — which is what let
    /// an earlier version of this test suppress the initial post — no
    /// longer parses.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn event_context_decimator_drops_the_second_update() {
        let pv = r#"bfr7:pv.{"dec":{"n":2}}"#;
        let frames = subscribe_then_put(pv, 55303, &[43.0, 44.0]).await;
        let event_adds: Vec<_> = frames
            .iter()
            .filter(|(cmd, _)| *cmd == CA_PROTO_EVENT_ADD)
            .collect();
        assert_eq!(
            event_adds.len(),
            2,
            "initial post plus the first update; the second update is \
             decimated away: got {frames:?}"
        );
    }
}

#[cfg(test)]
mod r46_zero_mask_event_add_tests {
    //! EVENT_ADD with mask=0 must be rejected with CA_PROTO_ERROR
    //! (ECA_ALLOCMEM) + connection close.
    //!
    //! C reference: `db_add_event` (dbEvent.c:437-439) returns NULL when
    //! `select==0`; `event_add_action` (camessage.c:1814-1822) then calls
    //! `send_err(ECA_ALLOCMEM)` and returns `RSRV_ERROR`, closing the
    //! connection.  Previously the Rust server silently installed a dead
    //! subscription whose `Subscriber::accepts` always returned false, so
    //! no events ever arrived after the initial snapshot.
    use super::non_graceful_disconnect_teardown_tests::{
        create_chan_frame, read_create_chan_sid, version_frame,
    };
    use super::*;
    use epics_base_rs::server::database::PvDatabase;
    use epics_base_rs::types::EpicsValue;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Build a CA_PROTO_EVENT_ADD request with mask=0 (the defect input).
    fn event_add_zero_mask_frame(sid: u32, sub_id: u32) -> Vec<u8> {
        let mut h = CaHeader::new(CA_PROTO_EVENT_ADD);
        h.data_type = epics_base_rs::types::DBR_TIME_DOUBLE;
        h.count = 1;
        h.cid = sid;
        h.available = sub_id;
        h.set_payload_size(16, 1, crate::protocol::CA_MINOR_VERSION)
            .expect("modern peer accepts the extended header");
        let mut frame = h.to_bytes().to_vec();
        frame.extend_from_slice(&0f32.to_be_bytes());
        frame.extend_from_slice(&0f32.to_be_bytes());
        frame.extend_from_slice(&0f32.to_be_bytes());
        frame.extend_from_slice(&0u16.to_be_bytes()); // mask = 0 — the defect input
        frame.extend_from_slice(&0u16.to_be_bytes()); // pad
        frame
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn zero_mask_event_add_replies_eca_allocmem_and_disconnects() {
        let db = Arc::new(PvDatabase::new());
        db.add_pv("r46:pv", EpicsValue::Double(0.0))
            .await
            .expect("add pv");
        let acf = epics_base_rs::server::access_security::new_acf_cell(None);
        let (_acf_reload_tx, acf_reload_rx) = broadcast::channel::<()>(4);
        let (conn_tx, _conn_rx) = broadcast::channel::<ServerConnectionEvent>(64);

        let (client_io, server_io) = tokio::io::duplex(4096);
        let peer: SocketAddr = "127.0.0.1:55400".parse().unwrap();

        let handle = tokio::spawn(async move {
            handle_client(
                server_io,
                peer,
                db,
                acf,
                acf_reload_rx,
                5064,
                None,
                None,
                None,
                Some(conn_tx),
                None,
                #[cfg(feature = "cap-tokens")]
                None,
                #[cfg(feature = "cap-tokens")]
                None,
            )
            .await
        });

        let mut client = client_io;
        client.write_all(&version_frame()).await.expect("version");
        client
            .write_all(&create_chan_frame(1, "r46:pv"))
            .await
            .expect("create_chan");
        client.flush().await.expect("flush create_chan");
        let sid = read_create_chan_sid(&mut client, Duration::from_secs(3)).await;

        // Send the defect input: EVENT_ADD with mask=0.
        client
            .write_all(&event_add_zero_mask_frame(sid, 0xC0DE))
            .await
            .expect("zero-mask event_add");
        client.flush().await.expect("flush zero-mask event_add");

        // Read server output until EOF; expect at least one CA_PROTO_ERROR
        // frame before the connection closes.
        let mut acc: Vec<u8> = Vec::new();
        let mut buf = [0u8; 256];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for server to close"
            );
            match tokio::time::timeout(remaining, client.read(&mut buf)).await {
                Ok(Ok(0)) => break, // EOF — server closed
                Ok(Ok(n)) => acc.extend_from_slice(&buf[..n]),
                Ok(Err(_)) | Err(_) => break,
            }
        }

        // Scan the accumulated bytes for a CA_PROTO_ERROR frame.
        let mut got_error = false;
        let mut offset = 0;
        while offset + CaHeader::SIZE <= acc.len() {
            if let Ok((hdr, hdr_size)) = CaHeader::from_bytes_extended(&acc[offset..]) {
                if hdr.cmmd == CA_PROTO_ERROR {
                    got_error = true;
                    break;
                }
                let msg_len = hdr_size + hdr.actual_postsize();
                if msg_len == 0 {
                    break;
                }
                offset += msg_len;
            } else {
                break;
            }
        }
        assert!(
            got_error,
            "zero-mask EVENT_ADD must produce a CA_PROTO_ERROR (ECA_ALLOCMEM) \
             reply before the connection closes (received {} bytes: {acc:?})",
            acc.len()
        );

        // The handler must exit with Err (RSRV_ERROR path, not graceful EOF).
        let res = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("handle_client completes after zero-mask rejection")
            .expect("join ok");
        assert!(
            res.is_err(),
            "zero-mask EVENT_ADD must close the connection with Err \
             (matches C RSRV_ERROR), got {res:?}"
        );
    }

    /// Build a CA_PROTO_EVENT_ADD request with an arbitrary `mask`.
    fn event_add_mask_frame(sid: u32, sub_id: u32, mask: u16) -> Vec<u8> {
        let mut h = CaHeader::new(CA_PROTO_EVENT_ADD);
        h.data_type = epics_base_rs::types::DBR_TIME_DOUBLE;
        h.count = 1;
        h.cid = sid;
        h.available = sub_id;
        h.set_payload_size(16, 1, crate::protocol::CA_MINOR_VERSION)
            .expect("modern peer accepts the extended header");
        let mut frame = h.to_bytes().to_vec();
        frame.extend_from_slice(&0f32.to_be_bytes());
        frame.extend_from_slice(&0f32.to_be_bytes());
        frame.extend_from_slice(&0f32.to_be_bytes());
        frame.extend_from_slice(&mask.to_be_bytes());
        frame.extend_from_slice(&0u16.to_be_bytes()); // pad
        frame
    }

    /// Regression: a mask above UCHAR_MAX (256, reachable because the CA
    /// wire mask is a `u16`) must be rejected exactly like mask==0. C
    /// `db_add_event` (dbEvent.c:437) returns NULL for
    /// `select == 0 || select > UCHAR_MAX`, which `event_add_action` turns
    /// into ECA_ALLOCMEM + RSRV_ERROR (camessage.c:1866-1877). The previous
    /// guard checked only mask==0, so a mask in 256..=65535 silently
    /// installed a never-firing subscription.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn over_max_mask_event_add_replies_eca_allocmem_and_disconnects() {
        let db = Arc::new(PvDatabase::new());
        db.add_pv("r46:pv", EpicsValue::Double(0.0))
            .await
            .expect("add pv");
        let acf = epics_base_rs::server::access_security::new_acf_cell(None);
        let (_acf_reload_tx, acf_reload_rx) = broadcast::channel::<()>(4);
        let (conn_tx, _conn_rx) = broadcast::channel::<ServerConnectionEvent>(64);

        let (client_io, server_io) = tokio::io::duplex(4096);
        let peer: SocketAddr = "127.0.0.1:55402".parse().unwrap();

        let handle = tokio::spawn(async move {
            handle_client(
                server_io,
                peer,
                db,
                acf,
                acf_reload_rx,
                5064,
                None,
                None,
                None,
                Some(conn_tx),
                None,
                #[cfg(feature = "cap-tokens")]
                None,
                #[cfg(feature = "cap-tokens")]
                None,
            )
            .await
        });

        let mut client = client_io;
        client.write_all(&version_frame()).await.expect("version");
        client
            .write_all(&create_chan_frame(1, "r46:pv"))
            .await
            .expect("create_chan");
        client.flush().await.expect("flush create_chan");
        let sid = read_create_chan_sid(&mut client, Duration::from_secs(3)).await;

        // mask = 256 — one past UCHAR_MAX.
        client
            .write_all(&event_add_mask_frame(sid, 0xC0DE, 256))
            .await
            .expect("over-max event_add");
        client.flush().await.expect("flush over-max event_add");

        let mut acc: Vec<u8> = Vec::new();
        let mut buf = [0u8; 256];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for server to close"
            );
            match tokio::time::timeout(remaining, client.read(&mut buf)).await {
                Ok(Ok(0)) => break, // EOF — server closed
                Ok(Ok(n)) => acc.extend_from_slice(&buf[..n]),
                Ok(Err(_)) | Err(_) => break,
            }
        }

        let mut got_error = false;
        let mut offset = 0;
        while offset + CaHeader::SIZE <= acc.len() {
            if let Ok((hdr, hdr_size)) = CaHeader::from_bytes_extended(&acc[offset..]) {
                if hdr.cmmd == CA_PROTO_ERROR {
                    got_error = true;
                    break;
                }
                let msg_len = hdr_size + hdr.actual_postsize();
                if msg_len == 0 {
                    break;
                }
                offset += msg_len;
            } else {
                break;
            }
        }
        assert!(
            got_error,
            "mask>UCHAR_MAX EVENT_ADD must produce a CA_PROTO_ERROR (ECA_ALLOCMEM) \
             reply before the connection closes (received {} bytes: {acc:?})",
            acc.len()
        );

        let res = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("handle_client completes after over-max-mask rejection")
            .expect("join ok");
        assert!(
            res.is_err(),
            "mask>UCHAR_MAX EVENT_ADD must close the connection with Err \
             (matches C RSRV_ERROR), got {res:?}"
        );
    }

    /// Guard-ordering regression: an EVENT_ADD carrying an unknown/stale
    /// SID *and* mask==0 must emit the bad-SID `ECA_INTERNAL` "Bad
    /// Resource ID" frame, NOT the spurious `ECA_ALLOCMEM` mask frame. In
    /// C `event_add_action` the missing-channel branch (`if (!pciu) {
    /// logBadId; return RSRV_ERROR; }`, camessage.c:1823-1827) runs
    /// *before* the `db_add_event` NULL (select==0) ALLOCMEM path
    /// (camessage.c:1866-1877). `logBadId` (camessage.c:312-325) sends
    /// `send_err(ECA_INTERNAL, "Bad Resource ID")` with the cid=0xFFFFFFFF
    /// sentinel (MPTOPCIU→NULL), flushed by camsgtask.c:142 before the
    /// disconnect — so an unknown SID draws ECA_INTERNAL regardless of
    /// mask. The defective guard ran the mask==0 check before the channel
    /// lookup and replied `CA_PROTO_ERROR(ECA_ALLOCMEM, m_cid=0xFFFF_FFFF)`
    /// here.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_sid_zero_mask_event_add_sends_bad_resource_id() {
        let db = Arc::new(PvDatabase::new());
        db.add_pv("r46:pv", EpicsValue::Double(0.0))
            .await
            .expect("add pv");
        let acf = epics_base_rs::server::access_security::new_acf_cell(None);
        let (_acf_reload_tx, acf_reload_rx) = broadcast::channel::<()>(4);
        let (conn_tx, _conn_rx) = broadcast::channel::<ServerConnectionEvent>(64);

        let (client_io, server_io) = tokio::io::duplex(4096);
        let peer: SocketAddr = "127.0.0.1:55401".parse().unwrap();

        let handle = tokio::spawn(async move {
            handle_client(
                server_io,
                peer,
                db,
                acf,
                acf_reload_rx,
                5064,
                None,
                None,
                None,
                Some(conn_tx),
                None,
                #[cfg(feature = "cap-tokens")]
                None,
                #[cfg(feature = "cap-tokens")]
                None,
            )
            .await
        });

        let mut client = client_io;
        client.write_all(&version_frame()).await.expect("version");
        client
            .write_all(&create_chan_frame(1, "r46:pv"))
            .await
            .expect("create_chan");
        client.flush().await.expect("flush create_chan");
        let sid = read_create_chan_sid(&mut client, Duration::from_secs(3)).await;

        // Target a SID the server never assigned: unknown/stale channel.
        let unknown_sid = sid.wrapping_add(0xDEAD);
        assert_ne!(
            unknown_sid, sid,
            "test must use a SID distinct from the real one"
        );
        client
            .write_all(&event_add_zero_mask_frame(unknown_sid, 0xC0DE))
            .await
            .expect("unknown-sid zero-mask event_add");
        client
            .flush()
            .await
            .expect("flush unknown-sid zero-mask event_add");

        // Read server output until EOF; the unknown SID must produce the
        // bad-SID `ECA_INTERNAL` frame, then the server closes.
        let acc = drain_to_eof(&mut client, Duration::from_secs(3)).await;

        // The first CA_PROTO_ERROR frame must be the bad-SID ECA_INTERNAL
        // ("Bad Resource ID") frame with the 0xFFFFFFFF cid sentinel — NOT
        // the mask-path ECA_ALLOCMEM frame the defective guard emitted.
        let err = first_ca_proto_error(&acc).unwrap_or_else(|| {
            panic!(
                "EVENT_ADD on an unknown SID with mask=0 must emit the bad-SID \
                 ECA_INTERNAL frame (C logBadId), but no CA_PROTO_ERROR was emitted \
                 (received {} bytes: {acc:?})",
                acc.len()
            )
        });
        assert_eq!(
            err.available, ECA_INTERNAL,
            "Guard ordering: unknown-SID EVENT_ADD must reply ECA_INTERNAL \
             (bad-SID logBadId), not ECA_ALLOCMEM (mask path); got eca={:#x}",
            err.available
        );
        assert_eq!(
            err.cid, 0xFFFF_FFFF,
            "bad-SID ECA_INTERNAL frame echoes the 0xFFFFFFFF cid sentinel \
             (MPTOPCIU→NULL ⇒ vsend_err cid=0xffffffff), got {:#x}",
            err.cid
        );

        // The handler still closes the connection with Err (RSRV_ERROR).
        let res = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("handle_client completes after unknown-sid event_add")
            .expect("join ok");
        assert!(
            res.is_err(),
            "Guard ordering: unknown-SID EVENT_ADD must close the connection with \
             Err (matches C RSRV_ERROR), got {res:?}"
        );
    }

    /// Build a deprecated CA_PROTO_READ (cmd=3) request addressing
    /// `sid` with DBR `data_type`, element `count`, and read `ioid`.
    fn read_frame(sid: u32, data_type: u16, count: u16, ioid: u32) -> Vec<u8> {
        let mut h = CaHeader::new(CA_PROTO_READ);
        h.data_type = data_type;
        h.count = count;
        h.cid = sid;
        h.available = ioid;
        h.to_bytes().to_vec()
    }

    /// Read all server output until EOF (server closed) or `timeout`.
    async fn drain_to_eof<R: tokio::io::AsyncRead + Unpin>(
        client: &mut R,
        timeout: Duration,
    ) -> Vec<u8> {
        let mut acc: Vec<u8> = Vec::new();
        let mut buf = [0u8; 256];
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for server to close"
            );
            match tokio::time::timeout(remaining, client.read(&mut buf)).await {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => acc.extend_from_slice(&buf[..n]),
                Ok(Err(_)) | Err(_) => break,
            }
        }
        acc
    }

    /// Scan `acc` for the first CA_PROTO_ERROR frame and return its
    /// parsed header, or `None` if there is none.
    fn first_ca_proto_error(acc: &[u8]) -> Option<CaHeader> {
        let mut offset = 0;
        while offset + CaHeader::SIZE <= acc.len() {
            if let Ok((hdr, hdr_size)) = CaHeader::from_bytes_extended(&acc[offset..]) {
                if hdr.cmmd == CA_PROTO_ERROR {
                    return Some(hdr);
                }
                let msg_len = hdr_size + hdr.actual_postsize();
                if msg_len == 0 {
                    break;
                }
                offset += msg_len;
            } else {
                break;
            }
        }
        None
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn deprecated_read_unknown_sid_bad_type_sends_bad_resource_id() {
        // C `read_action` (`rsrv/camessage.c:606-621`) resolves the
        // channel BEFORE checking the DBR type, so an unknown SID takes
        // the bad-SID `logBadId` branch even when the requested type is
        // also invalid. `logBadId` (camessage.c:312-325) sends
        // ECA_INTERNAL ("Bad Resource ID", cid=0xFFFFFFFF), flushed by
        // camsgtask.c:142 before the disconnect — NOT the post-lookup
        // ECA_BADTYPE. Pre-fix Rust checked the type first and emitted a
        // spurious ECA_BADTYPE(cid=0xFFFFFFFF) ahead of the lookup.
        let db = Arc::new(PvDatabase::new());
        db.add_pv("a41:pv", EpicsValue::Double(0.0))
            .await
            .expect("add pv");
        let acf = epics_base_rs::server::access_security::new_acf_cell(None);
        let (_acf_reload_tx, acf_reload_rx) = broadcast::channel::<()>(4);
        let (conn_tx, _conn_rx) = broadcast::channel::<ServerConnectionEvent>(64);

        let (client_io, server_io) = tokio::io::duplex(4096);
        let peer: SocketAddr = "127.0.0.1:55411".parse().unwrap();

        let handle = tokio::spawn(async move {
            handle_client(
                server_io,
                peer,
                db,
                acf,
                acf_reload_rx,
                5064,
                None,
                None,
                None,
                Some(conn_tx),
                None,
                #[cfg(feature = "cap-tokens")]
                None,
                #[cfg(feature = "cap-tokens")]
                None,
            )
            .await
        });

        let mut client = client_io;
        client.write_all(&version_frame()).await.expect("version");
        client
            .write_all(&create_chan_frame(1, "a41:pv"))
            .await
            .expect("create_chan");
        client.flush().await.expect("flush create_chan");
        let sid = read_create_chan_sid(&mut client, Duration::from_secs(3)).await;

        let unknown_sid = sid.wrapping_add(0xDEAD);
        assert_ne!(
            unknown_sid, sid,
            "test must use a SID distinct from the real one"
        );
        // Bad DBR type (99 > LAST_BUFFER_TYPE = 38) on the unknown SID.
        client
            .write_all(&read_frame(unknown_sid, 99, 1, 0x4141))
            .await
            .expect("read unknown sid bad type");
        client.flush().await.expect("flush read");

        let acc = drain_to_eof(&mut client, Duration::from_secs(3)).await;
        let err = first_ca_proto_error(&acc).unwrap_or_else(|| {
            panic!(
                "deprecated READ on an unknown SID with a bad type must emit the \
                 bad-SID ECA_INTERNAL frame (C read_action logBadId), but no \
                 CA_PROTO_ERROR was emitted (received {} bytes: {acc:?})",
                acc.len()
            )
        });
        assert_eq!(
            err.available, ECA_INTERNAL,
            "unknown-SID READ takes the bad-SID logBadId branch (ECA_INTERNAL), \
             not the post-lookup ECA_BADTYPE; got eca={:#x}",
            err.available
        );
        assert_eq!(
            err.cid, 0xFFFF_FFFF,
            "bad-SID ECA_INTERNAL frame echoes the 0xFFFFFFFF cid sentinel, got {:#x}",
            err.cid
        );

        let res = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("handle_client completes after unknown-sid read")
            .expect("join ok");
        assert!(
            res.is_err(),
            "unknown-SID READ must close the connection with Err (C RSRV_ERROR), got {res:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn deprecated_read_known_sid_bad_type_sends_badtype_with_real_cid() {
        // with a VALID channel, C `read_action`
        // (`rsrv/camessage.c:621-624`) sends ECA_BADTYPE carrying the
        // channel's real cid (`pciu->cid`) + record name — never the
        // 0xFFFFFFFF sentinel the pre-fix code used.
        let db = Arc::new(PvDatabase::new());
        db.add_pv("a41:pv", EpicsValue::Double(0.0))
            .await
            .expect("add pv");
        let acf = epics_base_rs::server::access_security::new_acf_cell(None);
        let (_acf_reload_tx, acf_reload_rx) = broadcast::channel::<()>(4);
        let (conn_tx, _conn_rx) = broadcast::channel::<ServerConnectionEvent>(64);

        let (client_io, server_io) = tokio::io::duplex(4096);
        let peer: SocketAddr = "127.0.0.1:55412".parse().unwrap();

        let handle = tokio::spawn(async move {
            handle_client(
                server_io,
                peer,
                db,
                acf,
                acf_reload_rx,
                5064,
                None,
                None,
                None,
                Some(conn_tx),
                None,
                #[cfg(feature = "cap-tokens")]
                None,
                #[cfg(feature = "cap-tokens")]
                None,
            )
            .await
        });

        let mut client = client_io;
        client.write_all(&version_frame()).await.expect("version");
        // create_chan client cid = 1; the BADTYPE error must echo it.
        client
            .write_all(&create_chan_frame(1, "a41:pv"))
            .await
            .expect("create_chan");
        client.flush().await.expect("flush create_chan");
        let sid = read_create_chan_sid(&mut client, Duration::from_secs(3)).await;

        // Bad DBR type (99 > LAST_BUFFER_TYPE = 38) on the VALID sid.
        client
            .write_all(&read_frame(sid, 99, 1, 0x4242))
            .await
            .expect("read known sid bad type");
        client.flush().await.expect("flush read");

        let acc = drain_to_eof(&mut client, Duration::from_secs(3)).await;
        let err = first_ca_proto_error(&acc)
            .expect("deprecated READ with a valid SID + bad type must emit a CA_PROTO_ERROR");
        assert_eq!(
            err.available, ECA_BADTYPE,
            "status must be ECA_BADTYPE, got {:#x}",
            err.available
        );
        assert_ne!(
            err.cid,
            u32::MAX,
            "BADTYPE cid must be the channel's real cid, not the 0xFFFFFFFF sentinel"
        );
        assert_eq!(
            err.cid, 1,
            "BADTYPE cid must echo the create_chan client cid (pciu->cid)"
        );

        let res = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("handle_client completes after known-sid bad-type read")
            .expect("join ok");
        assert!(
            res.is_err(),
            "bad-type READ must close the connection with Err (C RSRV_ERROR), got {res:?}"
        );
    }
}

#[cfg(test)]
mod send_tmo_env_tests {
    //! Per-boundary coverage of the server's env-derived timeouts (R15-16).
    //!
    //! `EPICS_CAS_SEND_TMO=inf` used to panic `send_timeout()` on the
    //! FIRST client connect, taking the whole server down — a remotely
    //! triggerable DoS on a misconfigured host.
    use super::{resolve_inactivity_timeout, resolve_send_timeout};
    use std::time::Duration;

    /// SAFETY: gated by `serial_test::serial`; restored before return.
    fn with_env(name: &str, value: Option<&str>, f: impl FnOnce()) {
        let saved = std::env::var(name).ok();
        unsafe {
            match value {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
        }
        f();
        unsafe {
            match saved {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn send_tmo_boundaries() {
        let default = Duration::from_secs(5);
        let cases: &[(Option<&str>, Duration)] = &[
            (None, default),
            (Some(""), default),
            (Some("2.5"), Duration::from_millis(2500)),
            (Some(" 2.5 "), Duration::from_millis(2500)),
            (Some("0x10"), Duration::from_secs(16)),
            // Never-expiring send deadline instead of an abort.
            (Some("inf"), Duration::MAX),
            // Rejected by `epicsScanDouble` → default.
            (Some("1e400"), default),
            (Some("abc"), default),
            (Some("2x"), default),
            // Floor: sub-0.1 s (and NaN, which `f64::max` discards) clamp up.
            (Some("0.01"), Duration::from_millis(100)),
            (Some("0"), Duration::from_millis(100)),
            (Some("nan"), Duration::from_millis(100)),
        ];
        for (raw, want) in cases {
            with_env("EPICS_CAS_SEND_TMO", *raw, || {
                assert_eq!(
                    resolve_send_timeout(),
                    *want,
                    "EPICS_CAS_SEND_TMO={raw:?} must resolve to {want:?}"
                );
            });
        }
    }

    #[test]
    #[serial_test::serial]
    fn inactivity_tmo_boundaries() {
        let cases: &[(Option<&str>, Option<Duration>)] = &[
            // Disabled by default; an unparseable value keeps it disabled.
            (None, None),
            (Some(""), None),
            (Some("abc"), None),
            (Some("1e400"), None),
            // NaN fails the `> 0.0` gate, as every C comparison against it does.
            (Some("nan"), None),
            (Some("0"), None),
            (Some("-1"), None),
            // Positive values are honoured, floored at 30 s.
            (Some("60"), Some(Duration::from_secs(60))),
            (Some("1"), Some(Duration::from_secs(30))),
            (Some("0x40"), Some(Duration::from_secs(64))),
            (Some("inf"), Some(Duration::MAX)),
        ];
        for (raw, want) in cases {
            with_env("EPICS_CAS_INACTIVITY_TMO", *raw, || {
                assert_eq!(
                    resolve_inactivity_timeout(),
                    *want,
                    "EPICS_CAS_INACTIVITY_TMO={raw:?} must resolve to {want:?}"
                );
            });
        }
    }
}

#[cfg(test)]
mod read_reply_sans_io_tests {
    //! Sans-io proof for the READ / READ_NOTIFY reply: `build_read_reply`
    //! produces the exact wire frame from a hand-built [`Snapshot`] and the
    //! request parameters, with NO socket, NO `DuplexStream`, NO database,
    //! and NO async. Every assertion here inspects the returned frame's bytes
    //! directly. This is the increment-1 demonstration that the reply's
    //! byte production is a pure function of `(snapshot, request)` — the
    //! whole point of the sans-io split. The frame buffer it borrows is an
    //! allocator, not I/O: a test supplies its own.
    use super::*;
    use epics_base_rs::server::snapshot::Snapshot;
    use epics_base_rs::types::{DBR_LONG, DBR_STRING, EpicsValue};

    fn pool() -> std::sync::Arc<crate::server::frame::FramePool> {
        std::sync::Arc::new(crate::server::frame::FramePool::new())
    }

    fn scalar_long(v: i32) -> Snapshot {
        Snapshot::new(EpicsValue::Long(v), 0, 0, std::time::SystemTime::UNIX_EPOCH)
    }

    /// A refused get conversion is `ReadReplyError::GetFail`, which the
    /// dispatch answers with C's zeroed ECA_GETFAIL frame
    /// (`camessage.c:545-561`) and a live circuit — NOT `BadType`, which tears
    /// the connection down for a request the server can never serve.
    #[test]
    fn unparseable_string_field_read_as_double_is_err_getfail() {
        let snap = Snapshot::new(
            EpicsValue::String("hello".into()),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );
        let err = build_read_reply(
            &pool(),
            epics_base_rs::types::DBR_DOUBLE,
            1,
            true,
            &snap,
            0,
            0x11,
            CA_MINOR_VERSION,
        )
        .expect_err("a string that is not a number cannot be served as DBR_DOUBLE");
        assert!(matches!(err, ReadReplyError::GetFail));

        // A parseable one still frames normally.
        let ok = Snapshot::new(
            EpicsValue::String("3.125".into()),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );
        let frame = build_read_reply(
            &pool(),
            epics_base_rs::types::DBR_DOUBLE,
            1,
            true,
            &ok,
            0,
            0x11,
            CA_MINOR_VERSION,
        )
        .expect("a numeric string frames");
        assert_eq!(f64::from_be_bytes(frame[16..24].try_into().unwrap()), 3.125);
    }

    /// A `DBF_CHAR` waveform read as `DBR_STRING` ships one MAX_STRING_SIZE
    /// slot per element, so the declared count and the body agree: C
    /// `getCharString` (`dbConvert.c:417-437`) gives `m_count = 10` with
    /// `m_postsize = 400`. The collapsed conversion declared ten elements over
    /// a 40-byte body, and `size_dbr_reply` cannot repair that — it resizes
    /// only when the requested count differs from the live one, which here it
    /// does not.
    #[test]
    fn read_notify_char_waveform_as_string_is_forty_bytes_per_element() {
        let snap = Snapshot::new(
            EpicsValue::CharArray(b"ABCDEFGHIJ".to_vec()),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );
        let frame = build_read_reply(
            &pool(),
            DBR_STRING,
            0, // notify autosize -> the live count, 10
            true,
            &snap,
            0,
            0x99,
            CA_MINOR_VERSION,
        )
        .expect("char waveform as DBR_STRING frames");

        let hdr = CaHeader::from_bytes(&frame[..16]).expect("parse header");
        assert_eq!(hdr.count, 10, "one element per waveform byte");
        assert_eq!(hdr.postsize, 400, "ten MAX_STRING_SIZE slots");
        assert_eq!(frame.len(), 16 + 400);
        assert_eq!(&frame[16..18], b"65", "element 0 is cvtCharToString('A')");
        assert_eq!(&frame[16 + 360..16 + 362], b"74", "element 9 is 'J'");
    }

    /// A scalar READ_NOTIFY frame is header + the 8-aligned value payload,
    /// with the notify opcode, `cid == ECA_NORMAL` (the status slot), the
    /// echoed ioid, and count 1.
    #[test]
    fn read_notify_scalar_long_is_header_plus_padded_value() {
        let frame = build_read_reply(
            &pool(),
            DBR_LONG,
            0, // notify autosize → live count (scalar ⇒ 1)
            true,
            &scalar_long(0x0102_0304),
            0xAAAA_BBBB, // ignored for notify (cid slot carries status)
            0x1234_5678,
            CA_MINOR_VERSION,
        )
        .expect("scalar long read reply frames");

        assert_eq!(frame.len(), 16 + 8, "16-byte header + 8-aligned i32 body");
        let hdr = CaHeader::from_bytes(&frame[..16]).expect("parse header");
        assert_eq!(hdr.cmmd, CA_PROTO_READ_NOTIFY);
        assert_eq!(hdr.data_type, DBR_LONG);
        assert_eq!(hdr.actual_count(), 1);
        assert_eq!(hdr.cid, ECA_NORMAL, "notify cid slot is ECA_NORMAL");
        assert_eq!(hdr.available, 0x1234_5678, "ioid echoed into m_available");
        assert_eq!(hdr.actual_postsize(), 8);
        assert_eq!(&frame[16..20], &0x0102_0304i32.to_be_bytes(), "value bytes");
        assert_eq!(&frame[20..24], &[0, 0, 0, 0], "alignment padding is zero");
    }

    /// The deprecated synchronous CA_PROTO_READ carries the READ opcode and
    /// the channel's client-side CID (`pciu->cid`), not `ECA_NORMAL`.
    #[test]
    fn deprecated_read_uses_channel_cid_and_read_opcode() {
        let frame = build_read_reply(
            &pool(),
            DBR_LONG,
            1, // non-zero ⇒ ordinary scalar, not the count==0 metadata path
            false,
            &scalar_long(42),
            0x00C0_FFEE,
            0x9,
            CA_MINOR_VERSION,
        )
        .expect("deprecated scalar read frames");

        let hdr = CaHeader::from_bytes(&frame[..16]).expect("parse header");
        assert_eq!(hdr.cmmd, CA_PROTO_READ, "deprecated READ opcode");
        assert_eq!(
            hdr.cid, 0x00C0_FFEE,
            "deprecated READ echoes the channel CID"
        );
        assert_eq!(hdr.actual_count(), 1);
        assert_eq!(&frame[16..20], &42i32.to_be_bytes());
    }

    /// A deprecated CA_PROTO_READ with `m_count == 0` is NOT autosize: C
    /// sizes it with `dbr_size_n(type, 0)`, so a plain type ships count 0
    /// and a zero-length body (header only).
    #[test]
    fn deprecated_read_count_zero_ships_header_only_for_plain_type() {
        let frame = build_read_reply(
            &pool(),
            DBR_LONG,
            0,
            false,
            &scalar_long(7),
            0x1,
            0x2,
            CA_MINOR_VERSION,
        )
        .expect("count==0 deprecated read frames");

        assert_eq!(frame.len(), 16, "plain-type count==0 body is empty");
        let hdr = CaHeader::from_bytes(&frame[..16]).expect("parse header");
        assert_eq!(hdr.cmmd, CA_PROTO_READ);
        assert_eq!(hdr.actual_count(), 0, "count==0 stays 0 (not autosize)");
        assert_eq!(hdr.actual_postsize(), 0);
    }

    /// A short array framed at a LARGER requested count keeps the requested
    /// count in the header and zero-fills the missing elements.
    #[test]
    fn read_notify_array_pads_to_requested_count() {
        let snap = Snapshot::new(
            EpicsValue::LongArray(vec![10, 20, 30]),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );
        let frame = build_read_reply(&pool(), DBR_LONG, 5, true, &snap, 0, 0x7, CA_MINOR_VERSION)
            .expect("padded array frames");

        let hdr = CaHeader::from_bytes(&frame[..16]).expect("parse header");
        assert_eq!(hdr.actual_count(), 5, "header carries the requested count");
        // 5 * 4 = 20 value bytes, 8-aligned to 24.
        assert_eq!(hdr.actual_postsize(), 24);
        assert_eq!(frame.len(), 16 + 24);
        assert_eq!(&frame[16..20], &10i32.to_be_bytes(), "element 0");
        assert_eq!(&frame[20..24], &20i32.to_be_bytes(), "element 1");
        assert_eq!(&frame[24..28], &30i32.to_be_bytes(), "element 2");
        assert!(
            frame[28..].iter().all(|&b| b == 0),
            "over-requested elements + alignment are zero-filled"
        );
    }

    /// An array framed at a SMALLER requested count truncates to it.
    #[test]
    fn read_notify_array_truncates_under_requested_count() {
        let snap = Snapshot::new(
            EpicsValue::LongArray(vec![1, 2, 3, 4, 5]),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );
        let frame = build_read_reply(&pool(), DBR_LONG, 2, true, &snap, 0, 0x7, CA_MINOR_VERSION)
            .expect("truncated array frames");

        let hdr = CaHeader::from_bytes(&frame[..16]).expect("parse header");
        assert_eq!(hdr.actual_count(), 2, "count truncated to the requested 2");
        assert_eq!(frame.len(), 16 + 8, "2 * 4 = 8, already 8-aligned");
        assert_eq!(&frame[16..20], &1i32.to_be_bytes());
        assert_eq!(&frame[20..24], &2i32.to_be_bytes());
    }

    /// A reply that needs the extended (24-byte) header cannot be framed for
    /// a pre-V49 client — `build_read_reply` reports `Oversize`, which the
    /// dispatch handler maps to ECA_16KARRAYCLIENT.
    #[test]
    fn oversize_array_to_pre_v49_client_is_err_oversize() {
        let snap = Snapshot::new(
            EpicsValue::LongArray(vec![7; 20_000]), // 80 000 bytes > 0xFFFF
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );
        let err = build_read_reply(&pool(), DBR_LONG, 20_000, true, &snap, 0, 0x7, 8)
            .expect_err("pre-V49 client cannot frame an extended reply");
        assert!(matches!(err, ReadReplyError::Oversize));
    }

    /// An unencodable DBR type is `BadType` (C `INVALID_DB_REQ`), which the
    /// handler turns into ECA_BADTYPE (deprecated READ) or a silent drop
    /// (READ_NOTIFY).
    #[test]
    fn unsupported_dbr_type_is_err_badtype() {
        let err = build_read_reply(
            &pool(),
            99,
            1,
            false,
            &scalar_long(0),
            0x1,
            0x2,
            CA_MINOR_VERSION,
        )
        .expect_err("type 99 > LAST_BUFFER_TYPE cannot encode");
        assert!(matches!(err, ReadReplyError::BadType));
    }

    /// Deprecated scalar DBR_STRING contracts to its NUL-terminated length
    /// (value + NUL, 8-aligned), not the fixed 40-byte slot — C `read_action`
    /// `epicsStrnLen(pStr, 40) + 1`.
    #[test]
    fn deprecated_scalar_string_contracts_to_nul_terminated_length() {
        let snap = Snapshot::new(
            EpicsValue::String(epics_base_rs::types::PvString::from("OK")),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );
        let frame = build_read_reply(
            &pool(),
            DBR_STRING,
            1,
            false,
            &snap,
            0x1,
            0x2,
            CA_MINOR_VERSION,
        )
        .expect("scalar string frames");
        let hdr = CaHeader::from_bytes(&frame[..16]).expect("parse header");
        // "OK" + NUL = 3 bytes, 8-aligned to 8 — not the 40-byte slot.
        assert_eq!(
            hdr.actual_postsize(),
            8,
            "contracted to value + NUL, 8-aligned"
        );
        assert_eq!(&frame[16..18], b"OK");
        assert_eq!(frame[18], 0, "NUL terminator");
    }
}
