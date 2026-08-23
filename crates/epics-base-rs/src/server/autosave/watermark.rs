//! Change-detection watermark for the autosave polling loops.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::server::database::PvDatabase;

use super::manager::update_status_pvs;
use super::save_set::SaveSet;

/// What one poll of a save set decided about its watermark.
///
/// The polling loop cannot move a watermark without saying which of
/// these it is, so "the set changed" and "the set is already on disk"
/// can no longer share a commit site.
pub(super) enum Tick<W> {
    /// Nothing that belongs in the `.sav` file changed; the watermark
    /// may move without a save. This is also how the first poll
    /// establishes its baseline.
    Unchanged(W),
    /// The set changed; the watermark may move only once the save has
    /// actually landed on disk.
    Changed(W),
}

/// Single owner of one save set's change-detection watermark.
///
/// `state` is the state whose contents are known to be on disk: the
/// `OnChange` snapshot, or the `Triggered` trigger reading plus its
/// arming. The field is private to this module, so the loops in
/// [`super::manager`] have no way to assign it — the only way forward
/// is [`ChangeWatermark::advance`], which performs the save itself and
/// installs a [`Tick::Changed`] state only when `save_once` returned
/// `Ok`. A failed save therefore leaves the watermark where it was and
/// the next poll re-detects the same change and retries it.
///
/// Losing that retry is permanent and, by default, silent: the value is
/// never written, `changed` is false from then on, and the next restore
/// brings back the pre-change value. The `SR_<set>_status` PVs exist only
/// when a `status_prefix` is configured, so nothing else reports the loss.
pub(super) struct ChangeWatermark<W> {
    state: W,
    set: Arc<SaveSet>,
    lock: Arc<Mutex<()>>,
    db: Arc<PvDatabase>,
    status_prefix: Option<String>,
}

impl<W> ChangeWatermark<W> {
    pub(super) fn new(
        state: W,
        set: Arc<SaveSet>,
        lock: Arc<Mutex<()>>,
        db: Arc<PvDatabase>,
        status_prefix: Option<String>,
    ) -> Self {
        Self {
            state,
            set,
            lock,
            db,
            status_prefix,
        }
    }

    /// The state currently known to be on disk.
    pub(super) fn state(&self) -> &W {
        &self.state
    }

    pub(super) fn set(&self) -> &SaveSet {
        &self.set
    }

    pub(super) fn db(&self) -> &PvDatabase {
        &self.db
    }

    /// Apply one poll's decision, saving first when one is owed.
    pub(super) async fn advance(&mut self, tick: Tick<W>) {
        let candidate = match tick {
            Tick::Unchanged(next) => {
                self.state = next;
                return;
            }
            Tick::Changed(next) => next,
        };

        let _guard = self.lock.lock().await;
        let result = self.set.save_once(&self.db).await;
        if let Some(ref prefix) = self.status_prefix {
            update_status_pvs(&self.db, prefix, &self.set, &result).await;
        }
        if result.is_ok() {
            self.state = candidate;
        }
    }
}
