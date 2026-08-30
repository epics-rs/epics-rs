//! The run: boot the pair, drive identical operations, diff, adjudicate.
//!
//! # Case isolation, by construction
//!
//! Every put case gets **its own record instance**. This is the design choice
//! that makes the rest work:
//!
//! - It removes cross-contamination. If all cases shared one record, a put to
//!   `SCAN` or `HIHI` would change the database the *next* case is measuring,
//!   and a difference could not be attributed to the operation that caused it.
//! - It makes the reproducer minimal *by construction* rather than by an
//!   after-the-fact shrinking search. A failing case is already exactly one
//!   record, one field, one operation — there is nothing left to shrink.
//! - It lets the readback be batched: drive all the puts, then read every
//!   record's result in one `caget`. Without isolation the readback would have
//!   to be interleaved and the run would take hours.
//!
//! # The measurement is symmetric
//!
//! Both IOCs get the identical `.db` and the identical operation sequence,
//! driven by the identical C client tools. The only asymmetry is which server
//! is answering — which is the thing being measured.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::allowlist::{Allowlist, MatchContext};
use crate::cases::{BoundaryCase, boundary_cases};
use crate::catool::{CaTools, PutOutcome, ToolError, unattributed};
use crate::dbd::{Dbd, DbfType};
use crate::diff::{Comparison, Observation, Verdict, compare};
use crate::ioc::{CTools, Ioc, Pair, Side};
use crate::report::{CasePhase, CaseResult, Reproducer};
use crate::surface::{FieldRef, Surface, ValStatus, is_put_candidate};

/// How long to let monitor updates settle after driving the puts. A port that
/// posts *extra* events must be caught, so we cannot stop listening the moment
/// the puts return.
const MONITOR_SETTLE: Duration = Duration::from_millis(600);
const MONITOR_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Array-phase capacity. Fixed so both sides declare the identical `NELM` and
/// the over-capacity boundary is the same element count on each. Wide enough
/// that "partial" (`NELM/2`) is a distinct case from "single".
const ARRAY_NELM: u32 = 16;
const ARRAY_NELM_STR: &str = "16";
/// Element type for the array probe. `DOUBLE` because every array-capable
/// record type in the fat dbd (`waveform`/`aai`/`aao`/`subArray`) accepts it as
/// `FTVL`, so one type serves the whole phase.
const ARRAY_FTVL: &str = "DOUBLE";

pub struct Runner {
    tools: CTools,
    dbd: Dbd,
    /// Owned, not borrowed: the generated `.db` files must outlive every boot
    /// this runner drives and must not outlive the runner. See [`Workdir`].
    workdir: Workdir,
}

impl Runner {
    pub fn new(tools: CTools, dbd: Dbd, workdir: Workdir) -> Self {
        Self {
            tools,
            dbd,
            workdir,
        }
    }

    fn write_db(&self, name: &str, text: &str) -> Result<PathBuf, String> {
        let p = self.workdir.join(format!("{name}.db"));
        std::fs::write(&p, text).map_err(|e| format!("write {}: {e}", p.display()))?;
        Ok(p)
    }

    /// Phase A — the **read** probe: native type, element count, access rights,
    /// value (string and numeric) for every CA-observable field of one record
    /// type. No mutation, so the cases are order-independent and every field of
    /// the denominator is reachable in a single boot.
    pub fn probe_reads(
        &self,
        record_type: &str,
        surface: &Surface,
        allowlist: &mut Allowlist,
    ) -> Vec<CaseResult> {
        let rec = format!("ORACLE:{}", record_type.to_uppercase());
        let db_text = crate::record_stmt(record_type, &rec);

        let fields: Vec<FieldRef> = surface.fields_of(record_type).cloned().collect();
        if fields.is_empty() {
            return Vec::new();
        }
        let names: Vec<String> = fields.iter().map(|f| f.field.name.clone()).collect();
        let pvs: Vec<String> = names.iter().map(|f| format!("{rec}.{f}")).collect();

        let db = match self.write_db(&format!("read_{record_type}"), &db_text) {
            Ok(p) => p,
            Err(e) => {
                return errored_cases(
                    record_type,
                    &names,
                    CasePhase::Read,
                    None,
                    &db_text,
                    &unattributed("write-db", &e),
                );
            }
        };
        let pair = match Pair::boot(&self.tools, &db, &rec) {
            Ok(p) => p,
            // The IOC would not boot: every field of this type is an ERROR, and
            // not one of them is scored as agreement.
            Err(e) => {
                return errored_cases(
                    record_type,
                    &names,
                    CasePhase::Read,
                    None,
                    &db_text,
                    &e.tool_errors("boot"),
                );
            }
        };

        let c = CaTools::new(&self.tools, pair.c.port(), Side::C);
        let r = CaTools::new(&self.tools, pair.rust.port(), Side::Rust);
        let obs_c = read_observations(&c, &pvs);
        let obs_r = read_observations(&r, &pvs);

        fields
            .iter()
            .enumerate()
            .map(|(i, fr)| {
                let f = &fr.field.name;
                let repro = Reproducer {
                    db: db_text.clone(),
                    ops: vec![format!("caget {rec}.{f}"), format!("cainfo {rec}.{f}")],
                };
                adjudicate(
                    CaseRef {
                        record_type,
                        phase: CasePhase::Read,
                        field: f,
                        dbf: fr.field.dbf,
                        class: None,
                    },
                    repro,
                    &obs_c[i],
                    &obs_r[i],
                    allowlist,
                )
            })
            .collect()
    }

    /// Phase B — the **put** probe: drive every boundary value of every
    /// writable field, each into its own record instance, then read back what
    /// each side stored and what alarm it raised.
    pub fn probe_puts(
        &self,
        record_type: &str,
        surface: &Surface,
        allowlist: &mut Allowlist,
        max_cases: Option<usize>,
    ) -> Vec<CaseResult> {
        // Build the case list first: (field, boundary) -> its own record.
        let mut plan: Vec<(FieldRef, BoundaryCase)> = Vec::new();
        for fr in surface.fields_of(record_type) {
            if !is_put_candidate(&fr.field) {
                continue;
            }
            let choices = self.dbd.menu_choices(&fr.field);
            for bc in boundary_cases(&fr.field, choices) {
                plan.push((fr.clone(), bc));
            }
        }
        if let Some(m) = max_cases {
            plan.truncate(m);
        }
        if plan.is_empty() {
            return Vec::new();
        }

        // One record per case: isolation by construction.
        let rec_of = |i: usize| format!("ORACLE:{}:{i}", record_type.to_uppercase());
        let mut db_text = String::new();
        for i in 0..plan.len() {
            db_text.push_str(&crate::record_stmt(record_type, &rec_of(i)));
        }

        let names: Vec<String> = plan.iter().map(|(f, _)| f.field.name.clone()).collect();
        let classes: Vec<&str> = plan.iter().map(|(_, b)| b.class).collect();

        let db = match self.write_db(&format!("put_{record_type}"), &db_text) {
            Ok(p) => p,
            Err(e) => {
                return errored_puts(record_type, &plan, &db_text, &unattributed("write-db", &e));
            }
        };
        let pair = match Pair::boot(&self.tools, &db, &rec_of(0)) {
            Ok(p) => p,
            Err(e) => return errored_puts(record_type, &plan, &db_text, &e.tool_errors("boot")),
        };

        let c = CaTools::new(&self.tools, pair.c.port(), Side::C);
        let r = CaTools::new(&self.tools, pair.rust.port(), Side::Rust);

        // Drive the puts on both sides, then batch the readbacks. The two sides
        // see the identical sequence. They are separate IOCs on separate ports
        // with no shared state, so driving them concurrently changes nothing
        // either one observes -- it only stops each from waiting on the other.
        let (puts_c, puts_r) = std::thread::scope(|s| {
            let hc = s.spawn(|| drive_puts(&c, &plan, &rec_of));
            let hr = s.spawn(|| drive_puts(&r, &plan, &rec_of));
            (
                hc.join().expect("C put lane panicked"),
                hr.join().expect("Rust put lane panicked"),
            )
        });

        let val_pvs: Vec<String> = plan
            .iter()
            .enumerate()
            .map(|(i, (fr, _))| format!("{}.{}", rec_of(i), fr.field.name))
            .collect();
        let stat_pvs: Vec<String> = (0..plan.len())
            .map(|i| format!("{}.STAT", rec_of(i)))
            .collect();
        let sevr_pvs: Vec<String> = (0..plan.len())
            .map(|i| format!("{}.SEVR", rec_of(i)))
            .collect();

        let obs_c = readback(&c, &val_pvs, &stat_pvs, &sevr_pvs, puts_c);
        let obs_r = readback(&r, &val_pvs, &stat_pvs, &sevr_pvs, puts_r);

        plan.iter()
            .enumerate()
            .map(|(i, (fr, bc))| {
                let rec = rec_of(i);
                let f = &names[i];
                let repro = Reproducer {
                    // The minimal db is ONE record -- the other instances exist
                    // only to isolate the other cases in the same run.
                    db: crate::record_stmt(record_type, &rec),
                    ops: vec![
                        format!("caput {rec}.{f} '{}'", bc.value),
                        format!("caget {rec}.{f} {rec}.STAT {rec}.SEVR"),
                    ],
                };
                adjudicate(
                    CaseRef {
                        record_type,
                        phase: CasePhase::Put,
                        field: f,
                        dbf: fr.field.dbf,
                        class: Some(classes[i]),
                    },
                    repro,
                    &obs_c[i],
                    &obs_r[i],
                    allowlist,
                )
            })
            .collect()
    }

    /// Phase C — the **monitor** probe: subscribe with `camonitor`, drive a
    /// fixed put sequence, and diff the event stream both sides chose to post.
    ///
    /// The driven sequence is `1, 2, 2, 3` and the repeat is the point of it.
    /// C suppresses a monitor when the value did not change (subject to
    /// MDEL/ADEL), so a faithful port posts three updates, not four. "Posts an
    /// event per put regardless of change" is exactly the kind of divergence
    /// that a value-only diff cannot see — the final value agrees either way.
    /// # What the `.dbd` removes before a case exists
    ///
    /// The drive here is a *stimulus*, not the observation, so a record type
    /// whose `VAL` no client may write has nothing to measure: both sides
    /// refuse, both post nothing, and the identical traces describe an
    /// experiment that never ran. [`crate::surface::val_status`] is the single
    /// owner of that question for both protocols, so `sel` leaves the CA drive
    /// denominator for exactly the reason it already left the PVA one — rather
    /// than erroring here and being excluded there.
    pub fn probe_monitor(
        &self,
        record_type: &str,
        surface: &Surface,
        allowlist: &mut Allowlist,
    ) -> Option<CaseResult> {
        // Only meaningful where the .dbd leaves VAL drivable by some client.
        if crate::surface::val_status(surface, record_type) != ValStatus::Drivable {
            return None;
        }
        // `Drivable` was just proven, so VAL exists in the surface.
        let val_dbf = surface
            .fields_of(record_type)
            .find(|f| f.field.name == "VAL")
            .expect("val_status returned Drivable, so VAL is in the surface")
            .field
            .dbf;

        let rec = format!("ORACLE:MON:{}", record_type.to_uppercase());
        let db_text = crate::record_stmt(record_type, &rec);
        let seq = ["1", "2", "2", "3"];
        let repro = Reproducer {
            db: db_text.clone(),
            ops: {
                let mut v = vec![format!("camonitor {rec} &")];
                v.extend(seq.iter().map(|x| format!("caput {rec} {x}")));
                v.push("# the repeated '2' must NOT post a second update".into());
                v
            },
        };

        let db = match self.write_db(&format!("mon_{record_type}"), &db_text) {
            Ok(p) => p,
            Err(e) => {
                return Some(errored_case(
                    record_type,
                    "VAL",
                    CasePhase::Monitor,
                    None,
                    repro,
                    unattributed("write-db", &e),
                ));
            }
        };
        let pair = match Pair::boot(&self.tools, &db, &rec) {
            Ok(p) => p,
            Err(e) => {
                return Some(errored_case(
                    record_type,
                    "VAL",
                    CasePhase::Monitor,
                    None,
                    repro,
                    e.tool_errors("boot"),
                ));
            }
        };

        let pvs = vec![rec.clone()];
        // Every put in the sequence must land, or the trace below is a
        // measurement of nothing: see `CaTools::caput_drive`.
        let drive = |t: &CaTools| -> Result<(), ToolError> {
            for v in seq {
                t.caput_drive(&rec, v)?;
                // Space the puts so that a server which *does* post per change
                // has time to emit each one; without this, two puts inside one
                // scan period could legitimately coalesce on BOTH sides and the
                // probe would measure nothing.
                std::thread::sleep(Duration::from_millis(120));
            }
            Ok(())
        };

        let obs = |port: u16, side: Side| -> Observation {
            let t = CaTools::new(&self.tools, port, side);
            match t.monitor(&pvs, MONITOR_SETTLE, MONITOR_CONNECT_TIMEOUT, drive) {
                Ok(tr) => Observation {
                    monitor: Some(tr),
                    ..Default::default()
                },
                Err(e) => Observation {
                    errors: vec![e],
                    ..Default::default()
                },
            }
        };
        let obs_c = obs(pair.c.port(), Side::C);
        let obs_r = obs(pair.rust.port(), Side::Rust);

        Some(adjudicate(
            CaseRef {
                record_type,
                phase: CasePhase::Monitor,
                field: "VAL",
                dbf: val_dbf,
                class: None,
            },
            repro,
            &obs_c,
            &obs_r,
            allowlist,
        ))
    }

    /// Phase D — the **array** probe: put-and-readback of a bounded array VAL
    /// across the element-count boundaries that break ports (single, partial,
    /// exactly `NELM`, one past `NELM`). C truncates an over-capacity put to
    /// `NELM`; a port that rejects it, or writes past the end, differs
    /// observably in the readback count and `NORD`.
    ///
    /// Only record types whose VAL is a bounded array — both `NELM` and `FTVL`
    /// declared — are driven; the rest have no array to probe and yield no
    /// cases. That array VAL is `DBF_NOACCESS` in the `.dbd` (it is the record's
    /// raw `BPTR` pointer), so it is excluded from the scalar field denominator
    /// and never reached by the read/put/monitor phases. This phase is the only
    /// one that exercises it, over CA, against the same C ground truth.
    pub fn probe_array(&self, record_type: &str, allowlist: &mut Allowlist) -> Vec<CaseResult> {
        // Array-capable iff the record declares both a capacity (NELM) and an
        // element type (FTVL). Determined from the .dbd, not hard-listed.
        // `VAL`'s declared type comes along: it is `DBF_NOACCESS` for these
        // types (the record's raw BPTR), and the allowlist has to see that
        // rather than a guess, or a row scoped by destination type would match
        // the array phase on a type it never named.
        let val_dbf = self
            .dbd
            .record_type(record_type)
            .filter(|r| r.field("NELM").is_some() && r.field("FTVL").is_some())
            .and_then(|r| r.field("VAL"))
            .map(|f| f.dbf);
        let Some(val_dbf) = val_dbf else {
            return Vec::new();
        };

        let plan = crate::cases::array_cases(ARRAY_NELM);
        let rec_of = |i: usize| format!("ORACLE:ARR:{}:{i}", record_type.to_uppercase());
        let fields = &[("NELM", ARRAY_NELM_STR), ("FTVL", ARRAY_FTVL)];

        // One record per case (isolation), each declaring the same NELM/FTVL so
        // the capacity boundary is identical on both sides.
        let mut db_text = String::new();
        for i in 0..plan.len() {
            db_text.push_str(&crate::record_stmt_fields(record_type, &rec_of(i), fields));
        }

        let db = match self.write_db(&format!("arr_{record_type}"), &db_text) {
            Ok(p) => p,
            Err(e) => {
                return errored_array(record_type, &plan, &db_text, &unattributed("write-db", &e));
            }
        };
        let pair = match Pair::boot(&self.tools, &db, &rec_of(0)) {
            Ok(p) => p,
            Err(e) => return errored_array(record_type, &plan, &db_text, &e.tool_errors("boot")),
        };

        let c = CaTools::new(&self.tools, pair.c.port(), Side::C);
        let r = CaTools::new(&self.tools, pair.rust.port(), Side::Rust);

        // Drive the array puts on both sides concurrently, then read back.
        // Separate IOCs, separate ports, no shared state — concurrency changes
        // nothing either observes, it only stops each waiting on the other.
        let (obs_c, obs_r) = std::thread::scope(|s| {
            let hc = s.spawn(|| drive_array(&c, &plan, &rec_of));
            let hr = s.spawn(|| drive_array(&r, &plan, &rec_of));
            (
                hc.join().expect("C array lane panicked"),
                hr.join().expect("Rust array lane panicked"),
            )
        });

        plan.iter()
            .enumerate()
            .map(|(i, (values, class))| {
                let rec = rec_of(i);
                let repro = Reproducer {
                    db: crate::record_stmt_fields(record_type, &rec, fields),
                    ops: vec![
                        format!("caput -a {rec} {} {}", values.len(), values.join(" ")),
                        format!("caget -t -# {ARRAY_NELM} {rec}    # returned count + payload"),
                        format!("caget {rec}.NORD"),
                    ],
                };
                adjudicate(
                    CaseRef {
                        record_type,
                        phase: CasePhase::Array,
                        field: "VAL",
                        dbf: val_dbf,
                        class: Some(class),
                    },
                    repro,
                    &obs_c[i],
                    &obs_r[i],
                    allowlist,
                )
            })
            .collect()
    }
}

/// Probe `pvs` with an all-or-nothing batch tool, isolating the PVs that fail.
///
/// The C CA tools print NOTHING for a whole batch if one PV in it will not
/// connect, so a batch result is only usable when every PV in it succeeded. The
/// naive recovery — fall back to one spawn per PV — costs `n` spawns whenever a
/// single PV is bad, and in the put phase at least one always is (a field the
/// port does not serve). That made the fallback, not the batch, the cost of the
/// phase: 1082 PVs x 3 tools x 2 sides of serial process spawn.
///
/// So bisect instead. A failing batch is split and each half retried, so `k` bad
/// PVs among `n` cost O(k log n) spawns rather than O(n), and the good PVs keep
/// riding a batch. Attribution is unchanged: recursion bottoms out at a single
/// PV, so an error still lands on the exact PV that caused it and is never
/// spread across the batch or silently dropped.
fn probe_bisect<T: Send>(
    pvs: &[String],
    batch: &(impl Fn(&[String]) -> Result<Vec<T>, ToolError> + Sync),
    single: &(impl Fn(&str) -> Result<T, ToolError> + Sync),
) -> Vec<Result<T, ToolError>> {
    if pvs.is_empty() {
        return Vec::new();
    }
    if let Ok(vals) = batch(pvs)
        && vals.len() == pvs.len()
    {
        return vals.into_iter().map(Ok).collect();
    }
    if pvs.len() == 1 {
        return vec![single(&pvs[0])];
    }
    let (a, b) = pvs.split_at(pvs.len() / 2);
    let (mut left, right) = std::thread::scope(|s| {
        let ha = s.spawn(|| probe_bisect(a, batch, single));
        let hb = s.spawn(|| probe_bisect(b, batch, single));
        (
            ha.join().expect("bisect lane panicked"),
            hb.join().expect("bisect lane panicked"),
        )
    });
    left.extend(right);
    left
}

/// Read the type/shape/value surface for a list of PVs.
///
/// The three tool probes are independent reads of the same already-settled
/// state, so they run concurrently; within each, [`probe_bisect`] isolates the
/// PVs that will not connect without paying a spawn per good PV.
fn read_observations(t: &CaTools, pvs: &[String]) -> Vec<Observation> {
    let mut obs: Vec<Observation> = vec![Observation::default(); pvs.len()];

    let (strings, numerics, infos) = std::thread::scope(|s| {
        let hs = s.spawn(|| {
            probe_bisect(pvs, &|p: &[String]| t.caget_batch(p, false), &|pv: &str| {
                t.caget_string(pv)
            })
        });
        let hn = s.spawn(|| {
            probe_bisect(pvs, &|p: &[String]| t.caget_batch(p, true), &|pv: &str| {
                t.caget_numeric(pv)
            })
        });
        let hi = s.spawn(|| {
            probe_bisect(pvs, &|p: &[String]| t.cainfo_batch(p), &|pv: &str| {
                t.cainfo(pv)
            })
        });
        (
            hs.join().expect("caget-string lane panicked"),
            hn.join().expect("caget-numeric lane panicked"),
            hi.join().expect("cainfo lane panicked"),
        )
    });

    for (o, r) in obs.iter_mut().zip(strings) {
        match r {
            Ok(v) => o.value_string = Some(v),
            Err(e) => o.errors.push(e),
        }
    }
    for (o, r) in obs.iter_mut().zip(numerics) {
        match r {
            Ok(v) => o.value_numeric = Some(v),
            Err(e) => o.errors.push(e),
        }
    }
    for (o, r) in obs.iter_mut().zip(infos) {
        match r {
            Ok(i) => o.info = Some(i),
            Err(e) => o.errors.push(e),
        }
    }
    obs
}

/// How many `caput` processes one side may have in flight at once.
///
/// The put probe spawns one C `caput` per case, and a case that cannot connect
/// burns its full CA timeout before it fails. Serially that is the whole cost of
/// the phase: 7,625 cases x 2 sides of process spawn + CA search + connect.
///
/// Concurrency is sound here because **isolation already holds by construction**
/// — every case drives its OWN record instance (`rec_of`), so no two cases can
/// see each other's state, and the order they are driven in cannot change what
/// either side stores. What must NOT change is the *pairing*: both sides drive
/// the identical case list and each result stays at its own index.
fn put_lanes() -> usize {
    if let Some(n) = std::env::var("ORACLE_PUT_LANES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
    {
        return n;
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .saturating_sub(2)
        .clamp(1, 48)
        // Halved because both sides are driven concurrently, so this is the
        // per-side share of one total budget.
        .div_ceil(2)
}

/// Drive one side's puts, `put_lanes()` at a time, preserving case order.
///
/// An element is `Err` when the put could not be measured at all — the tool
/// never reached a server, or this harness could not run it. That is not a
/// refusal and [`readback`] must not let it become one.
fn drive_puts(
    t: &CaTools,
    plan: &[(FieldRef, BoundaryCase)],
    rec_of: &(impl Fn(usize) -> String + Sync),
) -> Vec<Result<PutOutcome, ToolError>> {
    let lanes = put_lanes().min(plan.len().max(1));
    let chunk = plan.len().div_ceil(lanes);

    std::thread::scope(|s| {
        let handles: Vec<_> = plan
            .chunks(chunk)
            .enumerate()
            .map(|(c, cases)| {
                let base = c * chunk;
                s.spawn(move || {
                    cases
                        .iter()
                        .enumerate()
                        .map(|(j, (fr, bc))| {
                            t.caput(
                                &format!("{}.{}", rec_of(base + j), fr.field.name),
                                &bc.value,
                            )
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        // Chunks are contiguous and joined in order, so index i of the result is
        // still case i. A panicked lane would silently drop cases, so it is a
        // hard failure, not a skipped case.
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("put lane panicked"))
            .collect()
    })
}

/// Drive one array put per case and read back what each side stored: the array
/// payload (with its leading returned count), `NORD`, native shape, and alarm.
///
/// Every surface is obtained or recorded as an error, never silently absent —
/// the same rule as the scalar [`readback`]. `NORD` in particular is not
/// supplementary here: it is *the* discriminator of this phase (C truncates an
/// over-capacity put to `NELM`; a port that rejects it or writes past the end
/// differs in `NORD` and often in nothing else), so dropping it left the
/// truncation contract scored AGREED while unverified.
/// Take the reading, or record why it could not be taken. The two outcomes stay
/// distinguishable, which is the whole point: `None` may only ever mean "this
/// surface does not apply", never "we tried and failed".
fn keep<T>(r: Result<T, ToolError>, errors: &mut Vec<ToolError>) -> Option<T> {
    match r {
        Ok(v) => Some(v),
        Err(e) => {
            errors.push(e);
            None
        }
    }
}

fn drive_array(
    t: &CaTools,
    plan: &[(Vec<String>, &'static str)],
    rec_of: &(impl Fn(usize) -> String + Sync),
) -> Vec<Observation> {
    plan.iter()
        .enumerate()
        .map(|(i, (values, _))| {
            let rec = rec_of(i);
            // Same rule as the scalar put phase: a put that could not be
            // measured is an ERROR for the case, never a refusal both sides
            // can agree on.
            let mut errors = Vec::new();
            let put = match t.caput_array(&rec, values) {
                Ok(p) => Some(p),
                Err(e) => {
                    errors.push(e);
                    None
                }
            };

            // Read back the whole declared capacity so a port that stored too
            // many or too few elements shows a different leading count and
            // payload than C's truncate-to-NELM.
            let value_string = match t.caget_array(&rec, ARRAY_NELM) {
                Ok(v) => Some(v),
                Err(e) => {
                    errors.push(e);
                    None
                }
            };
            let info = match t.cainfo(&rec) {
                Ok(ci) => Some(ci),
                Err(e) => {
                    errors.push(e);
                    None
                }
            };
            Observation {
                info,
                value_string,
                value_numeric: keep(t.caget_numeric(&format!("{rec}.NORD")), &mut errors),
                stat: keep(t.caget_string(&format!("{rec}.STAT")), &mut errors),
                sevr: keep(t.caget_string(&format!("{rec}.SEVR")), &mut errors),
                put,
                monitor: None,
                errors,
            }
        })
        .collect()
}

/// A boot failure in the array phase — one ERROR case per element-count case it
/// prevented, never one aggregate and never silence.
fn errored_array(
    record_type: &str,
    plan: &[(Vec<String>, &'static str)],
    db: &str,
    errors: &[ToolError],
) -> Vec<CaseResult> {
    plan.iter()
        .map(|(values, class)| {
            errored_case(
                record_type,
                "VAL",
                CasePhase::Array,
                Some(class),
                Reproducer {
                    db: db.to_string(),
                    ops: vec![format!(
                        "caput -a <rec> {} {}",
                        values.len(),
                        values.join(" ")
                    )],
                },
                errors.to_vec(),
            )
        })
        .collect()
}

/// Read one string field across many records, attributing a failed batch to the
/// exact PV that caused it.
///
/// The same [`probe_bisect`] mechanism [`read_observations`] uses for the value
/// surface, and the reason it is a named helper: STAT and SEVR used to be
/// `caget_batch(..).ok()`, so one unconnectable `.STAT` PV — the all-or-nothing
/// batch contract of [`CaTools::caget_batch`] — silently removed the alarm
/// comparison from *every* put case of that record type while all of them went
/// on being scored AGREED.
fn probe_strings(t: &CaTools, pvs: &[String]) -> Vec<Result<String, ToolError>> {
    probe_bisect(pvs, &|p: &[String]| t.caget_batch(p, false), &|pv: &str| {
        t.caget_string(pv)
    })
}

/// Batch the post-put readback: what each side stored, and what alarm it raised.
///
/// Every surface here is read the same way: obtained, or recorded as a
/// [`ToolError`] on the case it belongs to. There is no third state in which a
/// surface is quietly absent, because that state is indistinguishable from
/// "both sides agreed".
fn readback(
    t: &CaTools,
    val_pvs: &[String],
    stat_pvs: &[String],
    sevr_pvs: &[String],
    puts: Vec<Result<PutOutcome, ToolError>>,
) -> Vec<Observation> {
    let mut obs = read_observations(t, val_pvs);
    let (stats, sevrs) = std::thread::scope(|s| {
        let hs = s.spawn(|| probe_strings(t, stat_pvs));
        let hv = s.spawn(|| probe_strings(t, sevr_pvs));
        (
            hs.join().expect("STAT lane panicked"),
            hv.join().expect("SEVR lane panicked"),
        )
    });
    for (o, put) in obs.iter_mut().zip(puts) {
        // A put that could not be measured lands in `errors`, where
        // `adjudicate` scores it ERROR. Writing it into `put` as a refusal
        // would make both sides agree about a write neither one performed.
        match put {
            Ok(p) => o.put = Some(p),
            Err(e) => o.errors.push(e),
        }
    }
    for (o, r) in obs.iter_mut().zip(stats) {
        match r {
            Ok(v) => o.stat = Some(v),
            Err(e) => o.errors.push(e),
        }
    }
    for (o, r) in obs.iter_mut().zip(sevrs) {
        match r {
            Ok(v) => o.sevr = Some(v),
            Err(e) => o.errors.push(e),
        }
    }
    obs
}

/// Turn a pair of observations into a verdict.
///
/// The order of the checks is the policy:
/// 1. **Unreadable on either side => ERROR.** Checked first, so a case that
///    could not run can never be scored as agreement. This is the rule the
///    audit loop lacked, and the reason 21 "verified clean" verdicts were false.
/// 2. No differences => AGREED.
/// 3. Every difference justified by the allowlist => EXPECTED DEVIATION.
/// 4. Anything left => DEFECT.
///
/// Step 3 requires **every** difference to be justified. A case where one diff
/// is allowlisted and another is not is a DEFECT, not a partial pass — the
/// unjustified diff does not get laundered by the justified one.
/// Everything a CA case needs to identify itself, passed as one value so
/// [`adjudicate`] keeps a signature a caller can read — the same shape
/// [`crate::pvaread::ChannelRef`] gives the PVA phases.
struct CaseRef<'a> {
    record_type: &'a str,
    field: &'a str,
    /// Which probe this case came from. Stamped here so no consumer has to
    /// guess it from `class`, which the read and monitor phases both leave
    /// `None`.
    phase: CasePhase,
    /// The destination field's declared `.dbd` type. Carried because an
    /// allowlist row may be scoped by it (see [`crate::allowlist::MatchContext`]).
    dbf: DbfType,
    class: Option<&'a str>,
}

fn adjudicate(
    cr: CaseRef<'_>,
    repro: Reproducer,
    c: &Observation,
    r: &Observation,
    allowlist: &mut Allowlist,
) -> CaseResult {
    let CaseRef {
        record_type,
        field,
        phase,
        dbf,
        class,
    } = cr;
    let mut errors: Vec<ToolError> = Vec::new();
    errors.extend(c.errors.iter().cloned());
    errors.extend(r.errors.iter().cloned());

    let base = CaseResult {
        record_type: record_type.to_string(),
        field: field.to_string(),
        phase,
        class: class.map(str::to_string),
        verdict: Verdict::Errored,
        differences: Vec::new(),
        allowlisted: Vec::new(),
        errors: errors.clone(),
        reproducer: repro,
        c_side: c.clone(),
        rust_side: r.clone(),
    };

    // An error is an absence, with no exceptions left to carve out. A put the
    // server took and never finished is no longer one: it arrives as
    // `PutOutcome::NeverCompleted`, a reading, and is compared below like any
    // other outcome. That is what removed the special case that used to sit
    // here — an error list that had to be re-read to decide whether the case
    // was really unmeasured, which could only ever answer for the ONE shape of
    // non-answer it knew how to spell.
    if !errors.is_empty() {
        return base;
    }

    let Comparison {
        compared,
        differences,
    } = compare(c, r);

    let ctx = MatchContext {
        record_type,
        field,
        dbf,
        class,
    };
    // Tell the allowlist what this case LOOKED at, agreement or not — that is what
    // separates a deviation that stopped happening (stale: a finding) from one this
    // run never drove (unexercised: coverage). Must run before the agreed-early-out.
    allowlist.note_compared(&ctx, &compared);

    if differences.is_empty() {
        // Agreement, sub-classified. Two servers that agreed by both declining
        // to finish the put agreed about less than two that both finished it,
        // and the report has always said so rather than folding the two into
        // one number.
        let both_declined = matches!(
            (&c.put, &r.put),
            (
                Some(PutOutcome::NeverCompleted),
                Some(PutOutcome::NeverCompleted)
            )
        );
        return CaseResult {
            verdict: if both_declined {
                Verdict::NeitherCompleted
            } else {
                Verdict::Agreed
            },
            ..base
        };
    }

    let hits: Vec<Option<String>> = differences
        .iter()
        .map(|d| allowlist.match_diff(&ctx, d))
        .collect();

    let all_justified = hits.iter().all(Option::is_some);
    let allowlisted: Vec<String> = hits.into_iter().flatten().collect();

    CaseResult {
        verdict: if all_justified {
            Verdict::ExpectedDeviation
        } else {
            Verdict::Defect
        },
        differences,
        allowlisted,
        ..base
    }
}

fn errored_case(
    record_type: &str,
    field: &str,
    phase: CasePhase,
    class: Option<&str>,
    repro: Reproducer,
    errors: Vec<ToolError>,
) -> CaseResult {
    CaseResult {
        record_type: record_type.to_string(),
        field: field.to_string(),
        phase,
        class: class.map(str::to_string),
        verdict: Verdict::Errored,
        differences: Vec::new(),
        allowlisted: Vec::new(),
        // Attributed by whoever produced the failure -- `BootError::tool_errors`
        // for a boot, the tool's own `ToolError` otherwise. Recorded against
        // both sides only for a failure that belongs to neither, because a
        // failure recorded against a side that was fine is a wrong reading, not
        // a cautious one.
        errors,
        reproducer: repro,
        c_side: Observation::default(),
        rust_side: Observation::default(),
    }
}

/// A boot failure must produce one ERROR case per field it prevented from being
/// measured — not one aggregate error, and above all not silence. The
/// denominator does not shrink just because we could not look.
fn errored_cases(
    record_type: &str,
    fields: &[String],
    phase: CasePhase,
    class: Option<&str>,
    db: &str,
    errors: &[ToolError],
) -> Vec<CaseResult> {
    fields
        .iter()
        .map(|f| {
            errored_case(
                record_type,
                f,
                phase,
                class,
                Reproducer {
                    db: db.to_string(),
                    ops: vec![format!("caget ORACLE:*.{f}")],
                },
                errors.to_vec(),
            )
        })
        .collect()
}

fn errored_puts(
    record_type: &str,
    plan: &[(FieldRef, BoundaryCase)],
    db: &str,
    errors: &[ToolError],
) -> Vec<CaseResult> {
    plan.iter()
        .map(|(fr, bc)| {
            let f = &fr.field.name;
            errored_case(
                record_type,
                f,
                CasePhase::Put,
                Some(bc.class),
                Reproducer {
                    db: db.to_string(),
                    ops: vec![format!("caput <rec>.{f} '{}'", bc.value)],
                },
                errors.to_vec(),
            )
        })
        .collect()
}

/// Record types the run should cover: the intersection of the `.dbd` and what
/// the port implements, optionally filtered.
pub fn select_types(surface: &Surface, only: &Option<Vec<String>>) -> Vec<String> {
    let want: Option<BTreeSet<&str>> = only
        .as_ref()
        .map(|v| v.iter().map(String::as_str).collect());
    surface
        .covered_types
        .iter()
        .filter(|t| want.as_ref().is_none_or(|w| w.contains(t.as_str())))
        .cloned()
        .collect()
}

/// A harness workdir, and the right to it for exactly as long as this value
/// lives.
///
/// # Invariant
///
/// **A directory under `<tmp>/epics-oracle/` exists only while a live process
/// owns it.** [`workdir`] is the only mint, [`Workdir::drop`] the only removal
/// on the ordinary path, and `reap_own_residue` the only removal of what a
/// crash left behind. Nothing else creates or deletes under that root.
///
/// That invariant is what makes the pid a sound key rather than a
/// probabilistic one. A pid is unique among the LIVING; keying a directory on
/// it while never removing the directory keys it on the whole pid space
/// instead, and that space wraps. Measured on this host on 2026-08-25:
/// `/tmp/epics-oracle` held 3,916 leftovers spanning pids 19,083 to 4,137,878
/// against a `pid_max` of 4,194,304, and a suite run failed `mkdir
/// /tmp/epics-oracle/<pid>-0: File exists` against a leftover created 2h15m
/// earlier. The "crashed run whose pid has since been reused" that this
/// function used to call a residual case was the routine case, and its
/// probability grew with every run of the suite on the box.
#[derive(Debug)]
pub struct Workdir {
    path: PathBuf,
    /// `true` when this handle minted the directory and therefore owes its
    /// removal; `false` for a caller-supplied `base`, whose lifetime is the
    /// caller's to decide.
    minted: bool,
}

impl Workdir {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl std::ops::Deref for Workdir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for Workdir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for Workdir {
    /// Best-effort by necessity — `kill -9` runs no destructor, which is what
    /// `reap_own_residue` exists for — but the ordinary exit is the one that
    /// built the pile of 3,916, and for it this is not best-effort at all:
    /// every `Runner`, every probe and every test ends here.
    fn drop(&mut self) {
        if self.minted {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// Remove every `<pid>-<n>` leaf under `parent` that carries THIS process's id.
///
/// Called once per process, before that process mints its first workdir, and
/// that ordering is the whole proof: a leaf named for this pid that exists
/// before this process has minted anything cannot have a live owner — no other
/// living process has this pid, and this one has not used it yet. So it is
/// residue from a run that died before its [`Workdir`] could drop.
///
/// It is also the only place the invariant can be restored after a crash, and
/// the reason pid reuse is not simply an error: the process that would have
/// collided is exactly the one that can prove the collision is a ghost.
fn reap_own_residue(parent: &Path) {
    // The trailing dash is load-bearing: pid 1234 must not reap 12345's leaves.
    let mine = format!("{}-", std::process::id());
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(&mine) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// A directory for generated `.db` files, owned by the returned [`Workdir`].
///
/// The files inside are named after what they hold — `ai.db`, `pair_smoke.db`,
/// `pva_test_ai.db` — never after who wrote them. That naming is fine, and the
/// directory is what has to make it fine: two writers sharing one directory
/// share those names, and `fs::write` truncates a file before it fills it. A
/// `softIoc` that opens the `.db` inside that window loads an empty file,
/// prints `iocRun: All initialization complete`, serves no record at all, and
/// is caught only by [`crate::ioc::Pair::boot`]'s reachability probe — a
/// connect failure whose cause is a file, not a socket, and which reads as a
/// flaky test on a busy host.
///
/// So each call mints its own `<tmp>/epics-oracle/<pid>-<n>`: two live
/// processes differ by pid, two calls inside one process differ by `n`. Neither
/// nextest's process-per-test nor `cargo test`'s threads-in-one-process, nor
/// two checkouts of this repo running the suite at once, can put two writers on
/// one path — and the leaf is gone again when the returned handle drops, so the
/// pid stays a key over the living rather than over all pids ever issued.
///
/// `base` overrides all of it, and then the caller owns both the exclusivity
/// and the lifetime.
pub fn workdir(base: Option<&Path>) -> Result<Workdir, String> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    static REAP: Once = Once::new();
    match base {
        // The caller owns exclusivity here, so an existing directory is theirs
        // — and so is deciding when it goes away.
        Some(p) => {
            std::fs::create_dir_all(p).map_err(|e| format!("mkdir {}: {e}", p.display()))?;
            Ok(Workdir {
                path: p.to_path_buf(),
                minted: false,
            })
        }
        None => {
            let parent = std::env::temp_dir().join("epics-oracle");
            std::fs::create_dir_all(&parent)
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
            REAP.call_once(|| reap_own_residue(&parent));
            let dir = parent.join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            // `create_dir`, not `create_dir_all`: after the reap above, a
            // directory already standing on this path cannot be residue, so it
            // is a writer racing us for the name, and adopting it would be the
            // silent truncation this function exists to prevent.
            std::fs::create_dir(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
            Ok(Workdir {
                path: dir,
                minted: true,
            })
        }
    }
}

#[cfg(test)]
mod bisect_tests {
    use super::probe_bisect;
    use crate::catool::ToolError;
    use crate::ioc::Side;
    use std::sync::Mutex;

    /// A stand-in for the C tools: the batch is all-or-nothing, exactly as
    /// `caget` is — it yields nothing at all if any PV in it is unconnectable.
    /// Every invocation is counted, because the spawn count IS the cost this
    /// change exists to bound.
    struct FakeTool {
        bad: Vec<&'static str>,
        batches: Mutex<usize>,
        singles: Mutex<usize>,
    }

    impl FakeTool {
        fn new(bad: &[&'static str]) -> Self {
            Self {
                bad: bad.to_vec(),
                batches: Mutex::new(0),
                singles: Mutex::new(0),
            }
        }
        fn err(&self, pv: &str) -> ToolError {
            ToolError {
                side: Side::Rust,
                tool: "caget".into(),
                message: format!("{pv} not found"),
            }
        }
        fn batch(&self, pvs: &[String]) -> Result<Vec<String>, ToolError> {
            *self.batches.lock().unwrap() += 1;
            if let Some(b) = pvs.iter().find(|p| self.bad.contains(&p.as_str())) {
                return Err(self.err(b));
            }
            Ok(pvs.iter().map(|p| format!("v:{p}")).collect())
        }
        fn single(&self, pv: &str) -> Result<String, ToolError> {
            *self.singles.lock().unwrap() += 1;
            if self.bad.contains(&pv) {
                return Err(self.err(pv));
            }
            Ok(format!("v:{pv}"))
        }
        fn spawns(&self) -> usize {
            *self.batches.lock().unwrap() + *self.singles.lock().unwrap()
        }
    }

    fn pvs(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("PV{i}")).collect()
    }

    fn run(t: &FakeTool, list: &[String]) -> Vec<Result<String, ToolError>> {
        probe_bisect(list, &|p: &[String]| t.batch(p), &|pv: &str| t.single(pv))
    }

    /// Boundary: zero bad PVs. One batch, no per-PV spawn at all.
    #[test]
    fn a_clean_batch_costs_exactly_one_spawn() {
        let t = FakeTool::new(&[]);
        let list = pvs(64);
        let out = run(&t, &list);
        assert_eq!(out.len(), 64);
        assert!(out.iter().all(|r| r.is_ok()));
        assert_eq!(t.spawns(), 1, "a clean batch must not bisect");
    }

    /// Boundary: exactly one bad PV. The error lands on THAT PV -- attribution
    /// is what the naive per-PV fallback bought, and bisecting must not lose it.
    #[test]
    fn one_bad_pv_is_attributed_to_itself_and_no_one_else() {
        let t = FakeTool::new(&["PV37"]);
        let list = pvs(64);
        let out = run(&t, &list);

        for (i, r) in out.iter().enumerate() {
            if i == 37 {
                assert!(r.is_err(), "PV37 must carry the error");
                assert!(r.as_ref().unwrap_err().message.contains("PV37"));
            } else {
                assert_eq!(r.as_ref().unwrap(), &format!("v:PV{i}"), "PV{i} readable");
            }
        }
    }

    /// The cost boundary this change exists for: one bad PV among n must NOT
    /// cost n spawns. Serially it did -- that fallback was the whole cost of the
    /// put phase.
    #[test]
    fn one_bad_pv_among_many_does_not_cost_a_spawn_per_pv() {
        let t = FakeTool::new(&["PV37"]);
        let list = pvs(64);
        let _ = run(&t, &list);
        assert!(
            t.spawns() < 64,
            "bisect must beat per-PV fallback, took {} spawns for 64 PVs",
            t.spawns()
        );
    }

    /// Boundary: every PV bad. Bisect degrades to per-PV -- and must still
    /// attribute each error to its own PV rather than collapsing them.
    #[test]
    fn all_bad_still_attributes_each_error_to_its_own_pv() {
        let bad: Vec<&'static str> = vec!["PV0", "PV1", "PV2", "PV3"];
        let t = FakeTool::new(&bad);
        let list = pvs(4);
        let out = run(&t, &list);
        assert_eq!(out.len(), 4);
        for (i, r) in out.iter().enumerate() {
            assert!(r.as_ref().unwrap_err().message.contains(&format!("PV{i}")));
        }
    }

    /// Boundary: a single-PV list that fails. Recursion must bottom out on the
    /// single probe rather than splitting an empty half forever.
    #[test]
    fn a_single_bad_pv_bottoms_out() {
        let t = FakeTool::new(&["PV0"]);
        let out = run(&t, &pvs(1));
        assert_eq!(out.len(), 1);
        assert!(out[0].is_err());
    }

    /// Boundary: an empty list probes nothing.
    #[test]
    fn an_empty_list_spawns_nothing() {
        let t = FakeTool::new(&[]);
        let out = run(&t, &[]);
        assert!(out.is_empty());
        assert_eq!(t.spawns(), 0);
    }
}

/// The adjudication rule, exercised against the **shipped** allowlist with a
/// planted disagreement.
///
/// A planted difference is the only way to test an instrument's verdict: a live
/// run reports whatever the port happens to do today, so it can pass while the
/// rule underneath it is wrong. These plant a difference whose correct verdict
/// is known and assert the verdict the harness reaches, and the bucket the exit
/// code is computed from.
#[cfg(test)]
mod adjudicate_tests {
    // The three cases driven against the real pair carry `#[cfg(tokio_backend)]`:
    // they boot `oracle-ioc`, which refuses to start under the reactor-free
    // `exec_backend`, so they cannot pass there under any treatment. The rest
    // adjudicate against `FakeIoc` and stay selected on every backend.
    use super::*;
    use crate::dbd::DbfType;
    use crate::report::Counts;

    fn shipped() -> Allowlist {
        Allowlist::load(&Allowlist::default_path()).expect("shipped allowlist loads")
    }

    fn repro() -> Reproducer {
        Reproducer {
            db: String::new(),
            ops: Vec::new(),
        }
    }

    fn value(s: &str) -> Observation {
        Observation {
            value_string: Some(s.to_string()),
            ..Default::default()
        }
    }

    /// An `Observation` that read back cleanly but carries one tool error.
    fn value_with_error(v: &str, side: Side, msg: &str) -> Observation {
        Observation {
            value_string: Some(v.to_string()),
            errors: vec![ToolError {
                side,
                tool: "caput".into(),
                message: msg.into(),
            }],
            ..Default::default()
        }
    }

    /// An `Observation` that read back cleanly and carries a put outcome.
    fn value_with_put(v: &str, put: PutOutcome) -> Observation {
        Observation {
            value_string: Some(v.to_string()),
            put: Some(put),
            ..Default::default()
        }
    }

    fn put_case(c: &Observation, r: &Observation) -> CaseResult {
        adjudicate(
            CaseRef {
                record_type: "sub",
                phase: CasePhase::Put,
                field: "VAL",
                dbf: DbfType::Double,
                class: Some("zero"),
            },
            repro(),
            c,
            r,
            &mut shipped(),
        )
    }

    /// The bucket. Both IOCs took the write, neither ever said it finished, and
    /// every other surface agreed — measured on `sub`, whose empty `SNAM`
    /// latches `pact = TRUE` (`subRecord.c:119-122`) on the C side and whose
    /// 464 put cases are all of this shape.
    #[test]
    fn a_no_completion_from_both_sides_is_a_reading_not_a_measurement_failure() {
        let case = put_case(
            &value_with_put("0", PutOutcome::NeverCompleted),
            &value_with_put("0", PutOutcome::NeverCompleted),
        );
        assert_eq!(case.verdict, Verdict::NeitherCompleted);
        assert!(
            case.errors.is_empty(),
            "a server that declined to finish answered; that is not an error"
        );
    }

    /// The discriminator. One side finishing and the other not is a difference
    /// between the two servers — the finding this harness exists to make — and
    /// it is now reported as one, from either direction.
    ///
    /// It used to score ERROR, because a non-completion could only reach
    /// `adjudicate` as a `ToolError` and an error meant "no reading". C's `busy`
    /// record is the case that proves it wrong: it withholds the completion by
    /// design while `VAL` is non-zero, so a port that completes the put diverges
    /// on a surface both sides actually reported.
    #[test]
    fn a_one_sided_no_completion_is_a_defect_not_an_error() {
        for (c, r) in [
            (
                value_with_put("0", PutOutcome::NeverCompleted),
                value_with_put("0", PutOutcome::Completed),
            ),
            (
                value_with_put("0", PutOutcome::Completed),
                value_with_put("0", PutOutcome::NeverCompleted),
            ),
        ] {
            let case = put_case(&c, &r);
            assert_eq!(
                case.verdict,
                Verdict::Defect,
                "a one-sided no-completion is a divergence, not a shared reading"
            );
            assert_eq!(
                case.differences
                    .iter()
                    .map(|d| d.surface)
                    .collect::<Vec<_>>(),
                [crate::diff::Surface::PutAccepted],
                "and it lands on the put surface, once"
            );
        }
    }

    /// The other half of the discriminator: only the marker that describes the
    /// SERVER declining to finish is admitted. A channel that never connected,
    /// or one of the harness's own failures, means the measurement did not
    /// happen — two of those are not a reading, however symmetric.
    #[test]
    fn a_symmetric_failure_that_is_not_a_no_completion_stays_an_error() {
        for msg in ["Channel connect timed out", "spawn: no such file"] {
            let case = adjudicate(
                CaseRef {
                    record_type: "sub",
                    phase: CasePhase::Put,
                    field: "VAL",
                    dbf: DbfType::Double,
                    class: Some("zero"),
                },
                repro(),
                &value_with_error("0", Side::C, msg),
                &value_with_error("0", Side::Rust, msg),
                &mut shipped(),
            );
            assert_eq!(
                case.verdict,
                Verdict::Errored,
                "{msg} says the measurement never happened"
            );
        }
    }

    /// The bucket compares the case like any other, so it cannot become a place
    /// a difference hides: a shared no-completion with different readbacks is
    /// still a DEFECT.
    #[test]
    fn a_difference_under_a_shared_no_completion_is_still_a_defect() {
        let case = put_case(
            &value_with_put("0", PutOutcome::NeverCompleted),
            &value_with_put("7", PutOutcome::NeverCompleted),
        );
        assert_eq!(case.verdict, Verdict::Defect);
    }

    /// `ai.VAL` is `DBF_DOUBLE`. CBUG-E2 is `dbConvert`'s double->**integer**
    /// cast, so nothing about it justifies a difference here: C reporting `0`
    /// where the port reports `inf` is a DEFECT, and the run must fail on it.
    /// This is the exact shape of the 32 `over-double-max` cases the row used to
    /// absorb (`FINDINGS.md:133`).
    #[test]
    fn an_out_of_range_double_into_a_double_field_is_a_defect_not_an_expected_deviation() {
        let case = adjudicate(
            CaseRef {
                record_type: "ai",
                phase: CasePhase::Put,
                field: "VAL",
                dbf: DbfType::Double,
                class: Some("over-double-max"),
            },
            repro(),
            &value("0"),
            &value("inf"),
            &mut shipped(),
        );
        assert_eq!(
            case.verdict,
            Verdict::Defect,
            "CBUG-E2 describes a double->integer cast; ai.VAL is DBF_DOUBLE, \
             so it must not justify this. allowlisted={:?}",
            case.allowlisted
        );
        // The bucket the run's exit code is computed from.
        assert_eq!(Counts::tally(&[case]).defect, 1);
    }

    /// The other half of the same rule: on an **integer** destination the row is
    /// exactly what justifies the difference, so this must stay an EXPECTED
    /// DEVIATION. Narrowing the row must not have blinded the harness to the bug
    /// it was written for.
    #[test]
    fn the_same_class_on_an_integer_field_is_still_expected_deviation() {
        let case = adjudicate(
            CaseRef {
                record_type: "longin",
                phase: CasePhase::Put,
                field: "VAL",
                dbf: DbfType::Long,
                class: Some("over-max"),
            },
            repro(),
            &value("-2147483648"),
            &value("2147483647"),
            &mut shipped(),
        );
        assert_eq!(case.verdict, Verdict::ExpectedDeviation);
        assert_eq!(case.allowlisted, ["CBUG-E2"]);
        assert_eq!(Counts::tally(&[case]).defect, 0);
    }

    /// A put that never reached a server must score ERROR and fail the run.
    ///
    /// The measurement failure is planted for real: both `CaTools` are aimed at
    /// a port nothing is listening on, so `caput` cannot connect. Before this,
    /// every `Err` from the tool became a not-accepted `PutOutcome`, the
    /// two sides "agreed" the write had been refused, and the case was scored
    /// AGREED and counted as put coverage — an agreement claim about a write
    /// neither IOC ever saw.
    #[test]
    fn a_put_that_never_reached_a_server_is_an_error_not_a_refusal() {
        let tools = CTools::discover().expect(
            "the C EPICS tree must be built for the oracle to have ground truth; \
             set EPICS_BASE_BIN if it is not at the default path",
        );
        let dead = crate::ioc::alloc_free_port().expect("a port to aim at");

        let side_err = |side: Side| -> ToolError {
            CaTools::new(&tools, dead, side)
                .caput("ORACLE:NOSUCHPV.VAL", "1")
                .expect_err("a put that never reached a server is not a refusal")
        };
        let obs = |e: ToolError| Observation {
            errors: vec![e],
            ..Default::default()
        };

        let case = adjudicate(
            CaseRef {
                record_type: "ai",
                phase: CasePhase::Put,
                field: "VAL",
                dbf: DbfType::Double,
                class: Some("over-max"),
            },
            repro(),
            &obs(side_err(Side::C)),
            &obs(side_err(Side::Rust)),
            &mut shipped(),
        );
        assert_eq!(case.verdict, Verdict::Errored, "{:?}", case.errors);

        let counts = Counts::tally(&[case]);
        assert_eq!(counts.agreed, 0, "an unrun experiment is not agreement");
        assert_eq!(
            crate::report::exit_status(&crate::report::run_failures(&counts, &[], &[])),
            1,
        );
    }

    /// An unreadable `NORD` must ERROR the array case, not vanish from it.
    ///
    /// `NORD` is the array phase's discriminator: C truncates an over-capacity
    /// put to `NELM`, and a port that rejects it or writes past the end often
    /// differs in nothing else. Dropping it with `.ok()` left both sides with
    /// `value_numeric: None`, so `compare` skipped the surface and the
    /// truncation contract was reported AGREED while unverified.
    ///
    /// The plant is a record type that genuinely has no `NORD`: `ai`. The put
    /// and every other surface read fine, so only the missing discriminator is
    /// under test.
    #[cfg(tokio_backend)]
    #[test]
    fn an_unreadable_nord_errors_the_array_case_instead_of_agreeing() {
        let tools = CTools::discover().expect(
            "the C EPICS tree must be built for the oracle to have ground truth; \
             set EPICS_BASE_BIN if it is not at the default path",
        );
        let dir = workdir(None).expect("workdir");
        let db = dir.join("array_nord.db");
        std::fs::write(&db, crate::record_stmt("ai", "ORACLE:ARR:0")).expect("write db");
        let pair = Pair::boot(&tools, &db, "ORACLE:ARR:0").expect("both IOCs must boot");

        let plan: Vec<(Vec<String>, &'static str)> =
            vec![(vec!["1".to_string()], "array-single-element")];
        let rec_of = |_: usize| "ORACLE:ARR:0".to_string();
        let side = |port: u16, s: Side| drive_array(&CaTools::new(&tools, port, s), &plan, &rec_of);
        let obs_c = side(pair.c.port(), Side::C);
        let obs_r = side(pair.rust.port(), Side::Rust);
        assert!(
            obs_c[0].value_numeric.is_none() && obs_r[0].value_numeric.is_none(),
            "ai has no NORD, so neither side can produce the discriminator"
        );

        let case = adjudicate(
            CaseRef {
                record_type: "ai",
                phase: CasePhase::Array,
                field: "VAL",
                dbf: DbfType::Double,
                class: Some("array-single-element"),
            },
            repro(),
            &obs_c[0],
            &obs_r[0],
            &mut shipped(),
        );
        assert_eq!(
            case.verdict,
            Verdict::Errored,
            "a missing discriminator is not an agreement"
        );
        let counts = Counts::tally(&[case]);
        assert_eq!(
            crate::report::exit_status(&crate::report::run_failures(&counts, &[], &[])),
            1,
        );
    }

    /// A `.STAT` PV that will not connect must ERROR **its own case**, and only
    /// its own case.
    ///
    /// Both halves matter. `caget_batch` is all-or-nothing, so the old
    /// `caget_batch(stat_pvs, false).ok()` turned one unconnectable `.STAT`
    /// into `stat: None` for every case of the record type: `compare` then
    /// skipped the alarm surface on all of them and they stayed AGREED, so a
    /// port that stopped serving STAT reported as an improvement. The other
    /// half is that the fix must not paint every case ERROR — the good PV in
    /// the same batch is still measured, via `probe_bisect`.
    #[cfg(tokio_backend)]
    #[test]
    fn a_stat_pv_that_does_not_connect_errors_only_its_own_case() {
        let tools = CTools::discover().expect(
            "the C EPICS tree must be built for the oracle to have ground truth; \
             set EPICS_BASE_BIN if it is not at the default path",
        );
        let dir = workdir(None).expect("workdir");
        let db_text = format!(
            "{}{}",
            crate::record_stmt("ai", "ORACLE:RB:0"),
            crate::record_stmt("ai", "ORACLE:RB:1")
        );
        let db = dir.join("readback_stat.db");
        std::fs::write(&db, &db_text).expect("write db");
        let pair = Pair::boot(&tools, &db, "ORACLE:RB:0").expect("both IOCs must boot");

        let val_pvs = ["ORACLE:RB:0.VAL".to_string(), "ORACLE:RB:1.VAL".to_string()];
        // Case 1's STAT does not exist on either side: the reading cannot be
        // taken, which is not the same as the two sides agreeing about it.
        let stat_pvs = [
            "ORACLE:RB:0.STAT".to_string(),
            "ORACLE:NOSUCHRECORD.STAT".to_string(),
        ];
        let sevr_pvs = [
            "ORACLE:RB:0.SEVR".to_string(),
            "ORACLE:RB:1.SEVR".to_string(),
        ];
        let accepted = || vec![Ok(PutOutcome::Completed), Ok(PutOutcome::Completed)];

        let side = |port: u16, s: Side| {
            let t = CaTools::new(&tools, port, s);
            readback(&t, &val_pvs, &stat_pvs, &sevr_pvs, accepted())
        };
        let obs_c = side(pair.c.port(), Side::C);
        let obs_r = side(pair.rust.port(), Side::Rust);

        let verdict = |i: usize| {
            adjudicate(
                CaseRef {
                    record_type: "ai",
                    phase: CasePhase::Put,
                    field: "VAL",
                    dbf: DbfType::Double,
                    class: Some("over-max"),
                },
                repro(),
                &obs_c[i],
                &obs_r[i],
                &mut shipped(),
            )
        };

        let measured = verdict(0);
        assert_ne!(
            measured.verdict,
            Verdict::Errored,
            "the case whose STAT connected must still be measured: {:?}",
            measured.errors
        );
        assert!(
            obs_c[0].stat.is_some() && obs_r[0].stat.is_some(),
            "the alarm surface must be read where it can be"
        );

        let lost = verdict(1);
        assert_eq!(
            lost.verdict,
            Verdict::Errored,
            "an unread STAT is not an agreement"
        );
        let counts = Counts::tally(&[lost]);
        assert_eq!(
            crate::report::exit_status(&crate::report::run_failures(&counts, &[], &[])),
            1,
        );
    }

    /// The CA monitor probe reads the same drive rule as the PVA one.
    ///
    /// `sel.VAL` is `special(SPC_NOMOD)`, so no client can stimulate it and the
    /// case never existed on the PVA side — while the CA side built it, drove
    /// it, watched both servers refuse, and scored ERROR every run. The rule now
    /// has one owner ([`crate::surface::val_status`]); this fails if a second
    /// predicate is ever inlined here.
    ///
    /// No IOC is booted: the exclusion is decided from the `.dbd` before any
    /// `.db` is written, which is the point of it being static.
    #[cfg(tokio_backend)]
    #[test]
    fn the_monitor_probe_builds_no_case_for_a_val_the_dbd_forbids_writing() {
        const DBD: &str = r#"
recordtype(sel) {
    field(VAL, DBF_DOUBLE) { prompt("Result") special(SPC_NOMOD) }
}
recordtype(ai) {
    field(VAL, DBF_DOUBLE) { pp(TRUE) }
}
"#;
        let dbd = Dbd::parse(DBD).expect("parse");
        let types: std::collections::BTreeSet<String> =
            ["sel", "ai"].iter().map(|s| s.to_string()).collect();
        let surface = Surface::build(&dbd, &types);
        let tools = CTools::discover().expect(
            "the C EPICS tree must be built for the oracle to have ground truth; \
             set EPICS_BASE_BIN if it is not at the default path",
        );
        let runner = Runner::new(tools, dbd, workdir(None).expect("workdir"));

        assert!(
            runner
                .probe_monitor("sel", &surface, &mut shipped())
                .is_none(),
            "the .dbd forbids writing sel.VAL, so there is nothing to stimulate"
        );
    }

    /// A monitor case whose **drive** failed must score ERROR.
    ///
    /// The plant is a real refusal against the real pair: `NAME` is
    /// `special(SPC_NOMOD)`, so the put is refused on both sides, the
    /// subscription is never stimulated, and both traces are empty. Before
    /// this, the drive's outcome was discarded, `compare` found no difference
    /// between two empty traces, and the case was scored AGREED and counted as
    /// monitor coverage — a positive agreement claim about a subscription that
    /// was never given anything to post on.
    #[cfg(tokio_backend)]
    #[test]
    fn a_monitor_whose_drive_was_refused_scores_error_not_agreement() {
        let tools = CTools::discover().expect(
            "the C EPICS tree must be built for the oracle to have ground truth; \
             set EPICS_BASE_BIN if it is not at the default path",
        );
        let dir = workdir(None).expect("workdir");
        let db = dir.join("mon_drive_refused.db");
        std::fs::write(&db, crate::record_stmt("ai", "ORACLE:MONDRIVE")).expect("write db");
        let pair = Pair::boot(&tools, &db, "ORACLE:MONDRIVE").expect("both IOCs must boot");

        let pvs = vec!["ORACLE:MONDRIVE".to_string()];
        let obs = |port: u16, side: Side| -> Observation {
            let t = CaTools::new(&tools, port, side);
            match t.monitor(&pvs, MONITOR_SETTLE, MONITOR_CONNECT_TIMEOUT, |tt| {
                tt.caput_drive("ORACLE:MONDRIVE.NAME", "NOPE")
            }) {
                Ok(tr) => Observation {
                    monitor: Some(tr),
                    ..Default::default()
                },
                Err(e) => Observation {
                    errors: vec![e],
                    ..Default::default()
                },
            }
        };
        let c = obs(pair.c.port(), Side::C);
        let r = obs(pair.rust.port(), Side::Rust);

        let case = adjudicate(
            CaseRef {
                record_type: "ai",
                phase: CasePhase::Monitor,
                field: "VAL",
                dbf: DbfType::Double,
                class: None,
            },
            repro(),
            &c,
            &r,
            &mut shipped(),
        );
        assert_eq!(
            case.verdict,
            Verdict::Errored,
            "c_monitor={:?} rust_monitor={:?}",
            c.monitor,
            r.monitor
        );

        let counts = Counts::tally(&[case]);
        assert_eq!(
            counts.agreed, 0,
            "an unstimulated subscription is not agreement"
        );
        assert_eq!(
            crate::report::exit_status(&crate::report::run_failures(&counts, &[], &[])),
            1,
        );
    }
}

#[cfg(test)]
mod workdir_tests {
    use super::{reap_own_residue, workdir};

    /// Two writers must not be able to name one file.
    ///
    /// The boundary this closes is *within* a process, which is the half a
    /// per-pid directory would leave open: `cargo test` runs the tests of one
    /// binary as threads, so two `Runner`s built a microsecond apart would both
    /// write `ai.db`. The write below is the same shape the runner uses —
    /// `fs::write` into a directory it was handed — and it must not be able to
    /// reach the other one's file.
    #[test]
    fn two_workdirs_do_not_share_a_file_name() {
        let a = workdir(None).expect("workdir a");
        let b = workdir(None).expect("workdir b");
        assert_ne!(a.path(), b.path(), "each call owns its own directory");

        std::fs::write(a.join("ai.db"), "record(ai, \"A\") {}\n").expect("write a");
        std::fs::write(b.join("ai.db"), "record(ai, \"B\") {}\n").expect("write b");

        assert!(
            std::fs::read_to_string(a.join("ai.db"))
                .expect("read a")
                .contains("\"A\""),
            "the second writer truncated the first writer's db"
        );
    }

    /// The owner path of the invariant: what a handle mints, its drop takes
    /// away.
    ///
    /// This is the half that was missing, and its absence is what turned the
    /// pid from a key over the living into a key over every pid ever issued:
    /// 3,916 directories on this host, none of them owned by anything.
    #[test]
    fn a_dropped_workdir_leaves_nothing_behind() {
        let path = {
            let d = workdir(None).expect("workdir");
            std::fs::write(d.join("ai.db"), "record(ai, \"A\") {}\n").expect("write");
            d.to_path_buf()
        };
        assert!(
            !path.exists(),
            "a workdir outlived its handle: {}",
            path.display()
        );
    }

    /// The other arm of the same rule: a directory the caller supplied is the
    /// caller's, so the handle must not take it away with it.
    #[test]
    fn a_caller_supplied_base_survives_the_handle() {
        let base = workdir(None).expect("outer");
        let inner = base.join("supplied");
        {
            let d = workdir(Some(&inner)).expect("workdir on a supplied base");
            std::fs::write(d.join("ai.db"), "record(ai, \"A\") {}\n").expect("write");
        }
        assert!(
            inner.join("ai.db").exists(),
            "a caller-supplied base was removed by the handle that borrowed it"
        );
    }

    /// The formerly-bypassing path, and the one the box actually hit: a run
    /// that died left `<pid>-<n>` behind, a later process drew the same pid,
    /// and `create_dir` refused. Refusing was right while nothing could tell a
    /// ghost from an owner; now something can, because a leaf named for this
    /// pid that predates this process's first mint has no live owner by
    /// construction.
    ///
    /// Asserted on the reaper directly rather than through `workdir`, because
    /// the reap runs once per process before the first mint and a test cannot
    /// place itself before that point in a shared binary. The other pid's leaf
    /// is here to pin the trailing dash: reaping by a bare numeric prefix would
    /// make pid 1234 delete 12345's live directory.
    #[test]
    fn the_reaper_takes_this_process_s_residue_and_nothing_else() {
        let root = workdir(None).expect("a root to work in");
        let mine = root.join(format!("{}-99", std::process::id()));
        let neighbour = root.join(format!("{}9-0", std::process::id()));
        for d in [&mine, &neighbour] {
            std::fs::create_dir(d).expect("stand up a leaf");
            std::fs::write(d.join("ai.db"), "record(ai, \"DEAD\") {}\n").expect("leaf db");
        }

        reap_own_residue(&root);

        assert!(
            !mine.exists(),
            "this process's own residue survived the reap: {}",
            mine.display()
        );
        assert!(
            neighbour.exists(),
            "the reaper took a leaf belonging to another pid: {}",
            neighbour.display()
        );
    }

    /// After the reap, an occupied path is a racer and not a ghost, so the
    /// exclusive create still has to refuse it — adopting it is the truncation
    /// the whole function exists to prevent. Deterministic under nextest, which
    /// gives each test its own process and so its own counter.
    #[test]
    fn a_racer_on_the_next_path_is_refused_not_adopted() {
        // Learn where the counter is, then stand a directory on the very next
        // path this process will ask for.
        let first = workdir(None).expect("workdir");
        let parent = first.parent().expect("a parent").to_path_buf();
        let n: usize = first
            .file_name()
            .and_then(|f| f.to_str())
            .and_then(|f| f.rsplit('-').next())
            .and_then(|f| f.parse().ok())
            .expect("a counted leaf");
        let taken = parent.join(format!("{}-{}", std::process::id(), n + 1));
        std::fs::create_dir(&taken).expect("stand up the racer's workdir");
        std::fs::write(taken.join("ai.db"), "record(ai, \"RACER\") {}\n").expect("racer db");

        let got = workdir(None);
        assert!(
            got.is_err(),
            "an occupied workdir was adopted, not refused: {got:?}"
        );
        // The other writer's file must still be untouched underneath.
        assert!(
            std::fs::read_to_string(taken.join("ai.db"))
                .expect("read racer")
                .contains("RACER"),
            "the refused call still wrote into the occupied directory"
        );
        std::fs::remove_dir_all(&taken).expect("clean up the racer's workdir");
    }

    /// The cross-process half: the path carries this process's id, so no other
    /// live process can produce it. Asserted on the shape rather than by
    /// spawning a second process, because a pid is unique among the living by
    /// construction and there is nothing racy left to observe.
    #[test]
    fn a_workdir_is_named_for_the_process_that_owns_it() {
        let d = workdir(None).expect("workdir");
        let leaf = d
            .file_name()
            .expect("a leaf")
            .to_str()
            .expect("utf8")
            .to_string();
        assert!(
            leaf.starts_with(&format!("{}-", std::process::id())),
            "a workdir must be named for its owning process, got {leaf}"
        );
        assert_eq!(
            d.parent().and_then(|p| p.file_name()),
            Some(std::ffi::OsStr::new("epics-oracle")),
            "and it must still live under the harness's own root"
        );
    }
}
