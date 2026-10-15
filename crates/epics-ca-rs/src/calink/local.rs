//! The in-process CA link channel — C `dbContext` / `dbChannelIO`
//! (`dbContext.cpp`, `dbChannelIO.cpp`).
//!
//! `dbServiceIOInit` (`dbContext.cpp:59-66`, called from `dbCaLinkInit`,
//! `dbCa.c:344`) installs `dbContext` as libca's default service, so every
//! `ca_create_channel` dbCa makes first tries `dbChannel_create`
//! (`dbContext.cpp:90-121`). A name the local database resolves is served
//! straight from it — no search, no circuit, no rsrv — and only a name it
//! refuses reaches the network context. [`LocalChannel::create`] is that
//! first try; the resolver falls back to a `CaChannel` when it returns
//! `None`.

use std::borrow::Cow;
use std::time::Duration;

use epics_base_rs::server::database::channel_io::{self, ChannelTarget, DbAccessChannel};
use epics_base_rs::server::database::{LinkPutOp, PvDatabase};
use epics_base_rs::server::event_queue::{EventReader, EventUser, no_context};
use epics_base_rs::server::pv::{MonitorEvent, SubscribeRefused, WriteContext};
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::{DbFieldType, EpicsValue, decode_dbr, encode_dbr_into};

use crate::server::DeliveryStage;
use crate::server::monitor::with_live_options;
use crate::server::tcp::prepare_read;

/// How long a put callback waits for the channel's previous one before it
/// gives up — `dbPutNotifyBlocker::initiatePutNotify`'s 30 s
/// (`dbPutNotifyBlocker.cpp:146-149`).
const PUT_NOTIFY_BLOCK_TIMEOUT: Duration = Duration::from_secs(30);

/// C `dbChannelIO`: an open database channel plus the put-notify blocker
/// `dbContext` keeps for it.
pub(super) struct LocalChannel {
    db: PvDatabase,
    chan: DbAccessChannel,
    /// C `dbPutNotifyBlocker`: one put callback at a time per channel; the
    /// next one waits until the current one completes (`pNotify` cleared
    /// in `putNotifyCompletion`).
    blocker: tokio::sync::Mutex<()>,
}

impl LocalChannel {
    /// `dbContext::createChannel`'s `dbChannel_create`: `None` when the
    /// local database does not serve `name`.
    pub(super) async fn create(db: &PvDatabase, name: &[u8]) -> Option<Self> {
        let chan = channel_io::channel_create(db, name, None).await.ok()?;
        Some(Self {
            db: db.clone(),
            chan,
            blocker: tokio::sync::Mutex::new(()),
        })
    }

    /// `dbChannelIO::nativeType` — `dbChannelExportCAType`, the CA type
    /// the field promotes to, as a network channel would report it.
    pub(super) fn native_type(&self) -> DbFieldType {
        DbFieldType::from_u16(self.chan.native_type.ca_wire_type())
            .expect("ca_wire_type is a CA DBF code")
    }

    /// `dbChannelIO::nativeElementCount` — `dbChannelElements`.
    pub(super) fn element_count(&self) -> u32 {
        self.chan.final_element_count
    }

    /// `dbChannelIO::read` → `dbContextReadNotifyCache::callReadNotify`
    /// (`dbContextReadNotifyCache.cpp:60-110`): `dbChannel_get` of
    /// `dbr_type`, `count` elements (0 = the current count), completed
    /// synchronously. `None` is its `ECA_GETFAIL`.
    pub(super) fn read(&self, dbr_type: u16, count: u32) -> Option<Snapshot> {
        let snapshot = channel_io::get_full_snapshot(&self.db, &self.chan.target)?;
        let snapshot = prepare_read(
            &self.chan.target,
            self.chan.filter_chain(),
            self.chan.long_string_mode,
            self.chan.field_element_count,
            dbr_type,
            count,
            snapshot,
        );
        let mut buf = Vec::new();
        encode_dbr_into(&mut buf, dbr_type, &snapshot).ok()?;
        let n = if count == 0 {
            snapshot.value.count()
        } else {
            count
        };
        decode_dbr(dbr_type, &buf, n as usize).ok()
    }

    /// `dbContext::subscribe` → `dbSubscriptionIO` (`dbSubscriptionIO.cpp:
    /// 38-56`): `db_add_event` with `mask`, then `db_post_single_event`, so
    /// the current value is the first event. Updates are delivered as
    /// `dbr_type` (`callStateNotify`, count 0 = the current count).
    pub(super) fn subscribe(
        &self,
        dbr_type: u16,
        mask: u16,
    ) -> Result<LocalMonitor, SubscribeRefused> {
        let (pre_chain, post_chain) = self.chan.filter_chain().split();
        // C's `dbContext` owns one event context for all its subscriptions;
        // here each link owns one, so its queue is never shared with another
        // link's.
        let user = EventUser::new();
        let reader = channel_io::add_event(
            &self.db,
            &self.chan.target,
            &user,
            0,
            self.chan.native_type,
            mask,
            no_context(),
            pre_chain,
        )?;
        Ok(LocalMonitor {
            _user: user,
            reader,
            stage: DeliveryStage::new(
                post_chain,
                self.chan.long_string_mode,
                self.chan.field_element_count,
            ),
            dbr_type,
            db: self.db.clone(),
            target: self.chan.target.clone(),
        })
    }

    /// `dbChannelIO::write`: a plain put is `dbChannel_put` in the caller's
    /// thread (`dbChannelIO.cpp:78-91`); a put callback goes through the
    /// channel's `dbPutNotifyBlocker` → `dbProcessNotify` and completes when
    /// the record's processing does (`dbPutNotifyBlocker.cpp:132-176`).
    pub(super) async fn write(&self, value: EpicsValue, op: LinkPutOp) -> Result<(), String> {
        match op {
            LinkPutOp::Plain => self
                .put(value, false)
                .await
                .map(|_| ())
                .map_err(|e| format!("db_put_field() completed unsuccessfully: {e}")),
            LinkPutOp::Async => {
                let _blocker = epics_base_rs::runtime::task::timeout(
                    PUT_NOTIFY_BLOCK_TIMEOUT,
                    self.blocker.lock(),
                )
                .await
                .map_err(|_| "put callback request timed out".to_string())?;
                let completion = self
                    .put(value, true)
                    .await
                    .map_err(|e| format!("put notify unsuccessful: {e}"))?;
                if let Some(done) = completion.into_handle() {
                    done.await
                        .map_err(|_| "put notify unsuccessful: canceled".to_string())?;
                }
                Ok(())
            }
        }
    }

    async fn put(
        &self,
        value: EpicsValue,
        notify: bool,
    ) -> epics_base_rs::error::CaResult<epics_base_rs::server::record::ProcessCompletion> {
        channel_io::put(
            &self.db,
            &self.chan.target,
            self.chan.long_string_mode,
            value,
            notify,
            WriteContext::default,
        )
        .await
    }
}

/// C `dbSubscriptionIO`: one installed event plus what `callStateNotify`
/// needs to hand each one over as a DBR value. Dropping it cancels the
/// event (`db_cancel_event`).
pub(super) struct LocalMonitor {
    /// The event context the reader's queue belongs to.
    _user: EventUser,
    reader: EventReader,
    stage: DeliveryStage,
    dbr_type: u16,
    db: PvDatabase,
    target: ChannelTarget,
}

impl LocalMonitor {
    /// The next event, `None` once the subscription has ended. An inner
    /// `None` is an event the post chain dropped or one `dbChannel_get`
    /// failed on (`callStateNotify`'s `ECA_GETFAIL` exception).
    pub(super) async fn next(&mut self) -> Option<Option<Snapshot>> {
        let event = self.reader.recv().await?;
        Some(self.state_notify(event))
    }

    /// `dbContext::callStateNotify` (`dbContext.cpp:141-189`): the event as
    /// a `dbr_type` value of its current element count.
    fn state_notify(&self, event: MonitorEvent) -> Option<Snapshot> {
        let mut read = None;
        let update = with_live_options(self.dbr_type, Cow::Owned(event), &mut read, || {
            channel_io::get_full_snapshot(&self.db, &self.target)
        })?;
        let mut log = None;
        let delivery = update.prepare(&self.stage, 0, &mut log)?;
        let mut buf = Vec::new();
        delivery.encode_dbr_into(&mut buf, self.dbr_type).ok()?;
        decode_dbr(self.dbr_type, &buf, delivery.count() as usize).ok()
    }
}
