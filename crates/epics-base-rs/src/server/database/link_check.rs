//! C `checkLinks` and `checkLinksCallback`: the re-read that moves a link's
//! status between `Ext PV NC` and `Ext PV OK` as the link connects and drops
//! (`calcoutRecord.c:711-773`, and the synApps copies).

use std::time::Duration;

use super::PvDatabase;
use crate::server::record::RecordInstance;
use crate::server::records::link_status::{
    LINK_EXT_NC, LINK_EXT_OK, LINK_RECHECK, link_is_external,
};

impl PvDatabase {
    /// C `checkLinks` on the record `instance` holds: every link its rule
    /// visits whose status reads `Ext PV` takes its `dbIsLinkConnected`, and
    /// a status that moved posts. Returns whether it scheduled
    /// `checkLinksCallback` — an `Ext PV NC` remains and none was pending.
    pub(crate) fn check_links(&self, instance: &mut RecordInstance) -> bool {
        let Some(check) = instance.record.link_check() else {
            return false;
        };
        let spec = check.spec;
        let due = check
            .statuses
            .iter()
            .enumerate()
            .filter(|(_, s)| link_is_external(**s))
            .fold(0u64, |due, (i, _)| due | 1 << i);
        let (mut visited, mut connected) = (0u64, 0u64);
        for (i, &(link, _)) in spec.links.iter().enumerate() {
            if due & 1 << i == 0 {
                continue;
            }
            let Some(parsed) = instance.stored_link(link) else {
                continue;
            };
            if spec.rule.visits(parsed) {
                visited |= 1 << i;
                if self.link_connected(parsed) {
                    connected |= 1 << i;
                }
            }
        }
        let Some(check) = instance.record.link_check() else {
            return false;
        };
        let mut moved = 0u64;
        for (i, status) in check.statuses.iter_mut().enumerate() {
            if visited & 1 << i == 0 {
                continue;
            }
            let now = if connected & 1 << i != 0 {
                LINK_EXT_OK
            } else {
                LINK_EXT_NC
            };
            if *status != now {
                *status = now;
                moved |= 1 << i;
            }
        }
        check.state.ca_links = visited != 0;
        let arm = visited & !connected != 0 && !check.state.scheduled;
        check.state.scheduled |= arm;
        for (i, &(_, status)) in spec.links.iter().enumerate() {
            if moved & 1 << i != 0 {
                instance.notify_field(status, spec.mask);
            }
        }
        arm
    }

    /// [`Self::check_links`] from the process cycle, which C runs while
    /// `caLinkStat != NO_CA_LINKS` (`calcoutRecord.c:234`).
    pub(crate) fn check_links_in_cycle(&self, name: &[u8], instance: &mut RecordInstance) {
        if instance
            .record
            .link_check()
            .is_some_and(|check| check.state.ca_links)
            && self.check_links(instance)
        {
            self.arm_link_check(name, LINK_RECHECK);
        }
    }

    /// C `callbackRequestDelayed(&checkLinkCb, delay)`: after `delay`,
    /// `dbScanLock`, clear `cbScheduled`, `checkLinks` — again every 0.5 s
    /// while it re-arms.
    fn arm_link_check(&self, name: &[u8], delay: Duration) {
        let db = self.clone();
        let name = crate::types::RecordName::from(name);
        crate::runtime::task::spawn_background(
            crate::runtime::task::CallbackPriority::Low,
            async move {
                let mut delay = delay;
                loop {
                    crate::runtime::task::sleep_background(delay).await;
                    let Some(rec) = db.get_record_no_resolve(&name) else {
                        return;
                    };
                    let mut instance = rec.write();
                    let Some(check) = instance.record.link_check() else {
                        return;
                    };
                    check.state.scheduled = false;
                    if !db.check_links(&mut instance) {
                        return;
                    }
                    delay = LINK_RECHECK;
                }
            },
        );
    }

    /// After `init_record` or `special()` classified `record`'s links: an
    /// `Ext PV NC` sets `caLinkStat = CA_LINKS_NOT_OK`, and arms
    /// `checkLinksCallback` after `recheck` when the record's C schedules one
    /// there and none is pending.
    pub(crate) fn link_check_classified(&self, record: &[u8], recheck: Option<Duration>) {
        let Some(rec) = self.get_record_no_resolve(record) else {
            return;
        };
        let arm = {
            let mut instance = rec.write();
            let Some(check) = instance.record.link_check() else {
                return;
            };
            if !check.statuses.contains(&LINK_EXT_NC) {
                return;
            }
            check.state.ca_links = true;
            let arm = recheck.is_some() && !check.state.scheduled;
            check.state.scheduled |= arm;
            arm
        };
        if let (true, Some(delay)) = (arm, recheck) {
            self.arm_link_check(record, delay);
        }
    }
}
