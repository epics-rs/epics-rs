//! The measuring instrument: the **C** CA client tools, pointed at one IOC.
//!
//! Both sides of the differential pair are driven through these same binaries
//! (`caget`, `caput`, `cainfo`, `camonitor` from the built C tree). That is
//! deliberate and it is the core of the method:
//!
//! - It removes `epics-ca-rs`'s *client* from the experiment. If the harness
//!   drove the Rust IOC with the Rust client, a client-side bug would surface
//!   as a server-side "diff" and we would chase the wrong thing.
//! - It measures the contract we actually owe. Tier 1 says "a C client must
//!   not be able to tell the difference" — so the honest experiment is to put
//!   a real C client in front of both and see whether it can.
//!
//! Every tool invocation is bounded by a timeout and every non-zero exit is
//! surfaced as a [`ToolError`], never as an empty-but-successful reading. A
//! measurement that did not happen must never be scored as agreement.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::ioc::{CTools, Side};

/// A tool run that did not produce a usable reading.
///
/// Carries the [`Side`] it happened on. Without that, an ERROR case says only
/// "somebody could not read this field" -- which is useless for acting on,
/// because "C does not serve this field" and "the port does not serve this
/// field" are opposite findings.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ToolError {
    pub side: Side,
    pub tool: String,
    pub message: String,
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}: {}", self.side, self.tool, self.message)
    }
}

/// Wall-clock cap on any single tool invocation. The tools take `-w` (CA
/// timeout) but that governs the CA search, not the process, so we bound the
/// process too and treat an overrun as an error.
const TOOL_TIMEOUT: Duration = Duration::from_secs(8);
/// CA connect/read timeout handed to the tools via `-w`.
const CA_TIMEOUT_SECS: &str = "2";

/// The C CA tools aimed at exactly one IOC.
///
/// The env pins the client to `127.0.0.1:<port>` with `EPICS_CA_AUTO_ADDR_LIST=NO`,
/// so a search can only ever be answered by the IOC we mean to measure — never
/// by a stray IOC on the host, and never by the *other* side of the pair.
#[derive(Debug, Clone)]
pub struct CaTools {
    bin: PathBuf,
    port: u16,
    side: Side,
}

impl CaTools {
    pub fn new(tools: &CTools, port: u16, side: Side) -> Self {
        Self {
            bin: tools.bin.clone(),
            port,
            side,
        }
    }

    /// Which IOC these tools are aimed at.
    pub fn side(&self) -> Side {
        self.side
    }

    fn err(&self, tool: &str, message: impl Into<String>) -> ToolError {
        ToolError {
            side: self.side,
            tool: tool.to_string(),
            message: message.into(),
        }
    }

    fn env(&self) -> HashMap<&'static str, String> {
        HashMap::from([
            ("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{}", self.port)),
            ("EPICS_CA_AUTO_ADDR_LIST", "NO".to_string()),
            ("EPICS_CA_SERVER_PORT", self.port.to_string()),
        ])
    }

    /// Run a tool to completion, returning stdout on success.
    ///
    /// A non-zero exit is an error carrying stderr, because that is exactly how
    /// a rejected put or an unconnectable PV reports itself, and both are
    /// findings rather than absences.
    fn run(&self, tool: &str, args: &[String]) -> Result<String, ToolError> {
        let mut cmd = Command::new(self.bin.join(tool));
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in self.env() {
            cmd.env(k, v);
        }
        let child = cmd
            .spawn()
            .map_err(|e| self.err(tool, format!("spawn: {e}")))?;
        let out = wait_bounded(child, TOOL_TIMEOUT).map_err(|e| self.err(tool, e))?;

        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let msg = if stderr.is_empty() {
                stdout.trim().to_string()
            } else {
                stderr
            };
            return Err(self.err(
                tool,
                if msg.is_empty() {
                    format!("exit {:?}", out.status.code())
                } else {
                    msg
                },
            ));
        }
        Ok(stdout)
    }

    /// `caget -t <pv>` — the value as a client sees it by default. For an
    /// enum/menu field this is the **choice string**, which is the thing that
    /// has to match; a port agreeing on the ordinal but not the string is
    /// observably wrong.
    pub fn caget_string(&self, pv: &str) -> Result<String, ToolError> {
        let out = self.run(
            "caget",
            &[
                "-t".into(),
                "-w".into(),
                CA_TIMEOUT_SECS.into(),
                pv.to_string(),
            ],
        )?;
        Ok(out.trim().to_string())
    }

    /// `caget -t -n <pv>` — enum/menu fields as their **ordinal** instead of
    /// their string. Diffing both forms separates "wrong number" from "right
    /// number, wrong label", which are different port defects.
    pub fn caget_numeric(&self, pv: &str) -> Result<String, ToolError> {
        let out = self.run(
            "caget",
            &[
                "-t".into(),
                "-n".into(),
                "-w".into(),
                CA_TIMEOUT_SECS.into(),
                pv.to_string(),
            ],
        )?;
        Ok(out.trim().to_string())
    }

    /// `caget -t -# <n> <pv>` — the first `n` elements of an array.
    pub fn caget_array(&self, pv: &str, count: u32) -> Result<String, ToolError> {
        let out = self.run(
            "caget",
            &[
                "-t".into(),
                "-#".into(),
                count.to_string(),
                "-w".into(),
                CA_TIMEOUT_SECS.into(),
                pv.to_string(),
            ],
        )?;
        Ok(out.split_whitespace().collect::<Vec<_>>().join(" "))
    }

    /// `caget -t` over many PVs at once, returning one reading per PV, in order.
    ///
    /// Batching is what makes full-surface coverage affordable: one spawn reads
    /// a whole record type's fields instead of sixty. But the C tools are
    /// **all-or-nothing** on a connect failure — one unconnectable PV in the
    /// batch and `caget` prints nothing at all for the others and exits 1. So a
    /// failed batch tells us *something* broke but not *what*, and the caller
    /// must fall back to per-PV probing to attribute it (see
    /// [`CaTools::caget_each`]). A short read is treated the same way: if the
    /// line count does not match the PV count we do not know which value
    /// belongs to which PV, and guessing would silently mis-attribute readings.
    pub fn caget_batch(&self, pvs: &[String], numeric: bool) -> Result<Vec<String>, ToolError> {
        if pvs.is_empty() {
            return Ok(Vec::new());
        }
        let mut args: Vec<String> = vec!["-t".into(), "-w".into(), CA_TIMEOUT_SECS.into()];
        if numeric {
            args.push("-n".into());
        }
        args.extend(pvs.iter().cloned());
        let out = self.run("caget", &args)?;
        // `-t` prints exactly one line per PV (an empty field yields an empty
        // line), so a count mismatch means the mapping is unknown.
        let lines: Vec<String> = out.lines().map(|l| l.trim().to_string()).collect();
        if lines.len() != pvs.len() {
            return Err(self.err(
                "caget",
                format!(
                    "batch returned {} lines for {} PVs — cannot attribute readings",
                    lines.len(),
                    pvs.len()
                ),
            ));
        }
        Ok(lines)
    }

    /// Probe each PV on its own, so a failure is attributed to the exact PV
    /// that caused it. Slow; used only to explain a failed batch.
    pub fn caget_each(&self, pvs: &[String], numeric: bool) -> Vec<Result<String, ToolError>> {
        pvs.iter()
            .map(|pv| {
                if numeric {
                    self.caget_numeric(pv)
                } else {
                    self.caget_string(pv)
                }
            })
            .collect()
    }

    /// `cainfo` over many PVs at once. Same all-or-nothing caveat as
    /// [`CaTools::caget_batch`]; the caller falls back to per-PV on failure.
    pub fn cainfo_batch(&self, pvs: &[String]) -> Result<Vec<CaInfo>, ToolError> {
        if pvs.is_empty() {
            return Ok(Vec::new());
        }
        let mut args: Vec<String> = vec!["-w".into(), CA_TIMEOUT_SECS.into()];
        args.extend(pvs.iter().cloned());
        let out = self.run("cainfo", &args)?;
        let infos = parse_cainfo_batch(&out);
        if infos.len() != pvs.len() {
            return Err(self.err(
                "cainfo",
                format!(
                    "batch returned {} blocks for {} PVs — cannot attribute readings",
                    infos.len(),
                    pvs.len()
                ),
            ));
        }
        Ok(infos)
    }

    /// `caput` — returns whether the write was **accepted**, and the server's
    /// complaint if it was not.
    ///
    /// Put accept/reject is itself observable behavior (a `SPC_NOMOD` field
    /// must refuse the write, an out-of-range enum must refuse it), so a
    /// rejection is a *reading*, not a failure of the harness.
    pub fn caput(&self, pv: &str, value: &str) -> PutOutcome {
        match self.run(
            "caput",
            &[
                "-t".into(),
                "-w".into(),
                CA_TIMEOUT_SECS.into(),
                pv.to_string(),
                value.to_string(),
            ],
        ) {
            Ok(_) => PutOutcome {
                accepted: true,
                error: None,
            },
            Err(e) => PutOutcome {
                accepted: false,
                error: Some(normalize_ca_error(&e.message)),
            },
        }
    }

    /// `caput -a` — array put (`-a` takes a count then the elements).
    pub fn caput_array(&self, pv: &str, values: &[String]) -> PutOutcome {
        let mut args: Vec<String> = vec![
            "-t".into(),
            "-a".into(),
            "-w".into(),
            CA_TIMEOUT_SECS.into(),
            pv.to_string(),
            values.len().to_string(),
        ];
        args.extend(values.iter().cloned());
        match self.run("caput", &args) {
            Ok(_) => PutOutcome {
                accepted: true,
                error: None,
            },
            Err(e) => PutOutcome {
                accepted: false,
                error: Some(normalize_ca_error(&e.message)),
            },
        }
    }

    /// `cainfo` — native DBF type, element count, and access rights.
    pub fn cainfo(&self, pv: &str) -> Result<CaInfo, ToolError> {
        let out = self.run(
            "cainfo",
            &["-w".into(), CA_TIMEOUT_SECS.into(), pv.to_string()],
        )?;
        parse_cainfo(&out).ok_or_else(|| self.err("cainfo", format!("unparseable cainfo: {out:?}")))
    }
}

/// One `camonitor` update: an event the server *chose* to post.
///
/// The timestamp is deliberately **not** captured. Two IOCs will never agree on
/// wall-clock, so including it would make every comparison differ; what has to
/// match is which updates were posted, in what order, with what payload.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MonitorEvent {
    pub pv: String,
    /// The value as printed, whitespace-normalized (arrays become `n e1 e2 ...`).
    pub value: String,
    /// Alarm status/severity when the update carried them, e.g. `HIGH MAJOR`.
    pub alarm: Option<String>,
}

/// The observed monitor stream for one operation sequence.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct MonitorTrace {
    /// Updates posted *after* the initial connection update, in order.
    pub events: Vec<MonitorEvent>,
    /// The connection-time update(s) camonitor prints on subscribe. Separated
    /// out because they establish the baseline rather than reflecting the
    /// driven operations.
    pub initial: Vec<MonitorEvent>,
}

impl CaTools {
    /// Subscribe with `camonitor`, run `drive`, and return the posted updates.
    ///
    /// The ordering here is the whole experiment, so it is made deterministic
    /// rather than left to luck:
    ///
    /// 1. spawn `camonitor` on `pvs`;
    /// 2. **block until every PV has produced its initial update** — this
    ///    proves the subscription is established. Driving before that point
    ///    would race the subscription and lose events, and the harness would
    ///    then report a monitor-count difference that is an artifact of its own
    ///    timing rather than a difference in the IOC;
    /// 3. run `drive` (the puts);
    /// 4. hold the window open for `settle` so late/coalesced updates are
    ///    counted — a port that posts *extra* events must be caught, so we
    ///    cannot stop listening the instant the puts return;
    /// 5. kill the subscriber and parse.
    ///
    /// A PV that never produces an initial update inside `connect_timeout` is a
    /// [`ToolError`] — the case then scores ERROR, not agreement.
    pub fn monitor<F>(
        &self,
        pvs: &[String],
        settle: Duration,
        connect_timeout: Duration,
        drive: F,
    ) -> Result<MonitorTrace, ToolError>
    where
        F: FnOnce(&CaTools),
    {
        use std::io::{BufRead, BufReader};
        use std::sync::mpsc;

        let terr = |m: String| self.err("camonitor", m);

        let mut cmd = Command::new(self.bin.join("camonitor"));
        cmd.args(pvs)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in self.env() {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().map_err(|e| terr(format!("spawn: {e}")))?;

        // Stream stdout on a thread: camonitor never exits on its own, so it
        // can't go through wait_bounded, and an undrained pipe would wedge it.
        let stdout = child.stdout.take().expect("piped");
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });

        // (2) Sync point: one initial update per PV proves every subscription
        // is live before any operation is driven.
        let mut initial = Vec::new();
        let deadline = std::time::Instant::now() + connect_timeout;
        while initial.len() < pvs.len() {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                let _ = child.kill();
                let _ = child.wait();
                let seen: Vec<_> = initial.iter().map(|e: &MonitorEvent| &e.pv).collect();
                return Err(terr(format!(
                    "only {}/{} PVs connected within {connect_timeout:?} (got {seen:?}) \
                     — cannot attribute a monitor diff, scoring ERROR",
                    initial.len(),
                    pvs.len()
                )));
            }
            match rx.recv_timeout(left) {
                Ok(line) => {
                    if let Some(ev) = parse_monitor_line(&line) {
                        initial.push(ev);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(terr("camonitor exited before connecting".into()));
                }
            }
        }

        // (3) drive the operations, then (4) hold the window open.
        drive(self);
        std::thread::sleep(settle);

        // (5) stop and collect everything the subscriber managed to print.
        let _ = child.kill();
        let _ = child.wait();
        let mut events = Vec::new();
        while let Ok(line) = rx.try_recv() {
            if let Some(ev) = parse_monitor_line(&line) {
                events.push(ev);
            }
        }
        Ok(MonitorTrace { events, initial })
    }
}

/// Parse one camonitor line into an event, dropping the timestamp.
///
/// The timestamp comes in **two** shapes, and handling only the first is a trap:
///
/// ```text
/// T:AI   2026-07-13 10:00:00.123456 1.5          <- processed record: date + time
/// T:AI   <undefined> 1.5 UDF NO_ALARM            <- never processed: ONE token
/// ```
///
/// A record that has not been processed has an undefined timestamp, which
/// `camonitor` prints as the single token `<undefined>`. Every freshly-booted
/// record is in exactly that state, so a parser that insists on a `date time`
/// pair silently drops the *initial* update of every PV — and then both sides
/// report zero events and the diff scores a cheerful AGREED. That is a false
/// clean of precisely the kind this harness exists to eliminate, so the shape is
/// handled explicitly rather than assumed away.
fn parse_monitor_line(line: &str) -> Option<MonitorEvent> {
    let mut it = line.split_whitespace().peekable();
    let pv = it.next()?.to_string();

    // A disconnect is observable behavior and must be recorded, not dropped.
    // Checked before the timestamp, because camonitor prints it as
    // `PV <undefined> DISCONNECTED` — it carries the undefined-timestamp shape
    // and would otherwise be parsed as an update whose value is the word
    // DISCONNECTED.
    if line.contains("DISCONNECTED") {
        return Some(MonitorEvent {
            pv,
            value: "<DISCONNECTED>".into(),
            alarm: None,
        });
    }

    // Consume the timestamp, whichever shape it took.
    let first = *it.peek()?;
    if first == "<undefined>" {
        it.next();
    } else if first.len() == 10 && first.as_bytes()[4] == b'-' {
        it.next(); // date
        it.next()?; // time
    } else {
        return None;
    }

    let rest: Vec<&str> = it.collect();
    if rest.is_empty() {
        return None;
    }
    // A trailing `STAT SEVR` pair is alarm metadata, not value. Severity words
    // are a closed set, so the split is unambiguous.
    const SEVERITIES: [&str; 4] = ["NO_ALARM", "MINOR", "MAJOR", "INVALID"];
    let (value, alarm) = match rest.len() >= 2 && SEVERITIES.contains(&rest[rest.len() - 1]) {
        true => {
            let split = rest.len() - 2;
            (rest[..split].join(" "), Some(rest[split..].join(" ")))
        }
        false => (rest.join(" "), None),
    };
    Some(MonitorEvent { pv, value, alarm })
}

/// Did the server accept the write, and if not, what did it say?
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PutOutcome {
    pub accepted: bool,
    /// The server's rejection, normalized so that host/port/timing noise does
    /// not masquerade as a behavioral difference.
    pub error: Option<String>,
}

/// What `cainfo` reports about a channel — the type/shape/rights surface.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CaInfo {
    /// e.g. `DBF_DOUBLE`
    pub native_type: String,
    pub element_count: u64,
    /// e.g. `read, write` / `read` / `no access`
    pub access: String,
}

/// Split a multi-PV `cainfo` dump into one [`CaInfo`] per channel block.
///
/// Each block starts at a non-indented PV-name line; the fields follow indented.
fn parse_cainfo_batch(out: &str) -> Vec<CaInfo> {
    let mut blocks: Vec<String> = Vec::new();
    for line in out.lines() {
        let is_header = !line.starts_with(char::is_whitespace) && !line.trim().is_empty();
        if is_header {
            blocks.push(String::new());
        }
        if let Some(b) = blocks.last_mut() {
            b.push_str(line);
            b.push('\n');
        }
    }
    blocks.iter().filter_map(|b| parse_cainfo(b)).collect()
}

fn parse_cainfo(out: &str) -> Option<CaInfo> {
    let mut native_type = None;
    let mut element_count = None;
    let mut access = None;
    for line in out.lines() {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("Native data type:") {
            native_type = Some(v.trim().to_string());
        } else if let Some(v) = t.strip_prefix("Element count:") {
            element_count = v.trim().parse::<u64>().ok();
        } else if let Some(v) = t.strip_prefix("Access:") {
            access = Some(v.trim().to_string());
        }
    }
    Some(CaInfo {
        native_type: native_type?,
        element_count: element_count?,
        access: access?,
    })
}

/// Strip host/port/timing noise from a CA error so that two servers refusing a
/// put *for the same reason* compare equal even though their messages carry
/// different endpoints.
///
/// This normalizes only provably-incidental text. The reason phrase itself is
/// preserved, so a port that rejects with the wrong *reason* still shows up as
/// a diff.
fn normalize_ca_error(msg: &str) -> String {
    let mut out = String::new();
    for line in msg.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        // Drop the endpoint/timestamp tail the CA client library appends.
        let t = t.split(", context ").next().unwrap_or(t);
        let t = t.split(" at ").next().unwrap_or(t);
        if !out.is_empty() {
            out.push_str(" | ");
        }
        out.push_str(t.trim());
    }
    out
}

/// Run a child to completion, killing it if it overruns `timeout`.
///
/// A hung tool must become an ERROR, never a stall and never an empty reading,
/// so the deadline is enforced by killing the child and reaping it — the
/// process is owned here throughout, so the kill always lands.
///
/// The pipes are only drained after exit. That is safe because every tool
/// routed through here (`caget`/`caput`/`cainfo`) writes a handful of lines,
/// far below the OS pipe buffer. The one tool that can emit unbounded output —
/// `camonitor` — is deliberately *not* run through this path; it streams
/// (see [`monitor`]).
fn wait_bounded(
    mut child: std::process::Child,
    timeout: Duration,
) -> Result<std::process::Output, String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("timed out after {timeout:?}"));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(format!("wait: {e}")),
        }
    }
    child
        .wait_with_output()
        .map_err(|e| format!("collect output: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cainfo_surface() {
        let out = "\
T:AI
    State:            connected
    Host:             localhost:33309
    Access:           read, write
    Native data type: DBF_DOUBLE
    Request type:     DBR_DOUBLE
    Element count:    1
";
        let info = parse_cainfo(out).expect("parse");
        assert_eq!(info.native_type, "DBF_DOUBLE");
        assert_eq!(info.element_count, 1);
        assert_eq!(info.access, "read, write");
    }

    #[test]
    fn parses_readonly_access() {
        let out = "    Access:           read\n    Native data type: DBF_STRING\n    Element count:    1\n";
        let info = parse_cainfo(out).unwrap();
        assert_eq!(info.access, "read");
    }

    #[test]
    fn missing_field_makes_cainfo_unparseable_rather_than_defaulted() {
        // A truncated cainfo must NOT silently yield element_count = 0; that
        // would be a fabricated reading.
        assert!(parse_cainfo("    Access: read\n").is_none());
    }

    #[test]
    fn error_normalization_keeps_the_reason_drops_the_endpoint() {
        let raw = "Write access denied, context \"127.0.0.1:33309\"";
        assert_eq!(normalize_ca_error(raw), "Write access denied");
    }

    #[test]
    fn cainfo_batch_splits_one_block_per_channel() {
        let out = "\
T:AI.VAL
    Access:           read, write
    Native data type: DBF_DOUBLE
    Element count:    1
T:BI.VAL
    Access:           read
    Native data type: DBF_ENUM
    Element count:    1
";
        let infos = parse_cainfo_batch(out);
        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].native_type, "DBF_DOUBLE");
        assert_eq!(infos[1].native_type, "DBF_ENUM");
        assert_eq!(infos[1].access, "read");
    }

    #[test]
    fn monitor_line_drops_timestamp_keeps_value() {
        let ev = parse_monitor_line("T:AI    2026-07-13 10:00:00.123456 1.5").expect("parse");
        assert_eq!(ev.pv, "T:AI");
        assert_eq!(ev.value, "1.5");
        assert_eq!(ev.alarm, None);
    }

    /// A never-processed record has an undefined timestamp, printed as ONE
    /// token. Every freshly-booted record is in this state, so dropping this
    /// shape loses the initial update of every PV — and a monitor diff of
    /// nothing-vs-nothing reads as agreement.
    #[test]
    fn monitor_line_handles_the_undefined_timestamp_of_an_unprocessed_record() {
        let ev = parse_monitor_line("T:AI    <undefined> 1.5 UDF NO_ALARM").expect("parse");
        assert_eq!(ev.pv, "T:AI");
        assert_eq!(ev.value, "1.5");
        assert_eq!(ev.alarm.as_deref(), Some("UDF NO_ALARM"));
    }

    #[test]
    fn monitor_line_splits_alarm_from_value() {
        let ev =
            parse_monitor_line("T:AI  2026-07-13 10:00:00.123456 9.9 HIGH MAJOR").expect("parse");
        assert_eq!(ev.value, "9.9");
        assert_eq!(ev.alarm.as_deref(), Some("HIGH MAJOR"));
    }

    #[test]
    fn monitor_line_keeps_array_payload_whole() {
        let ev = parse_monitor_line("T:WF  2026-07-13 10:00:00.1 3 1 2 3").expect("parse");
        assert_eq!(ev.value, "3 1 2 3");
        assert_eq!(ev.alarm, None);
    }

    /// An enum update's value is a *string*; it must survive intact so that a
    /// port posting the right ordinal with the wrong label still diffs.
    #[test]
    fn monitor_line_keeps_enum_string() {
        let ev = parse_monitor_line("T:BI  2026-07-13 10:00:00.1 On").expect("parse");
        assert_eq!(ev.value, "On");
    }

    /// A disconnect is observable behavior and must not be silently dropped.
    #[test]
    fn monitor_disconnect_is_recorded_not_swallowed() {
        let ev = parse_monitor_line("T:AI <undefined> DISCONNECTED").expect("parse");
        assert_eq!(ev.value, "<DISCONNECTED>");
    }

    #[test]
    fn error_normalization_preserves_distinct_reasons() {
        // Two different refusals must not normalize to the same string, or the
        // harness would call a wrong-reason rejection an agreement.
        let a = normalize_ca_error("Write access denied, context \"x\"");
        let b = normalize_ca_error("Invalid value, context \"x\"");
        assert_ne!(a, b);
    }
}
