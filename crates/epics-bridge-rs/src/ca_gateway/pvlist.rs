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
//! ps\([0-9]\)      ALIAS PSCurrent\1.ai PowerSupply 1
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
//! - Patterns use ca-gateway's DEFAULT GNU basic-regular-expression (BRE)
//!   dialect — the build with `USE_PCRE` commented out (`configure/CONFIG_SITE`),
//!   which compiles patterns via `re_compile_pattern` under the GNU
//!   `re_syntax_options` default of 0 (`gateAs.cc:236`). `build_pattern`
//!   translates that dialect into the Rust `regex` (RE2) source before
//!   compiling, so `\(`/`\)` are capture groups, bare `(`/`)` are literal
//!   parentheses, `\|` is alternation, and braces are literal — matching the
//!   shipped `example/GATEWAY.pvlist` and `pvlist_bre.txt` rules. (PCRE mode is
//!   an opt-in upstream build flag; the Rust gateway tracks the default build.)
//! - Backreference substitution is implemented manually because Rust
//!   `regex` doesn't support backreferences in the pattern, but
//!   capture groups (numbered after the BRE→RE2 translation) are available
//!   for replacement.
//! - The DENY `FROM host` clause is host-scoped: it denies only when the
//!   requester host matches. It is enforced at the put-hook path via
//!   [`PvList::is_host_denied`] and at downstream search/create
//!   resolution via [`PvList::match_name_for_host`] (the
//!   parity equivalent of C `gateServer::pvExistTest` →
//!   `gateAs::findEntry(pvname, hostname)`). Host-less callers (preload,
//!   pvlist-reload prune) use [`PvList::match_name`], which by design
//!   sees only global (`FROM`-less) DENY rules — a host-targeted deny
//!   must not remove a PV that is still admissible for other hosts.

// RTEMS-EXEC-MODEL-ALLOW(6): checked - these run and pass in the feature-ON suite.

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

/// The identity `.pvlist` `DENY FROM` rules are matched against.
///
/// **Constructible only from a peer socket address.** That is the whole
/// point of the type: the two enforcement points that consult `DENY FROM`
/// used to disagree about what "host" meant — the search/create path passed
/// the socket address while the write path passed `WriteContext::host`, the
/// name the CA client *claims* in `HOST_NAME`. A client therefore chose which
/// DENY row applied to its own writes, and since `from_hosts` holds only
/// resolved IPv4 quads after [`PvList::resolve_hosts`], a client sending its
/// real hostname (CA's default) matched no row at all — the write-side deny
/// never fired.
///
/// C pins this to the socket and nothing else: `gateServer::pvExistTest`
/// takes `clientAddress.getSockIP()` through `ipAddrToDottedIP` and strips
/// the port, with the `getClientHostName(ctx, ...)` call left commented out
/// immediately above (`gateServer.cc:1523-1530`). So this is parity, not
/// added strictness.
///
/// Making it a type rather than a convention is what stops the two points
/// drifting apart again: a claimed name is not a `PolicyHost` and will not
/// compile at either call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyHost(String);

impl PolicyHost {
    /// The bracket-less host form of a peer address (`192.0.2.1`, `::1`),
    /// which is the shape `from_hosts` holds after resolution.
    pub fn from_peer(peer: std::net::SocketAddr) -> Self {
        PolicyHost(peer.ip().to_string())
    }

    /// Same, from the `"ip:port"` string a `WriteContext` carries.
    ///
    /// `None` when the string is not a socket address. Callers must treat
    /// that as **deny**: if the peer cannot be established, a blacklist
    /// cannot be shown not to apply.
    pub fn from_peer_str(peer: &str) -> Option<Self> {
        peer.parse::<std::net::SocketAddr>()
            .ok()
            .map(Self::from_peer)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Test-only escape hatch, so the rule-matching tests can feed the
    /// unresolved hostnames a `.pvlist` file may contain before
    /// [`PvList::resolve_hosts`] runs. Deliberately `cfg(test)`: production
    /// has exactly two constructors and both take a peer.
    #[cfg(test)]
    pub(crate) fn for_test(host: &str) -> Self {
        PolicyHost(host.to_string())
    }
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
    /// - Tokens that are already **IPv4**-address literals are kept verbatim.
    ///   IPv6 literals are dropped, mirroring C `aToIPAddr` (which parses only
    ///   IPv4 forms and rejects a `:`-leading literal): C never creates a
    ///   host-scoped deny for any IPv6 address. A dropped IPv6 literal is
    ///   treated like an unresolvable host (collapses to a global deny when it
    ///   is the rule's only token).
    /// - Hostnames are resolved via `tokio::net::lookup_host` and reduced to a
    ///   **single IPv4 address — the first one returned**. C ca-gateway resolves
    ///   each DENY FROM token through `aToIPAddr` → `hostToIPAddr`, which sets
    ///   an `AF_INET` hint and breaks after the first `getaddrinfo` result
    ///   (`osdSock.c:250-267`), then stores exactly one dotted-decimal IPv4
    ///   address (`gateAs.cc:493-503`). Expanding a multi-homed name into one
    ///   entry per address — including secondary A records and IPv6 — would
    ///   deny strictly more peers than C, so we keep only the first IPv4 to
    ///   match C's single-address selection. A name with no IPv4 record
    ///   resolves to nothing in C (the `AF_INET` lookup fails) and is dropped
    ///   here for the same reason.
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
                    // Already an IPv4 literal? Preserve verbatim. C
                    // `aToIPAddr` (libcom aToIPAddr.c:97-173) parses only
                    // dotted/raw IPv4 forms; its host-name fallback uses
                    // `sscanf("%511[^:]")`, which matches zero characters for
                    // a `:`-leading IPv6 literal, so `aToIPAddr("::1")` fails
                    // outright. The `from_hosts` invariant after this pass is
                    // "IPv4 dotted strings only".
                    if token.parse::<std::net::Ipv4Addr>().is_ok() {
                        from_hosts.push(token);
                        continue;
                    }
                    // An IPv6 literal is NOT a usable host-scoped deny in C:
                    // `aToIPAddr` rejects it (above) and the `hostToIPAddr`
                    // fallback is AF_INET-only (osdSock.c:250-267), so C never
                    // builds a scoped deny for any IPv6 address. Treat it
                    // exactly like C's `aToIPAddr` failure — drop the token; if
                    // it is the rule's only host the rule collapses to a global
                    // deny (gateAs.cc:540-556).
                    if token.parse::<std::net::IpAddr>().is_ok() {
                        tracing::warn!(
                            host = %token,
                            "pvlist DENY FROM: IPv6 literal is not a host-scoped \
                             deny in C (aToIPAddr is IPv4-only) — host dropped; if \
                             it is the rule's only host the rule collapses to a \
                             global deny (matches C gateAs.cc:540-556)"
                        );
                        continue;
                    }
                    // Hostname — resolve to a single IPv4 address. Append `:0`
                    // as the required port sentinel for lookup_host. C's
                    // hostToIPAddr is AF_INET and keeps only the first result
                    // (osdSock.c:250-267 / gateAs.cc:493-503), so take the
                    // first IPv4 address and ignore the rest (including any
                    // IPv6 records, which C's AF_INET lookup never returns).
                    match tokio::net::lookup_host(format!("{token}:0")).await {
                        Ok(addrs) => {
                            match addrs.map(|sa| sa.ip()).find(std::net::IpAddr::is_ipv4) {
                                Some(ip) => from_hosts.push(ip.to_string()),
                                None => tracing::warn!(
                                    hostname = %token,
                                    "pvlist DENY FROM: hostname resolved to no IPv4 address \
                                     — host dropped (C hostToIPAddr is AF_INET-only); if it is \
                                     the rule's only host the rule collapses to a global deny \
                                     (matches C gateAs.cc:540-556)"
                                ),
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
    /// entry in `from_hosts` is an **IPv4** dotted-decimal string (C
    /// `aToIPAddr` is IPv4-only, see [`Self::resolve_hosts`]), so the
    /// comparison is exact. An IPv6 peer therefore never matches a
    /// host-scoped rule — matching C, which converts the requester to an
    /// IPv4 dotted string before `findEntry`. Callers pass the TCP peer IP
    /// in bracket-less form (`192.0.2.1`).
    pub fn is_host_denied(&self, name: &str, host: &PolicyHost) -> bool {
        let host = host.as_str();
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
    pub fn match_name_for_host(&self, name: &str, host: &PolicyHost) -> Option<PvListMatch> {
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
///
/// Parsing is per-line recoverable, matching C `gateAs::readPvList`
/// (gateAs.cc:530-632): a malformed line is logged and SKIPPED, and the
/// valid rules before and after it still load. C diagnoses every error
/// family — missing command (`:532`), missing/invalid ORDER operands
/// (`:581,:593`), missing ALIAS target (`:602`), unknown command, and
/// `gateAsEntry::init()`/regex failures — with `fprintf` and then `continue`
/// or falls through to the next line; an `AS`/`PVL` reload keeps the gateway
/// running on the newly parsed valid subset. An invalid ORDER line leaves the
/// previous `order` value unchanged (C never assigns `eval_order` on the
/// invalid branch). The only hard error is an I/O failure reading the file,
/// which lives in [`parse_pvlist_file`]; this string parser therefore always
/// returns `Ok`.
pub fn parse_pvlist(content: &str) -> BridgeResult<PvList> {
    let mut list = PvList::new();

    for (lineno, raw) in content.lines().enumerate() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }

        // ORDER directive. C ca-gateway does not special-case an
        // "EVALUATION ORDER" prefix: it tokenizes every line uniformly into a
        // pattern token + a command token, then matches the command with
        // `strcasecmp(cmd,"ORDER")` (gateAs.cc:531-535,579). So the leading
        // word ("EVALUATION") is just an ignored pattern token, the command
        // and operands are matched case-insensitively, and the two operands
        // are tokenized with `strtok(NULL,", \t\n")` (gateAs.cc:581-582) —
        // comma and whitespace are interchangeable separators. Detecting the
        // directive by "second token == ORDER" (rather than a literal
        // uppercase prefix) reproduces that and keeps the requirement that a
        // leading word precede ORDER, so `ORDER ALLOW DENY` is still a normal
        // rule line (pattern "ORDER", keyword "ALLOW") exactly as in C.
        let mut head = line.split_whitespace();
        let _pattern_tok = head.next();
        if head
            .next()
            .is_some_and(|cmd| cmd.eq_ignore_ascii_case("ORDER"))
        {
            // `head` now yields the operand tokens (after the pattern token and
            // the ORDER command). A malformed ORDER line is skipped and leaves
            // `list.order` unchanged (C falls through without reassigning
            // `eval_order`, gateAs.cc:593-597).
            match parse_order_directive(head, lineno + 1) {
                Ok(order) => list.order = order,
                Err(e) => tracing::warn!(
                    line = lineno + 1,
                    error = %e,
                    "pvlist: skipping malformed EVALUATION ORDER line, keeping prior order \
                     (C gateAs.cc:593-597 logs and continues)"
                ),
            }
            continue;
        }

        // Pattern rule: pattern KEYWORD [args...]. A malformed rule (missing
        // keyword, bad regex, missing ALIAS target, unknown command) is
        // skipped, not fatal — C drops just that rule and keeps the rest.
        match parse_rule_line(line, lineno + 1) {
            Ok(entry) => list.entries.push(entry),
            Err(e) => tracing::warn!(
                line = lineno + 1,
                error = %e,
                "pvlist: skipping malformed rule line (C gateAs.cc:532-628 logs and continues)"
            ),
        }
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

/// Parse the operands of an `[EVALUATION] ORDER <a>, <b>` directive.
///
/// C ca-gateway (gateAs.cc:580-595) reads exactly two operands with
/// `strtok(NULL,", \t\n")` — so comma and whitespace are interchangeable
/// separators — and matches each with `strcasecmp` against `ALLOW`/`DENY`.
/// `ALLOW, DENY`, `ALLOW,DENY`, `allow deny`, and `DENY ALLOW` are therefore
/// all accepted; only the two recognised orderings are valid. C reads just the
/// first two operands and ignores any trailing tokens, so we do too.
fn parse_order_directive<'a>(
    operands: impl Iterator<Item = &'a str>,
    lineno: usize,
) -> BridgeResult<EvaluationOrder> {
    // strtok ", \t\n": split each whitespace token further on commas, drop
    // the empties a stray comma (e.g. `ALLOW ,DENY`) would produce.
    let mut ops = operands
        .flat_map(|t| t.split(','))
        .filter(|s| !s.is_empty());
    let a = ops.next();
    let b = ops.next();
    let (Some(a), Some(b)) = (a, b) else {
        return Err(BridgeError::GroupConfigError(format!(
            "line {lineno}: ORDER requires two operands (ALLOW, DENY or DENY, ALLOW)"
        )));
    };
    if a.eq_ignore_ascii_case("ALLOW") && b.eq_ignore_ascii_case("DENY") {
        Ok(EvaluationOrder::AllowDeny)
    } else if a.eq_ignore_ascii_case("DENY") && b.eq_ignore_ascii_case("ALLOW") {
        Ok(EvaluationOrder::DenyAllow)
    } else {
        Err(BridgeError::GroupConfigError(format!(
            "line {lineno}: invalid ORDER operands '{a} {b}' \
             (expected ALLOW, DENY or DENY, ALLOW)"
        )))
    }
}

/// Parse a leading signed decimal integer prefix exactly like C
/// `sscanf(token, "%d", &lev)`: an optional `+`/`-` sign followed by one or
/// more decimal digits is consumed, and any trailing non-numeric bytes are
/// ignored (`0junk` → `0`, `1foo` → `1`, `-1tail` → `-1`). Returns `None`
/// when no digit follows the optional sign — the case where C `sscanf`
/// returns 0 and the caller applies the `lev=1` fallback. Tokens reach here
/// already split on whitespace, so there is no leading whitespace for
/// `sscanf` to skip.
fn sscanf_int_prefix(token: &str) -> Option<i32> {
    let bytes = token.as_bytes();
    let mut end = 0;
    if matches!(bytes.first(), Some(b'+' | b'-')) {
        end = 1;
    }
    let digits_start = end;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end == digits_start {
        return None; // no digits after optional sign → C sscanf returns != 1
    }
    // A prefix that overflows `i32` is degenerate input C leaves undefined; map
    // it to the same no-conversion fallback rather than guessing C's wrap.
    token[..end].parse::<i32>().ok()
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
/// - The ASL token is read with C `sscanf("%d")` prefix semantics (see
///   [`sscanf_int_prefix`]): a leading signed decimal prefix is consumed and
///   trailing bytes are ignored, so `0junk` installs level 0, `1foo` level 1,
///   and `-1tail` level -1. Only a token with **no** integer prefix falls back
///   to **level 1** (`sscanf(...)!=1 → lev=1`) instead of aborting. A single
///   typo such as `PV.* ALLOW BeamGroup typo` keeps serving `PV.*` at ASL 1 in
///   C; an earlier whole-token `s.parse::<i32>()` both rejected the pvlist on a
///   typo and mis-rejected a valid C prefix like `0junk`. The omitted-ASL and
///   no-prefix cases both resolve to 1 — `Some(1)` here for the no-prefix case
///   records the explicit fallback, `None` for the omitted case defaults to 1
///   via [`PvListMatch::effective_asl`].
///
/// Genuine syntax errors C also rejects (a missing ALIAS target) remain hard
/// errors at their call site, not here.
fn parse_asg_asl<'a>(
    tokens: &mut impl Iterator<Item = &'a str>,
    lineno: usize,
) -> (Option<String>, Option<i32>) {
    let asg = tokens.next().map(String::from);
    let asl = asg.is_some().then(|| tokens.next()).flatten().map(|s| {
        sscanf_int_prefix(s).unwrap_or_else(|| {
            tracing::warn!(
                line = lineno,
                token = %s,
                "pvlist: ASL token has no integer prefix — falling back to level 1 \
                 (C gateAs.cc:611 sscanf!=1 → lev=1)"
            );
            1
        })
    });
    (asg, asl)
}

fn build_pattern(pat: &str, lineno: usize) -> BridgeResult<Regex> {
    // C ca-gateway's DEFAULT build compiles `.pvlist` patterns as GNU basic
    // regular expressions: configure/CONFIG_SITE leaves USE_PCRE commented out
    // (CONFIG_SITE:15-16), so gateAs::readPvList calls re_compile_pattern
    // (gateAs.cc:236) with the GNU `re_syntax_options` default of 0 — it never
    // calls re_set_syntax. Under that syntax the grouping/alternation
    // metacharacters are INVERTED relative to Rust's `regex` (an ERE/RE2
    // dialect): `\(`/`\)` open/close a capture group while bare `(`/`)` are
    // literal, `\|` is alternation while `|` is literal, and intervals are off
    // so `{`/`}` are literal. Feeding such a pattern straight into
    // `Regex::new` (the previous behavior) made the shipped GNU example/test
    // pvlist rules — `gateway:\(.*\) ALIAS ioc:\1` (example/GATEWAY.pvlist:83,
    // testTop/pyTestsApp/pvlist_bre.txt:6) — compile to literal parentheses
    // with no capture group, so `\1` alias backreferences expanded to "".
    //
    // Translate GNU/BRE → Rust at this single compile boundary (the one owner
    // of "pattern string -> Regex") so capture-group numbering lines up with
    // the `\1`..`\9` template expansion in `expand_template` by construction.
    let translated = translate_gnu_bre_to_rust(pat).map_err(|e| {
        BridgeError::GroupConfigError(format!("line {lineno}: pvlist pattern '{pat}': {e}"))
    })?;
    // Anchor the pattern to match the full PV name (C++ ca-gateway behavior:
    // gateAs.cc:386-405 admits a name only when `re_match(...) == len`, a
    // whole-string match of whichever alternation branch matched). The
    // translated source must be wrapped in a group before anchoring: in Rust
    // `regex` (as in EREs) alternation `|` binds LOWER than the `^`/`$`
    // anchors, so a translated `foo|bar` anchored bare as `^foo|bar$` parses
    // as `(^foo)|(bar$)` and matches `foobar` (via `^foo`) and `xxbar` (via
    // `bar$`) — names C `re_match` rejects because their full length never
    // equals the matched branch length. A NON-capturing group `(?:…)` scopes
    // the alternation under both anchors without consuming a capture index, so
    // the `\1`..`\9` ALIAS backreference numbering in `expand_template` (which
    // indexes captures positionally) is unchanged.
    let anchored = format!("^(?:{translated})$");
    Regex::new(&anchored).map_err(|e| {
        BridgeError::GroupConfigError(format!("line {lineno}: invalid regex '{pat}': {e}"))
    })
}

/// Translate a GNU basic-regular-expression (`re_syntax_options == 0`) pattern
/// — ca-gateway's default `.pvlist` dialect — into the equivalent Rust `regex`
/// (ERE/RE2) source.
///
/// Only the metacharacters whose meaning is INVERTED between the two dialects
/// are rewritten; `*`, `+`, `?`, `.`, `^`, `$`, `\.`, `\*` and other escapes
/// already mean the same thing in both and pass through verbatim:
///
/// - `\(` → `(`, `\)` → `)` (BRE group → RE2 group)
/// - bare `(` → `\(`, `)` → `\)` (BRE literal paren → RE2 literal)
/// - `\|` → `|` (BRE alternation → RE2 alternation)
/// - bare `|` → `\|` (BRE literal bar → RE2 literal)
/// - `{`, `}`, `\{`, `\}` → `\{`, `\}` (GNU syntax-0 has RE_INTERVALS off, so
///   every brace form is literal; RE2 treats bare `{` as an interval, so it
///   must be escaped to stay literal)
///
/// Inside a `[...]` bracket expression none of those inversions apply (parens,
/// braces and bars are literal in both dialects), but GNU treats a backslash as
/// an ordinary character there (RE_BACKSLASH_ESCAPE_IN_LISTS is off) while RE2
/// treats it as an escape — so a literal `\` inside a list is emitted as `\\`.
/// POSIX classes `[[:alpha:]]` are recognized so their inner `]` does not close
/// the list early.
///
/// A `\1`..`\9` back-reference inside the *pattern* (a GNU feature RE2 cannot
/// represent) is rejected with a clear diagnostic rather than silently
/// mis-compiled. Alias-target backreferences are unaffected — those are
/// expanded by [`expand_template`], not by the regex engine.
fn translate_gnu_bre_to_rust(pat: &str) -> Result<String, String> {
    let bytes = pat.as_bytes();
    let mut out = String::with_capacity(pat.len() + 8);
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'\\' => {
                if i + 1 >= bytes.len() {
                    // Trailing backslash: GNU errors; emit a literal backslash
                    // so RE2 does not choke on a dangling escape.
                    out.push_str("\\\\");
                    i += 1;
                    continue;
                }
                let n = bytes[i + 1];
                match n {
                    b'(' => out.push('('),
                    b')' => out.push(')'),
                    b'|' => out.push('|'),
                    b'{' => out.push_str("\\{"),
                    b'}' => out.push_str("\\}"),
                    b'1'..=b'9' => {
                        return Err(format!(
                            "GNU back-reference '\\{}' in a pattern is unsupported by the Rust \
                             regex engine; rewrite the rule without an in-pattern backreference \
                             (alias-target backreferences in the ALIAS target are still supported)",
                            n as char
                        ));
                    }
                    // Every other escape (\., \*, \\, \w, \s, …) means the same
                    // in both dialects — copy it through unchanged.
                    other => {
                        out.push('\\');
                        out.push(other as char);
                    }
                }
                i += 2;
            }
            b'(' => {
                out.push_str("\\(");
                i += 1;
            }
            b')' => {
                out.push_str("\\)");
                i += 1;
            }
            b'|' => {
                out.push_str("\\|");
                i += 1;
            }
            b'{' => {
                out.push_str("\\{");
                i += 1;
            }
            b'}' => {
                out.push_str("\\}");
                i += 1;
            }
            b'[' => {
                i = translate_bracket_expr(bytes, i, &mut out);
            }
            _ => {
                out.push(c as char);
                i += 1;
            }
        }
    }
    Ok(out)
}

/// Copy a `[...]` bracket expression starting at `bytes[start] == b'['`,
/// applying the GNU→RE2 list rules (literal `\` → `\\`, POSIX-class passthrough,
/// leading `^`/`]` handling). Returns the index just past the closing `]`, or
/// past the unterminated tail (so the surrounding `Regex::new` reports the
/// missing-bracket error, matching GNU's own compile failure).
fn translate_bracket_expr(bytes: &[u8], start: usize, out: &mut String) -> usize {
    out.push('[');
    let mut i = start + 1;
    // Optional negation.
    if i < bytes.len() && bytes[i] == b'^' {
        out.push('^');
        i += 1;
    }
    // A `]` immediately here is a literal member, not the terminator.
    if i < bytes.len() && bytes[i] == b']' {
        out.push(']');
        i += 1;
    }
    while i < bytes.len() {
        match bytes[i] {
            b']' => {
                out.push(']');
                return i + 1;
            }
            b'[' if i + 1 < bytes.len() && bytes[i + 1] == b':' => {
                // POSIX class [:name:] — copy through to the closing ":]".
                out.push_str("[:");
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b':' && bytes[i + 1] == b']') {
                    out.push(bytes[i] as char);
                    i += 1;
                }
                if i + 1 < bytes.len() {
                    out.push_str(":]");
                    i += 2;
                }
            }
            b'\\' => {
                // GNU: backslash is an ordinary list member. RE2: it escapes,
                // so emit a literal backslash as `\\`.
                out.push_str("\\\\");
                i += 1;
            }
            other => {
                out.push(other as char);
                i += 1;
            }
        }
    }
    i
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

    /// C ca-gateway tokenizes the ORDER directive case-insensitively and
    /// accepts comma OR whitespace between operands (gateAs.cc:580-595). A
    /// pvlist that C loads must not fail Rust startup just because the
    /// operator wrote it in lower case or dropped the comma.
    #[test]
    fn parse_evaluation_order_case_and_delimiter_tolerant() {
        // Documented uppercase comma form still works.
        assert_eq!(
            parse_pvlist("EVALUATION ORDER ALLOW, DENY").unwrap().order,
            EvaluationOrder::AllowDeny
        );
        // Lower case.
        assert_eq!(
            parse_pvlist("evaluation order deny, allow").unwrap().order,
            EvaluationOrder::DenyAllow
        );
        // Whitespace-only operand separator (no comma).
        assert_eq!(
            parse_pvlist("EVALUATION ORDER DENY ALLOW").unwrap().order,
            EvaluationOrder::DenyAllow
        );
        assert_eq!(
            parse_pvlist("evaluation order allow deny").unwrap().order,
            EvaluationOrder::AllowDeny
        );
        // No-comma, no-space (single token "ALLOW,DENY").
        assert_eq!(
            parse_pvlist("EVALUATION ORDER ALLOW,DENY").unwrap().order,
            EvaluationOrder::AllowDeny
        );
        // The leading word is an ignored pattern token in C — any word works.
        assert_eq!(
            parse_pvlist("FOO ORDER ALLOW DENY").unwrap().order,
            EvaluationOrder::AllowDeny
        );

        // Genuinely invalid operands are skipped (logged) and leave the order
        // at its prior value — C logs "invalid"/"missing argument" and
        // `continue`s WITHOUT reassigning `eval_order` (gateAs.cc:585,593-597).
        // With no preceding ORDER line the default (AllowDeny) is retained.
        assert_eq!(
            parse_pvlist("EVALUATION ORDER ALLOW ALLOW").unwrap().order,
            EvaluationOrder::AllowDeny,
            "invalid ORDER operands leave default order unchanged"
        );
        assert_eq!(
            parse_pvlist("EVALUATION ORDER ALLOW").unwrap().order,
            EvaluationOrder::AllowDeny,
            "missing ORDER operand leaves default order unchanged"
        );

        // `ORDER` as the *first* token is NOT a directive — it is a pattern
        // named "ORDER" with keyword ALLOW (matches C: pattern, then cmd).
        let list = parse_pvlist("ORDER ALLOW").unwrap();
        assert_eq!(list.entries.len(), 1);
        assert!(matches!(list.entries[0], PvListEntry::Allow { .. }));
        assert_eq!(list.order, EvaluationOrder::AllowDeny, "order unchanged");
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

        // No-FROM combined with comma tokenization (host splitting still
        // applies without the keyword).
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
        // Unknown command: C logs "invalid command '%s'" and the loop iterates
        // to the next line (gateAs.cc:627-629) — the malformed line is dropped,
        // not a whole-file abort. parse_rule_line still reports the error; the
        // file parser skips it.
        assert!(parse_rule_line("foo BAD", 1).is_err());
        let list = parse_pvlist("foo BAD").unwrap();
        assert!(list.entries.is_empty(), "unknown command line skipped");
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
        // Bad pattern: C's gateAsEntry::init() fails, the entry is deleted and
        // the loop continues (gateAs.cc:619-620). parse_rule_line reports the
        // error; the file parser skips just that line.
        assert!(parse_rule_line("[invalid ALLOW", 1).is_err());
        let list = parse_pvlist("[invalid ALLOW").unwrap();
        assert!(list.entries.is_empty(), "invalid-regex line skipped");
    }

    #[test]
    fn parse_alias_missing_target() {
        // Missing ALIAS real-name target: C logs "missing real name in ALIAS
        // command" and `continue`s (gateAs.cc:602-605). parse_rule_line reports
        // the error; the file parser skips just that line, not the whole file.
        assert!(parse_rule_line("foo ALIAS", 1).is_err());
        let list = parse_pvlist("foo ALIAS").unwrap();
        assert!(list.entries.is_empty(), "ALIAS-missing-target line skipped");
    }

    /// A single malformed
    /// `.pvlist` line must NOT abort the whole load/reload. C `gateAs::readPvList`
    /// (gateAs.cc:530-632) logs and `continue`s on every malformed line family —
    /// missing command (:532-535), missing/invalid ORDER operands (:585,:593-597,
    /// `eval_order` left unchanged), missing ALIAS target (:602-605), unknown
    /// command (:627-629), and `gateAsEntry::init()`/regex failures (:619-620) —
    /// keeping the valid rules before and after it. This test sandwiches each
    /// malformed family between valid ALLOW lines and asserts (a) all valid
    /// siblings survive and (b) the invalid ORDER leaves the previous order value
    /// unchanged.
    #[test]
    fn malformed_lines_are_skipped_valid_siblings_survive() {
        let content = r#"
            A0:.* ALLOW
            LonelyPattern
            A1:.* ALLOW
            EVALUATION ORDER DENY, ALLOW
            EVALUATION ORDER ALLOW ALLOW
            A2:.* ALLOW
            foo ALIAS
            A3:.* ALLOW
            bar BAZ
            A4:.* ALLOW
            [invalid ALLOW
            A5:.* ALLOW
        "#;
        let list = parse_pvlist(content).unwrap();

        // The six valid ALLOW lines (one before and one after each malformed
        // family) all load; the five malformed lines contribute nothing.
        assert_eq!(
            list.entries.len(),
            6,
            "all valid ALLOW siblings survive; malformed lines skipped"
        );
        for i in 0..6 {
            assert!(
                list.match_name(&format!("A{i}:x")).is_some(),
                "valid sibling A{i} survived",
            );
        }

        // The valid `DENY, ALLOW` line set DenyAllow; the immediately-following
        // invalid `ALLOW ALLOW` line is skipped and must NOT reset the order —
        // C never reassigns `eval_order` on the invalid branch. A reset would
        // leave the default AllowDeny, so DenyAllow proves the invalid line was
        // a no-op for the order.
        assert_eq!(
            list.order,
            EvaluationOrder::DenyAllow,
            "invalid ORDER line leaves the previously-set order unchanged"
        );
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

        // A missing ALIAS target is a genuine per-line syntax error (C logs
        // "missing real name in ALIAS command" and `continue`s,
        // gateAs.cc:602-605) — it must NOT fall through to a level-1 ALIAS
        // rule. parse_rule_line rejects it; the file parser skips the line,
        // leaving no entry, distinct from the unparseable-ASL case above which
        // keeps the rule at level 1.
        assert!(parse_rule_line("foo ALIAS", 1).is_err());
        let list = parse_pvlist("foo ALIAS").unwrap();
        assert!(
            list.entries.is_empty(),
            "missing ALIAS target is skipped, not installed at level 1"
        );
    }

    /// C reads the ASL token with
    /// `sscanf(asl,"%d",&lev)` (gateAs.cc:611), which consumes a leading signed
    /// decimal prefix and ignores trailing bytes. A whole-token
    /// `s.parse::<i32>()` rejected `0junk`/`1foo`/`-1tail` and fell back to
    /// level 1, silently raising `0junk` from ASL 0 to ASL 1 — a stricter
    /// access level than C installs. Only a token with no integer prefix
    /// (`typo`) takes the level-1 fallback.
    #[test]
    fn parse_asl_numeric_prefix_matches_c_sscanf() {
        for (token, want) in [("0junk", 0), ("1foo", 1), ("-1tail", -1), ("2tail", 2)] {
            let list = parse_pvlist(&format!("PV.* ALLOW grp {token}")).unwrap();
            match &list.entries[0] {
                PvListEntry::Allow { asl, .. } => assert_eq!(
                    *asl,
                    Some(want),
                    "ASL token {token:?} must take its C sscanf %d prefix {want}"
                ),
                other => panic!("{token}: expected Allow, got {other:?}"),
            }
        }

        // No integer prefix → C sscanf returns 0 → level-1 fallback. A bare
        // sign with no digit (`+`) also has no prefix.
        for token in ["typo", "+", "junk7"] {
            let list = parse_pvlist(&format!("PV.* ALLOW grp {token}")).unwrap();
            match &list.entries[0] {
                PvListEntry::Allow { asl, .. } => {
                    assert_eq!(*asl, Some(1), "no-prefix ASL token {token:?} → level 1")
                }
                other => panic!("{token}: expected Allow, got {other:?}"),
            }
        }
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
        assert!(
            list.match_name_for_host("PV:x", &PolicyHost::for_test("bad.host"))
                .is_none()
        );
        assert!(
            list.match_name_for_host("PV:x", &PolicyHost::for_test("10.0.0.9"))
                .is_none()
        );
        // Any other host is admitted.
        assert!(
            list.match_name_for_host("PV:x", &PolicyHost::for_test("good.host"))
                .is_some()
        );
        assert!(
            list.match_name_for_host("PV:x", &PolicyHost::for_test("10.0.0.1"))
                .is_some()
        );
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
        assert!(
            list.match_name_for_host("PV:x", &PolicyHost::for_test("bad.host"))
                .is_none()
        );
        assert!(
            list.match_name_for_host("PV:x", &PolicyHost::for_test("good.host"))
                .is_some()
        );
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
        assert!(
            list.match_name_for_host("PV:x", &PolicyHost::for_test("BAD.HOST"))
                .is_none()
        );
        assert!(
            list.match_name_for_host("PV:x", &PolicyHost::for_test("bad.host"))
                .is_none()
        );
        assert!(
            list.match_name_for_host("PV:x", &PolicyHost::for_test("other.host"))
                .is_some()
        );
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
                \(.*\)        ALIAS general_\1 GenGrp 1
                SEC:\(.*\)    ALIAS secure_\1 SecGrp 0
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
        // BRE grouping `\(...\)` — ca-gateway's default dialect.
        let list = parse_pvlist(r"ps\([0-9]\) ALIAS PSCurrent\1.ai PSGroup 1").unwrap();
        let m = list.match_name("ps3").unwrap();
        assert!(m.is_alias);
        assert_eq!(m.resolved_name, "PSCurrent3.ai");
        assert_eq!(m.asg.as_deref(), Some("PSGroup"));
        assert_eq!(m.asl, Some(1));
    }

    #[test]
    fn match_alias_multiple_groups() {
        // BRE grouping with two capture groups.
        let list = parse_pvlist(r"\([a-z]*\):\([0-9]*\) ALIAS \1_record\2.VAL").unwrap();
        let m = list.match_name("temp:7").unwrap();
        assert_eq!(m.resolved_name, "temp_record7.VAL");
    }

    /// The grouping defect: ca-gateway's DEFAULT build treats `.pvlist`
    /// patterns as GNU/BRE, so `\(...\)` is a capture group and bare `(...)`
    /// is literal. The shipped `example/GATEWAY.pvlist:83` and
    /// `testTop/pyTestsApp/pvlist_bre.txt:6` alias rules must match and expand
    /// `\1` exactly as ca-gateway does.
    #[test]
    fn bre_grouping_matches_shipped_upstream_examples() {
        // pvlist_bre.txt:6 — gateway: prefix → ioc: prefix.
        let list = parse_pvlist(r"gateway:\(.*\) ALIAS ioc:\1").unwrap();
        let m = list
            .match_name("gateway:HV:voltage")
            .expect("BRE alias matches");
        assert!(m.is_alias);
        assert_eq!(m.resolved_name, "ioc:HV:voltage");

        // example/GATEWAY.pvlist:80 — ps\([0-9]\) → PSCurrent\1.ai
        let list = parse_pvlist(r"ps\([0-9]\) ALIAS PSCurrent\1.ai PowerSupply 1").unwrap();
        let m = list.match_name("ps7").expect("BRE alias matches");
        assert_eq!(m.resolved_name, "PSCurrent7.ai");
        // The unescaped form must NOT match the digit — bare parens are literal.
        assert!(
            list.match_name("ps(7)").is_none(),
            "no literal-paren PV exists"
        );
    }

    /// Inverse of the grouping rule: under GNU/BRE bare `(`/`)` are LITERAL
    /// parentheses, so an ERE-style `(.*)` pattern matches a name that actually
    /// contains parentheses — and yields no capture group (matching C, where
    /// such a rule is a literal-paren match with an empty `\1`).
    #[test]
    fn bare_parens_are_literal_in_bre() {
        let list = parse_pvlist(r"weird(.*) ALLOW").unwrap();
        // Bare `(.*)`  ==  literal `(`, `.*`, literal `)`.
        assert!(
            list.match_name("weird(abc)").is_some(),
            "literal parens match"
        );
        assert!(
            list.match_name("weirdabc").is_none(),
            "without literal parens it must not match"
        );
    }

    /// `\|` is BRE alternation; bare `|` is a literal pipe. Grouped so the
    /// `^…$` anchoring scopes the alternation (un-grouped `^a\|b$` would, by
    /// regex precedence, mean `^a` OR `b$` in either dialect).
    #[test]
    fn bre_alternation_and_literal_pipe() {
        let list = parse_pvlist(r"\(foo\|bar\) ALLOW").unwrap();
        assert!(list.match_name("foo").is_some(), "BRE \\| alternates");
        assert!(list.match_name("bar").is_some(), "BRE \\| alternates");
        assert!(list.match_name("foobar").is_none());

        let list = parse_pvlist(r"a|b ALLOW").unwrap();
        assert!(list.match_name("a|b").is_some(), "bare | is literal");
        assert!(list.match_name("a").is_none(), "bare | does not alternate");
    }

    /// An UNGROUPED GNU/BRE
    /// alternation `foo\|bar` is C-valid: C compiles it with
    /// `re_compile_pattern` (gateAs.cc:236) and admits a name only when
    /// `re_match(...) == len` (gateAs.cc:386-405) — a WHOLE-string match of
    /// whichever branch matched, so `foo\|bar` matches exactly `foo` or `bar`
    /// and never `foobar`/`xxbar`. The translated source `foo|bar` must be
    /// scoped by a group before the `^…$` anchors; the previous bare
    /// `^foo|bar$` parsed (by `|`'s low precedence) as `^foo` OR `bar$` and
    /// over-matched `foobar` (prefix) and `xxbar` (suffix) — names C rejects.
    #[test]
    fn bre_ungrouped_alternation_is_whole_string_anchored() {
        let list = parse_pvlist(r"foo\|bar ALLOW").unwrap();
        assert!(
            list.match_name("foo").is_some(),
            "left branch matches the whole name"
        );
        assert!(
            list.match_name("bar").is_some(),
            "right branch matches the whole name"
        );
        assert!(
            list.match_name("foobar").is_none(),
            "must not match via the ^foo prefix (bare ^foo|bar$ precedence)"
        );
        assert!(
            list.match_name("xxbar").is_none(),
            "must not match via the bar$ suffix (bare ^foo|bar$ precedence)"
        );

        // The wrapper is NON-capturing, so positional ALIAS backreferences are
        // unchanged: a top-level alternation of two capture groups still
        // expands `\1`/`\2` to groups 1 and 2 (the absent branch expands to "").
        let aliases = parse_pvlist(r"dev:\(.*\)\|sys:\(.*\) ALIAS out:\1\2").unwrap();
        let m = aliases
            .match_name("dev:hv")
            .expect("left branch alias matches");
        assert!(m.is_alias);
        assert_eq!(m.resolved_name, "out:hv", "\\1 from group 1, \\2 absent");
        let m = aliases
            .match_name("sys:lv")
            .expect("right branch alias matches");
        assert_eq!(m.resolved_name, "out:lv", "\\2 from group 2, \\1 absent");
    }

    /// A GNU back-reference inside the *pattern* is unrepresentable in the Rust
    /// regex engine; the parser rejects it with a clear diagnostic rather than
    /// silently mis-compiling (fix-direction option (b)).
    #[test]
    fn in_pattern_backreference_is_rejected() {
        // parse_rule_line still produces the clear back-reference diagnostic...
        let err = parse_rule_line(r"\(.*\)\1 ALLOW", 1).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("back-reference"),
            "expected a back-reference diagnostic, got: {msg}"
        );
        // ...but at the file level the unrepresentable line is skipped, not a
        // whole-file abort — C compiles BRE `\1` fine, so dropping just this
        // line keeps the Rust port's reload behaviour C-compatible for the
        // valid siblings.
        let list = parse_pvlist(r"\(.*\)\1 ALLOW").unwrap();
        assert!(list.entries.is_empty(), "back-reference line skipped");
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
        assert!(list.is_host_denied("PV:x", &PolicyHost::for_test("192.0.2.1")));
        // Other IPs are not denied.
        assert!(!list.is_host_denied("PV:x", &PolicyHost::for_test("192.0.2.2")));
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
            // The resolved IP (127.0.0.1) must deny the peer.
            let denied = from_hosts
                .iter()
                .any(|ip| list.is_host_denied("PV:x", &PolicyHost::for_test(ip)));
            assert!(denied, "resolved IP must be denied");
        } else {
            panic!("expected Deny");
        }
    }

    /// C ca-gateway resolves each DENY FROM hostname to a SINGLE IPv4 address
    /// (`hostToIPAddr` is AF_INET and breaks after the first getaddrinfo
    /// result, osdSock.c:250-267; gateAs.cc:493-503 stores one dotted IP).
    /// `resolve_hosts` must therefore store at most one address per hostname,
    /// and that address must be IPv4 — never the multi-address / IPv6 fan-out
    /// that would deny strictly more peers than C.
    #[tokio::test]
    async fn resolve_hosts_hostname_keeps_single_ipv4() {
        let mut list = parse_pvlist("PV.* DENY FROM localhost").unwrap();
        list.resolve_hosts().await;
        let PvListEntry::Deny { from_hosts, .. } = &list.entries[0] else {
            panic!("expected Deny");
        };
        // At most one address (C keeps only the first); on any normal host
        // `localhost` has an IPv4 record so exactly one is expected.
        assert!(
            from_hosts.len() <= 1,
            "DENY FROM hostname must store a single address, got {from_hosts:?}"
        );
        for h in from_hosts {
            let ip: std::net::IpAddr = h.parse().expect("resolved entry is an IP");
            assert!(
                ip.is_ipv4(),
                "resolved DENY FROM address must be IPv4, got {ip}"
            );
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
        assert!(!list.is_host_denied("PV:x", &PolicyHost::for_test("10.0.0.1")));
        // Distinguishing assertion (fail-closed vs fail-open): under the default
        // ALLOW,DENY order the collapsed global deny must make `match_name`
        // reject the pattern outright. `is_host_denied` alone is true under both
        // fail-open and fail-closed, so this is what actually pins the behavior.
        assert!(
            list.match_name("PV:x").is_none(),
            "all-unresolvable DENY FROM must collapse to a global deny (fail-closed)"
        );
    }

    /// An IPv6 literal in `DENY FROM` is dropped — C `aToIPAddr` is
    /// IPv4-only and never creates a host-scoped deny for an IPv6 address,
    /// so the token is treated like an unresolvable host and the rule (with
    /// no surviving host token) collapses to a global deny (fail-closed,
    /// matches C `gateAs.cc:540-556`). Rust must NOT keep `::1` as a scoped
    /// IPv6-only deny C cannot express.
    #[tokio::test]
    async fn resolve_hosts_ipv6_literal_dropped() {
        let mut list = parse_pvlist(
            r#"
            PV.* ALLOW
            PV.* DENY FROM ::1
            "#,
        )
        .unwrap();
        list.resolve_hosts().await;
        let PvListEntry::Deny { from_hosts, .. } = &list.entries[1] else {
            panic!("expected Deny");
        };
        assert!(
            from_hosts.is_empty(),
            "IPv6 literal must be dropped (C aToIPAddr is IPv4-only); got {from_hosts:?}"
        );
        // The IPv6 loopback peer is not host-scoped denied (C cannot scope it).
        assert!(!list.is_host_denied("PV:x", &PolicyHost::for_test("::1")));
        // Having no surviving host token, the rule is now a global deny — under
        // the default ALLOW,DENY order `match_name` rejects the pattern.
        assert!(
            list.match_name("PV:x").is_none(),
            "DENY FROM ::1 must collapse to a global deny once the IPv6 token is dropped"
        );
    }

    /// A mixed `DENY FROM 192.0.2.1 ::1` keeps the IPv4 token host-scoped
    /// while dropping the IPv6 token — the IPv4 deny still fires for its
    /// peer, and no Rust-only scoped IPv6 deny is created. Because a host
    /// token survives, the rule stays host-targeted (NOT a global deny).
    #[tokio::test]
    async fn resolve_hosts_mixed_ipv4_ipv6_keeps_only_ipv4() {
        let mut list = parse_pvlist(
            r#"
            PV.* ALLOW
            PV.* DENY FROM 192.0.2.1 ::1
            "#,
        )
        .unwrap();
        list.resolve_hosts().await;
        let PvListEntry::Deny { from_hosts, .. } = &list.entries[1] else {
            panic!("expected Deny");
        };
        assert_eq!(
            from_hosts,
            &["192.0.2.1"],
            "only the IPv4 token survives; the IPv6 token is dropped"
        );
        // IPv4 token still scopes the deny to its peer.
        assert!(list.is_host_denied("PV:x", &PolicyHost::for_test("192.0.2.1")));
        // IPv6 peer is not denied (no Rust-only scoped IPv6 deny).
        assert!(!list.is_host_denied("PV:x", &PolicyHost::for_test("::1")));
        // A surviving host token keeps the rule host-targeted, so a different
        // host that is NOT 192.0.2.1 still reaches the ALLOW.
        assert!(
            list.match_name("PV:x").is_some(),
            "a surviving IPv4 host token keeps the deny host-scoped, not global"
        );
    }
    /// The identity a `DENY FROM` row is matched against comes from the
    /// socket and nothing else — a client cannot select which row applies to
    /// it by choosing what to claim in CA `HOST_NAME`.
    ///
    /// Before this, the write path passed `WriteContext::host` (the claimed
    /// name) while the search/create path passed the peer address, so the two
    /// enforcement points disagreed about what "host" meant. Both now take a
    /// `PolicyHost`, which only a peer address can produce.
    #[test]
    fn a_claimed_host_cannot_select_which_deny_row_applies() {
        let list = parse_pvlist(
            "EVALUATION ORDER ALLOW, DENY\n             .* ALLOW\n             SECRET:.* DENY FROM 192.0.2.7\n",
        )
        .expect("pvlist parses");

        // The denied peer is denied, whatever it claims to be. The claim is
        // not even expressible here: `from_peer` reads the socket.
        let denied: std::net::SocketAddr = "192.0.2.7:44321".parse().unwrap();
        assert!(
            list.is_host_denied("SECRET:PV", &PolicyHost::from_peer(denied)),
            "the row must fire on the peer address"
        );
        assert!(
            list.match_name_for_host("SECRET:PV", &PolicyHost::from_peer(denied))
                .is_none()
        );

        // A different peer is not denied, and cannot be made denied by
        // claiming to be the denied host either.
        let other: std::net::SocketAddr = "198.51.100.9:1234".parse().unwrap();
        assert!(!list.is_host_denied("SECRET:PV", &PolicyHost::from_peer(other)));
        assert!(
            list.match_name_for_host("SECRET:PV", &PolicyHost::from_peer(other))
                .is_some()
        );

        // The port is not part of the identity: the same host on any port is
        // the same policy subject. C strips it explicitly
        // (`gateServer.cc:1529`, `strchr(hostname, \':\')`).
        for port in [1u16, 5064, 65535] {
            let same = std::net::SocketAddr::new(denied.ip(), port);
            assert!(
                list.is_host_denied("SECRET:PV", &PolicyHost::from_peer(same)),
                "port {port} must not change the decision"
            );
        }

        // Both enforcement points reach the same verdict for the same peer —
        // that is the property that was broken.
        for peer in [denied, other] {
            let h = PolicyHost::from_peer(peer);
            assert_eq!(
                list.is_host_denied("SECRET:PV", &h),
                list.match_name_for_host("SECRET:PV", &h).is_none(),
                "the write path and the search path must agree for {peer}"
            );
        }
    }

    /// `WriteContext::peer` is an `"ip:port"` string; the funnel accepts it
    /// and rejects anything that is not a socket address, so a caller cannot
    /// smuggle a name in through the string form.
    #[test]
    fn the_peer_string_funnel_accepts_only_socket_addresses() {
        assert_eq!(
            PolicyHost::from_peer_str("192.0.2.7:44321").map(|h| h.as_str().to_string()),
            Some("192.0.2.7".to_string())
        );
        assert_eq!(
            PolicyHost::from_peer_str("[::1]:5064").map(|h| h.as_str().to_string()),
            Some("::1".to_string()),
            "IPv6 is bracket-less once it is a policy host"
        );
        for not_a_peer in [
            "opi-1",               // a claimed name
            "trusted-console.lab", // a claimed name that looks authoritative
            "192.0.2.7",           // an address with no port is not a peer
            "",
        ] {
            assert!(
                PolicyHost::from_peer_str(not_a_peer).is_none(),
                "{not_a_peer:?} is not a peer address and must not become a PolicyHost"
            );
        }
    }

    /// Source guard on the property the type is carrying: in production,
    /// `PolicyHost` can be built ONLY from a peer.
    ///
    /// The compiler already enforces the call sites — reverting either
    /// enforcement point to a claimed name does not compile, because a
    /// `String` is not a `PolicyHost` and the only test constructor is
    /// `cfg(test)`. What the compiler cannot notice is someone ADDING a
    /// production constructor that takes an arbitrary string, which would
    /// quietly reopen the whole finding. That is what this reads.
    #[test]
    fn policy_host_has_no_production_constructor_from_an_arbitrary_string() {
        let src = include_str!("pvlist.rs");
        let start = src.find("impl PolicyHost {").expect("impl block");
        let body = &src[start..];
        let end = body.find("\n}\n").expect("end of impl block");
        let body = &body[..end];

        let mut cfg_test = false;
        let mut production_fns = Vec::new();
        for line in body.lines() {
            let t = line.trim();
            if t == "#[cfg(test)]" {
                cfg_test = true;
                continue;
            }
            if let Some(rest) = t.strip_prefix("pub fn ").or_else(|| t.strip_prefix("fn ")) {
                let name = rest.split(['(', '<']).next().unwrap_or("").to_string();
                if cfg_test {
                    assert_eq!(
                        name, "for_test",
                        "the only cfg(test) member may be the test constructor"
                    );
                } else {
                    production_fns.push(name);
                }
                cfg_test = false;
            }
        }
        production_fns.sort();
        assert_eq!(
            production_fns,
            vec![
                "as_str".to_string(),
                "from_peer".to_string(),
                "from_peer_str".to_string()
            ],
            "PolicyHost's production surface changed. Every constructor must \
             take a peer address; a constructor accepting an arbitrary string \
             reopens the claimed-host finding this type exists to close."
        );
    }
}
