use std::collections::HashMap;

use crate::error::{CaError, CaResult};

/// Access level for a channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessLevel {
    NoAccess,
    Read,
    ReadWrite,
}

/// Opaque proof that an access check has been performed.
///
/// Round 40 (type-state ACF gate): every `ChannelSource` op that
/// touches a PV by name now demands an `AccessChecked` instead of
/// raw `(name, ctx)`. The struct has only one public constructor —
/// [`AccessGate::check`] — so it is impossible to call a gated op
/// without first running the check. This is the structural fix for
/// the missed-path pattern that surfaced across rounds 32-39
/// (round-29 added ACF on three ops, then five subsequent rounds
/// uncovered four more wire paths that skipped the check).
///
/// The private `_seal` field blocks external struct-literal
/// construction; the constructor is reachable only through
/// `AccessGate::check`.
#[derive(Debug, Clone)]
pub struct AccessChecked {
    pv_name: String,
    level: AccessLevel,
    // Private nominal type; external crates cannot construct
    // `AccessSeal` and therefore cannot fabricate `AccessChecked`
    // via struct literal.
    _seal: AccessSeal,
}

#[derive(Debug, Clone)]
struct AccessSeal;

impl AccessChecked {
    /// The PV name the check was performed against.
    pub fn pv_name(&self) -> &str {
        &self.pv_name
    }

    /// Resolved access level for `(peer, asg, asl)`.
    pub fn level(&self) -> AccessLevel {
        self.level
    }

    /// True iff the level grants at least READ.
    pub fn allows_read(&self) -> bool {
        !matches!(self.level, AccessLevel::NoAccess)
    }

    /// True iff the level grants WRITE.
    pub fn allows_write(&self) -> bool {
        matches!(self.level, AccessLevel::ReadWrite)
    }
}

/// Per-source access policy holder. Wraps an optional
/// [`AccessSecurityConfig`] cell plus the PV → ASG/ASL resolution
/// hooks the source provides. The wire dispatcher (tcp.rs) asks
/// the source for its `AccessGate`, calls
/// [`AccessGate::check`] once per op, and threads the resulting
/// [`AccessChecked`] into the source's typed op methods.
///
/// Two variants:
///
/// * `Required` — an ACF cell is attached. The check evaluates it
///   under the read lock; absent ACF still produces a permissive
///   token (matching pre-Round-40 behaviour for sources whose ACF
///   cell is `None`).
/// * `Open` — the source explicitly opts out of ACF entirely
///   (e.g. test fixtures, in-process sources that never touch the
///   network). All checks return a `ReadWrite` token.
pub struct AccessGate {
    inner: AccessGateInner,
    /// Round 48 (R48-G3): generation counter bumped whenever the
    /// gate's underlying ACF policy changes (reload / clear / hot
    /// swap). Long-lived consumers (PVA monitor tasks spawned at
    /// SUBSCRIBE time, gateway bridge tasks) capture the value at
    /// spawn and compare on each event; a mismatch forces a fresh
    /// `check()` so a peer that was allowed at subscribe time but
    /// is now `NoAccess` under the new policy sees its subscription
    /// torn down on the next event (matching the round-39 CA-side
    /// `reeval_access_rights` semantics).
    ///
    /// Round 50 (R50-G1): two backing shapes —
    /// * `Atomic`: owned `AtomicU64` for terminal gates
    ///   (`Required`, `Open`). `bump_acl_version` `fetch_add`s.
    /// * `Aggregator`: a closure that returns a derived version
    ///   from sub-gates. `CompositeSource` uses this to expose a
    ///   gate whose `acl_version()` is the `wrapping_sum` of its
    ///   inner sources' versions (NOT `max` — see the round-50
    ///   audit follow-up: max produced false negatives when an
    ///   inner bumped to a value still under the existing peak),
    ///   so a bump on any inner (e.g. a
    ///   `GatewayChannelSource::set_acf` on a child) is visible at
    ///   the composite's top-level gate. Note: this gate is only a
    ///   **change signal** — the allow/deny authority remains the
    ///   matched inner source's gate; see
    ///   `ChannelSource::revalidate_read` for the owner path the
    ///   monitor reload loop uses. Pre-fix the composite
    ///   inherited the default `Open` gate (version=0 forever) and
    ///   tcp.rs's monitor loop compared against that stale value,
    ///   missing every inner reload.
    acl_version: AclVersionSource,
}

#[derive(Clone)]
enum AclVersionSource {
    Atomic(std::sync::Arc<std::sync::atomic::AtomicU64>),
    Aggregator(std::sync::Arc<dyn Fn() -> u64 + Send + Sync>),
}

/// Asynchronous closure that resolves `pv_name → (ASG, ASL)` for a
/// source. Sources install one when constructing an
/// [`AccessGate::required`].
pub type AsgAslResolver = std::sync::Arc<
    dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = (String, u8)> + Send>>
        + Send
        + Sync,
>;

enum AccessGateInner {
    /// ACF cell + resolver. The cell may hold `None` for "no
    /// policy attached" — the gate then issues permissive tokens
    /// (level = `ReadWrite`) so legacy behaviour is preserved when
    /// the operator hasn't loaded an ACF file.
    Required {
        acf: std::sync::Arc<tokio::sync::RwLock<Option<AccessSecurityConfig>>>,
        resolver: AsgAslResolver,
    },
    /// Always-permissive. Used by sources that have no security
    /// boundary by design (composite test fixtures, ControlSource
    /// for gateway diagnostic PVs, etc.).
    Open,
}

impl AccessGate {
    /// Build a gate that consults an ACF cell + a per-name
    /// `(ASG, ASL)` resolver. Allocates a fresh `acl_version`
    /// counter; use [`Self::required_with_version`] to share the
    /// counter with the owning server (so its `reload_acf_from`
    /// can signal the same generation bump this gate observes).
    pub fn required(
        acf: std::sync::Arc<tokio::sync::RwLock<Option<AccessSecurityConfig>>>,
        resolver: AsgAslResolver,
    ) -> Self {
        Self::required_with_version(
            acf,
            resolver,
            std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        )
    }

    /// Build a gate with an externally-supplied `acl_version`
    /// counter. The owning server (e.g. `PvaServer`) keeps the
    /// same `Arc` and `fetch_add`s on every `reload_acf_from` /
    /// `clear_acf` so monitor tasks holding the gate observe a
    /// version bump on their next event.
    pub fn required_with_version(
        acf: std::sync::Arc<tokio::sync::RwLock<Option<AccessSecurityConfig>>>,
        resolver: AsgAslResolver,
        acl_version: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        Self {
            inner: AccessGateInner::Required { acf, resolver },
            acl_version: AclVersionSource::Atomic(acl_version),
        }
    }

    /// Build a gate that grants `ReadWrite` to everyone. Used for
    /// sources that have no ACF semantics — composite test
    /// fixtures, in-process diagnostic sources, etc.
    pub fn open() -> Self {
        Self {
            inner: AccessGateInner::Open,
            acl_version: AclVersionSource::Atomic(std::sync::Arc::new(
                std::sync::atomic::AtomicU64::new(0),
            )),
        }
    }

    /// Build a permissive gate whose `acl_version()` is derived
    /// from a caller-supplied closure. Used by `CompositeSource`
    /// to aggregate inner sub-gates' versions — the closure
    /// returns `wrapping_sum(inner.access_gate().acl_version())`
    /// so a bump on any sub-source moves the aggregate (every
    /// per-inner version is monotonic via `fetch_add`, so the sum
    /// changes iff some inner moved). NOT `max(...)` — that shape
    /// produced false negatives when a smaller inner bumped under
    /// the existing peak (see round-50 audit). This gate is only
    /// a **change signal** for the monitor reload loop; the
    /// allow/deny authority is the matched inner source's gate,
    /// reached via `ChannelSource::revalidate_read`.
    ///
    /// `bump_acl_version()` on an `Aggregator` gate is a no-op:
    /// the version is derived, not owned. The aggregator's
    /// underlying gates own their own counters.
    pub fn open_with_aggregator(
        f: std::sync::Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Self {
        Self {
            inner: AccessGateInner::Open,
            acl_version: AclVersionSource::Aggregator(f),
        }
    }

    /// Current ACL generation. Monitor / subscription tasks capture
    /// this at spawn time and compare on each event. A bump (via
    /// [`Self::bump_acl_version`]) signals "the underlying ACF
    /// changed — re-check before forwarding the next event".
    pub fn acl_version(&self) -> u64 {
        match &self.acl_version {
            AclVersionSource::Atomic(a) => a.load(std::sync::atomic::Ordering::Acquire),
            AclVersionSource::Aggregator(f) => f(),
        }
    }

    /// Bump the ACL generation. Called by the owning server after
    /// swapping the ACF policy. Long-lived consumers detect the
    /// change on their next event and re-check.
    ///
    /// On an `Aggregator`-backed gate this is a no-op — the
    /// version is read-through to the underlying gates, which own
    /// their own counters.
    pub fn bump_acl_version(&self) {
        if let AclVersionSource::Atomic(a) = &self.acl_version {
            a.fetch_add(1, std::sync::atomic::Ordering::Release);
        }
    }

    /// Perform the access check for `pv_name` under the connecting
    /// peer's `(host, user, method, authority)`. Returns the only
    /// kind of value the source's op methods will accept.
    pub async fn check(
        &self,
        pv_name: impl Into<String>,
        host: &str,
        user: &str,
        method: &str,
        authority: &str,
    ) -> AccessChecked {
        let pv_name = pv_name.into();
        let level = match &self.inner {
            AccessGateInner::Open => AccessLevel::ReadWrite,
            AccessGateInner::Required { acf, resolver } => {
                let guard = acf.read().await;
                match *guard {
                    None => AccessLevel::ReadWrite,
                    Some(ref cfg) => {
                        let (asg, asl) = resolver(pv_name.clone()).await;
                        cfg.check_access_method(&asg, host, user, asl, method, authority)
                    }
                }
            }
        };
        AccessChecked {
            pv_name,
            level,
            _seal: AccessSeal,
        }
    }
}

#[cfg(test)]
mod access_checked_tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn open_gate_grants_read_write() {
        let gate = AccessGate::open();
        let checked = gate.check("any:pv", "h", "u", "anonymous", "").await;
        assert_eq!(checked.level(), AccessLevel::ReadWrite);
        assert!(checked.allows_read());
        assert!(checked.allows_write());
        assert_eq!(checked.pv_name(), "any:pv");
    }

    #[tokio::test]
    async fn required_gate_with_no_acf_attached_is_permissive() {
        let cell = Arc::new(tokio::sync::RwLock::new(None));
        let resolver: AsgAslResolver = Arc::new(|_pv| {
            Box::pin(async { ("DEFAULT".to_string(), 0u8) })
        });
        let gate = AccessGate::required(cell, resolver);
        let checked = gate.check("any:pv", "h", "u", "anonymous", "").await;
        assert_eq!(checked.level(), AccessLevel::ReadWrite);
    }

    #[tokio::test]
    async fn required_gate_with_acf_denies_unprivileged_peer() {
        let cfg = parse_acf(
            r#"
UAG(ops) { alice }
ASG(DEFAULT) {
    RULE(0, READ) { UAG(ops) }
}
"#,
        )
        .unwrap();
        let cell = Arc::new(tokio::sync::RwLock::new(Some(cfg)));
        let resolver: AsgAslResolver = Arc::new(|_pv| {
            Box::pin(async { ("DEFAULT".to_string(), 0u8) })
        });
        let gate = AccessGate::required(cell, resolver);

        let allowed = gate.check("x", "h", "alice", "anonymous", "").await;
        assert!(allowed.allows_read());
        assert!(!allowed.allows_write());

        let denied = gate.check("x", "h", "intruder", "anonymous", "").await;
        assert_eq!(denied.level(), AccessLevel::NoAccess);
        assert!(!denied.allows_read());
    }
}

/// A single access rule within an ASG.
#[derive(Debug, Clone, Default)]
pub struct AccessRule {
    pub level: u8,
    pub write: bool, // true = WRITE rule, false = READ rule
    pub uag: Vec<String>,
    pub hag: Vec<String>,
    /// Authentication method scope (epics-base PR #563). When set,
    /// the rule only applies when the requesting client authenticated
    /// via one of the listed methods. Common values: `"anonymous"`,
    /// `"ca"`, `"x509"`, `"cap-token"`. Empty vector means "any method".
    pub method: Vec<String>,
    /// Cert authority / issuer scope (epics-base PR #563 + #618).
    /// When set, the rule only applies when the client's authenticator
    /// was vouched by one of the listed authorities — e.g. an
    /// X.509 issuer DN, or the cap-token issuer ID. Empty means "any
    /// authority".
    pub authority: Vec<String>,
}

/// Access Security Group.
#[derive(Debug, Clone, Default)]
pub struct AccessSecurityGroup {
    pub rules: Vec<AccessRule>,
}

/// Access Security Configuration parsed from an ACF file.
#[derive(Debug, Clone)]
pub struct AccessSecurityConfig {
    pub uag: HashMap<String, Vec<String>>,
    pub hag: HashMap<String, Vec<String>>,
    pub asg: HashMap<String, AccessSecurityGroup>,
    pub unknown_access: AccessLevel,
}

impl AccessSecurityConfig {
    /// Check access for a given ASG, hostname, and username.
    ///
    /// Convenience that omits the ASL gate (treats every rule as
    /// applicable). Equivalent to `check_access_asl(..., 0)` with
    /// rules typically declared at level 0/1. New code should call
    /// [`Self::check_access_asl`] so a per-record ASL can correctly
    /// disable a rule whose level is below the record's ASL.
    pub fn check_access(&self, asg_name: &str, host: &str, user: &str) -> AccessLevel {
        self.check_access_asl(asg_name, host, user, 0)
    }

    /// Method/authority-aware access check. Mirrors epics-base PR
    /// #563 (METHOD/AUTHORITY) and PR #618 (cert-based ACF). When
    /// `method` and `authority` are provided, rules with non-empty
    /// `method`/`authority` lists are gated on a literal match.
    /// Rules with empty `method`/`authority` ignore those scopes
    /// (legacy behaviour preserved).
    pub fn check_access_method(
        &self,
        asg_name: &str,
        host: &str,
        user: &str,
        record_asl: u8,
        method: &str,
        authority: &str,
    ) -> AccessLevel {
        let asg = match self.asg.get(asg_name) {
            Some(a) => a,
            None => match self.asg.get("DEFAULT") {
                Some(a) => a,
                None => return AccessLevel::ReadWrite,
            },
        };
        // Empty rule set: ASG declared but no RULE — legacy semantics
        // grant ReadWrite (matching `check_access_asl`'s pre-PR #563
        // behaviour). The C-G6 fix (record-ASL gate) does not apply
        // when no rules exist to gate.
        if asg.rules.is_empty() {
            return AccessLevel::ReadWrite;
        }
        if user.is_empty() || host.is_empty() {
            return self.unknown_access;
        }
        let mut can_read = false;
        let mut can_write = false;
        for rule in &asg.rules {
            if record_asl > rule.level {
                continue;
            }
            let user_match = rule.uag.is_empty()
                || rule.uag.iter().any(|g| {
                    self.uag
                        .get(g)
                        .map(|members| members.iter().any(|m| m == user))
                        .unwrap_or(false)
                });
            let host_match = rule.hag.is_empty()
                || rule.hag.iter().any(|g| {
                    self.hag
                        .get(g)
                        .map(|members| members.iter().any(|m| m == host))
                        .unwrap_or(false)
                });
            let method_match = rule.method.is_empty()
                || rule.method.iter().any(|m| m.eq_ignore_ascii_case(method));
            let authority_match = rule.authority.is_empty()
                || rule
                    .authority
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(authority));
            if user_match && host_match && method_match && authority_match {
                if rule.write {
                    can_write = true;
                    can_read = true;
                } else {
                    can_read = true;
                }
            }
        }
        match (can_read, can_write) {
            (_, true) => AccessLevel::ReadWrite,
            (true, false) => AccessLevel::Read,
            _ => AccessLevel::NoAccess,
        }
    }

    /// Check access taking the per-record ASL into account.
    ///
    /// Per epics-base `asLibRoutines.c::asCompute`: a rule with
    /// `RULE(N, …)` only applies when the record's ASL ≤ N. The
    /// canonical example is `RULE(0, READ) RULE(1, WRITE)` — every
    /// record is readable, but only records with ASL ≥ 1 are
    /// writable. Without this gate, a low-ASL record's protection
    /// is silently equivalent to ASL 0 (closes C-G6).
    pub fn check_access_asl(
        &self,
        asg_name: &str,
        host: &str,
        user: &str,
        record_asl: u8,
    ) -> AccessLevel {
        // Forward to the method-aware path with default scopes
        // (any method, any authority). Mirrors epics-base PR #563:
        // legacy ACF rules without `METHOD`/`AUTHORITY` clauses match
        // every authentication method and authority. New code should
        // call `check_access_method` directly when method/authority
        // negotiation is observable.
        self.check_access_method(asg_name, host, user, record_asl, "", "")
    }
}

/// Parse an ACF (Access Control File).
pub fn parse_acf(content: &str) -> CaResult<AccessSecurityConfig> {
    let mut config = AccessSecurityConfig {
        uag: HashMap::new(),
        hag: HashMap::new(),
        asg: HashMap::new(),
        unknown_access: AccessLevel::Read,
    };

    let mut chars = content.chars().peekable();
    let mut buf = String::new();

    while chars.peek().is_some() {
        skip_ws_comments(&mut chars);
        buf.clear();
        read_word(&mut chars, &mut buf);

        match buf.as_str() {
            "UAG" => {
                let name = read_paren_name(&mut chars)?;
                let members = read_brace_list(&mut chars)?;
                config.uag.insert(name, members);
            }
            "HAG" => {
                let name = read_paren_name(&mut chars)?;
                let members = read_brace_list(&mut chars)?;
                config.hag.insert(name, members);
            }
            "ASG" => {
                let name = read_paren_name(&mut chars)?;
                let asg = parse_asg_body(&mut chars)?;
                config.asg.insert(name, asg);
            }
            "" => break,
            other => {
                return Err(CaError::Protocol(format!(
                    "ACF: unexpected keyword '{other}'"
                )));
            }
        }
    }

    Ok(config)
}

fn skip_ws_comments(chars: &mut std::iter::Peekable<std::str::Chars>) {
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '#' {
            // Skip line comment
            while let Some(&c) = chars.peek() {
                chars.next();
                if c == '\n' {
                    break;
                }
            }
        } else {
            break;
        }
    }
}

fn read_word(chars: &mut std::iter::Peekable<std::str::Chars>, buf: &mut String) {
    while let Some(&c) = chars.peek() {
        if c.is_alphanumeric() || c == '_' {
            buf.push(c);
            chars.next();
        } else {
            break;
        }
    }
}

fn read_paren_name(chars: &mut std::iter::Peekable<std::str::Chars>) -> CaResult<String> {
    skip_ws_comments(chars);
    if chars.next() != Some('(') {
        return Err(CaError::Protocol("ACF: expected '('".into()));
    }
    skip_ws_comments(chars);
    let mut name = String::new();
    while let Some(&c) = chars.peek() {
        if c == ')' {
            chars.next();
            break;
        }
        if !c.is_whitespace() {
            name.push(c);
        }
        chars.next();
    }
    Ok(name)
}

fn read_brace_list(chars: &mut std::iter::Peekable<std::str::Chars>) -> CaResult<Vec<String>> {
    skip_ws_comments(chars);
    if chars.next() != Some('{') {
        return Err(CaError::Protocol("ACF: expected '{'".into()));
    }
    let mut items = Vec::new();
    let mut current = String::new();

    loop {
        skip_ws_comments(chars);
        match chars.peek() {
            Some(&'}') => {
                chars.next();
                break;
            }
            Some(&',') => {
                chars.next();
                if !current.is_empty() {
                    items.push(current.clone());
                    current.clear();
                }
            }
            Some(&c) if c.is_alphanumeric() || c == '_' || c == '.' || c == '-' => {
                current.push(c);
                chars.next();
            }
            Some(_) => {
                chars.next();
            }
            None => return Err(CaError::Protocol("ACF: unterminated '{'".into())),
        }
    }
    if !current.is_empty() {
        items.push(current);
    }
    Ok(items)
}

fn parse_asg_body(
    chars: &mut std::iter::Peekable<std::str::Chars>,
) -> CaResult<AccessSecurityGroup> {
    skip_ws_comments(chars);
    if chars.next() != Some('{') {
        return Err(CaError::Protocol("ACF: expected '{' after ASG name".into()));
    }

    let mut asg = AccessSecurityGroup::default();

    loop {
        skip_ws_comments(chars);
        match chars.peek() {
            Some(&'}') => {
                chars.next();
                break;
            }
            Some(_) => {
                let mut kw = String::new();
                read_word(chars, &mut kw);
                if kw == "RULE" {
                    let rule = parse_rule(chars)?;
                    asg.rules.push(rule);
                } else if kw.is_empty() {
                    chars.next(); // skip unknown char
                }
            }
            None => return Err(CaError::Protocol("ACF: unterminated ASG".into())),
        }
    }

    Ok(asg)
}

fn parse_rule(chars: &mut std::iter::Peekable<std::str::Chars>) -> CaResult<AccessRule> {
    skip_ws_comments(chars);
    if chars.next() != Some('(') {
        return Err(CaError::Protocol("ACF: expected '(' after RULE".into()));
    }

    // Read level
    skip_ws_comments(chars);
    let mut level_str = String::new();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            level_str.push(c);
            chars.next();
        } else {
            break;
        }
    }
    let level: u8 = level_str.parse().unwrap_or(1);

    skip_ws_comments(chars);
    if chars.peek() == Some(&',') {
        chars.next();
    }

    // Read access type
    skip_ws_comments(chars);
    let mut access_str = String::new();
    read_word(chars, &mut access_str);
    let write = access_str.eq_ignore_ascii_case("WRITE");

    skip_ws_comments(chars);
    if chars.peek() == Some(&')') {
        chars.next();
    }

    // Optional body with UAG/HAG/METHOD/AUTHORITY.
    let mut uag = Vec::new();
    let mut hag = Vec::new();
    let mut method = Vec::new();
    let mut authority = Vec::new();

    skip_ws_comments(chars);
    if chars.peek() == Some(&'{') {
        chars.next();
        loop {
            skip_ws_comments(chars);
            match chars.peek() {
                Some(&'}') => {
                    chars.next();
                    break;
                }
                Some(_) => {
                    let mut kw = String::new();
                    read_word(chars, &mut kw);
                    if kw == "UAG" {
                        let name = read_paren_name(chars)?;
                        uag.push(name);
                    } else if kw == "HAG" {
                        let name = read_paren_name(chars)?;
                        hag.push(name);
                    } else if kw == "METHOD" {
                        // PR #563: METHOD("ca", "x509", ...)
                        method.extend(read_paren_string_list(chars)?);
                    } else if kw == "AUTHORITY" {
                        // PR #563/#618: AUTHORITY("CA Issuer", ...)
                        authority.extend(read_paren_string_list(chars)?);
                    } else if kw.is_empty() {
                        // Unknown punctuation — advance to avoid infinite loop.
                        chars.next();
                    }
                    // Unknown alphanumeric keywords are silently ignored
                    // (forward-compat with future RULE body extensions).
                }
                None => break,
            }
        }
    }

    Ok(AccessRule {
        level,
        write,
        uag,
        hag,
        method,
        authority,
    })
}

/// Parse `(item1, "item 2", ...)` — commas separate items, optional
/// quotes around each item are stripped. Used for METHOD/AUTHORITY
/// rule clauses (epics-base PR #563/#618). Whitespace inside an
/// unquoted item is preserved verbatim *between* word characters but
/// trimmed at the boundaries; the typical caller passes quoted strings.
fn read_paren_string_list(
    chars: &mut std::iter::Peekable<std::str::Chars>,
) -> CaResult<Vec<String>> {
    skip_ws_comments(chars);
    if chars.next() != Some('(') {
        return Err(CaError::Protocol(
            "ACF: expected '(' after METHOD/AUTHORITY".into(),
        ));
    }
    let mut items = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    loop {
        match chars.peek() {
            Some(&'"') => {
                chars.next();
                in_quotes = !in_quotes;
            }
            Some(&')') if !in_quotes => {
                chars.next();
                break;
            }
            Some(&',') if !in_quotes => {
                chars.next();
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    items.push(trimmed);
                }
                current.clear();
            }
            Some(&c) => {
                current.push(c);
                chars.next();
            }
            None => {
                return Err(CaError::Protocol(
                    "ACF: unterminated METHOD/AUTHORITY list".into(),
                ));
            }
        }
    }
    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() {
        items.push(trimmed);
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_acf_basic() {
        let acf = r#"
UAG(admins) { user1, user2 }
HAG(operators) { host1, host2 }
ASG(DEFAULT) {
    RULE(1, WRITE) { UAG(admins) HAG(operators) }
    RULE(1, READ)
}
"#;
        let config = parse_acf(acf).unwrap();
        assert_eq!(config.uag.get("admins").unwrap(), &["user1", "user2"]);
        assert_eq!(config.hag.get("operators").unwrap(), &["host1", "host2"]);
        assert!(config.asg.contains_key("DEFAULT"));
        assert_eq!(config.asg["DEFAULT"].rules.len(), 2);
    }

    #[test]
    fn test_parse_acf_hag_uag() {
        let acf = r#"
UAG(ops) { alice, bob }
HAG(lab) { lab-pc1 }
ASG(SECURE) {
    RULE(1, WRITE) { UAG(ops) HAG(lab) }
    RULE(1, READ)
}
"#;
        let config = parse_acf(acf).unwrap();
        assert_eq!(config.uag["ops"], vec!["alice", "bob"]);
        assert_eq!(config.hag["lab"], vec!["lab-pc1"]);
    }

    #[test]
    fn test_check_access_default_rw() {
        let acf = "ASG(DEFAULT) { RULE(1, WRITE) RULE(1, READ) }";
        let config = parse_acf(acf).unwrap();
        assert_eq!(
            config.check_access("DEFAULT", "host1", "user1"),
            AccessLevel::ReadWrite
        );
    }

    #[test]
    fn test_check_access_read_only() {
        let acf = r#"
UAG(admins) { admin1 }
ASG(READONLY) {
    RULE(1, READ)
    RULE(1, WRITE) { UAG(admins) }
}
"#;
        let config = parse_acf(acf).unwrap();
        // admin1 gets RW
        assert_eq!(
            config.check_access("READONLY", "host1", "admin1"),
            AccessLevel::ReadWrite
        );
        // Other users get read only
        assert_eq!(
            config.check_access("READONLY", "host1", "regular"),
            AccessLevel::Read
        );
    }

    #[test]
    fn test_check_access_hag_uag_match() {
        let acf = r#"
UAG(ops) { alice }
HAG(lab) { lab-pc1 }
ASG(CONTROLLED) {
    RULE(1, WRITE) { UAG(ops) HAG(lab) }
    RULE(1, READ)
}
"#;
        let config = parse_acf(acf).unwrap();
        // Alice on lab-pc1 gets RW
        assert_eq!(
            config.check_access("CONTROLLED", "lab-pc1", "alice"),
            AccessLevel::ReadWrite
        );
        // Alice on wrong host gets READ
        assert_eq!(
            config.check_access("CONTROLLED", "other-host", "alice"),
            AccessLevel::Read
        );
        // Wrong user on lab-pc1 gets READ
        assert_eq!(
            config.check_access("CONTROLLED", "lab-pc1", "bob"),
            AccessLevel::Read
        );
    }

    #[test]
    fn test_check_access_unknown_user() {
        let acf = r#"
ASG(DEFAULT) {
    RULE(1, WRITE)
    RULE(1, READ)
}
"#;
        let config = parse_acf(acf).unwrap();
        // Unknown user/host → conservative default
        assert_eq!(config.check_access("DEFAULT", "", ""), AccessLevel::Read);
    }

    // ----- epics-base PR #563/#618: METHOD / AUTHORITY -----

    #[test]
    fn parse_acf_captures_method_and_authority() {
        let acf = r#"
ASG(SECURE) {
    RULE(1, WRITE) {
        METHOD("ca", "x509")
        AUTHORITY("ANL CA")
    }
    RULE(1, READ)
}
"#;
        let config = parse_acf(acf).unwrap();
        let asg = &config.asg["SECURE"];
        assert_eq!(asg.rules.len(), 2);
        assert_eq!(asg.rules[0].method, vec!["ca", "x509"]);
        assert_eq!(asg.rules[0].authority, vec!["ANL CA"]);
        assert!(
            asg.rules[1].method.is_empty(),
            "READ rule must not inherit METHOD list",
        );
        assert!(asg.rules[1].authority.is_empty());
    }

    #[test]
    fn check_access_method_gates_on_method() {
        let acf = r#"
ASG(METHOD_GATED) {
    RULE(1, WRITE) {
        METHOD("x509")
    }
    RULE(1, READ)
}
"#;
        let config = parse_acf(acf).unwrap();
        // x509 method → WRITE matches.
        assert_eq!(
            config.check_access_method("METHOD_GATED", "h", "u", 0, "x509", ""),
            AccessLevel::ReadWrite
        );
        // ca method → only the unconstrained READ rule matches.
        assert_eq!(
            config.check_access_method("METHOD_GATED", "h", "u", 0, "ca", ""),
            AccessLevel::Read
        );
    }

    #[test]
    fn check_access_method_gates_on_authority() {
        let acf = r#"
ASG(AUTH_GATED) {
    RULE(1, WRITE) {
        AUTHORITY("Trusted Root")
    }
    RULE(1, READ)
}
"#;
        let config = parse_acf(acf).unwrap();
        assert_eq!(
            config.check_access_method("AUTH_GATED", "h", "u", 0, "x509", "Trusted Root"),
            AccessLevel::ReadWrite
        );
        assert_eq!(
            config.check_access_method("AUTH_GATED", "h", "u", 0, "x509", "Other CA"),
            AccessLevel::Read
        );
    }

    #[test]
    fn check_access_asl_legacy_path_matches_when_method_empty() {
        // Legacy ACF without METHOD/AUTHORITY clauses must continue
        // to match every method/authority — exactly what
        // `check_access_asl` forwards as ("", "").
        let acf = r#"
ASG(LEGACY) {
    RULE(1, WRITE)
    RULE(1, READ)
}
"#;
        let config = parse_acf(acf).unwrap();
        assert_eq!(
            config.check_access_asl("LEGACY", "h", "u", 0),
            AccessLevel::ReadWrite
        );
    }

    #[test]
    fn check_access_method_match_is_case_insensitive() {
        let acf = r#"
ASG(MIXED_CASE) {
    RULE(1, WRITE) {
        METHOD("X509")
    }
}
"#;
        let config = parse_acf(acf).unwrap();
        assert_eq!(
            config.check_access_method("MIXED_CASE", "h", "u", 0, "x509", ""),
            AccessLevel::ReadWrite
        );
    }
}
