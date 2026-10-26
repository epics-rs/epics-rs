//! C `checkLinks` and `checkLinksCallback`: the re-read that moves a link's
//! status between `Ext PV NC` and `Ext PV OK` as the link connects and drops
//! (`calcoutRecord.c:711-773`, and the synApps copies).

use std::time::Duration;

use super::PvDatabase;
use crate::server::record::{ParsedLink, RecordInstance};
use crate::server::records::link_status::{
    DBF_UNKNOWN, LINK_CON, LINK_EXT_NC, LINK_EXT_OK, LINK_LOC, LINK_RECHECK, dbf_static_code,
    link_is_external,
};

impl PvDatabase {
    /// C `checkLinks` on the record `instance` holds, by each link's type
    /// after `dbInitLink`: every CA link its rule visits takes its
    /// `dbIsLinkConnected` (and its `dbGetLinkDBFtype`, for a record keeping
    /// one), which moves an `Ext PV` status, and a status that moved posts —
    /// every DB link's `Local PV` too under `repost_local`. Returns whether it
    /// scheduled `checkLinksCallback` — a visited CA link is down and none was
    /// pending.
    pub(crate) fn check_links(&self, instance: &mut RecordInstance) -> bool {
        let Some(check) = instance.record.link_check() else {
            return false;
        };
        let spec = check.spec;
        let typed = !check.field_types.is_empty();
        let external = check
            .statuses
            .iter()
            .enumerate()
            .filter(|(_, s)| link_is_external(**s))
            .fold(0u64, |ext, (i, _)| ext | 1 << i);
        let (mut db_links, mut visited, mut connected) = (0u64, 0u64, 0u64);
        let mut types = Vec::new();
        for (i, &(link, _)) in spec.links.iter().enumerate() {
            let Some(parsed) = instance.stored_link(link) else {
                continue;
            };
            if matches!(parsed, ParsedLink::Db(_)) {
                db_links |= 1 << i;
            }
            // sseq reads every named link's `dbGetLinkDBFtype`
            // (sseqRecord.c:884, :941), so a CA link `special()` called
            // `Local PV` too; the calcs ask only an `Ext PV` one.
            if !spec.rule.visits(parsed) || !typed && external & 1 << i == 0 {
                continue;
            }
            visited |= 1 << i;
            if self.link_connected(parsed) {
                connected |= 1 << i;
            }
            if typed {
                let ft = self.external_link_dbf_type(parsed);
                types.push((i, ft.map_or(DBF_UNKNOWN, dbf_static_code)));
            }
        }
        let Some(mut check) = instance.record.link_check() else {
            return false;
        };
        let local = if spec.repost_local { db_links } else { 0 };
        let mut moved = 0u64;
        for (i, status) in check.statuses.iter_mut().enumerate() {
            let now = if local & 1 << i != 0 {
                LINK_LOC
            } else if visited & external & 1 << i == 0 {
                continue;
            } else if connected & 1 << i != 0 {
                LINK_EXT_OK
            } else {
                LINK_EXT_NC
            };
            if *status != now {
                *status = now;
                moved |= 1 << i;
            }
        }
        // C sets every link's type `DBF_unknown` before asking
        // `dbGetLinkDBFtype` of the named ones (sseqRecord.c:881, :940); a
        // local link's cannot have moved.
        for (status, ft) in check.statuses.iter().zip(check.field_types.iter_mut()) {
            if *status == LINK_CON {
                *ft = DBF_UNKNOWN;
            }
        }
        for (i, ft) in types {
            check.field_types[i] = ft;
        }
        check.state.ca_links = visited != 0;
        let arm = visited & !connected != 0 && !check.state.scheduled;
        check.state.scheduled |= arm;
        let tail = check
            .tail
            .as_deref_mut()
            .map(|tail| (tail.pass(db_links), tail.fields()));
        for (i, &(_, status)) in spec.links.iter().enumerate() {
            if (moved | local) & 1 << i != 0 {
                instance.notify_field(status, spec.mask);
            }
        }
        if let Some((moved, fields)) = tail {
            for (i, field) in fields.iter().enumerate() {
                if moved & 1 << i != 0 {
                    instance.notify_field(field, spec.mask);
                }
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

    /// sseq's `if (!pending_checkLinksCB) callbackRequestDelayed(..., 0.5)`
    /// (`sseqRecord.c:1018`, `:1073`, `:1092`): arm a pass after `delay`
    /// unless one is pending, whatever the statuses read.
    pub(crate) fn link_check_after(&self, record: &[u8], delay: Duration) {
        let Some(rec) = self.get_record_no_resolve(record) else {
            return;
        };
        let arm = {
            let mut instance = rec.write();
            let Some(check) = instance.record.link_check() else {
                return;
            };
            let arm = !check.state.scheduled;
            check.state.scheduled = true;
            arm
        };
        if arm {
            self.arm_link_check(record, delay);
        }
    }

    /// C `checkLink(prec)` from inside `special()` on the record `instance`
    /// holds (throttle's SYNC arm, throttleRecord.c:376-389).
    pub(crate) fn link_check_held(&self, record: &[u8], instance: &mut RecordInstance) {
        if self.check_links(instance) {
            self.arm_link_check(record, LINK_RECHECK);
        }
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
