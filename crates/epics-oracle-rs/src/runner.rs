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
use std::time::Duration;

use crate::allowlist::{Allowlist, MatchContext};
use crate::cases::{BoundaryCase, boundary_cases};
use crate::catool::{CaTools, PutOutcome, ToolError};
use crate::dbd::{Dbd, DbfType};
use crate::diff::{Comparison, Observation, Verdict, compare};
use crate::ioc::{CTools, Ioc, Pair, Side};
use crate::report::{CasePhase, CaseResult, Reproducer};
use crate::surface::{FieldRef, Surface, is_put_candidate};

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
    workdir: PathBuf,
}

impl Runner {
    pub fn new(tools: CTools, dbd: Dbd, workdir: PathBuf) -> Self {
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
                return errored_cases(record_type, &names, CasePhase::Read, None, &db_text, &e);
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
                    &e.to_string(),
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
            Err(e) => return errored_puts(record_type, &plan, &db_text, &e),
        };
        let pair = match Pair::boot(&self.tools, &db, &rec_of(0)) {
            Ok(p) => p,
            Err(e) => return errored_puts(record_type, &plan, &db_text, &e.to_string()),
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
    pub fn probe_monitor(
        &self,
        record_type: &str,
        surface: &Surface,
        allowlist: &mut Allowlist,
    ) -> Option<CaseResult> {
        // Only meaningful where VAL is a writable scalar the client can drive.
        let val = surface
            .fields_of(record_type)
            .find(|f| f.field.name == "VAL")?;
        if !is_put_candidate(&val.field) || val.field.dbf.is_link() {
            return None;
        }
        let val_dbf = val.field.dbf;

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
                    &e,
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
                    &e.to_string(),
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
            Err(e) => return errored_array(record_type, &plan, &db_text, &e),
        };
        let pair = match Pair::boot(&self.tools, &db, &rec_of(0)) {
            Ok(p) => p,
            Err(e) => return errored_array(record_type, &plan, &db_text, &e.to_string()),
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
    msg: &str,
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
                msg,
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
        return CaseResult {
            verdict: Verdict::Agreed,
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
    msg: &str,
) -> CaseResult {
    CaseResult {
        record_type: record_type.to_string(),
        field: field.to_string(),
        phase,
        class: class.map(str::to_string),
        verdict: Verdict::Errored,
        differences: Vec::new(),
        allowlisted: Vec::new(),
        // A boot failure is not attributable to one side by construction, so it
        // is recorded against both -- never dropped, never guessed.
        errors: vec![
            ToolError {
                side: Side::C,
                tool: "boot".into(),
                message: msg.to_string(),
            },
            ToolError {
                side: Side::Rust,
                tool: "boot".into(),
                message: msg.to_string(),
            },
        ],
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
    msg: &str,
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
                msg,
            )
        })
        .collect()
}

fn errored_puts(
    record_type: &str,
    plan: &[(FieldRef, BoundaryCase)],
    db: &str,
    msg: &str,
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
                msg,
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

/// The workdir for generated `.db` files.
pub fn workdir(base: Option<&Path>) -> Result<PathBuf, String> {
    let dir = match base {
        Some(p) => p.to_path_buf(),
        None => std::env::temp_dir().join("epics-oracle"),
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    Ok(dir)
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
    /// every `Err` from the tool became `PutOutcome { accepted: false, … }`, the
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
        let accepted = || {
            vec![
                Ok(PutOutcome {
                    accepted: true,
                    error: None,
                }),
                Ok(PutOutcome {
                    accepted: true,
                    error: None,
                }),
            ]
        };

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

    /// A monitor case whose **drive** failed must score ERROR.
    ///
    /// The plant is a real refusal against the real pair: `NAME` is
    /// `special(SPC_NOMOD)`, so the put is refused on both sides, the
    /// subscription is never stimulated, and both traces are empty. Before
    /// this, the drive's outcome was discarded, `compare` found no difference
    /// between two empty traces, and the case was scored AGREED and counted as
    /// monitor coverage — a positive agreement claim about a subscription that
    /// was never given anything to post on.
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
