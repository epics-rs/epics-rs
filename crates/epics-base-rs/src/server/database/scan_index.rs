use crate::server::record::{PiniMode, RecordInstance, ScanType};

use super::PvDatabase;

impl PvDatabase {
    /// Update scan index when a record's SCAN or PHAS field changes.
    ///
    /// Takes `registration_mutex` so the read of
    /// the records map (to verify the record still exists) and the
    /// scan_index mutation are atomic vs. concurrent `remove_record`.
    ///
    /// The `new_scan` / `new_phas` parameters
    /// the caller passes are advisory only. After acquiring the
    /// mutex we read the LIVE record's current scan/phas and insert
    /// based on those. Pre-fix a put-then-update sequence could
    /// race a remove+re-add of the same name: the caller's
    /// `new_scan` reflected the old (now-removed) record's value;
    /// inserting that under the fresh record's name produced a
    /// stale scan-index entry pointing at a wrong scan rate. The
    /// live-read makes the index strictly reflect the record's
    /// current state at insert time.
    ///
    /// **Synchronous.** It had exactly two suspension points and neither was a
    /// real one: the `registration_mutex` acquisition (L46) and two
    /// `scan_index` write acquisitions (L8b) — both awaited before step 4.
    /// Both are blocking PI mutexes now, so the whole scan-index update is a
    /// bounded critical
    /// section — which is what lets it run *inside* the L1 record-gate window
    /// without putting an `.await` there. C reaches `scanAdd`/`scanDelete`
    /// (`dbScan.c:241-330`) the same way, from inside `dbPut` under
    /// `dbScanLock`. `doc/rtems-priority-locks-design.md` §5 step 4.
    pub fn update_scan_index(
        &self,
        name: &str,
        old_scan: ScanType,
        _new_scan: ScanType,
        old_phas: i16,
        _new_phas: i16,
    ) {
        let _gate = self.inner.registration_mutex.lock();
        let _ = old_phas; // entry matched by name; PHAS not needed.
        // 1) Remove the OLD entry the caller knew about — even if
        // remove_record already swept it. Match by record name so a
        // stale PHAS / load_order does not leave a phantom entry.
        if let Some(list) = old_scan.scan_list() {
            self.inner
                .scan_index
                .bucket(list)
                .lock()
                .retain(|(_, _, n)| n != name);
        }
        // 2) Look up the LIVE record under the mutex. If concurrent
        // remove+re-add replaced the Arc with a fresh one whose
        // scan differs from the caller's `_new_scan`, we re-insert
        // based on the fresh record's state. The fresh record's
        // own `add_record` call also registered its scan index, so
        // duplicate-insertion of the same (phas, name) pair into
        // the same scan bucket is a no-op (`BTreeSet::insert`
        // returns false on present key).
        let rec_arc = match self.inner.records.read().get(name).cloned() {
            Some(r) => r,
            None => return,
        };
        let (cur_scan, cur_phas) = {
            let inst = rec_arc.read();
            (inst.common.scan, inst.common.phas)
        };
        // C `scanAdd` refuses both `Passive` and an out-of-menu index — the
        // latter with "scanAdd detected illegal SCAN value" — so neither can
        // key a bucket. [`ScanList::of`] is that refusal.
        if let Some(list) = cur_scan.scan_list() {
            // Re-use the record's existing load-order sequence so the
            // scan-index secondary key stays stable across SCAN/PHAS
            // edits. A record loaded before should always scan before
            // a later-loaded record at the same PHAS.
            let seq = self.inner.load_order.load().get(name).copied().unwrap_or(0);
            self.inner
                .scan_index
                .bucket(list)
                .lock()
                .insert((cur_phas, seq, name.to_string()));
        }
    }

    /// Get record names for a given scan type, sorted by PHAS then
    /// database load order (C `dbScan.c` stable same-PHAS FIFO).
    ///
    /// A SCAN value that names no list (`Passive`, or an index outside
    /// `menuScan`) holds no records — there is no such bucket to look up.
    pub async fn records_for_scan(&self, scan_type: ScanType) -> Vec<String> {
        let Some(list) = scan_type.scan_list() else {
            return Vec::new();
        };
        // One bucket's lock, held for the clone only and released before the
        // caller's processing loop — C `scanList` (`dbScan.c:1007-1051`) holds
        // `psl->lock` for the cursor step alone and releases it around every
        // `dbProcess`. Neither holds its lock across processing.
        self.inner
            .scan_index
            .bucket(list)
            .lock()
            .iter()
            .map(|(_, _, name)| name.clone())
            .collect()
    }

    /// Get all record names whose `PINI` is **exactly** `mode`.
    ///
    /// C matches the menu index with `!=` (`iocInit.c:598`
    /// `if (precord->pini != pphase->pini) return;`), so each `menuPini`
    /// choice selects a disjoint set of records driven by a *different* pass:
    /// `YES` at `initialProcess()` (`iocInit.c:656`), `RUN`/`RUNNING`/`PAUSE`/
    /// `PAUSED` from `piniProcessHook` (`iocInit.c:629-646`). A `PINI=RUN`
    /// record must NOT be processed by the `YES` pass.
    ///
    /// Snapshot the records map under the outer
    /// read lock, then drop it before fanning out per-record reads.
    /// Pre-fix the outer `records.read()` lock was held across every
    /// `rec.read().await` — under contention with a pending
    /// `add_record` (which now takes the registration_mutex →
    /// records.write()), startup could stall while every PINI
    /// record was inspected serially.
    pub async fn pini_records(&self, mode: PiniMode) -> Vec<String> {
        let mut result = Vec::new();
        for (name, rec) in self.records_in_load_order().await {
            if rec.read().common.pini == mode.to_u16() as i16 {
                result.push(name);
            }
        }
        result
    }

    /// Every record, in database **load order** — the port's analogue of C's
    /// `iterateRecords`, which walks the record-type / record-instance lists in
    /// the order the `.db` declared them. That order is what makes two
    /// same-`PHAS` records process deterministically; `load_order` is already
    /// the `scan_index` secondary sort key for the same reason
    /// (`database/mod.rs:184`).
    ///
    /// Snapshots the map under the records read lock and releases it before the
    /// caller takes any per-record lock.
    async fn records_in_load_order(
        &self,
    ) -> Vec<(String, std::sync::Arc<parking_lot::RwLock<RecordInstance>>)> {
        let snapshot: Vec<_> = {
            let records = self.inner.records.read();
            records
                .iter()
                .map(|(n, r)| (n.clone(), r.clone()))
                .collect()
        };
        let mut keyed: Vec<_> = {
            let load_order = self.inner.load_order.load();
            snapshot
                .into_iter()
                .map(|(name, rec)| (load_order.get(&name).copied().unwrap_or(0), name, rec))
                .collect()
        };
        keyed.sort_unstable_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        keyed
            .into_iter()
            .map(|(_seq, name, rec)| (name, rec))
            .collect()
    }

    /// C `piniProcess` (`iocInit.c:608-627`) — process every record whose
    /// `PINI` is exactly `mode`, **in ascending `PHAS` order**, each with its
    /// full link chain.
    ///
    /// The single owner of "run a PINI pass": `initialProcess()` calls it with
    /// [`PiniMode::Yes`], and the `initHook` lifecycle calls it with
    /// [`PiniMode::Run`] / [`PiniMode::Running`]. Every driver goes through
    /// here, so neither the pass selection nor the phase ordering can diverge
    /// between them.
    ///
    /// The sweep is C's, not a pre-sorted list: each pass over the database
    /// processes the records at the current phase and, while doing so, finds
    /// the *next* lowest `PHAS` still ahead of it. C spells this out
    /// (`iocInit.c:614-619`) — "PHAS fields can be changed at runtime, so we
    /// have to look for the lowest value of PHAS each time" — so a record whose
    /// phase is raised by an earlier PINI record's processing is still picked
    /// up in the correct later pass, which a snapshot-and-sort would miss.
    pub async fn pini_process(&self, mode: PiniMode) {
        // C `dbScan.h:34-35`: MAX_PHASE = SHRT_MAX, MIN_PHASE = SHRT_MIN. The
        // phase cursors are `int` (`phaseData_t`), so `MAX_PHASE + 1` — the
        // "no further phase found" sentinel — does not overflow.
        const MIN_PHASE: i32 = i16::MIN as i32;
        const NO_NEXT_PHASE: i32 = i16::MAX as i32 + 1;

        let mut next = MIN_PHASE;
        loop {
            let this = next;
            next = NO_NEXT_PHASE;
            // `doRecordPini` (`iocInit.c:592-606`) over the record list. PINI
            // and PHAS are read at the moment the record is visited, not
            // snapshotted up front, so a PHAS that an earlier record's
            // processing changed is honoured — the reason C re-scans rather
            // than sorting once.
            for (name, rec) in self.records_in_load_order().await {
                let (pini, phas) = {
                    let instance = rec.read();
                    (instance.common.pini, i32::from(instance.common.phas))
                };
                if pini != mode.to_u16() as i16 {
                    continue;
                }
                if phas == this {
                    let mut visited = std::collections::HashSet::new();
                    let _ = self.process_record_with_links(&name, &mut visited, 0).await;
                } else if phas > this && phas < next {
                    next = phas;
                }
            }
            if next == NO_NEXT_PHASE {
                return;
            }
        }
    }

    /// Process all records with `SCAN=Event`, regardless of `EVNT`.
    ///
    /// Back-compat entry point for the iocsh `postEvent` command,
    /// whose handler currently drops the numeric event argument.
    /// Prefer [`Self::post_event_named`] for C-correct per-event
    /// routing — see `dbScan.c:548-552` `post_event` →
    /// `postEvent(pevent_list[event])`.
    pub async fn post_event(&self) {
        let names = self.records_for_scan(ScanType::Event).await;
        for name in &names {
            let mut visited = std::collections::HashSet::new();
            let _ = self.process_record_with_links(name, &mut visited, 0).await;
        }
    }

    /// Process only the `SCAN=Event` records whose `EVNT` resolves to
    /// `event_name`. Mirrors C `dbScan.c` event routing: each
    /// `event_list` (`eventNameToHandle`) holds exactly the records
    /// whose `EVNT` matches, and `postEvent` walks only that list.
    ///
    /// Event-name matching follows `eventNameToHandle` (`dbScan.c:469`):
    /// surrounding whitespace is trimmed, and a numeric string with an
    /// integer part in `[1,255]` is normalised to its integer form so
    /// `"5"`, `" 5 "` and `"5.0"` all name the same event.
    pub async fn post_event_named(&self, event_name: &str) {
        let want = normalize_event_name(event_name);
        if want.is_empty() {
            // `eventNameToHandle` returns NULL for "0"/empty — no event.
            return;
        }
        let names = self.records_for_scan(ScanType::Event).await;
        for name in &names {
            // Read the record's EVNT and compare against the posted
            // event name. Records that do not match are skipped — a
            // record configured `EVNT=5` only fires on event 5.
            let evnt = match self.get_record(name) {
                Some(rec) => rec.read().common.evnt.clone(),
                None => continue,
            };
            if normalize_event_name(&evnt) != want {
                continue;
            }
            let mut visited = std::collections::HashSet::new();
            let _ = self.process_record_with_links(name, &mut visited, 0).await;
        }
    }
}

/// Normalise an EPICS event name for routing comparison.
///
/// Mirrors `dbScan.c::eventNameToHandle` (`dbScan.c:469-533`):
/// * leading/trailing whitespace is stripped;
/// * a string that parses as a number with an integer part in
///   `[1,255]` is canonicalised to that integer's decimal form
///   (so numeric events from calc records match symbolic "5");
/// * `"0"` (and anything that resolves to event 0) becomes empty —
///   C's `eventNameToHandle` returns NULL for event 0.
pub(crate) fn normalize_event_name(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Ok(num) = trimmed.parse::<f64>() {
        if num >= 0.0 && num < 256.0 {
            let int = num as i64;
            if int < 1 {
                // event 0 → no event
                return String::new();
            }
            return int.to_string();
        }
        // Numeric but outside [0,256): fall through to literal match.
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::normalize_event_name;

    #[test]
    fn event_name_numeric_normalisation() {
        // Whitespace trimmed, numeric forms canonicalised.
        assert_eq!(normalize_event_name(" 5 "), "5");
        assert_eq!(normalize_event_name("5.0"), "5");
        assert_eq!(normalize_event_name("5"), "5");
        // Event 0 / empty → no event.
        assert_eq!(normalize_event_name("0"), "");
        assert_eq!(normalize_event_name(""), "");
        assert_eq!(normalize_event_name("   "), "");
        // Symbolic name preserved.
        assert_eq!(normalize_event_name("myEvent"), "myEvent");
        assert_eq!(normalize_event_name(" myEvent "), "myEvent");
        // Numeric out of [0,256) is treated literally.
        assert_eq!(normalize_event_name("999"), "999");
    }
}
