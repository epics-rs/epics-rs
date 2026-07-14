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
use crate::dbd::Dbd;
use crate::diff::{Comparison, Observation, Verdict, compare};
use crate::ioc::{CTools, Ioc, Pair, Side};
use crate::report::{CaseResult, Reproducer};
use crate::surface::{Surface, is_put_candidate};

/// How long to let monitor updates settle after driving the puts. A port that
/// posts *extra* events must be caught, so we cannot stop listening the moment
/// the puts return.
const MONITOR_SETTLE: Duration = Duration::from_millis(600);
const MONITOR_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

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
        let db_text = format!("record({record_type}, \"{rec}\") {{}}\n");

        let fields: Vec<String> = surface
            .fields_of(record_type)
            .map(|f| f.field.name.clone())
            .collect();
        if fields.is_empty() {
            return Vec::new();
        }
        let pvs: Vec<String> = fields.iter().map(|f| format!("{rec}.{f}")).collect();

        let db = match self.write_db(&format!("read_{record_type}"), &db_text) {
            Ok(p) => p,
            Err(e) => return errored_cases(record_type, &fields, None, &db_text, &e),
        };
        let pair = match Pair::boot(&self.tools, &db) {
            Ok(p) => p,
            // The IOC would not boot: every field of this type is an ERROR, and
            // not one of them is scored as agreement.
            Err(e) => {
                return errored_cases(record_type, &fields, None, &db_text, &e.to_string());
            }
        };

        let c = CaTools::new(&self.tools, pair.c.port(), Side::C);
        let r = CaTools::new(&self.tools, pair.rust.port(), Side::Rust);
        let obs_c = read_observations(&c, &pvs);
        let obs_r = read_observations(&r, &pvs);

        fields
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let repro = Reproducer {
                    db: db_text.clone(),
                    ops: vec![format!("caget {rec}.{f}"), format!("cainfo {rec}.{f}")],
                };
                adjudicate(record_type, f, None, repro, &obs_c[i], &obs_r[i], allowlist)
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
        let mut plan: Vec<(String, BoundaryCase)> = Vec::new();
        for fr in surface.fields_of(record_type) {
            if !is_put_candidate(&fr.field) {
                continue;
            }
            let choices = self.dbd.menu_choices(&fr.field);
            for bc in boundary_cases(&fr.field, choices) {
                plan.push((fr.field.name.clone(), bc));
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
            db_text.push_str(&format!("record({record_type}, \"{}\") {{}}\n", rec_of(i)));
        }

        let names: Vec<String> = plan.iter().map(|(f, _)| f.clone()).collect();
        let classes: Vec<&str> = plan.iter().map(|(_, b)| b.class).collect();

        let db = match self.write_db(&format!("put_{record_type}"), &db_text) {
            Ok(p) => p,
            Err(e) => return errored_puts(record_type, &plan, &db_text, &e),
        };
        let pair = match Pair::boot(&self.tools, &db) {
            Ok(p) => p,
            Err(e) => return errored_puts(record_type, &plan, &db_text, &e.to_string()),
        };

        let c = CaTools::new(&self.tools, pair.c.port(), Side::C);
        let r = CaTools::new(&self.tools, pair.rust.port(), Side::Rust);

        // Drive the puts on both sides, then batch the readbacks. The two sides
        // see the identical sequence.
        let puts_c = drive_puts(&c, &plan, &rec_of);
        let puts_r = drive_puts(&r, &plan, &rec_of);

        let val_pvs: Vec<String> = plan
            .iter()
            .enumerate()
            .map(|(i, (f, _))| format!("{}.{f}", rec_of(i)))
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
            .map(|(i, (_, bc))| {
                let rec = rec_of(i);
                let f = &names[i];
                let repro = Reproducer {
                    // The minimal db is ONE record -- the other instances exist
                    // only to isolate the other cases in the same run.
                    db: format!("record({record_type}, \"{rec}\") {{}}\n"),
                    ops: vec![
                        format!("caput {rec}.{f} '{}'", bc.value),
                        format!("caget {rec}.{f} {rec}.STAT {rec}.SEVR"),
                    ],
                };
                adjudicate(
                    record_type,
                    f,
                    Some(classes[i]),
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

        let rec = format!("ORACLE:MON:{}", record_type.to_uppercase());
        let db_text = format!("record({record_type}, \"{rec}\") {{}}\n");
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
            Err(e) => return Some(errored_case(record_type, "VAL", None, repro, &e)),
        };
        let pair = match Pair::boot(&self.tools, &db) {
            Ok(p) => p,
            Err(e) => {
                return Some(errored_case(
                    record_type,
                    "VAL",
                    None,
                    repro,
                    &e.to_string(),
                ));
            }
        };

        let pvs = vec![rec.clone()];
        let drive = |t: &CaTools| {
            for v in seq {
                t.caput(&rec, v);
                // Space the puts so that a server which *does* post per change
                // has time to emit each one; without this, two puts inside one
                // scan period could legitimately coalesce on BOTH sides and the
                // probe would measure nothing.
                std::thread::sleep(Duration::from_millis(120));
            }
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
            record_type,
            "VAL",
            None,
            repro,
            &obs_c,
            &obs_r,
            allowlist,
        ))
    }
}

/// Read the type/shape/value surface for a list of PVs.
///
/// Fast path: one batched `caget`/`cainfo`. If the batch fails (the C tools are
/// all-or-nothing on a bad PV), fall back to per-PV probes so the failure is
/// attributed to the exact field that caused it — never to the whole batch, and
/// never silently dropped.
fn read_observations(t: &CaTools, pvs: &[String]) -> Vec<Observation> {
    let mut obs: Vec<Observation> = vec![Observation::default(); pvs.len()];

    match t.caget_batch(pvs, false) {
        Ok(vals) => {
            for (o, v) in obs.iter_mut().zip(vals) {
                o.value_string = Some(v);
            }
        }
        Err(_) => {
            for (o, r) in obs.iter_mut().zip(t.caget_each(pvs, false)) {
                match r {
                    Ok(v) => o.value_string = Some(v),
                    Err(e) => o.errors.push(e),
                }
            }
        }
    }
    match t.caget_batch(pvs, true) {
        Ok(vals) => {
            for (o, v) in obs.iter_mut().zip(vals) {
                o.value_numeric = Some(v);
            }
        }
        Err(_) => {
            for (o, r) in obs.iter_mut().zip(t.caget_each(pvs, true)) {
                match r {
                    Ok(v) => o.value_numeric = Some(v),
                    Err(e) => o.errors.push(e),
                }
            }
        }
    }
    match t.cainfo_batch(pvs) {
        Ok(infos) => {
            for (o, i) in obs.iter_mut().zip(infos) {
                o.info = Some(i);
            }
        }
        Err(_) => {
            for (o, pv) in obs.iter_mut().zip(pvs) {
                match t.cainfo(pv) {
                    Ok(i) => o.info = Some(i),
                    Err(e) => o.errors.push(e),
                }
            }
        }
    }
    obs
}

fn drive_puts(
    t: &CaTools,
    plan: &[(String, BoundaryCase)],
    rec_of: &impl Fn(usize) -> String,
) -> Vec<PutOutcome> {
    plan.iter()
        .enumerate()
        .map(|(i, (f, bc))| t.caput(&format!("{}.{f}", rec_of(i)), &bc.value))
        .collect()
}

/// Batch the post-put readback: what each side stored, and what alarm it raised.
fn readback(
    t: &CaTools,
    val_pvs: &[String],
    stat_pvs: &[String],
    sevr_pvs: &[String],
    puts: Vec<PutOutcome>,
) -> Vec<Observation> {
    let mut obs = read_observations(t, val_pvs);
    let stats = t.caget_batch(stat_pvs, false).ok();
    let sevrs = t.caget_batch(sevr_pvs, false).ok();
    for (i, o) in obs.iter_mut().enumerate() {
        o.put = puts.get(i).cloned();
        o.stat = stats.as_ref().and_then(|s| s.get(i).cloned());
        o.sevr = sevrs.as_ref().and_then(|s| s.get(i).cloned());
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
fn adjudicate(
    record_type: &str,
    field: &str,
    class: Option<&str>,
    repro: Reproducer,
    c: &Observation,
    r: &Observation,
    allowlist: &mut Allowlist,
) -> CaseResult {
    let mut errors: Vec<ToolError> = Vec::new();
    errors.extend(c.errors.iter().cloned());
    errors.extend(r.errors.iter().cloned());

    let base = CaseResult {
        record_type: record_type.to_string(),
        field: field.to_string(),
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
    class: Option<&str>,
    repro: Reproducer,
    msg: &str,
) -> CaseResult {
    CaseResult {
        record_type: record_type.to_string(),
        field: field.to_string(),
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
    plan: &[(String, BoundaryCase)],
    db: &str,
    msg: &str,
) -> Vec<CaseResult> {
    plan.iter()
        .map(|(f, bc)| {
            errored_case(
                record_type,
                f,
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
