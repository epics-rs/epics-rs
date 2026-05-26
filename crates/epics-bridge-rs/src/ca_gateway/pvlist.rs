//! `.pvlist` configuration file parser.
//!
//! Corresponds to C++ `gateAs::readPvList` (`gateAs.cc`).
//!
//! ## File format
//!
//! ```text
//! EVALUATION ORDER ALLOW, DENY
//!
//! # comments start with #
//! Beam:.*          ALLOW Beam 1
//! PS.*             ALLOW PowerSupply 1
//! ps([0-9])        ALIAS PSCurrent\1.ai PowerSupply 1
//! test.*           DENY
//! ```
//!
//! Each non-comment line is one of:
//! - `EVALUATION ORDER ALLOW, DENY` (or `DENY, ALLOW`) — sets match order
//! - `pattern ALLOW [asg [asl]]` — allow access, optional access security group/level
//! - `pattern DENY [FROM host1 host2 ...]` — deny access (optional host list)
//! - `pattern ALIAS target [asg [asl]]` — alias to a different upstream PV.
//!   Target may contain backreferences `\0`–`\9` to capture groups.
//!
//! ## Notes
//!
//! - Patterns are full regex (Rust `regex` crate). C++ uses POSIX regex
//!   or PCRE optionally — most simple patterns are compatible.
//! - Backreference substitution is implemented manually because Rust
//!   `regex` doesn't support backreferences in the pattern, but
//!   capture groups are available for replacement.
//! - The DENY `FROM host` clause is host-scoped: it denies only when the
//!   requester host matches. It is enforced at the put-hook path via
//!   [`PvList::is_host_denied`] and at downstream search/create
//!   resolution via [`PvList::match_name_for_host`] (BRIDGE-FR-10, the
//!   parity equivalent of C `gateServer::pvExistTest` →
//!   `gateAs::findEntry(pvname, hostname)`). Host-less callers (preload,
//!   pvlist-reload prune) use [`PvList::match_name`], which by design
//!   sees only global (`FROM`-less) DENY rules — a host-targeted deny
//!   must not remove a PV that is still admissible for other hosts.

use std::path::Path;

use regex::Regex;

use crate::error::{BridgeError, BridgeResult};

/// How to combine ALLOW and DENY rules.
///
/// `AllowDeny` (default): match ALLOW rules first; if any matches, DENY rules
/// can override. `DenyAllow`: match DENY rules first; if any matches, ALLOW
/// rules can override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EvaluationOrder {
    /// `EVALUATION ORDER ALLOW, DENY` (default)
    #[default]
    AllowDeny,
    /// `EVALUATION ORDER DENY, ALLOW`
    DenyAllow,
}

/// One rule in a `.pvlist` file.
#[derive(Debug, Clone)]
pub enum PvListEntry {
    /// `pattern ALLOW [asg [asl]]`
    Allow {
        pattern: Regex,
        asg: Option<String>,
        asl: Option<i32>,
    },
    /// `pattern DENY [FROM host ...]`
    Deny {
        pattern: Regex,
        from_hosts: Vec<String>,
    },
    /// `pattern ALIAS target [asg [asl]]`
    Alias {
        pattern: Regex,
        target_template: String,
        asg: Option<String>,
        asl: Option<i32>,
    },
}

impl PvListEntry {
    fn pattern(&self) -> &Regex {
        match self {
            Self::Allow { pattern, .. } => pattern,
            Self::Deny { pattern, .. } => pattern,
            Self::Alias { pattern, .. } => pattern,
        }
    }

    fn is_allow(&self) -> bool {
        matches!(self, Self::Allow { .. } | Self::Alias { .. })
    }

    /// A DENY rule with NO `FROM host …` list — an unconditional
    /// (global) deny that participates in host-less [`PvList::match_name`].
    ///
    /// BRIDGE-FR-10: a host-targeted `pattern DENY FROM host …` rule is
    /// NOT a global deny. It applies only when the requester host
    /// matches, via the host-aware path ([`PvList::match_name_for_host`]
    /// / [`PvList::is_host_denied`]) — mirroring C ca-gateway's
    /// `gateAs::findEntry`, where the `deny_from_table` is consulted only
    /// when the passed host matches (gateAs.h:257-267). Treating it as a
    /// global deny in host-less matching is exactly the defect FR-10
    /// closes: it over-denies every host with `ALLOW,DENY` order and is
    /// silently bypassed (allow wins) with `DENY,ALLOW` order.
    fn is_global_deny(&self) -> bool {
        matches!(self, Self::Deny { from_hosts, .. } if from_hosts.is_empty())
    }
}

/// Result of matching a PV name against the rule list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PvListMatch {
    /// Resolved upstream PV name (after alias substitution if applicable).
    /// Equal to the input name unless an `ALIAS` rule matched.
    pub resolved_name: String,
    /// Access security group (from rule), if specified.
    pub asg: Option<String>,
    /// Access security level (from rule), if specified.
    pub asl: Option<i32>,
    /// Whether this came from an Alias rule.
    pub is_alias: bool,
}

/// A parsed `.pvlist` file.
#[derive(Debug)]
pub struct PvList {
    pub order: EvaluationOrder,
    pub entries: Vec<PvListEntry>,
}

impl PvList {
    pub fn new() -> Self {
        Self {
            order: EvaluationOrder::default(),
            entries: Vec::new(),
        }
    }

    /// Resolve all `DENY FROM <hostname>` entries to IP address strings.
    ///
    /// Called **once after parsing** (before the `PvList` is put into
    /// service). Mirrors C ca-gateway's `aToIPAddr` pass at pvlist load
    /// time (`gateAs.cc:488–509`):
    ///
    /// - Tokens that are already IP-address literals are kept verbatim.
    /// - Hostnames are resolved via `tokio::net::lookup_host`; a hostname
    ///   that resolves to multiple addresses expands into one IP entry per
    ///   address.
    /// - A hostname that fails to resolve is logged at `WARN` level and
    ///   **dropped** from the deny list (matches C's fail-open behaviour:
    ///   `fprintf(stderr, "cannot resolve host name >%s<\n", ...)`).
    ///
    /// After this call, every non-empty `from_hosts` vec contains only
    /// IP-address strings, so [`Self::is_host_denied`] can compare them
    /// directly against the TCP peer IP that callers pass.
    pub async fn resolve_hosts(&mut self) {
        for entry in &mut self.entries {
            if let PvListEntry::Deny { from_hosts, .. } = entry {
                if from_hosts.is_empty() {
                    continue; // global deny — no hosts to resolve
                }
                let tokens = std::mem::take(from_hosts);
                for token in tokens {
                    // Already an IP literal? Preserve verbatim.
                    if token.parse::<std::net::IpAddr>().is_ok() {
                        from_hosts.push(token);
                        continue;
                    }
                    // Hostname — resolve to IP(s). Append `:0` as the
                    // required port sentinel for lookup_host.
                    match tokio::net::lookup_host(format!("{token}:0")).await {
                        Ok(addrs) => {
                            let mut resolved_any = false;
                            for sa in addrs {
                                from_hosts.push(sa.ip().to_string());
                                resolved_any = true;
                            }
                            if !resolved_any {
                                tracing::warn!(
                                    hostname = %token,
                                    "pvlist DENY FROM: hostname resolved to no addresses \
                                     — entry has no effect (matches C gateAs.cc:504-506)"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                hostname = %token,
                                error = %e,
                                "pvlist DENY FROM: cannot resolve hostname \
                                 — entry has no effect (matches C gateAs.cc:504-506)"
                            );
                        }
                    }
                }
            }
        }
    }

    /// Whether the put from `host` for PV `name` is denied by a host-
    /// targeted DENY rule (`pattern DENY FROM host1 host2 …`).
    ///
    /// **Scope**: only host-targeted DENY rules participate in this
    /// check. Untargeted `pattern DENY` rules are evaluated by
    /// [`Self::match_name`] (which honors `EvaluationOrder`); a put
    /// for a name that `match_name` returns `Some` for has already
    /// passed the search-time policy. `is_host_denied` is therefore
    /// strictly additional — it can only further restrict, never
    /// override an ALLOW.
    ///
    /// This matches C ca-gateway semantics: `DENY FROM host` is a
    /// hard host-blacklist that applies regardless of `EVALUATION
    /// ORDER`. After [`Self::resolve_hosts`] has been called, every
    /// entry in `from_hosts` is an IP-address string, so comparison
    /// is exact (case-insensitive for IPv6 hex digits; IPv4 is all
    /// numeric). Callers must pass the TCP peer IP in bracket-less
    /// form (`192.0.2.1`, `::1`).
    pub fn is_host_denied(&self, name: &str, host: &str) -> bool {
        for entry in &self.entries {
            if let PvListEntry::Deny {
                pattern,
                from_hosts,
            } = entry
            {
                // Untargeted DENY rules are handled by `match_name` +
                // EvaluationOrder; skip them here so we only enforce
                // the strictly-additional host blacklist.
                if from_hosts.is_empty() {
                    continue;
                }
                if !pattern.is_match(name) {
                    continue;
                }
                if from_hosts.iter().any(|h| h.eq_ignore_ascii_case(host)) {
                    return true;
                }
            }
        }
        false
    }

    /// Match a PV name against the rule list, WITHOUT a requester host.
    ///
    /// Returns `Some(PvListMatch)` if the name should be served (allowed,
    /// possibly via alias), or `None` if the name is denied.
    ///
    /// BRIDGE-FR-10: only *global* DENY rules (`pattern DENY`, no `FROM`)
    /// participate here. Host-targeted `pattern DENY FROM host …` rules
    /// are excluded from the deny set — they cannot be evaluated without
    /// a host, and treating them as global denies is the FR-10 defect.
    /// Callers that know the downstream client host MUST use
    /// [`Self::match_name_for_host`] so host-scoped denials are honored;
    /// host-less callers (preload, pvlist-reload prune) intentionally see
    /// only global rules, because a host-targeted deny must not remove a
    /// PV that is still admissible for other hosts.
    pub fn match_name(&self, name: &str) -> Option<PvListMatch> {
        // Find first matching ALLOW (or ALIAS) and first matching global
        // DENY. Host-targeted DENY FROM rules do not participate here.
        let allow_match = self
            .entries
            .iter()
            .find(|e| e.is_allow() && e.pattern().is_match(name));
        let deny_match = self
            .entries
            .iter()
            .find(|e| e.is_global_deny() && e.pattern().is_match(name));

        let allow_decision: Option<PvListMatch> = allow_match.map(|e| match e {
            PvListEntry::Allow { asg, asl, .. } => PvListMatch {
                resolved_name: name.to_string(),
                asg: asg.clone(),
                asl: *asl,
                is_alias: false,
            },
            PvListEntry::Alias {
                pattern,
                target_template,
                asg,
                asl,
            } => {
                let resolved = expand_template(pattern, name, target_template);
                PvListMatch {
                    resolved_name: resolved,
                    asg: asg.clone(),
                    asl: *asl,
                    is_alias: true,
                }
            }
            _ => unreachable!(),
        });

        match self.order {
            EvaluationOrder::AllowDeny => {
                // ALLOW first, DENY can override
                if deny_match.is_some() {
                    None
                } else {
                    allow_decision
                }
            }
            EvaluationOrder::DenyAllow => {
                // DENY first, ALLOW can override.
                // - allow rule matches → grant (overrides any DENY)
                // - allow rule misses → deny (whether or not a DENY rule matched)
                allow_decision
            }
        }
    }

    /// Host-aware name admission for downstream CA search/create
    /// resolution.
    ///
    /// BRIDGE-FR-10: this is the parity equivalent of C ca-gateway's
    /// `gateServer::pvExistTest` calling `gateAs::findEntry(pvname,
    /// hostname)` (gateServer.cc:1537). A host-targeted `pattern DENY
    /// FROM host …` rule is evaluated FIRST and unconditionally: if the
    /// requester `host` matches such a rule for `name`, admission is
    /// denied regardless of `EVALUATION ORDER`, exactly as C consults the
    /// `deny_from_table` before the normal allow/deny decision
    /// (gateAs.h:257-267). Otherwise the normal host-less decision
    /// ([`Self::match_name`], which honors `EVALUATION ORDER` over the
    /// global ALLOW/DENY rules) applies.
    ///
    /// `host` must be the bracket-less socket-address host form derived
    /// from the downstream client's address (`127.0.0.1`, `::1`) so it
    /// matches the `.pvlist` `FROM` syntax — see [`Self::is_host_denied`].
    pub fn match_name_for_host(&self, name: &str, host: &str) -> Option<PvListMatch> {
        // Host-scoped DENY FROM is a hard blacklist consulted before the
        // normal allow/deny decision (mirrors gateAs::findEntry).
        if self.is_host_denied(name, host) {
            return None;
        }
        self.match_name(name)
    }
}

impl Default for PvList {
    fn default() -> Self {
        Self::new()
    }
}

/// Expand `\0`–`\9` backreferences in a template using regex captures.
///
/// `\0` refers to the entire match. `\1`–`\9` refer to capture groups.
fn expand_template(pattern: &Regex, input: &str, template: &str) -> String {
    let captures = match pattern.captures(input) {
        Some(c) => c,
        None => return template.to_string(),
    };

    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            let c = bytes[i + 1];
            if c.is_ascii_digit() {
                let group_idx = (c - b'0') as usize;
                if let Some(g) = captures.get(group_idx) {
                    out.push_str(g.as_str());
                }
                i += 2;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Parse a `.pvlist` file from string content.
pub fn parse_pvlist(content: &str) -> BridgeResult<PvList> {
    let mut list = PvList::new();

    for (lineno, raw) in content.lines().enumerate() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }

        // EVALUATION ORDER directive
        if let Some(rest) = line.strip_prefix("EVALUATION ORDER") {
            let rest = rest.trim();
            if rest.eq_ignore_ascii_case("ALLOW, DENY") || rest.eq_ignore_ascii_case("ALLOW,DENY") {
                list.order = EvaluationOrder::AllowDeny;
            } else if rest.eq_ignore_ascii_case("DENY, ALLOW")
                || rest.eq_ignore_ascii_case("DENY,ALLOW")
            {
                list.order = EvaluationOrder::DenyAllow;
            } else {
                return Err(BridgeError::GroupConfigError(format!(
                    "line {}: invalid EVALUATION ORDER '{}'",
                    lineno + 1,
                    rest
                )));
            }
            continue;
        }

        // Pattern rule: pattern KEYWORD [args...]
        let entry = parse_rule_line(line, lineno + 1)?;
        list.entries.push(entry);
    }

    Ok(list)
}

/// Parse a `.pvlist` file from disk.
pub fn parse_pvlist_file(path: &Path) -> BridgeResult<PvList> {
    let content = std::fs::read_to_string(path)?;
    parse_pvlist(&content)
}

fn strip_comment(line: &str) -> &str {
    match line.find('#') {
        Some(i) => &line[..i],
        None => line,
    }
}

fn parse_rule_line(line: &str, lineno: usize) -> BridgeResult<PvListEntry> {
    let mut tokens = line.split_whitespace();

    let pattern_str = tokens
        .next()
        .ok_or_else(|| BridgeError::GroupConfigError(format!("line {lineno}: missing pattern")))?;
    let keyword = tokens
        .next()
        .ok_or_else(|| BridgeError::GroupConfigError(format!("line {lineno}: missing keyword")))?;

    let pattern = build_pattern(pattern_str, lineno)?;

    match keyword.to_ascii_uppercase().as_str() {
        "ALLOW" => {
            let asg = tokens.next().map(String::from);
            let asl = tokens
                .next()
                .map(|s| {
                    s.parse::<i32>().map_err(|e| {
                        BridgeError::GroupConfigError(format!(
                            "line {lineno}: invalid asl '{s}': {e}"
                        ))
                    })
                })
                .transpose()?;
            Ok(PvListEntry::Allow { pattern, asg, asl })
        }
        "DENY" => {
            // Optional FROM host1 host2 ...
            let mut from_hosts = Vec::new();
            if let Some(t) = tokens.next() {
                if t.eq_ignore_ascii_case("FROM") {
                    for h in tokens {
                        // Stored as-is here; PvList::resolve_hosts() converts
                        // hostnames to IP strings at load time (BR-R53 fix),
                        // mirroring C aToIPAddr (gateAs.cc:488-506).
                        from_hosts.push(h.to_string());
                    }
                } else {
                    return Err(BridgeError::GroupConfigError(format!(
                        "line {lineno}: expected FROM after DENY, got '{t}'"
                    )));
                }
            }
            Ok(PvListEntry::Deny {
                pattern,
                from_hosts,
            })
        }
        "ALIAS" => {
            let target = tokens.next().ok_or_else(|| {
                BridgeError::GroupConfigError(format!(
                    "line {lineno}: ALIAS requires a target name"
                ))
            })?;
            let asg = tokens.next().map(String::from);
            let asl = tokens
                .next()
                .map(|s| {
                    s.parse::<i32>().map_err(|e| {
                        BridgeError::GroupConfigError(format!(
                            "line {lineno}: invalid asl '{s}': {e}"
                        ))
                    })
                })
                .transpose()?;
            Ok(PvListEntry::Alias {
                pattern,
                target_template: target.to_string(),
                asg,
                asl,
            })
        }
        other => Err(BridgeError::GroupConfigError(format!(
            "line {lineno}: unknown keyword '{other}', expected ALLOW/DENY/ALIAS"
        ))),
    }
}

fn build_pattern(pat: &str, lineno: usize) -> BridgeResult<Regex> {
    // Anchor the pattern to match the full PV name (C++ ca-gateway behavior).
    let anchored = format!("^{pat}$");
    Regex::new(&anchored).map_err(|e| {
        BridgeError::GroupConfigError(format!("line {lineno}: invalid regex '{pat}': {e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty() {
        let list = parse_pvlist("").unwrap();
        assert_eq!(list.order, EvaluationOrder::AllowDeny);
        assert!(list.entries.is_empty());
    }

    #[test]
    fn parse_comments_and_blanks() {
        let content = r#"
            # This is a comment

            # Another one

        "#;
        let list = parse_pvlist(content).unwrap();
        assert!(list.entries.is_empty());
    }

    #[test]
    fn parse_evaluation_order() {
        let list = parse_pvlist("EVALUATION ORDER DENY, ALLOW").unwrap();
        assert_eq!(list.order, EvaluationOrder::DenyAllow);

        let list = parse_pvlist("EVALUATION ORDER ALLOW, DENY").unwrap();
        assert_eq!(list.order, EvaluationOrder::AllowDeny);
    }

    #[test]
    fn parse_simple_allow() {
        let list = parse_pvlist("Beam:.* ALLOW").unwrap();
        assert_eq!(list.entries.len(), 1);
        assert!(matches!(list.entries[0], PvListEntry::Allow { .. }));
    }

    #[test]
    fn parse_allow_with_asg_asl() {
        let list = parse_pvlist("Beam:.* ALLOW BeamGroup 2").unwrap();
        if let PvListEntry::Allow { asg, asl, .. } = &list.entries[0] {
            assert_eq!(asg.as_deref(), Some("BeamGroup"));
            assert_eq!(*asl, Some(2));
        } else {
            panic!("expected Allow");
        }
    }

    #[test]
    fn parse_deny() {
        let list = parse_pvlist("test.* DENY").unwrap();
        assert!(matches!(list.entries[0], PvListEntry::Deny { .. }));
    }

    #[test]
    fn parse_deny_from_hosts() {
        let list = parse_pvlist("test.* DENY FROM bad.host evil.host").unwrap();
        if let PvListEntry::Deny { from_hosts, .. } = &list.entries[0] {
            assert_eq!(from_hosts, &["bad.host", "evil.host"]);
        } else {
            panic!("expected Deny");
        }
    }

    #[test]
    fn parse_alias() {
        let list = parse_pvlist(r"ps([0-9]) ALIAS PSCurrent\1.ai PSGroup 1").unwrap();
        if let PvListEntry::Alias {
            target_template,
            asg,
            asl,
            ..
        } = &list.entries[0]
        {
            assert_eq!(target_template, r"PSCurrent\1.ai");
            assert_eq!(asg.as_deref(), Some("PSGroup"));
            assert_eq!(*asl, Some(1));
        } else {
            panic!("expected Alias");
        }
    }

    #[test]
    fn parse_full_example() {
        let content = r#"
            EVALUATION ORDER ALLOW, DENY

            # Beam line PVs
            Beam:.*       ALLOW BeamGroup 1

            # Power supplies via alias
            ps([0-9])     ALIAS PSCurrent\1.ai PSGroup 1

            # Block test PVs
            test.*        DENY
        "#;
        let list = parse_pvlist(content).unwrap();
        assert_eq!(list.entries.len(), 3);
    }

    #[test]
    fn parse_invalid_keyword() {
        assert!(parse_pvlist("foo BAD").is_err());
    }

    #[test]
    fn parse_invalid_regex() {
        assert!(parse_pvlist("[invalid ALLOW").is_err());
    }

    #[test]
    fn parse_alias_missing_target() {
        assert!(parse_pvlist("foo ALIAS").is_err());
    }

    #[test]
    fn match_simple_allow() {
        let list = parse_pvlist("Beam:.* ALLOW").unwrap();
        let m = list.match_name("Beam:current").unwrap();
        assert_eq!(m.resolved_name, "Beam:current");
        assert!(!m.is_alias);

        assert!(list.match_name("Other:pv").is_none());
    }

    #[test]
    fn match_deny_overrides_allow() {
        // ALLOW, DENY order: DENY overrides
        let list = parse_pvlist(
            r#"
                EVALUATION ORDER ALLOW, DENY
                .*  ALLOW
                bad.* DENY
            "#,
        )
        .unwrap();
        assert!(list.match_name("good:pv").is_some());
        assert!(list.match_name("bad:pv").is_none());
    }

    #[test]
    fn match_allow_overrides_deny() {
        // DENY, ALLOW order: ALLOW overrides
        let list = parse_pvlist(
            r#"
                EVALUATION ORDER DENY, ALLOW
                .*    DENY
                Beam:.* ALLOW
            "#,
        )
        .unwrap();
        assert!(list.match_name("Beam:current").is_some());
        assert!(list.match_name("Other:pv").is_none());
    }

    // ---- BRIDGE-FR-10: host-aware DENY FROM admission ----

    #[test]
    fn fr10_host_targeted_deny_is_not_a_global_deny() {
        // ALLOW,DENY order. A `DENY FROM bad.host` rule must NOT reject
        // every host at host-less search; before FR-10 it folded into
        // `match_name`'s deny set and denied everyone.
        let list = parse_pvlist(
            r#"
                EVALUATION ORDER ALLOW, DENY
                PV.*  ALLOW
                PV.*  DENY FROM bad.host
            "#,
        )
        .unwrap();
        assert!(
            list.match_name("PV:x").is_some(),
            "host-targeted DENY FROM must not deny the host-less search"
        );

        // A global (FROM-less) DENY still denies host-lessly.
        let g = parse_pvlist(
            r#"
                EVALUATION ORDER ALLOW, DENY
                PV.*  ALLOW
                PV.*  DENY
            "#,
        )
        .unwrap();
        assert!(
            g.match_name("PV:x").is_none(),
            "a global DENY must still deny in match_name"
        );
    }

    #[test]
    fn fr10_match_name_for_host_denies_only_listed_host_allow_deny() {
        let list = parse_pvlist(
            r#"
                EVALUATION ORDER ALLOW, DENY
                PV.*  ALLOW
                PV.*  DENY FROM bad.host 10.0.0.9
            "#,
        )
        .unwrap();
        // Listed hosts (by name and by IP) are rejected at search time.
        assert!(list.match_name_for_host("PV:x", "bad.host").is_none());
        assert!(list.match_name_for_host("PV:x", "10.0.0.9").is_none());
        // Any other host is admitted.
        assert!(list.match_name_for_host("PV:x", "good.host").is_some());
        assert!(list.match_name_for_host("PV:x", "10.0.0.1").is_some());
    }

    #[test]
    fn fr10_host_deny_preempts_allow_in_deny_allow_order() {
        // DENY,ALLOW order. The host-targeted deny must win over the
        // ALLOW rule for the listed host (C `gateAs::findEntry` checks
        // the deny_from_table before the normal allow/deny decision),
        // while a different host is still admitted by the ALLOW rule.
        let list = parse_pvlist(
            r#"
                EVALUATION ORDER DENY, ALLOW
                PV.*  DENY FROM bad.host
                PV.*  ALLOW
            "#,
        )
        .unwrap();
        assert!(list.match_name_for_host("PV:x", "bad.host").is_none());
        assert!(list.match_name_for_host("PV:x", "good.host").is_some());
    }

    #[test]
    fn fr10_host_match_is_case_insensitive() {
        // ALLOW rule admits the PV; the host-targeted deny then denies
        // the listed host regardless of ASCII case. (Without an ALLOW
        // rule, ALLOW,DENY order is default-deny — so the ALLOW is
        // needed to isolate the case-insensitive host match.)
        let list = parse_pvlist(
            r#"
                PV.*  ALLOW
                PV.*  DENY FROM Bad.Host
            "#,
        )
        .unwrap();
        assert!(list.match_name_for_host("PV:x", "BAD.HOST").is_none());
        assert!(list.match_name_for_host("PV:x", "bad.host").is_none());
        assert!(list.match_name_for_host("PV:x", "other.host").is_some());
    }

    #[test]
    fn match_alias_with_backreference() {
        let list = parse_pvlist(r"ps([0-9]) ALIAS PSCurrent\1.ai PSGroup 1").unwrap();
        let m = list.match_name("ps3").unwrap();
        assert!(m.is_alias);
        assert_eq!(m.resolved_name, "PSCurrent3.ai");
        assert_eq!(m.asg.as_deref(), Some("PSGroup"));
        assert_eq!(m.asl, Some(1));
    }

    #[test]
    fn match_alias_multiple_groups() {
        let list = parse_pvlist(r"(\w+):(\d+) ALIAS \1_record\2.VAL").unwrap();
        let m = list.match_name("temp:7").unwrap();
        assert_eq!(m.resolved_name, "temp_record7.VAL");
    }

    #[test]
    fn pattern_anchored() {
        // Pattern is implicitly anchored — partial matches should fail
        let list = parse_pvlist("foo ALLOW").unwrap();
        assert!(list.match_name("foo").is_some());
        assert!(list.match_name("foobar").is_none());
        assert!(list.match_name("xfoo").is_none());
    }

    #[test]
    fn expand_template_zero_group() {
        let pat = Regex::new(r"^(\w+)$").unwrap();
        // \0 is the whole match
        let result = expand_template(&pat, "hello", r"prefix_\0_suffix");
        assert_eq!(result, "prefix_hello_suffix");
    }

    // --- BR-R53: resolve_hosts converts DENY FROM hostnames to IPs ---

    /// An IP literal in DENY FROM is preserved verbatim after resolve_hosts.
    #[tokio::test]
    async fn resolve_hosts_ip_literal_unchanged() {
        let mut list = parse_pvlist(
            r#"
            PV.* ALLOW
            PV.* DENY FROM 192.0.2.1
            "#,
        )
        .unwrap();
        list.resolve_hosts().await;
        if let PvListEntry::Deny { from_hosts, .. } = &list.entries[1] {
            assert_eq!(from_hosts, &["192.0.2.1"]);
        } else {
            panic!("expected Deny");
        }
        // Deny fires for the literal IP.
        assert!(list.is_host_denied("PV:x", "192.0.2.1"));
        // Other IPs are not denied.
        assert!(!list.is_host_denied("PV:x", "192.0.2.2"));
    }

    /// A hostname in DENY FROM is resolved and the resolved IP denies
    /// the peer. Uses `localhost` which resolves reliably in CI.
    #[tokio::test]
    async fn resolve_hosts_hostname_resolves_to_ip() {
        let mut list = parse_pvlist(
            r#"
            PV.* ALLOW
            PV.* DENY FROM localhost
            "#,
        )
        .unwrap();
        list.resolve_hosts().await;
        if let PvListEntry::Deny { from_hosts, .. } = &list.entries[1] {
            // After resolution, from_hosts must contain IPs, not "localhost".
            assert!(
                !from_hosts.is_empty(),
                "localhost must resolve to at least one IP"
            );
            for h in from_hosts {
                assert!(
                    h.parse::<std::net::IpAddr>().is_ok(),
                    "resolved entry must be an IP string, got {h:?}"
                );
            }
            // The resolved IPs (127.0.0.1 and/or ::1) must deny the peer.
            let denied = from_hosts.iter().any(|ip| list.is_host_denied("PV:x", ip));
            assert!(denied, "resolved IP must be denied");
        } else {
            panic!("expected Deny");
        }
    }

    /// An unresolvable hostname is dropped (fail-open, matching C).
    #[tokio::test]
    async fn resolve_hosts_unresolvable_dropped() {
        let mut list = parse_pvlist(
            r#"
            PV.* ALLOW
            PV.* DENY FROM this.hostname.definitely.does.not.exist.invalid
            "#,
        )
        .unwrap();
        list.resolve_hosts().await;
        if let PvListEntry::Deny { from_hosts, .. } = &list.entries[1] {
            assert!(
                from_hosts.is_empty(),
                "unresolvable hostname must be dropped; got {from_hosts:?}"
            );
        } else {
            panic!("expected Deny");
        }
        // With no resolved hosts, no peer is denied by this rule.
        assert!(!list.is_host_denied("PV:x", "10.0.0.1"));
    }
}
