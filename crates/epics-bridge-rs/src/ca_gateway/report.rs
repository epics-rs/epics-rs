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
//!   `gateServer.cc:955-979` (`as->report(fp)` → `gateAs::report`,
//!   `gateAs.cc:760-828`): the `.pvlist` Allowed/Denied/Denied-from-host
//!   tables, the evaluation order, the rules-installed flags, then the
//!   parsed UAG/HAG/ASG/RULE access-security dump
//!   (`asDumpFP(fp, NULL, NULL, TRUE)`).
//!
//! R3 renders from the gateway's *active* parsed structures — the live
//! [`super::pvlist::PvList`] and the parsed
//! [`epics_base_rs::server::access_security::AccessSecurityConfig`]
//! (dumped via its `dump_report()`, shared with the `asdbdump` iocsh
//! command) — never the raw `.access` file text, which could be stale
//! relative to the live rules after a hot reload (the defect this
//! closes). The one piece C's
//! verbose `asDumpFP(..., TRUE)` adds that R3 omits is the live
//! AS-member/client listing: this gateway models no `asgMemberList`
//! (see the `aspmem` iocsh command note), so the dump covers the parsed
//! configuration structures only.

use std::io::Write;
use std::path::Path;

use super::cache::PvState;
use super::pvlist::{EvaluationOrder, PvList, PvListEntry};

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

/// Render the **R3** access-security report from the gateway's *active*
/// parsed structures, mirroring C `gateAs::report()`
/// (`gateAs.cc:760-828`): the `.pvlist` Allowed/Denied/Denied-from-host
/// tables, the evaluation order, the rules-installed flags, then the
/// parsed UAG/HAG/ASG/RULE access-security dump.
///
/// - `pvlist` is the live parsed rule list (ALLOW/DENY/ALIAS entries plus
///   the evaluation order).
/// - `as_dump` is [`super::access::AccessConfig::dump_report`] output:
///   `Some` when an `.access` file was loaded, `None` for the file-less
///   read-only / allow-all default (no parsed structures to dump).
///   `as_dump.is_some()` also distinguishes C's `rules_installed` from
///   `use_default_rules` (`gateAs.cc:816-817`).
///
/// Patterns are shown as the compiled match regex actually enforced
/// (`^(?:…)$`, GNU-BRE-translated — see [`super::pvlist`]), i.e. the
/// active admission test rather than the raw `.pvlist` source line. The
/// raw `.access` file text the old R3 emitted is deliberately dropped: it
/// could be stale relative to the live rules after a hot reload.
pub fn render_r3(
    mode_summary: &str,
    acf_path: Option<&str>,
    pvlist: &PvList,
    as_dump: Option<&str>,
) -> String {
    let mut out = String::from("==== ca-gateway-rs R3 (access security report) ====\n");
    out.push_str(&format!("mode: {mode_summary}\n"));
    out.push_str(&format!("acf source: {}\n", acf_path.unwrap_or("(none)")));

    // ---- Allowed PV report (.pvlist ALLOW + ALIAS rules) ----
    // C "Allowed PV Report" (gateAs.cc:767-777): pattern, ASG, ASL, alias.
    out.push_str("--- allowed PV report ---\n");
    out.push_str(&format!(
        "  {:<30} {:<16} {:>3} {}\n",
        "pattern", "asg", "asl", "alias"
    ));
    for entry in &pvlist.entries {
        match entry {
            PvListEntry::Allow { pattern, asg, asl } => {
                out.push_str(&format!(
                    "  {:<30} {:<16} {:>3}\n",
                    pattern.as_str(),
                    asg_label(asg),
                    asl.unwrap_or(1),
                ));
            }
            PvListEntry::Alias {
                pattern,
                target_template,
                asg,
                asl,
            } => {
                out.push_str(&format!(
                    "  {:<30} {:<16} {:>3} {}\n",
                    pattern.as_str(),
                    asg_label(asg),
                    asl.unwrap_or(1),
                    target_template,
                ));
            }
            PvListEntry::Deny { .. } => {}
        }
    }

    // ---- Denied PV report ----
    // C "Denied PV Report" (gateAs.cc:779-809): global denies, then
    // per-host denies grouped by host.
    out.push_str("--- denied PV report ---\n");
    let global: Vec<&str> = pvlist
        .entries
        .iter()
        .filter_map(|e| match e {
            PvListEntry::Deny {
                pattern,
                from_hosts,
            } if from_hosts.is_empty() => Some(pattern.as_str()),
            _ => None,
        })
        .collect();
    if !global.is_empty() {
        out.push_str("  denied from ALL hosts:\n");
        for p in global {
            out.push_str(&format!("    {p}\n"));
        }
    }
    // Per-host denies, grouped by host in a stable (sorted) order.
    let mut by_host: std::collections::BTreeMap<&str, Vec<&str>> =
        std::collections::BTreeMap::new();
    for entry in &pvlist.entries {
        if let PvListEntry::Deny {
            pattern,
            from_hosts,
        } = entry
        {
            for h in from_hosts {
                by_host
                    .entry(h.as_str())
                    .or_default()
                    .push(pattern.as_str());
            }
        }
    }
    for (host, patterns) in &by_host {
        out.push_str(&format!("  denied from host {host}:\n"));
        for p in patterns {
            out.push_str(&format!("    {p}\n"));
        }
    }

    // ---- Evaluation order + rules-installed flags (gateAs.cc:811-817) ----
    out.push_str(match pvlist.order {
        EvaluationOrder::DenyAllow => "evaluation order: deny, allow\n",
        EvaluationOrder::AllowDeny => "evaluation order: allow, deny\n",
    });
    if as_dump.is_some() {
        out.push_str("access security rules are installed.\n");
    } else {
        out.push_str("using default access rules.\n");
    }

    // ---- Parsed access-security dump (UAG/HAG/ASG/RULE) ----
    // C `asDumpFP(fp, NULL, NULL, TRUE)` (gateAs.cc:821-822). The verbose
    // member/client listing is not reproduced (no live AS-member registry).
    out.push_str("--- access security dump ---\n");
    match as_dump {
        Some(dump) if !dump.is_empty() => {
            out.push_str(dump);
            if !dump.ends_with('\n') {
                out.push('\n');
            }
        }
        Some(_) => out.push_str("(no UAG/HAG/ASG defined)\n"),
        None => out.push_str("(no .access file loaded — default rules in effect)\n"),
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

    fn sample_pvlist() -> PvList {
        use regex::Regex;
        PvList {
            order: EvaluationOrder::DenyAllow,
            entries: vec![
                PvListEntry::Allow {
                    pattern: Regex::new("^(?:BEAM:.*)$").unwrap(),
                    asg: Some("RWGROUP".to_string()),
                    asl: Some(1),
                },
                PvListEntry::Alias {
                    pattern: Regex::new("^(?:GW:(.*))$").unwrap(),
                    target_template: "UPSTREAM:$1".to_string(),
                    asg: None,
                    asl: None,
                },
                // Global (FROM-less) deny.
                PvListEntry::Deny {
                    pattern: Regex::new("^(?:SECRET:.*)$").unwrap(),
                    from_hosts: vec![],
                },
                // Host-targeted deny (from_hosts hold resolved addresses).
                PvListEntry::Deny {
                    pattern: Regex::new("^(?:LOCAL:.*)$").unwrap(),
                    from_hosts: vec!["10.0.0.5".to_string()],
                },
            ],
        }
    }

    #[test]
    fn r3_renders_active_pvlist_and_as_dump() {
        let as_dump = "UAG(ops)\n\talice\nASG(DEFAULT)\n\tRULE(1,WRITE)\n\t\tUAG(ops)\n";
        let out = render_r3(
            "rules parsed from .access file",
            Some("/etc/gw/access.acf"),
            &sample_pvlist(),
            Some(as_dump),
        );
        assert!(out.contains("R3 (access security report)"));
        assert!(out.contains("mode: rules parsed from .access file"));
        assert!(out.contains("acf source: /etc/gw/access.acf"));

        // Allowed report: ALLOW with its ASG/ASL, and the ALIAS target.
        assert!(out.contains("--- allowed PV report ---"));
        assert!(out.contains("^(?:BEAM:.*)$"));
        assert!(out.contains("RWGROUP"));
        // ALIAS renders its target; an omitted ASG defaults to DEFAULT,
        // an omitted ASL defaults to 1.
        assert!(out.contains("^(?:GW:(.*))$"));
        assert!(out.contains("UPSTREAM:$1"));

        // Denied report: global deny under ALL hosts, host deny grouped.
        assert!(out.contains("--- denied PV report ---"));
        assert!(out.contains("denied from ALL hosts:"));
        assert!(out.contains("^(?:SECRET:.*)$"));
        assert!(out.contains("denied from host 10.0.0.5:"));
        assert!(out.contains("^(?:LOCAL:.*)$"));

        // Evaluation order reflects the parsed PvList order.
        assert!(out.contains("evaluation order: deny, allow"));
        // as_dump present => rules installed.
        assert!(out.contains("access security rules are installed."));

        // Parsed AS dump appears verbatim, not the raw .access file text.
        assert!(out.contains("--- access security dump ---"));
        assert!(out.contains("UAG(ops)"));
        assert!(out.contains("RULE(1,WRITE)"));
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn r3_without_acf_uses_default_rules_note() {
        // No .access file => no parsed dump => "using default access rules"
        // and the default-rules note instead of a UAG/HAG/ASG dump.
        let out = render_r3(
            "read-only default (ASG(DEFAULT){RULE(1,READ)}, no .access file)",
            None,
            &PvList::new(),
            None,
        );
        assert!(out.contains("acf source: (none)"));
        assert!(out.contains("using default access rules."));
        assert!(out.contains("(no .access file loaded — default rules in effect)"));
        // The old behaviour (raw .access file dump) must be gone.
        assert!(!out.contains("acf file contents"));
    }

    #[test]
    fn r3_loaded_acf_with_empty_structures_notes_absence() {
        // .access file loaded (as_dump Some) but it defined no UAG/HAG/ASG.
        let out = render_r3(
            "rules parsed from .access file",
            Some("/etc/gw/empty.acf"),
            &PvList::new(),
            Some(""),
        );
        assert!(out.contains("access security rules are installed."));
        assert!(out.contains("(no UAG/HAG/ASG defined)"));
    }
}
