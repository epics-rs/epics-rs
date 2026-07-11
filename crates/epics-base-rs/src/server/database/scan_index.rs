use crate::server::record::ScanType;

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
    pub async fn update_scan_index(
        &self,
        name: &str,
        old_scan: ScanType,
        _new_scan: ScanType,
        old_phas: i16,
        _new_phas: i16,
    ) {
        let _gate = self.inner.registration_mutex.lock().await;
        let _ = old_phas; // entry matched by name; PHAS not needed.
        // 1) Remove the OLD entry the caller knew about — even if
        // remove_record already swept it. Match by record name so a
        // stale PHAS / load_order does not leave a phantom entry.
        {
            let mut index = self.inner.scan_index.write().await;
            if old_scan != ScanType::Passive {
                if let Some(set) = index.get_mut(&old_scan) {
                    set.retain(|(_, _, n)| n != name);
                    if set.is_empty() {
                        index.remove(&old_scan);
                    }
                }
            }
        }
        // 2) Look up the LIVE record under the mutex. If concurrent
        // remove+re-add replaced the Arc with a fresh one whose
        // scan differs from the caller's `_new_scan`, we re-insert
        // based on the fresh record's state. The fresh record's
        // own `add_record` call also registered its scan index, so
        // duplicate-insertion of the same (phas, name) pair into
        // the same scan bucket is a no-op (`BTreeSet::insert`
        // returns false on present key).
        let rec_arc = match self.inner.records.read().await.get(name).cloned() {
            Some(r) => r,
            None => return,
        };
        let (cur_scan, cur_phas) = {
            let inst = rec_arc.read().await;
            (inst.common.scan, inst.common.phas)
        };
        if cur_scan != ScanType::Passive {
            // Re-use the record's existing load-order sequence so the
            // scan-index secondary key stays stable across SCAN/PHAS
            // edits. A record loaded before should always scan before
            // a later-loaded record at the same PHAS.
            let seq = self
                .inner
                .load_order
                .read()
                .await
                .get(name)
                .copied()
                .unwrap_or(0);
            self.inner
                .scan_index
                .write()
                .await
                .entry(cur_scan)
                .or_default()
                .insert((cur_phas, seq, name.to_string()));
        }
    }

    /// Get record names for a given scan type, sorted by PHAS then
    /// database load order (C `dbScan.c` stable same-PHAS FIFO).
    pub async fn records_for_scan(&self, scan_type: ScanType) -> Vec<String> {
        self.inner
            .scan_index
            .read()
            .await
            .get(&scan_type)
            .map(|s| s.iter().map(|(_, _, name)| name.clone()).collect())
            .unwrap_or_default()
    }

    /// Get all record names whose `PINI` selects the `initialProcess()` pass
    /// (`menuPiniYES` — C `iocInit.c:656` `piniProcess(menuPiniYES)`).
    ///
    /// C matches the menu index exactly (`iocInit.c:598`
    /// `if (precord->pini != pphase->pini) return;`), so `RUN`/`RUNNING`/
    /// `PAUSE`/`PAUSED` records are NOT in this pass — they belong to their
    /// own lifecycle hook.
    ///
    /// Snapshot the records map under the outer
    /// read lock, then drop it before fanning out per-record reads.
    /// Pre-fix the outer `records.read()` lock was held across every
    /// `rec.read().await` — under contention with a pending
    /// `add_record` (which now takes the registration_mutex →
    /// records.write()), startup could stall while every PINI
    /// record was inspected serially.
    pub async fn pini_records(&self) -> Vec<String> {
        let snapshot: Vec<_> = {
            let records = self.inner.records.read().await;
            records
                .iter()
                .map(|(n, r)| (n.clone(), r.clone()))
                .collect()
        };
        let mut result = Vec::new();
        for (name, rec) in snapshot {
            let instance = rec.read().await;
            if instance.common.pini == crate::server::record::PiniMode::Yes {
                result.push(name);
            }
        }
        result
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
            let evnt = match self.get_record(name).await {
                Some(rec) => rec.read().await.common.evnt.clone(),
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
