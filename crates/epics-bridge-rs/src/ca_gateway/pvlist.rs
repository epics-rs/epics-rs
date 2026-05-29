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
//!   resolution via [`PvList::match_name_for_host`] (the
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
    /// a host-targeted `pattern DENY FROM host …` rule is
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

impl PvListMatch {
    /// Access-security level for this match, applying the C ca-gateway
    /// default of **1** when the rule omitted the ASL field. In
    /// `gateAs.cc` every rule carries `int lev=1`: an omitted ASG, an
    /// omitted ASL, or an unparseable level all fall back to 1, and the
    /// implicit `.* allow` rule (no pvlist file) is created at level 1.
    /// AS activates a rule iff `client_asl <= rule.level`, so defaulting
    /// to 0 — as the call sites previously did via `unwrap_or(0)` —
    /// applies a superset of rules, over-permitting a level-0 WRITE/READ
    /// grant that C would deny. Centralising the default here keeps every
    /// consumer from re-deriving it (and re-introducing the `0` bug).
    pub fn effective_asl(&self) -> i32 {
        self.asl.unwrap_or(1)
    }
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
    ///   dropped from this rule's host set, mirroring C's pass-1 omission
    ///   (`gateAs.cc:504-507` — the unresolved host is simply not appended).
    ///   If a rule's hosts **all** fail to resolve, its `from_hosts` ends
    ///   empty and the rule is thereafter treated as a **global** deny by
    ///   [`Self::is_global_deny`] — fail-**closed**, matching canonical
    ///   ca-gateway (`USE_DENYFROM`), whose two-pass parser re-parses the
    ///   host-stripped line into the global `deny_list` (`gateAs.cc:540-556`).
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
                                     — host dropped; if it is the rule's only host the \
                                     rule collapses to a global deny (matches C gateAs.cc:540-556)"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                hostname = %token,
                                error = %e,
                                "pvlist DENY FROM: cannot resolve hostname \
                                 — host dropped; if it is the rule's only host the \
                                 rule collapses to a global deny (matches C gateAs.cc:540-556)"
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
    /// only *global* DENY rules (`pattern DENY`, no `FROM`)
    /// participate here. Host-targeted `pattern DENY FROM host …` rules
    /// are excluded from the deny set — they cannot be evaluated without
    /// a host, and treating them as global denies is the FR-10 defect.
    /// Callers that know the downstream client host MUST use
    /// [`Self::match_name_for_host`] so host-scoped denials are honored;
    /// host-less callers (preload, pvlist-reload prune) intentionally see
    /// only global rules, because a host-targeted deny must not remove a
    /// PV that is still admissible for other hosts.
    pub fn match_name(&self, name: &str) -> Option<PvListMatch> {
        // Select the LAST-in-file matching ALLOW (or ALIAS) and the
        // LAST-in-file matching global DENY, each from its own list.
        // C ca-gateway keeps separate allow/deny `tsSLList`s and inserts
        // every parsed rule at the HEAD (`tsSLList::add` — "add to the
        // beginning of the list", tsSLList.h:62), then `findEntryInList`
        // iterates from the head and returns the first hit
        // (gateAs.cc:386-410). Head-insertion + front-iteration means the
        // rule nearest the BOTTOM of the file wins — the documented
        // bottom-to-top precedence (Gateway.html:694-696,746-747) where a
        // later, more-specific rule overrides an earlier general one.
        // Forward `.find()` selected the TOP-most match and inverted that
        // precedence; `.rev().find()` reproduces C's bottom-most selection
        // per list. Host-targeted DENY FROM rules do not participate here.
        let allow_match = self
            .entries
            .iter()
            .rev()
            .find(|e| e.is_allow() && e.pattern().is_match(name));
        let deny_match = self
            .entries
            .iter()
            .rev()
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
    /// this is the parity equivalent of C ca-gateway's
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
        // C ca-gateway (gateAs.cc:617-621) treats ALLOW, ALIAS, PATTERN and
        // PV identically: all four insert a `gateAsEntry` into the same
        // `allow_list` with the same ASG/ASL parsing. PATTERN and PV are
        // therefore plain synonyms for ALLOW (they predate the ALLOW keyword
        // and remain accepted for legacy pvlist files). Rejecting them broke
        // load/reload of C-compatible pvlist files that use the older syntax.
        "ALLOW" | "PATTERN" | "PV" => {
            let (asg, asl) = parse_asg_asl(&mut tokens, lineno);
            Ok(PvListEntry::Allow { pattern, asg, asl })
        }
        "DENY" => {
            // `pattern DENY [FROM] [host ...]`. C ca-gateway (gateAs.cc:539-552,
            // USE_DENYFROM branch):
            //
            //   if((hname=strtok(NULL,", \t\n")) && strcasecmp(hname,"FROM")==0)
            //       hname=strtok(NULL,", \t\n");
            //   if(hname) { do { ... } while((hname=strtok(NULL,", \t\n"))); }
            //   else { /* global deny */ }
            //
            // Two C behaviors this reproduces:
            //
            // - `FROM` is OPTIONAL. The first token after DENY is consumed as
            //   the `FROM` keyword ONLY when it matches case-insensitively;
            //   otherwise it is the first host. So `PV.* DENY h1 h2` is a
            //   host-scoped deny exactly like `PV.* DENY FROM h1 h2`. The old
            //   code rejected the whole pvlist when `FROM` was omitted.
            // - Hosts are tokenized with `strtok(NULL,", \t\n")`, so comma is a
            //   delimiter just like whitespace; `h1,h2` is two hosts.
            //   `split_whitespace` only split on whitespace, so each remaining
            //   token is further split on commas with empties dropped, giving
            //   `h1,h2`, `h1, h2`, and `h1 ,h2` the same host set.
            //
            // A bare `DENY` (no hosts) stays a global deny. Hosts are stored
            // verbatim here; PvList::resolve_hosts() converts hostnames to IP
            // strings at load time, mirroring C aToIPAddr (gateAs.cc:488-506).
            let mut hosts = tokens
                .flat_map(|t| t.split(','))
                .filter(|s| !s.is_empty())
                .peekable();
            if hosts.peek().is_some_and(|h| h.eq_ignore_ascii_case("FROM")) {
                hosts.next();
            }
            let from_hosts: Vec<String> = hosts.map(String::from).collect();
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
            let (asg, asl) = parse_asg_asl(&mut tokens, lineno);
            Ok(PvListEntry::Alias {
                pattern,
                target_template: target.to_string(),
                asg,
                asl,
            })
        }
        other => Err(BridgeError::GroupConfigError(format!(
            "line {lineno}: unknown keyword '{other}', expected ALLOW/PATTERN/PV/DENY/ALIAS"
        ))),
    }
}

/// Parse the optional trailing `[asg [asl]]` of an ALLOW/PATTERN/PV/ALIAS
/// rule, mirroring C ca-gateway (gateAs.cc:609-613):
///
/// ```c
/// if((asg=strtok(NULL," \t\n"))) {
///     if((asl=strtok(NULL," \t\n")) && (sscanf(asl,"%d",&lev)!=1)) lev=1;
/// } else { asg=default_group; lev=1; }
/// ```
///
/// Two C behaviors this reproduces:
///
/// - The ASL token is read **only** when an ASG token precedes it (C reads
///   `asl` inside the `if(asg)` block); a bare `pattern ALLOW` carries no ASG
///   and no ASL.
/// - An ASL token that is present but not an integer falls back to **level 1**
///   (`sscanf(...)!=1 → lev=1`) instead of aborting. A single typo such as
///   `PV.* ALLOW BeamGroup typo` keeps serving `PV.*` at ASL 1 in C; the
///   previous `s.parse::<i32>()?` rejected the whole pvlist (or reload). The
///   omitted-ASL and invalid-ASL cases both resolve to 1 — `Some(1)` here for
///   the invalid case records the explicit fallback, `None` for the omitted
///   case defaults to 1 via [`PvListMatch::effective_asl`].
///
/// Genuine syntax errors C also rejects (a missing ALIAS target) remain hard
/// errors at their call site, not here.
fn parse_asg_asl<'a>(
    tokens: &mut impl Iterator<Item = &'a str>,
    lineno: usize,
) -> (Option<String>, Option<i32>) {
    let asg = tokens.next().map(String::from);
    let asl = asg.is_some().then(|| tokens.next()).flatten().map(|s| {
        s.parse::<i32>().unwrap_or_else(|_| {
            tracing::warn!(
                line = lineno,
                token = %s,
                "pvlist: invalid ASL token — falling back to level 1 \
                 (C gateAs.cc:612 sscanf!=1 → lev=1)"
            );
            1
        })
    });
    (asg, asl)
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

    /// an omitted `.pvlist` ASL must default to C ca-gateway's
    /// `int lev=1` (gateAs.cc), not 0. The call sites previously used
    /// `unwrap_or(0)`, which over-permitted: AS activates a rule iff
    /// `client_asl <= rule.level`, so a level-0 default applies a
    /// superset of rules — a level-0 WRITE/READ grant C would deny.
    #[test]
    fn effective_asl_defaults_to_one_when_omitted() {
        let base = PvListMatch {
            resolved_name: "PV".into(),
            asg: None,
            asl: None,
            is_alias: false,
        };
        assert_eq!(base.effective_asl(), 1, "omitted ASL → 1 (C default)");
        assert_eq!(
            PvListMatch {
                asl: Some(2),
                ..base.clone()
            }
            .effective_asl(),
            2,
            "explicit ASL preserved"
        );
        assert_eq!(
            PvListMatch {
                asl: Some(0),
                ..base.clone()
            }
            .effective_asl(),
            0,
            "explicit level-0 preserved (only the omitted case defaults to 1)"
        );

        // Integration: an ALLOW rule with an ASG but no ASL token parses
        // to asl=None and resolves to level 1.
        let list = parse_pvlist("PV.*  ALLOW  grp\n").unwrap();
        let m = list.match_name("PV:x").expect("ALLOW match");
        assert_eq!(m.asl, None, "ASG-only rule leaves ASL unset");
        assert_eq!(m.effective_asl(), 1);
    }

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

    /// C ca-gateway tokenizes DENY FROM host lists on comma AND
    /// whitespace (`strtok(NULL, ", \t\n")`, gateAs.cc:469-472). A
    /// comma-joined list must parse to one entry per host, not a single
    /// unresolvable `bad1,bad2` token that collapses into a global deny.
    #[test]
    fn parse_deny_from_comma_separated_hosts() {
        // No spaces around the comma.
        let list = parse_pvlist("test.* DENY FROM bad1.example,bad2.example").unwrap();
        if let PvListEntry::Deny { from_hosts, .. } = &list.entries[0] {
            assert_eq!(from_hosts, &["bad1.example", "bad2.example"]);
        } else {
            panic!("expected Deny");
        }

        // Space after the comma → split_whitespace already separates the
        // second host; comma-stripping must not leave a trailing comma.
        let list = parse_pvlist("test.* DENY FROM bad1.example, bad2.example").unwrap();
        if let PvListEntry::Deny { from_hosts, .. } = &list.entries[0] {
            assert_eq!(from_hosts, &["bad1.example", "bad2.example"]);
        } else {
            panic!("expected Deny");
        }

        // Mixed: leading-comma token and a three-host comma list with a
        // stray space — every host is recovered, no empty entries.
        let list = parse_pvlist("test.* DENY FROM h1,h2 ,h3").unwrap();
        if let PvListEntry::Deny { from_hosts, .. } = &list.entries[0] {
            assert_eq!(from_hosts, &["h1", "h2", "h3"]);
        } else {
            panic!("expected Deny");
        }
    }

    /// C ca-gateway (gateAs.cc:539-552) makes the `FROM` keyword OPTIONAL on
    /// host-scoped DENY rules: the first post-DENY token is the `FROM` keyword
    /// only when it matches case-insensitively, otherwise it is the first host.
    /// `PV.* DENY h1 h2` must therefore parse as a host-scoped deny, not reject
    /// the whole pvlist.
    #[test]
    fn parse_deny_optional_from_keyword() {
        // No FROM: first token is a host.
        let list = parse_pvlist("PV.* DENY host1 host2").unwrap();
        if let PvListEntry::Deny { from_hosts, .. } = &list.entries[0] {
            assert_eq!(from_hosts, &["host1", "host2"]);
        } else {
            panic!("expected Deny");
        }

        // FROM present (case-insensitive) is consumed, not stored as a host.
        for kw in ["FROM", "from", "From"] {
            let list = parse_pvlist(&format!("PV.* DENY {kw} host1 host2")).unwrap();
            if let PvListEntry::Deny { from_hosts, .. } = &list.entries[0] {
                assert_eq!(from_hosts, &["host1", "host2"], "{kw} consumed");
            } else {
                panic!("{kw}: expected Deny");
            }
        }

        // No-FROM combined with comma tokenization (BRIDGE-RS-2026-05-28-12
        // host splitting still applies without the keyword).
        let list = parse_pvlist("PV.* DENY h1,h2 ,h3").unwrap();
        if let PvListEntry::Deny { from_hosts, .. } = &list.entries[0] {
            assert_eq!(from_hosts, &["h1", "h2", "h3"]);
        } else {
            panic!("expected Deny");
        }

        // Bare DENY stays a global deny (empty host set).
        let list = parse_pvlist("PV.* DENY").unwrap();
        if let PvListEntry::Deny { from_hosts, .. } = &list.entries[0] {
            assert!(from_hosts.is_empty(), "bare DENY is global");
        } else {
            panic!("expected Deny");
        }

        // `DENY FROM` with no following host is also a global deny (FROM
        // consumed, nothing left) — matches C (hname==NULL → deny_list).
        let list = parse_pvlist("PV.* DENY FROM").unwrap();
        if let PvListEntry::Deny { from_hosts, .. } = &list.entries[0] {
            assert!(from_hosts.is_empty(), "DENY FROM with no host is global");
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

    /// C ca-gateway (gateAs.cc:617-621) accepts PATTERN and PV as synonyms
    /// for ALLOW — all four insert into the same allow_list with identical
    /// ASG/ASL parsing. A legacy pvlist using PATTERN/PV must load (and
    /// match, alias, and carry ASG/ASL) exactly like the ALLOW form.
    #[test]
    fn parse_pattern_and_pv_are_allow_synonyms() {
        for kw in ["PATTERN", "PV", "pattern", "pv"] {
            let list = parse_pvlist(&format!("PV.* {kw} BeamGroup 2")).unwrap();
            assert_eq!(list.entries.len(), 1, "{kw}: one entry");
            match &list.entries[0] {
                PvListEntry::Allow { asg, asl, .. } => {
                    assert_eq!(asg.as_deref(), Some("BeamGroup"), "{kw}: ASG");
                    assert_eq!(*asl, Some(2), "{kw}: ASL");
                }
                other => panic!("{kw}: expected Allow, got {other:?}"),
            }
            // Behaves as an allow rule end-to-end.
            let m = list.match_name("PV:current").expect("{kw}: admitted");
            assert!(!m.is_alias);
            assert_eq!(m.resolved_name, "PV:current");
            assert!(
                list.match_name("Other:x").is_none(),
                "{kw}: non-match denied"
            );
        }

        // Default ASG/ASL (no tokens) — PATTERN with bare pattern is admitted
        // at the C level-1 default, same as bare ALLOW.
        let list = parse_pvlist("Beam:.* PATTERN").unwrap();
        let m = list.match_name("Beam:x").expect("admitted");
        assert_eq!(m.asl, None);
        assert_eq!(m.effective_asl(), 1);
    }

    #[test]
    fn parse_invalid_regex() {
        assert!(parse_pvlist("[invalid ALLOW").is_err());
    }

    #[test]
    fn parse_alias_missing_target() {
        assert!(parse_pvlist("foo ALIAS").is_err());
    }

    /// C ca-gateway (gateAs.cc:612) parses ASL with `sscanf("%d")`; when that
    /// fails it sets `lev=1` and still installs the rule. A pvlist line with a
    /// typo'd ASL field must therefore keep serving the pattern at level 1, not
    /// abort the whole file/reload. Applies to ALLOW, PATTERN, PV and ALIAS,
    /// which all route through the shared `parse_asg_asl` helper.
    #[test]
    fn parse_invalid_asl_falls_back_to_level_one() {
        // ALLOW with a non-integer ASL: parses, ASG kept, level → 1.
        let list = parse_pvlist("PV.* ALLOW BeamGroup typo").unwrap();
        match &list.entries[0] {
            PvListEntry::Allow { asg, asl, .. } => {
                assert_eq!(asg.as_deref(), Some("BeamGroup"), "ASG preserved");
                assert_eq!(*asl, Some(1), "unparseable ASL → level 1");
            }
            other => panic!("expected Allow, got {other:?}"),
        }
        assert_eq!(list.match_name("PV:x").expect("served").effective_asl(), 1);

        // Same fallback for the legacy PATTERN/PV synonyms and for ALIAS.
        for kw in ["PATTERN", "PV"] {
            let list = parse_pvlist(&format!("PV.* {kw} grp xyz")).unwrap();
            match &list.entries[0] {
                PvListEntry::Allow { asl, .. } => assert_eq!(*asl, Some(1), "{kw}"),
                other => panic!("{kw}: expected Allow, got {other:?}"),
            }
        }
        let list = parse_pvlist(r"ps([0-9]) ALIAS PSCurrent\1.ai PSGroup oops").unwrap();
        match &list.entries[0] {
            PvListEntry::Alias { asg, asl, .. } => {
                assert_eq!(asg.as_deref(), Some("PSGroup"));
                assert_eq!(*asl, Some(1), "ALIAS unparseable ASL → level 1");
            }
            other => panic!("expected Alias, got {other:?}"),
        }

        // A valid ASL is still parsed exactly; only the unparseable case falls
        // back. Level 0 (more permissive) must NOT be silently produced from a
        // valid "0" token by the fallback path.
        let list = parse_pvlist("PV.* ALLOW grp 0").unwrap();
        match &list.entries[0] {
            PvListEntry::Allow { asl, .. } => assert_eq!(*asl, Some(0), "valid 0 preserved"),
            other => panic!("expected Allow, got {other:?}"),
        }

        // A missing ALIAS target is a genuine syntax error C also rejects —
        // it must stay a hard parse error, not fall through to level 1.
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

    // ---- host-aware DENY FROM admission ----

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

    /// C ca-gateway precedence is bottom-to-top: a later (lower-in-file),
    /// more-specific rule overrides an earlier general one. The review's
    /// canonical case — `.* ALLOW DEFAULT 1` above `SEC:.* ALLOW Secure 0`
    /// — must resolve `SEC:x` to the specific rule's ASG/ASL, not the
    /// broad rule at the top.
    #[test]
    fn match_specific_rule_below_general_rule_wins() {
        let list = parse_pvlist(
            r#"
                EVALUATION ORDER ALLOW, DENY
                .*       ALLOW DEFAULT 1
                SEC:.*   ALLOW Secure 0
            "#,
        )
        .unwrap();

        // The specific rule sits below the general one and must win.
        let m = list.match_name("SEC:hv").expect("SEC:hv allowed");
        assert_eq!(m.asg.as_deref(), Some("Secure"), "bottom-most rule's ASG");
        assert_eq!(m.asl, Some(0), "bottom-most rule's ASL");

        // A name only the general rule matches still resolves to it.
        let g = list.match_name("OTHER:x").expect("OTHER:x allowed");
        assert_eq!(g.asg.as_deref(), Some("DEFAULT"));
        assert_eq!(g.asl, Some(1));
    }

    /// Alias precedence is also bottom-to-top: a specific ALIAS placed
    /// below a general ALIAS must win, carrying its own target + ASG/ASL.
    #[test]
    fn match_specific_alias_below_general_alias_wins() {
        let list = parse_pvlist(
            r#"
                EVALUATION ORDER ALLOW, DENY
                (.*)        ALIAS general_\1 GenGrp 1
                SEC:(.*)    ALIAS secure_\1 SecGrp 0
            "#,
        )
        .unwrap();

        let m = list.match_name("SEC:hv").expect("SEC:hv aliased");
        assert!(m.is_alias);
        assert_eq!(m.resolved_name, "secure_hv", "bottom-most alias target");
        assert_eq!(m.asg.as_deref(), Some("SecGrp"));
        assert_eq!(m.asl, Some(0));
    }

    /// A specific DENY below a general ALLOW must win under ALLOW,DENY
    /// order (the deny list is searched bottom-up too), while a name only
    /// the general ALLOW covers is still served.
    #[test]
    fn match_specific_deny_below_general_allow_wins() {
        let list = parse_pvlist(
            r#"
                EVALUATION ORDER ALLOW, DENY
                .*       ALLOW
                SEC:.*   DENY
            "#,
        )
        .unwrap();
        assert!(list.match_name("SEC:hv").is_none(), "specific DENY wins");
        assert!(list.match_name("OTHER:x").is_some(), "general ALLOW serves");
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

    // --- resolve_hosts converts DENY FROM hostnames to IPs ---

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

    /// An all-unresolvable `DENY FROM` rule ends with empty `from_hosts`, so
    /// `is_host_denied` (host-targeted check only) no longer matches it — the
    /// deny collapses to a global deny enforced by `match_name`. Fail-closed,
    /// matching canonical C (`gateAs.cc:540-556`).
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
        // The rule collapsed to a global deny: no longer host-targeted, so the
        // host-targeted `is_host_denied` check does not match it. The deny is
        // now enforced globally by `match_name` (see `is_global_deny`).
        assert!(!list.is_host_denied("PV:x", "10.0.0.1"));
        // Distinguishing assertion (fail-closed vs fail-open): under the default
        // ALLOW,DENY order the collapsed global deny must make `match_name`
        // reject the pattern outright. `is_host_denied` alone is true under both
        // fail-open and fail-closed, so this is what actually pins the behavior.
        assert!(
            list.match_name("PV:x").is_none(),
            "all-unresolvable DENY FROM must collapse to a global deny (fail-closed)"
        );
    }
}
