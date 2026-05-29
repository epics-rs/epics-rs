//! C-compatible R1/R2/R3 report rendering + append-to-file.
//!
//! C ca-gateway's SIGUSR1 command file accepts `R1`/`R2`/`R3` and its
//! SIGUSR2 shortcut runs `R2`; each opens the configured `-report` file
//! in *append* mode, writes a section, and closes it
//! (`gateServer.cc:689-979`). The default report file is `gateway.report`
//! (`docs/Gateway.html:821-830`). The Rust port previously turned these
//! into terse one-line `tracing` strings, so runbooks that
//! `kill -USR1`/`-USR2` and then read `gateway.report` found nothing.
//!
//! This module renders the three sections from snapshots the command
//! handler collects (cache + pvlist + stats + access), and appends them
//! to the report file. The render functions are pure (snapshot in,
//! `String` out) so they unit-test without a live gateway; the handler
//! does the async cache read and the file IO.
//!
//! Report ↔ C mapping:
//!
//! - **R1** = `report1()` — the stats block plus one line per *virtual
//!   connection* (each served PV; the Rust cache table is exactly C's
//!   `vcTable`), `gateServer.cc:689-734`.
//! - **R2** = `report2()` — counts by state, then the PV inventory
//!   grouped by Connecting/Dead/Disconnect/Inactive/Active with each
//!   PV's AS group and level, `gateServer.cc:736-953`. SIGUSR2 shortcut,
//!   `gateServer.cc:2403-2407`.
//! - **R3** = `report3()` — the access-security report,
//!   `gateServer.cc:955-979` (`as->report(fp)`).
//!
//! R3 limitation: C's `as->report(fp)` dumps the parsed UAG/HAG/ASG/RULE
//! structures. `epics-base-rs`'s `AccessSecurityConfig` exposes no
//! enumeration of those structures, and adding one is outside this
//! change's crate lease, so R3 reports the gateway's effective access
//! *mode* and the verbatim `.access` file contents — the same
//! configuration an operator reads, sourced from the path the gateway
//! already holds for reloads.

use std::io::Write;
use std::path::Path;

use super::cache::PvState;

/// Stats counters captured at report time (read from the live
/// [`super::stats::Stats`] atomics by the command handler).
#[derive(Debug, Clone)]
pub struct StatsSnapshot {
    pub prefix: String,
    pub client_event_count: u64,
    pub post_event_count: u64,
    pub exist_test_count: u64,
    pub put_count: u64,
    pub read_only_rejects: u64,
    pub loop_count: u64,
    pub heartbeat: u64,
    pub connected_hosts: usize,
}

/// One served PV (a "virtual connection" in C terms), with the AS group
/// and level resolved from the current pvlist match.
#[derive(Debug, Clone)]
pub struct PvReportEntry {
    pub name: String,
    pub state: PvState,
    pub subscribers: usize,
    pub events: u64,
    /// AS group from the matched pvlist rule; `None` renders as `DEFAULT`.
    pub asg: Option<String>,
    /// Effective AS level (pvlist `effective_asl`, C-default 1).
    pub asl: i32,
    /// Alias target, when the matched rule rewrote the name.
    pub resolved_name: Option<String>,
}

/// State groups in C report2's print order
/// (`gateServer.cc` walks Connecting, Dead, Disconnect, Inactive, Active).
const R2_GROUP_ORDER: [PvState; 5] = [
    PvState::Connecting,
    PvState::Dead,
    PvState::Disconnect,
    PvState::Inactive,
    PvState::Active,
];

fn stats_block(stats: &StatsSnapshot) -> String {
    format!(
        "stats prefix={prefix}\n  \
         clientEventCount={cec} postEventCount={pec} existTestCount={etc} \
         putCount={put} readOnlyRejects={ror} loopCount={loops} heartbeat={hb} \
         connectedHosts={hosts}\n",
        prefix = stats.prefix,
        cec = stats.client_event_count,
        pec = stats.post_event_count,
        etc = stats.exist_test_count,
        put = stats.put_count,
        ror = stats.read_only_rejects,
        loops = stats.loop_count,
        hb = stats.heartbeat,
        hosts = stats.connected_hosts,
    )
}

fn asg_label(asg: &Option<String>) -> &str {
    match asg {
        Some(g) if !g.is_empty() => g.as_str(),
        _ => "DEFAULT",
    }
}

/// Render the **R1** PV report: stats block followed by one line per
/// served PV (virtual connection).
pub fn render_r1(stats: &StatsSnapshot, pvs: &[PvReportEntry]) -> String {
    let mut out = String::from("==== ca-gateway-rs R1 (PV report) ====\n");
    out.push_str(&stats_block(stats));
    out.push_str(&format!("virtual connections ({}):\n", pvs.len()));
    for pv in pvs {
        out.push_str(&format!(
            "  {} state={:?} subscribers={} events={}",
            pv.name, pv.state, pv.subscribers, pv.events
        ));
        if let Some(resolved) = &pv.resolved_name {
            if resolved != &pv.name {
                out.push_str(&format!(" -> {resolved}"));
            }
        }
        out.push('\n');
    }
    out
}

/// Render the **R2** process-variable report: counts by state, then the
/// PV inventory grouped by state with AS group and level.
pub fn render_r2(stats: &StatsSnapshot, pvs: &[PvReportEntry]) -> String {
    let count_in = |state: PvState| pvs.iter().filter(|p| p.state == state).count();
    let mut out = String::from("==== ca-gateway-rs R2 (process variable report) ====\n");
    out.push_str(&format!(
        "total PVs={total} connecting={c} dead={d} disconnect={x} inactive={i} active={a}\n",
        total = pvs.len(),
        c = count_in(PvState::Connecting),
        d = count_in(PvState::Dead),
        x = count_in(PvState::Disconnect),
        i = count_in(PvState::Inactive),
        a = count_in(PvState::Active),
    ));
    out.push_str(&format!("existTestCount={}\n", stats.exist_test_count));
    for group in R2_GROUP_ORDER {
        let members: Vec<&PvReportEntry> = pvs.iter().filter(|p| p.state == group).collect();
        out.push_str(&format!("[{:?}] {}\n", group, members.len()));
        for pv in members {
            out.push_str(&format!(
                "  {} asg={} level={} subscribers={} events={}\n",
                pv.name,
                asg_label(&pv.asg),
                pv.asl,
                pv.subscribers,
                pv.events
            ));
        }
    }
    out
}

/// Render the **R3** access-security report: the effective mode plus the
/// verbatim `.access` configuration (see the module-level R3 note on why
/// this is the file contents rather than a parsed-structure dump).
pub fn render_r3(mode_summary: &str, acf_path: Option<&str>, acf_content: Option<&str>) -> String {
    let mut out = String::from("==== ca-gateway-rs R3 (access security report) ====\n");
    out.push_str(&format!("mode: {mode_summary}\n"));
    out.push_str(&format!("acf source: {}\n", acf_path.unwrap_or("(none)")));
    out.push_str("--- acf file contents ---\n");
    match acf_content {
        Some(content) => {
            out.push_str(content);
            if !content.ends_with('\n') {
                out.push('\n');
            }
        }
        None => out.push_str("(no .access file configured)\n"),
    }
    out
}

/// Append a rendered report section to `path`, creating it if absent.
/// Mirrors C's open-append-write-close per report (`gateServer.cc:689`).
pub fn append_report(path: &Path, section: &str) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(section.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_stats() -> StatsSnapshot {
        StatsSnapshot {
            prefix: "gw".to_string(),
            client_event_count: 7,
            post_event_count: 5,
            exist_test_count: 11,
            put_count: 3,
            read_only_rejects: 1,
            loop_count: 99,
            heartbeat: 42,
            connected_hosts: 2,
        }
    }

    fn sample_pvs() -> Vec<PvReportEntry> {
        vec![
            PvReportEntry {
                name: "BEAM:CURRENT".to_string(),
                state: PvState::Active,
                subscribers: 4,
                events: 100,
                asg: Some("DEFAULT".to_string()),
                asl: 1,
                resolved_name: None,
            },
            PvReportEntry {
                name: "GW:ALIAS".to_string(),
                state: PvState::Inactive,
                subscribers: 0,
                events: 0,
                asg: Some("RWGROUP".to_string()),
                asl: 0,
                resolved_name: Some("UPSTREAM:REAL".to_string()),
            },
            PvReportEntry {
                name: "DEAD:PV".to_string(),
                state: PvState::Dead,
                subscribers: 0,
                events: 0,
                asg: None,
                asl: 1,
                resolved_name: None,
            },
        ]
    }

    #[test]
    fn r1_has_stats_block_and_virtual_connection_lines() {
        let out = render_r1(&sample_stats(), &sample_pvs());
        assert!(out.contains("R1 (PV report)"));
        assert!(out.contains("clientEventCount=7"));
        assert!(out.contains("existTestCount=11"));
        assert!(out.contains("virtual connections (3):"));
        assert!(out.contains("BEAM:CURRENT state=Active subscribers=4 events=100"));
        // alias target rendered for the rewritten name
        assert!(out.contains("GW:ALIAS state=Inactive subscribers=0 events=0 -> UPSTREAM:REAL"));
    }

    #[test]
    fn r2_groups_by_state_with_asg_and_level() {
        let out = render_r2(&sample_stats(), &sample_pvs());
        assert!(out.contains("R2 (process variable report)"));
        assert!(out.contains("total PVs=3 connecting=0 dead=1 disconnect=0 inactive=1 active=1"));
        assert!(out.contains("existTestCount=11"));
        // groups appear in C's print order
        let dead_pos = out.find("[Dead]").expect("Dead group");
        let active_pos = out.find("[Active]").expect("Active group");
        let inactive_pos = out.find("[Inactive]").expect("Inactive group");
        assert!(dead_pos < inactive_pos && inactive_pos < active_pos);
        // a None asg renders as DEFAULT; an explicit group is preserved
        assert!(out.contains("DEAD:PV asg=DEFAULT level=1"));
        assert!(out.contains("GW:ALIAS asg=RWGROUP level=0"));
    }

    #[test]
    fn r3_emits_mode_and_acf_contents() {
        let out = render_r3(
            "rules from file",
            Some("/etc/gw/access.acf"),
            Some("ASG(DEFAULT) {\n  RULE(1, READ)\n}"),
        );
        assert!(out.contains("R3 (access security report)"));
        assert!(out.contains("mode: rules from file"));
        assert!(out.contains("acf source: /etc/gw/access.acf"));
        assert!(out.contains("RULE(1, READ)"));
        // trailing newline normalised even when the ACF lacked one
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn r3_without_acf_file_notes_absence() {
        let out = render_r3("read-only default", None, None);
        assert!(out.contains("acf source: (none)"));
        assert!(out.contains("(no .access file configured)"));
    }
}
