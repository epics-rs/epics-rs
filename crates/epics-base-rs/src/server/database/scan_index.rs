use std::collections::BTreeSet;
use std::sync::Arc;

use crate::server::record::{PiniMode, RecordCell, ScanList, ScanType};

use super::PvDatabase;

/// The scan buckets themselves. `bucket` is private to THIS module, so no
/// other file in `database` can open a bucket and write it — the only
/// transitions are [`PvDatabase::add_to_scan_list`] and
/// [`PvDatabase::delete_from_scan_list`], which is C's arrangement:
/// `addToList`/`deleteFromList` are `static` in `dbScan.c` and `scanAdd` /
/// `scanDelete` (`dbScan.c:240`/`:308` at R7.0.10) are the whole public
/// surface.
/// One scan list's records, and the revision that says whether they are still
/// the ones a cursor last saw.
///
/// The two live together because the revision is only meaningful as a
/// statement about these keys: [`ScanCursor`] walks the list as it stood at a
/// revision, and the only thing that can invalidate that is a record entering,
/// leaving, or being re-keyed within THIS bucket. Every such transition goes
/// through [`ScanBucket::transition`], which moves the revision itself, so a
/// bucket cannot change without saying so.
///
/// The revision sits OUTSIDE the mutex because that is what lets an ordinary
/// sweep step skip the mutex entirely: while the bucket still reads the value
/// the cursor holds, the cursor's copy of the list is the list. It is written
/// only under the mutex and published `Release`, so a cursor that sees a new
/// value and then takes the mutex sees the keys that produced it.
struct ScanBucket {
    revision: std::sync::atomic::AtomicU64,
    entries: crate::runtime::sync::PriorityInheritanceMutex<Bucket>,
}

/// The keys themselves, and the ordered form a cursor walks.
struct Bucket {
    keys: BTreeSet<super::ScanKey>,
    /// `keys` in order, materialised on the first ask after a transition and
    /// shared with every ask until the next one. Written only by
    /// [`ScanBucket::transition`] and [`ScanBucket::snapshot`].
    ordered: Option<Arc<[super::ScanKey]>>,
}

impl ScanBucket {
    fn new() -> Self {
        Self {
            revision: std::sync::atomic::AtomicU64::new(0),
            entries: crate::runtime::sync::PriorityInheritanceMutex::new(Bucket {
                keys: BTreeSet::new(),
                ordered: None,
            }),
        }
    }

    /// The ONE way this bucket's keys change — C keeps `addToList` and
    /// `deleteFromList` `static` in `dbScan.c` for the same reason.
    ///
    /// Moving the revision and dropping the now-stale ordered form are this
    /// method's own writes rather than the caller's, so neither can be
    /// forgotten and the bucket cannot announce a change while still handing
    /// out the keys from before it. The revision moves on every call whether
    /// or not the closure changed anything: an insert of an equal key is the
    /// re-key case, and a cursor standing on the old key has to be told.
    fn transition(&self, f: impl FnOnce(&mut BTreeSet<super::ScanKey>)) {
        let mut entries = self.entries.lock();
        f(&mut entries.keys);
        entries.ordered = None;
        self.revision
            .fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    /// The keys in order, and the revision they are the keys at.
    fn snapshot(&self) -> (u64, Arc<[super::ScanKey]>) {
        let mut entries = self.entries.lock();
        if entries.ordered.is_none() {
            let ordered: Arc<[super::ScanKey]> = entries.keys.iter().cloned().collect();
            entries.ordered = Some(ordered);
        }
        (
            // Read under the mutex, beside the keys it describes. A
            // transition landing after this returns is one the cursor meets
            // on its next step, which is where C meets it too.
            self.revision.load(std::sync::atomic::Ordering::Relaxed),
            entries.ordered.clone().expect("just materialised"),
        )
    }
}

pub(super) struct ScanIndex {
    /// One bucket per scan list. Sized from the LOADED `menuScan`
    /// ([`ScanList::count`]), not from a compile-time rate list, and built on
    /// first use rather than at construction: the site's `menuScan.dbd` is
    /// loaded after the database object exists, so sizing this eagerly would
    /// freeze the menu before the loader could install one. C reaches the same
    /// point by ordering — `dbLoadDatabase`, then `iocInit` → `initPeriodic`
    /// sizes `papPeriodic`.
    buckets: std::sync::OnceLock<Box<[ScanBucket]>>,
    /// Cumulative over-runs per list — C `periodic_scan_list::overruns`
    /// (`dbScan.c:95`), which `scanppl` prints beside the list it belongs to
    /// (`dbScan.c:408-409`). It lives here for the same reason C puts it on
    /// `periodic_scan_list`: one owner per rate holds both the list and its
    /// over-run count, so the counter cannot drift away from the list it
    /// counts. Only the periodic scan threads write it.
    overruns: std::sync::OnceLock<Box<[std::sync::atomic::AtomicU64]>>,
}

impl ScanIndex {
    pub(super) fn new() -> Self {
        Self {
            buckets: std::sync::OnceLock::new(),
            overruns: std::sync::OnceLock::new(),
        }
    }

    /// The bucket holding `list`'s records. Total — see [`ScanList::slot`].
    fn bucket(&self, list: ScanList) -> &ScanBucket {
        &self
            .buckets
            .get_or_init(|| (0..ScanList::count()).map(|_| ScanBucket::new()).collect())
            .as_ref()[list.slot()]
    }

    /// This list's over-run counter — the same lazy sizing as [`Self::bucket`].
    fn overrun(&self, list: ScanList) -> &std::sync::atomic::AtomicU64 {
        &self
            .overruns
            .get_or_init(|| {
                (0..ScanList::count())
                    .map(|_| std::sync::atomic::AtomicU64::new(0))
                    .collect()
            })
            .as_ref()[list.slot()]
    }
}

impl PvDatabase {
    /// C `addToList` (`dbScan.c:1074` at R7.0.10) — the ONE place a record
    /// enters a scan bucket.
    ///
    /// C keeps it `static` so that `scanAdd` is the only way in; the port's
    /// equivalent is [`ScanBucket::transition`], which this and
    /// [`Self::delete_from_scan_list`] are the only callers of. It takes the
    /// bucket lock itself, exactly as C takes `psl->lock`, and takes no
    /// registration lock: L46 belongs to
    /// whichever owner called — [`Self::update_scan_index`], which acquires it,
    /// or `add_record`, which is already inside it. Both used to open-code the
    /// bucket write instead, which is how a transition could bypass its owner.
    ///
    /// A SCAN that names no list (`Passive`, or an index outside `menuScan`)
    /// keys no bucket — C `scanAdd` refuses the same two, the latter with
    /// "scanAdd detected illegal SCAN value".
    pub(super) fn add_to_scan_list(
        &self,
        scan: ScanType,
        phas: i16,
        record_type: &str,
        load_order: u64,
        name: &str,
    ) {
        let Some(list) = scan.scan_list() else {
            return;
        };
        // Resolved once here, where a record enters the list, instead of once
        // per record per sweep inside the process frame.
        let handle = self
            .get_record_no_resolve(name)
            .map(|rec| std::sync::Arc::downgrade(&rec))
            .unwrap_or_default();
        self.inner.scan_index.bucket(list).transition(|keys| {
            keys.insert(super::ScanKey::new(
                phas,
                record_type,
                load_order,
                name,
                handle,
            ));
        });
    }

    /// C `deleteFromList` (`dbScan.c:1096` at R7.0.10) — the ONE place a
    /// record leaves a scan bucket. Same ownership rules as
    /// [`Self::add_to_scan_list`].
    ///
    /// Matches by record name alone: PHAS and load-order may be stale relative
    /// to the entry actually present, and a stale secondary key would leave a
    /// phantom entry behind.
    pub(super) fn delete_from_scan_list(&self, scan: ScanType, name: &str) {
        let Some(list) = scan.scan_list() else {
            return;
        };
        self.inner
            .scan_index
            .bucket(list)
            .transition(|keys| keys.retain(|k| k.name.as_ref() != name));
    }

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
    /// `dbScanLock`.
    pub fn update_scan_index(
        &self,
        name: &str,
        old_scan: ScanType,
        _new_scan: ScanType,
        old_phas: i16,
        _new_phas: i16,
    ) {
        let _ = old_phas; // entry matched by name; PHAS not needed.
        // The LIVE record's SCAN/PHAS are read under L46 so that the map
        // check and the index mutation are one transaction against a
        // concurrent `remove_record`. Record data is behind the record's
        // lock set, which sits ABOVE L46, so the set is taken first — a
        // re-entry for every caller, which holds it already (C reaches
        // `scanAdd` from `dbPut` under `dbScanLock`) — and the map is
        // re-read under the gate to prove the handle locked is the handle
        // registered. A remove+re-add between the two reads is retried
        // against the fresh record.
        let (rec_arc, _record_gate, _gate) = loop {
            let rec_arc = self.inner.records.read().get(name).cloned();
            let record_gate = rec_arc.as_ref().map(|rec| self.lock_instance(rec));
            let gate = self.lock_registration("update_scan_index");
            let live = self.inner.records.read().get(name).cloned();
            match (&rec_arc, &live) {
                (Some(locked), Some(live)) if Arc::ptr_eq(locked, live) => {}
                (None, None) => {}
                _ => continue,
            }
            break (rec_arc, record_gate, gate);
        };
        // 1) Remove the OLD entry the caller knew about — even if
        // remove_record already swept it.
        self.delete_from_scan_list(old_scan, name);
        // 2) Re-insert from the LIVE record's state. If concurrent
        // remove+re-add replaced the Arc with a fresh one whose
        // scan differs from the caller's `_new_scan`, we re-insert
        // based on the fresh record's state. The fresh record's
        // own `add_record` call also registered its scan index, so
        // duplicate-insertion of the same (phas, name) pair into
        // the same scan bucket is a no-op (`BTreeSet::insert`
        // returns false on present key).
        let Some(rec_arc) = rec_arc else {
            return;
        };
        let (cur_scan, cur_phas, cur_type) = {
            let inst = rec_arc.read();
            (
                inst.common.scan,
                inst.common.phas,
                inst.record.record_type(),
            )
        };
        // Re-use the record's existing load-order sequence so the scan-index
        // secondary key stays stable across SCAN/PHAS edits. A record loaded
        // before should always scan before a later-loaded record at the same
        // PHAS.
        let seq = self.inner.load_order.load().get(name).copied().unwrap_or(0);
        self.add_to_scan_list(cur_scan, cur_phas, cur_type, seq, name);
    }

    /// Count one over-run for `scan`'s list — C `ppsl->overruns++`
    /// (`dbScan.c:827`). The periodic scan thread for that rate is the only
    /// caller; a SCAN value naming no list has no counter to move.
    pub(crate) fn record_scan_overrun(&self, scan: ScanType) {
        if let Some(list) = scan.scan_list() {
            self.inner
                .scan_index
                .overrun(list)
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// How many times `list`'s sweep has run past its deadline since boot —
    /// what C's `scanppl` prints as `(%lu over-runs)` (`dbScan.c:408-409`).
    pub(crate) fn scan_overruns(&self, list: crate::server::record::ScanList) -> u64 {
        self.inner
            .scan_index
            .overrun(list)
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// A **snapshot** of a scan list, in C's order: PHAS, then DBD
    /// record-type order, then `.db` load order — see `ScanKey`. The stable
    /// same-PHAS FIFO is `addToList`; the order it is a FIFO over is
    /// `buildScanLists`, and both are in the key.
    ///
    /// For reporting and counting only (`scanppl`, `dbstat`). A sweep that
    /// PROCESSES the list must not use this: `dbProcess` can change the SCAN
    /// field of an arbitrary number of records, so a snapshot processes
    /// records the list no longer holds. `Self::scan_list_once` is the sweep.
    ///
    /// A SCAN value that names no list (`Passive`, or an index outside
    /// `menuScan`) holds no records — there is no such bucket to look up.
    pub async fn records_for_scan(&self, scan_type: ScanType) -> Vec<String> {
        let Some(list) = scan_type.scan_list() else {
            return Vec::new();
        };
        // One bucket's lock, held for the clone alone. C `scanList`
        // (`dbScan.c:1007-1051`) also releases `psl->lock` around every
        // `dbProcess` — but it releases it holding a cursor INTO the list and
        // re-reads `ellNext` under the lock on the next step, so the two are
        // not the same construction and this must not be read as parity with
        // it. Detaching from the list is only sound because nothing here
        // processes; [`Self::scan_list_once`] is what C's `scanList` is.
        self.inner
            .scan_index
            .bucket(list)
            .snapshot()
            .1
            .iter()
            .map(|k| k.name.to_string())
            .collect()
    }

    /// The scan key `name` would be inserted under RIGHT NOW — its live PHAS,
    /// record type and load-order sequence. `None` if the record is gone.
    fn live_scan_key(&self, name: &str) -> Option<super::ScanKey> {
        let rec = self.get_record_no_resolve(name)?;
        let (phas, record_type) = {
            let inst = rec.read();
            (inst.common.phas, inst.record.record_type())
        };
        let seq = self.inner.load_order.load().get(name).copied().unwrap_or(0);
        Some(super::ScanKey::new(
            phas,
            record_type,
            seq,
            name,
            std::sync::Arc::downgrade(&rec),
        ))
    }

    /// A cursor over `list` as it stands at each step — see [`ScanCursor`].
    pub(crate) fn scan_cursor(&self, list: ScanList) -> ScanCursor {
        ScanCursor {
            list,
            snapshot: None,
            next: 0,
            revision: 0,
        }
    }

    /// Sweep one scan list, processing every record still in it — C `scanList`
    /// (`dbScan.c:998-1051`), the single owner of "walk a scan list and
    /// process it". Every driver goes through here: the periodic threads, and
    /// the event lists, whose `eventCallback` (`dbScan.c:459-465`) is a bare
    /// `scanList` call.
    pub(crate) async fn scan_list_once(&self, list: ScanList) {
        let mut cursor = self.scan_cursor(list);
        // One set for the whole sweep. `run_process_frame` owns the unwind and
        // takes its own marker back out on every exit, so the set is empty
        // again when a record's cascade returns — reusing it is the same set a
        // fresh `HashSet::new()` would be, minus the table allocation each
        // record was paying for its first insert.
        let mut visited = crate::server::database::ProcStack::new();
        while let Some((name, rec)) = cursor.next(self) {
            let _ = match rec {
                Some(rec) => self.process_record_with_links_resolved(name, rec, &mut visited),
                None => self.process_record_with_links_sync(name, &mut visited),
            };
            debug_assert!(
                visited.is_empty(),
                "a returned process frame left its cycle marker behind"
            );
        }
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
    /// same-`PHAS` records process deterministically; `load_order` is one of
    /// the `scan_index` sort keys for the same reason (see `ScanKey`, which
    /// also carries the record-type ordinal C's `iterateRecords` walks first —
    /// this PINI sweep does not, and that is a separate gap).
    ///
    /// Snapshots the map under the records read lock and releases it before the
    /// caller takes any per-record lock.
    async fn records_in_load_order(&self) -> Vec<(String, std::sync::Arc<RecordCell>)> {
        let snapshot: Vec<_> = {
            let records = self.inner.records.read();
            records
                .iter()
                .map(|(n, r)| (n.to_string(), r.clone()))
                .collect()
        };
        let mut keyed: Vec<_> = {
            let load_order = self.inner.load_order.load();
            snapshot
                .into_iter()
                .map(|(name, rec)| {
                    (
                        load_order.get(name.as_str()).copied().unwrap_or(0),
                        name,
                        rec,
                    )
                })
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
            // See `scan_list_once`: one set per sweep, emptied by each frame's
            // own unwind.
            let mut visited = crate::server::database::ProcStack::new();
            for (name, rec) in self.records_in_load_order().await {
                let (pini, phas) = {
                    let instance = rec.read();
                    (instance.common.pini, i32::from(instance.common.phas))
                };
                if pini != mode.to_u16() as i16 {
                    continue;
                }
                if phas == this {
                    let _ =
                        self.process_record_with_links_resolved(&name, rec.clone(), &mut visited);
                    debug_assert!(
                        visited.is_empty(),
                        "a returned process frame left its cycle marker behind"
                    );
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
        // C `postEvent` (`dbScan.c:536-539`): an event posted while the
        // facility is not running queues nothing at all.
        if !crate::server::scan::scan_is_running() {
            return;
        }
        if let Some(list) = ScanType::Event.scan_list() {
            self.scan_list_once(list).await;
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
        // C `postEvent` (`dbScan.c:536-539`), reached through
        // `post_event`/`eventNameToHandle` — the same gate, applied before
        // the name lookup because C applies it before touching the list.
        if !crate::server::scan::scan_is_running() {
            return;
        }
        let want = normalize_event_name(event_name);
        if want.is_empty() {
            // `eventNameToHandle` returns NULL for "0"/empty — no event.
            return;
        }
        let Some(list) = ScanType::Event.scan_list() else {
            return;
        };
        // Same live cursor as every other sweep: C keeps one `scan_list` per
        // (event, priority) and `eventCallback` hands it to `scanList`
        // (`dbScan.c:459-465`), so a record whose SCAN changes mid-sweep leaves
        // the walk here exactly as it does on a periodic list. The port keeps
        // one Event list and filters by EVNT at the cursor instead.
        let mut cursor = self.scan_cursor(list);
        // See `scan_list_once`: one set per sweep, emptied by each frame's own
        // unwind.
        let mut visited = crate::server::database::ProcStack::new();
        while let Some((name, rec)) = cursor.next(self) {
            // Read the record's EVNT and compare against the posted
            // event name. Records that do not match are skipped — a
            // record configured `EVNT=5` only fires on event 5.
            let Some(rec) = rec.or_else(|| self.get_record(name)) else {
                continue;
            };
            let evnt = rec.read().common.evnt.clone();
            if normalize_event_name(&evnt) != want {
                continue;
            }
            let _ = self.process_record_with_links_resolved(name, rec, &mut visited);
            debug_assert!(
                visited.is_empty(),
                "a returned process frame left its cycle marker behind"
            );
        }
    }
}

/// A cursor over a LIVE scan list — C `scanList`'s `pse` (`dbScan.c:998-1051`).
///
/// The invariant: **the sweep observes the list, it does not own a copy of it.**
/// C re-reads `ellNext(&pse->node)` under `psl->lock` on every step and carries
/// a cursor-repair walk that exists precisely because `dbProcess` can change
/// the SCAN field of an arbitrary number of records mid-sweep. A snapshot taken
/// once at the top of the tick processes records the list no longer holds.
///
/// The cursor does hold the list in a `snapshot`, and that does not weaken the
/// invariant, because the bucket's revision is what the snapshot is read
/// through: the revision moves on every transition ([`ScanBucket::transition`]),
/// so while it still reads what the cursor holds, no record has entered, left
/// or been re-keyed and the snapshot IS the live list. The moment it differs
/// the snapshot is discarded and the place re-found. What that buys is the
/// step: an index, against a bucket lock, an ordered-set lookup and a key
/// clone per record per sweep to learn that a list nothing had touched still
/// held what it held.
///
/// The port's list is an ordered set, not a linked list, so "my element left
/// the list" has one answer instead of C's three: the next key strictly greater
/// than where the cursor stood. That subsumes C's prev/next repair
/// (`dbScan.c:1030-1044`) — and, because the position is a key rather than a
/// pointer, it has no counterpart to C's "too many changes, wait till the next
/// period" (`:1045-1048`), which is an artefact of losing the place rather than
/// a scanning rule.
///
/// The step re-reads the last record's CURRENT key before advancing, so a
/// record that moved within this list (its own processing changed its PHAS) is
/// advanced from where it is now — C's `pse->pscan_list == psl` branch
/// (`:1023-1029`).
pub(crate) struct ScanCursor {
    list: ScanList,
    /// The list as it stood at [`Self::revision`]; `None` before the first
    /// step, which is the one state in which there is no place to keep.
    snapshot: Option<Arc<[super::ScanKey]>>,
    /// Index into `snapshot` of the entry the next step hands out. The entry
    /// before it is where the cursor stands.
    next: usize,
    /// The bucket revision `snapshot` was taken at.
    revision: u64,
}

impl ScanCursor {
    /// Where the cursor stands: the entry the last step handed out.
    fn standing(&self) -> Option<&super::ScanKey> {
        self.snapshot.as_ref()?.get(self.next.checked_sub(1)?)
    }

    /// Take the list again and re-find the place in it — C's cursor repair
    /// (`dbScan.c:1023-1044`), now paid once per transition rather than once
    /// per record.
    ///
    /// The record last handed out may have moved WITHIN this list, its own
    /// processing having changed its PHAS, so its key is rebuilt from live
    /// state before the resume point is looked up. `live_scan_key` reads the
    /// records map and the record itself, so no bucket lock is held across it.
    fn resync(&mut self, db: &PvDatabase) {
        let was = self.standing().cloned();
        let live = was.as_ref().and_then(|w| db.live_scan_key(&w.name));
        let (revision, snapshot) = db.inner.scan_index.bucket(self.list).snapshot();
        let resume = match live {
            // The rebuilt key is the resume point only if THIS list is where
            // the record landed; one whose SCAN moved it to another list
            // resumes the sweep from where it stood.
            Some(k) if snapshot.binary_search(&k).is_ok() => Some(k),
            _ => was,
        };
        // The next key strictly greater than the resume point — C's
        // `ellNext`, and the same answer for a record that left the list as
        // for one that is still in it.
        self.next = resume.map_or(0, |r| snapshot.partition_point(|k| *k <= r));
        self.revision = revision;
        self.snapshot = Some(snapshot);
    }

    /// The next record still in the list, or `None` at its end: the name, and
    /// the instance the list holds beside it (`None` only for a key whose
    /// record has since been dropped).
    ///
    /// Takes no lock while the list is unchanged, and the bucket lock for the
    /// re-find alone when it is — never across processing, as C releases
    /// `psl->lock` around every `dbProcess`.
    #[allow(clippy::type_complexity)]
    pub(crate) fn next(&mut self, db: &PvDatabase) -> Option<(&str, Option<Arc<RecordCell>>)> {
        let bucket = db.inner.scan_index.bucket(self.list);
        if self.snapshot.is_none()
            || bucket.revision.load(std::sync::atomic::Ordering::Acquire) != self.revision
        {
            self.resync(db);
        }
        let key = self.snapshot.as_ref()?.get(self.next)?;
        self.next += 1;
        Some((&key.name, key.handle.upgrade()))
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
    use super::PvDatabase;
    use super::normalize_event_name;
    use crate::server::record::ScanType;

    /// The cycle guard belongs to the frame that inserted it, and a frame
    /// that does not run must not insert one. The set is ONE per sweep
    /// (`scan_list_once`), so a marker left behind by a name that is no
    /// longer in the database reads as "already on this stack" for every
    /// later entry in that sweep — silencing every forward link onto it.
    #[test]
    fn a_frame_that_finds_no_record_leaves_no_cycle_marker() {
        let db = PvDatabase::new();
        let mut visited = crate::server::database::ProcStack::new();

        let result = db.process_record_with_links_sync("NO:SUCH:RECORD", &mut visited);

        assert!(
            result.is_err(),
            "a name the database does not hold is an error"
        );
        assert!(
            visited.is_empty(),
            "the frame left its cycle marker behind: {visited:?}"
        );
    }

    /// The ordering rule, stated as the thing that must hold rather than as
    /// the record shape that once broke it: **`update_scan_index` takes L46
    /// itself, so no caller may hold L46 when calling it.**
    ///
    /// A caller that breaks it used to park on itself forever, because
    /// `PriorityInheritanceMutex` is not reentrant. The failure reached CI as
    /// a 120-second timeout on whatever test happened to register a record —
    /// a shape that reads as a load flake and hides which caller is at fault.
    /// This pins the replacement: the violating call panics, immediately, and
    /// names both ends.
    ///
    /// Deliberately expressed with a bare `lock_registration` + direct
    /// `update_scan_index` pair and no record, no SIML and no SCAN field: the
    /// rule belongs to the caller/owner contract, not to the one composition
    /// (`add_loaded_record` → `rec_gbl_init_simm` → `apply_simm_scan_swap`)
    /// that first exposed it. Any future tail added under a registration gate
    /// trips this the same way.
    #[test]
    fn a_caller_holding_l46_cannot_reach_the_scan_index_owner() {
        let db = PvDatabase::new();
        let held = db.lock_registration("a_test_standing_in_for_a_registration_entry_point");

        let violation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            db.update_scan_index("ANY", ScanType::Passive, ScanType::SEC01, 0, 0);
        }));

        let payload = violation
            .expect_err("holding L46 across update_scan_index must panic, not park the thread");
        let msg = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or("");
        assert!(
            msg.contains("not reentrant") && msg.contains("update_scan_index"),
            "the panic must name the rule and the violating site, got: {msg}"
        );
        drop(held);
    }

    /// The other side of the same boundary — with no gate held, the owner
    /// takes L46 itself and completes. Without this case the test above would
    /// still pass if `update_scan_index` panicked unconditionally.
    #[test]
    fn the_scan_index_owner_takes_l46_itself_when_no_caller_holds_it() {
        let db = PvDatabase::new();
        db.update_scan_index("ANY", ScanType::Passive, ScanType::SEC01, 0, 0);
    }

    /// The gate is released on drop, including by unwinding out of a panic
    /// between acquisitions. A leaked flag would make every later
    /// registration on this thread panic and turn the tripwire into its own
    /// outage.
    #[test]
    fn the_registration_gate_clears_on_drop_and_on_unwind() {
        let db = PvDatabase::new();
        drop(db.lock_registration("first"));
        let _second = db.lock_registration("second");
        drop(_second);

        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = db.lock_registration("panics_while_held");
            panic!("unwind with the gate live");
        }));
        drop(db.lock_registration("after_unwind"));
    }

    /// A cursor skips its repair walk for as long as the bucket revision it
    /// carries still matches, so the revision has to move on every transition
    /// that can invalidate a key it holds — including an insert of a key the
    /// bucket already has, which is how a re-key that keeps the same PHAS
    /// arrives.
    #[test]
    fn every_scan_bucket_transition_moves_the_revision() {
        let db = PvDatabase::new();
        let list = ScanType::SEC01.scan_list().expect("1 second names a list");
        let revision = || {
            db.inner
                .scan_index
                .bucket(list)
                .revision
                .load(std::sync::atomic::Ordering::Relaxed)
        };

        let empty = revision();
        db.add_to_scan_list(ScanType::SEC01, 0, "calc", 0, "R:ONE");
        let added = revision();
        assert_ne!(added, empty, "an insert is a transition");

        db.add_to_scan_list(ScanType::SEC01, 0, "calc", 0, "R:ONE");
        let re_added = revision();
        assert_ne!(
            re_added, added,
            "re-inserting a key the bucket already holds is a transition too"
        );

        db.delete_from_scan_list(ScanType::SEC01, "R:ONE");
        assert_ne!(revision(), re_added, "a removal is a transition");
    }

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
