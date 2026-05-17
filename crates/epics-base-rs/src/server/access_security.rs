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
#[derive(Clone)]
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

#[derive(Clone)]
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
    pub fn open_with_aggregator(f: std::sync::Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
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
        let resolver: AsgAslResolver =
            Arc::new(|_pv| Box::pin(async { ("DEFAULT".to_string(), 0u8) }));
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
        let resolver: AsgAslResolver =
            Arc::new(|_pv| Box::pin(async { ("DEFAULT".to_string(), 0u8) }));
        let gate = AccessGate::required(cell, resolver);

        let allowed = gate.check("x", "h", "alice", "anonymous", "").await;
        assert!(allowed.allows_read());
        assert!(!allowed.allows_write());

        let denied = gate.check("x", "h", "intruder", "anonymous", "").await;
        assert_eq!(denied.level(), AccessLevel::NoAccess);
        assert!(!denied.allows_read());
    }
}

/// Access granted by a matching `RULE`. Mirrors the C three-way
/// `asAccessRights` enum (`asNOACCESS` / `asREAD` / `asWRITE`) used by
/// `rule_head_mandatory` in `asLib.y:253-269`. The Rust port previously
/// collapsed this to a `write: bool`, which turned `RULE(0, NONE)` —
/// and any misspelled keyword — into a READ-granting rule (M-3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RuleAccess {
    /// `RULE(N, NONE)` — grants `asNOACCESS`.
    #[default]
    None,
    /// `RULE(N, READ)` — grants `asREAD`.
    Read,
    /// `RULE(N, WRITE)` — grants `asWRITE`.
    Write,
}

/// A single access rule within an ASG.
#[derive(Debug, Clone, Default)]
pub struct AccessRule {
    pub level: u8,
    /// Three-way access this rule grants when it matches. C
    /// `asLib.y:259-267` distinguishes `NONE`/`READ`/`WRITE`.
    pub access: RuleAccess,
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
    /// Write-trap mask (epics-base `asLib.y:272-283` `rule_log_option`,
    /// `AS_TRAP_WRITE`). `true` when the RULE header carried the
    /// `TRAPWRITE` option, `false` for `NOTRAPWRITE` or no option.
    /// The grammar is honoured here; the `asTrapWrite` put-logging
    /// listener that consumes this mask is a separate subsystem not
    /// present in this crate (see the H-2 UNFIXED note).
    pub trap: bool,
    /// CALC condition expression (epics-base `asLib.y:294-299`,
    /// `RULE(...) { CALC("A=1") }`). `None` means an unconditional
    /// rule. When `Some`, the rule only grants access while the
    /// expression evaluates to 1 against the ASG's `INP*` link values.
    pub calc: Option<String>,
    /// True when the rule must be treated as inert by `asComputePvt`.
    /// C `asAsgRuleDisable` (`asLib.y:300-306`) sets `pasgrule->ignore`
    /// for a RULE that contains an unsupported keyword. This port also
    /// sets it for a `CALC` clause that cannot be evaluated here (no
    /// `INP*` link resolution), so an un-evaluable conditional rule
    /// fails CLOSED instead of becoming unconditional (H-3).
    pub ignore: bool,
}

/// The access a matching rule grants, with the `ignore` flag folded in
/// — an ignored rule is inert (`None`). Helper for `asComputePvt`.
fn rule_access(rule: &AccessRule) -> AccessLevel {
    if rule.ignore {
        return AccessLevel::NoAccess;
    }
    match rule.access {
        RuleAccess::None => AccessLevel::NoAccess,
        RuleAccess::Read => AccessLevel::Read,
        RuleAccess::Write => AccessLevel::ReadWrite,
    }
}

/// Monotonic ordering of access levels used by `asComputePvt`'s
/// `access >= pasgrule->access` short-circuit.
fn rule_rank(level: AccessLevel) -> u8 {
    match level {
        AccessLevel::NoAccess => 0,
        AccessLevel::Read => 1,
        AccessLevel::ReadWrite => 2,
    }
}

/// Access Security Group.
#[derive(Debug, Clone, Default)]
pub struct AccessSecurityGroup {
    pub rules: Vec<AccessRule>,
    /// `INP(A..U)` database link declarations (epics-base
    /// `asLib.y:234-243`). Index 0 = `INPA`, .. 20 = `INPU`. Each
    /// entry is the link string. Stored for `asdbdump` / `ascar`
    /// inspection and to feed `CALC` rule evaluation; the link
    /// values are not resolved by this crate (see `AccessRule::calc`).
    pub inp: Vec<AsgInp>,
}

/// A single `INP(A..U)` link declaration within an ASG.
#[derive(Debug, Clone)]
pub struct AsgInp {
    /// Letter index: 0 = `A`, .. 20 = `U`.
    pub index: u8,
    /// The link string (typically a record.field PV name).
    pub link: String,
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
        // C `asAddMemberPvt` (asLibRoutines.c:893-928): a member whose
        // ASG name is not present in the parsed config is silently
        // reassigned to `DEFAULT`. `asInitialize` (asLibRoutines.c:107)
        // *always* synthesises a `DEFAULT` ASG before parsing, so this
        // lookup never legitimately misses — `parse_acf` reproduces
        // that by always inserting an (empty) `DEFAULT`. A missing
        // `DEFAULT` here would mean the config was built by hand
        // bypassing `parse_acf`; fail CLOSED rather than open.
        let asg = match self.asg.get(asg_name) {
            Some(a) => a,
            None => match self.asg.get("DEFAULT") {
                Some(a) => a,
                // C-2 fix: never grant ReadWrite on an ASG-lookup
                // miss. C resolves every miss to the always-present
                // empty `DEFAULT` ⇒ `asNOACCESS`.
                None => return AccessLevel::NoAccess,
            },
        };
        // C `asComputePvt` (asLibRoutines.c:983) initialises
        // `access = asNOACCESS` and only ever *raises* it on a matching
        // RULE. An ASG with no RULE statements (`ASG(LOCKED) { }`)
        // therefore denies every client. C-1 fix: never short-circuit
        // an empty rule list to ReadWrite.
        //
        // An empty/unknown user or host cannot match a UAG/HAG-scoped
        // rule, but a rule with empty `uag`/`hag` lists still applies
        // (C `asComputePvt` only checks the UAG list when
        // `ellCount(&pasgrule->uagList) > 0`). So the loop below is run
        // unconditionally — it naturally denies a `("", "")` peer for
        // any UAG/HAG-scoped rule while still honouring an
        // unconditional `RULE(0, READ)`.
        let mut access = AccessLevel::NoAccess;
        for rule in &asg.rules {
            // C `asComputePvt`: a rule disabled by `asAsgRuleDisable`
            // (unsupported keyword / un-evaluable CALC) is skipped.
            if rule.ignore {
                continue;
            }
            // Monotonic raise: once WRITE is reached nothing can lower
            // it, and a rule whose access is not stronger than the
            // current level cannot change the outcome.
            if access == AccessLevel::ReadWrite {
                break;
            }
            if rule_rank(rule_access(rule)) <= rule_rank(access) {
                continue;
            }
            if record_asl > rule.level {
                continue;
            }
            // UAG: only consulted when the rule scopes one. An empty
            // UAG list means "any user" — including an empty username.
            let user_match = rule.uag.is_empty()
                || rule.uag.iter().any(|g| {
                    self.uag
                        .get(g)
                        .map(|members| members.iter().any(|m| m == user))
                        .unwrap_or(false)
                });
            if !user_match {
                continue;
            }
            // HAG: host comparison is case-insensitive (H-1). C stores
            // every HAG host lowercased (`asHagAddHost`) and lowercases
            // the connecting client's host before `asComputePvt`.
            let host_lc = host.to_ascii_lowercase();
            let host_match = rule.hag.is_empty()
                || rule.hag.iter().any(|g| {
                    self.hag
                        .get(g)
                        .map(|members| {
                            members
                                .iter()
                                .any(|m| m.eq_ignore_ascii_case(&host_lc))
                        })
                        .unwrap_or(false)
                });
            if !host_match {
                continue;
            }
            let method_match = rule.method.is_empty()
                || rule.method.iter().any(|m| m.eq_ignore_ascii_case(method));
            if !method_match {
                continue;
            }
            let authority_match = rule.authority.is_empty()
                || rule
                    .authority
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(authority));
            if !authority_match {
                continue;
            }
            access = rule_access(rule);
        }
        access
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

    // C `asInitialize` (asLibRoutines.c:107) calls `asAsgAdd(DEFAULT)`
    // *before* parsing the file, so a `DEFAULT` ASG always exists.
    // Synthesise it here unconditionally (C-2/C-3): any record whose
    // `ASG` field names an unknown group resolves to this empty
    // `DEFAULT`, which has no RULEs ⇒ `asNOACCESS` ⇒ access denied.
    // A `DEFAULT` block declared in the file simply overwrites this
    // placeholder below.
    config
        .asg
        .insert("DEFAULT".to_string(), AccessSecurityGroup::default());

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
                // H-1: store every HAG host lowercased — C
                // `asHagAddHost` (asLibRoutines.c:1218-1256)
                // `tolower()`s each host char so host matching is
                // case-insensitive.
                let lowered: Vec<String> =
                    members.iter().map(|m| m.to_ascii_lowercase()).collect();
                let expanded = expand_hag_members(&lowered);
                config.hag.insert(name, expanded);
            }
            "ASG" => {
                let name = read_paren_name(&mut chars)?;
                let asg = parse_asg_body(&mut chars)?;
                config.asg.insert(name, asg);
            }
            "" => break,
            other => {
                // M-4: C `asLib.y:88-103` treats an unrecognised
                // top-level block as a *warning*
                // (`yywarn "Ignoring unsupported TOP LEVEL block"`)
                // and parsing continues — forward-compat with
                // future/vendor ACF extensions. Skip the unknown
                // keyword's `(...)` head and `{...}` body if present
                // so the rest of the file still parses, instead of
                // aborting the whole load (which, per C-3, would
                // otherwise leave the IOC with no security layer).
                tracing::warn!(
                    target: "epics_base_rs::access_security",
                    keyword = %other,
                    "ACF: ignoring unsupported top-level block"
                );
                skip_unknown_top_level_block(&mut chars);
            }
        }
    }

    Ok(config)
}

/// Skip an unrecognised top-level block: an optional `(...)` head and
/// an optional `{...}` body. Mirrors C's recover-and-continue posture
/// for `yywarn "Ignoring unsupported TOP LEVEL block"` (M-4).
fn skip_unknown_top_level_block(chars: &mut std::iter::Peekable<std::str::Chars>) {
    skip_ws_comments(chars);
    if chars.peek() == Some(&'(') {
        // Consume balanced parens.
        let mut depth = 0;
        while let Some(&c) = chars.peek() {
            chars.next();
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    skip_ws_comments(chars);
    if chars.peek() == Some(&'{') {
        let mut depth = 0;
        while let Some(&c) = chars.peek() {
            chars.next();
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
    }
}

/// Expand HAG members with their resolved IP addresses for soft
/// hostname matching.
///
/// Mirrors epics-base libcom commit 932e9f3 ("asLib: soft fallback on
/// DNS lookup failure"). Upstream resolves each hostname to its IP at
/// parse time and matches the connecting client's IP against the
/// resolved set; the fix made DNS failure a non-fatal warning instead
/// of an `abort()`.
///
/// We keep the original behaviour of matching the literal HOST_NAME
/// the client sent over CA (which is what the CA protocol gives us —
/// `HOST_NAME` is the peer's self-reported hostname, not its IP) and
/// additionally append every resolved IPv4/IPv6 literal so an
/// IP-presenting client (gateway, NATed peer with no reverse DNS) still
/// matches. DNS failures are dropped silently — the parser never
/// aborts, so a single bad entry doesn't deny the entire IOC.
fn expand_hag_members(members: &[String]) -> Vec<String> {
    use std::net::ToSocketAddrs;
    let mut out: Vec<String> = Vec::with_capacity(members.len());
    for m in members {
        out.push(m.clone());
        // Bare IP literal: nothing more to add — `m` already covers
        // the IP-match path. The to_socket_addrs() trick below would
        // round-trip an IP to itself, but skipping the syscall keeps
        // ACF reload latency proportional to actual hostnames.
        if m.parse::<std::net::IpAddr>().is_ok() {
            continue;
        }
        match format!("{m}:0").to_socket_addrs() {
            Ok(iter) => {
                for sa in iter {
                    let ip = sa.ip().to_string();
                    if !out.iter().any(|s| s == &ip) {
                        out.push(ip);
                    }
                }
            }
            Err(e) => {
                tracing::debug!(
                    target: "epics_base_rs::access_security",
                    host = %m,
                    error = %e,
                    "ACF HAG: DNS lookup failed; keeping literal entry (libcom 932e9f3 soft fallback)"
                );
            }
        }
    }
    out
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
    // L-4: C's lexer requires a single `tokenSTRING` then `')'`.
    // Accept an optional double-quoted form; in the unquoted form
    // interior whitespace ends the name — a second non-space run
    // before `)` is a parse error rather than being silently merged
    // (`UAG(my group)` must NOT become `mygroup`). EOF before `)` is
    // also an error.
    let mut name = String::new();
    if chars.peek() == Some(&'"') {
        chars.next();
        let mut closed = false;
        while let Some(&c) = chars.peek() {
            chars.next();
            if c == '"' {
                closed = true;
                break;
            }
            name.push(c);
        }
        if !closed {
            return Err(CaError::Protocol(
                "ACF: unterminated quoted name".into(),
            ));
        }
        skip_ws_comments(chars);
        if chars.next() != Some(')') {
            return Err(CaError::Protocol(
                "ACF: expected ')' after quoted name".into(),
            ));
        }
        return Ok(name);
    }
    loop {
        match chars.peek() {
            Some(&')') => {
                chars.next();
                break;
            }
            Some(&c) if c.is_whitespace() => {
                // Whitespace ends the name. Allow only trailing
                // whitespace before `)`; reject embedded whitespace.
                skip_ws_comments(chars);
                match chars.peek() {
                    Some(&')') => {
                        chars.next();
                        break;
                    }
                    Some(_) => {
                        return Err(CaError::Protocol(
                            "ACF: whitespace inside parenthesised name".into(),
                        ));
                    }
                    None => {
                        return Err(CaError::Protocol(
                            "ACF: unterminated '(' — missing ')'".into(),
                        ));
                    }
                }
            }
            Some(&c) => {
                name.push(c);
                chars.next();
            }
            None => {
                return Err(CaError::Protocol(
                    "ACF: unterminated '(' — missing ')'".into(),
                ));
            }
        }
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
                } else if let Some(stripped) = kw.strip_prefix("INP") {
                    // H-4: `INP(A..U)("link")` — C `asLib_lex.l:48-52`
                    // lexes `INP[A-U]` as one token whose `Int64`
                    // payload is the letter index (`yytext[3] - 'A'`).
                    // `asLib.y:234-243` then reads the parenthesised
                    // link string.
                    let index = match parse_inp_index(stripped) {
                        Some(i) => i,
                        None => {
                            return Err(CaError::Protocol(format!(
                                "ACF: invalid INP link selector 'INP{stripped}' \
                                 (expected INPA..INPU)"
                            )));
                        }
                    };
                    let link = read_paren_name(chars)?;
                    asg.inp.push(AsgInp { index, link });
                } else if kw.is_empty() {
                    chars.next(); // skip unknown char
                }
                // Unknown alphanumeric keywords inside an ASG body are
                // skipped (forward-compat); the next loop iteration
                // resumes from the following token.
            }
            None => return Err(CaError::Protocol("ACF: unterminated ASG".into())),
        }
    }

    Ok(asg)
}

/// Parse the `A..U` selector suffix of an `INP` token into a 0-based
/// letter index. `"A"` → 0, .. `"U"` → 20. Anything else → `None`.
fn parse_inp_index(suffix: &str) -> Option<u8> {
    let mut it = suffix.chars();
    let c = it.next()?;
    if it.next().is_some() {
        return None; // INP selector is exactly one letter
    }
    let c = c.to_ascii_uppercase();
    if ('A'..='U').contains(&c) {
        Some((c as u8) - b'A')
    } else {
        None
    }
}

fn parse_rule(chars: &mut std::iter::Peekable<std::str::Chars>) -> CaResult<AccessRule> {
    skip_ws_comments(chars);
    if chars.next() != Some('(') {
        return Err(CaError::Protocol("ACF: expected '(' after RULE".into()));
    }

    // Read level. M-2: C `asLib.y:253-258` requires `tokenINT64` and
    // rejects a negative or non-numeric level with `yyerror`, which
    // fails the whole ACF load (a fail-safe abort). Accept an optional
    // leading sign so a `RULE(-1, ...)` is detected and rejected
    // rather than silently re-read as level 1.
    skip_ws_comments(chars);
    let mut level_str = String::new();
    if matches!(chars.peek(), Some('+') | Some('-')) {
        level_str.push(chars.next().unwrap());
    }
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            level_str.push(c);
            chars.next();
        } else {
            break;
        }
    }
    let level_num: i64 = level_str.parse().map_err(|_| {
        CaError::Protocol(format!(
            "ACF: RULE level must be an integer, got '{level_str}'"
        ))
    })?;
    if level_num < 0 {
        return Err(CaError::Protocol(format!(
            "ACF: RULE LEVEL must be positive: {level_num}"
        )));
    }
    let level: u8 = u8::try_from(level_num).map_err(|_| {
        CaError::Protocol(format!("ACF: RULE level out of range: {level_num}"))
    })?;

    skip_ws_comments(chars);
    if chars.peek() == Some(&',') {
        chars.next();
    }

    // Read access keyword. M-3: C `asLib.y:259-267` distinguishes
    // `NONE`/`READ`/`WRITE`; any other keyword triggers
    // `yywarn "Ignoring RULE that contains an unsupported keyword"`
    // and the rule is dropped. We keep the rule but mark it `ignore`
    // (inert) so a misspelled keyword fails CLOSED.
    skip_ws_comments(chars);
    let mut access_str = String::new();
    read_word(chars, &mut access_str);
    let (access, mut ignore) = if access_str.eq_ignore_ascii_case("WRITE") {
        (RuleAccess::Write, false)
    } else if access_str.eq_ignore_ascii_case("READ") {
        (RuleAccess::Read, false)
    } else if access_str.eq_ignore_ascii_case("NONE") {
        (RuleAccess::None, false)
    } else {
        tracing::warn!(
            target: "epics_base_rs::access_security",
            keyword = %access_str,
            "ACF: ignoring RULE with unsupported access keyword"
        );
        (RuleAccess::None, true)
    };

    // Optional log option: `RULE(level, access, TRAPWRITE)` /
    // `RULE(level, access, NOTRAPWRITE)`. H-2: C `asLib.y:272-283`
    // (`rule_log_option`). The grammar is honoured and the trap mask
    // captured in `AccessRule::trap`; the `asTrapWrite` put-logging
    // listener that would consume the mask is a separate subsystem
    // not present in this crate (see the H-2 UNFIXED note).
    let mut trap = false;
    skip_ws_comments(chars);
    if chars.peek() == Some(&',') {
        chars.next();
        skip_ws_comments(chars);
        let mut log_opt = String::new();
        read_word(chars, &mut log_opt);
        if log_opt.eq_ignore_ascii_case("TRAPWRITE") {
            trap = true;
        } else if !log_opt.eq_ignore_ascii_case("NOTRAPWRITE") {
            return Err(CaError::Protocol(format!(
                "ACF: RULE log option must be TRAPWRITE or NOTRAPWRITE, got '{log_opt}'"
            )));
        }
    }

    skip_ws_comments(chars);
    if chars.peek() == Some(&')') {
        chars.next();
    }

    // Optional body with UAG/HAG/METHOD/AUTHORITY/CALC.
    let mut uag = Vec::new();
    let mut hag = Vec::new();
    let mut method = Vec::new();
    let mut authority = Vec::new();
    let mut calc: Option<String> = None;

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
                    } else if kw == "CALC" {
                        // H-3: `CALC("<expr>")` — C `asLib.y:294-299`.
                        // The expression gates the rule against the
                        // ASG's INP* link values. Take the *last*
                        // CALC clause if several are given (matches
                        // C `asAsgRuleCalc` last-wins overwrite).
                        let expr = read_paren_name_raw(chars)?;
                        calc = Some(expr);
                    } else if kw.is_empty() {
                        // Unknown punctuation — advance to avoid infinite loop.
                        chars.next();
                    } else {
                        // M-3 / C `asLib.y:300-306`: a RULE body with
                        // an unsupported keyword is *disabled* by
                        // `asAsgRuleDisable`. Mark the rule inert and
                        // consume the keyword's `(...)` argument if
                        // present so parsing recovers.
                        tracing::warn!(
                            target: "epics_base_rs::access_security",
                            keyword = %kw,
                            "ACF: ignoring RULE with unsupported keyword — rule disabled"
                        );
                        ignore = true;
                        skip_ws_comments(chars);
                        if chars.peek() == Some(&'(') {
                            let _ = read_paren_name(chars)?;
                        }
                    }
                }
                None => break,
            }
        }
    }

    // H-3: a CALC clause must actually gate the rule. This crate's
    // access-security layer has no `INP*` database-link resolution
    // (the `AsgInp` links are stored but never read), so the calc
    // expression cannot be evaluated at access-check time. C disables
    // any rule it cannot fully honour (`asAsgRuleDisable`); to fail
    // CLOSED we do the same — a present-but-unevaluable CALC condition
    // marks the rule inert rather than letting it become an
    // unconditional grant. The expression is still validated (compiled)
    // here so a syntactically broken CALC is rejected exactly as C's
    // `postfix()` rejects it in `asAsgRuleCalc`.
    if let Some(ref expr) = calc {
        match crate::calc::compile(expr) {
            Ok(_) => {
                ignore = true;
            }
            Err(e) => {
                return Err(CaError::Protocol(format!(
                    "ACF: bad CALC expression '{expr}': {e}"
                )));
            }
        }
    }

    Ok(AccessRule {
        level,
        access,
        uag,
        hag,
        method,
        authority,
        trap,
        calc,
        ignore,
    })
}

/// Read a parenthesised, double-quoted string verbatim — used for the
/// `CALC("<expr>")` clause where the expression contains operators and
/// spaces that `read_paren_name` would mangle (it strips whitespace).
/// Accepts `( "expr" )` or `( expr )`; whitespace around the parens is
/// skipped, whitespace *inside* a quoted body is preserved.
fn read_paren_name_raw(chars: &mut std::iter::Peekable<std::str::Chars>) -> CaResult<String> {
    skip_ws_comments(chars);
    if chars.next() != Some('(') {
        return Err(CaError::Protocol("ACF: expected '(' after CALC".into()));
    }
    skip_ws_comments(chars);
    let mut body = String::new();
    if chars.peek() == Some(&'"') {
        chars.next();
        while let Some(&c) = chars.peek() {
            chars.next();
            if c == '"' {
                break;
            }
            body.push(c);
        }
        skip_ws_comments(chars);
        if chars.next() != Some(')') {
            return Err(CaError::Protocol(
                "ACF: expected ')' after CALC expression".into(),
            ));
        }
    } else {
        // Unquoted form — read until the closing paren.
        while let Some(&c) = chars.peek() {
            if c == ')' {
                chars.next();
                break;
            }
            body.push(c);
            chars.next();
        }
    }
    Ok(body.trim().to_string())
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
        // Use `.invalid` so DNS resolution is guaranteed to fail
        // (RFC 6761 — every resolver returns NXDOMAIN). This isolates
        // the test from `expand_hag_members`' soft-DNS path: the
        // literal entry is preserved, and no resolved IPs are
        // appended.
        let acf = r#"
UAG(ops) { alice, bob }
HAG(lab) { lab-pc1.invalid }
ASG(SECURE) {
    RULE(1, WRITE) { UAG(ops) HAG(lab) }
    RULE(1, READ)
}
"#;
        let config = parse_acf(acf).unwrap();
        assert_eq!(config.uag["ops"], vec!["alice", "bob"]);
        assert_eq!(config.hag["lab"], vec!["lab-pc1.invalid"]);
    }

    #[test]
    fn hag_dns_resolution_appends_ip_for_match() {
        // Loopback name `localhost` is universally resolvable on
        // Linux/macOS/Windows. The expansion must add 127.0.0.1 (and
        // optionally ::1) alongside the literal entry so a peer
        // presenting `127.0.0.1` as its HOST_NAME still matches the
        // HAG, mirroring upstream's IP-based comparison.
        let acf = r#"
HAG(local) { localhost }
ASG(DEFAULT) {
    RULE(1, WRITE) { HAG(local) }
}
"#;
        let config = parse_acf(acf).unwrap();
        let entries = &config.hag["local"];
        assert!(
            entries.contains(&"localhost".to_string()),
            "literal hostname always preserved"
        );
        assert!(
            entries.iter().any(|s| s == "127.0.0.1"),
            "resolved IPv4 appended for IP-presenting peers"
        );
    }

    #[test]
    fn hag_unresolvable_name_does_not_abort_parser() {
        // `.invalid` TLD guarantees NXDOMAIN (RFC 6761). Pre-fix
        // upstream would `abort()` here; we keep the literal entries
        // verbatim — no resolved IPs are appended and the parser
        // returns Ok. Comma separator matches the brace-list parser's
        // tokenization (whitespace alone is consumed silently).
        let acf = r#"
HAG(quarantine) { gone.invalid, alive.invalid }
ASG(DEFAULT) {
    RULE(1, WRITE) { HAG(quarantine) }
}
"#;
        let config = parse_acf(acf).expect("parser must not abort on bad DNS");
        let entries = &config.hag["quarantine"];
        assert_eq!(
            entries.len(),
            2,
            "literal entries preserved verbatim; no resolved IPs appended"
        );
        assert_eq!(entries[0], "gone.invalid");
        assert_eq!(entries[1], "alive.invalid");
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
        // C `asComputePvt` parity: a RULE with an empty UAG list
        // applies to *every* client regardless of user/host — the
        // UAG check is skipped when `ellCount(&pasgrule->uagList)==0`.
        // So an unconditional `RULE(1, WRITE)` grants WRITE even to a
        // client with an empty/unknown user. (The old port returned
        // `Read` here via a `unknown_access` special-case that C does
        // not have.)
        assert_eq!(
            config.check_access("DEFAULT", "", ""),
            AccessLevel::ReadWrite
        );
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
    fn tls_x509_acf_rule_grants_write_on_issuer_match() {
        // PR #641 end-to-end: an ACF rule that requires both
        // METHOD("x509") and AUTHORITY(<issuer>) must succeed only
        // when an mTLS peer presents a cert signed by that issuer.
        let cfg = parse_acf(
            r#"
ASG(TLS_ONLY) {
    RULE(1, WRITE) { METHOD("x509") AUTHORITY("CN=ops-ca, O=Lab") }
    RULE(1, READ)
}
"#,
        )
        .unwrap();
        // Plaintext (no method) → READ only.
        assert_eq!(
            cfg.check_access_method("TLS_ONLY", "h", "u", 0, "", ""),
            AccessLevel::Read
        );
        // mTLS, wrong issuer → READ only.
        assert_eq!(
            cfg.check_access_method("TLS_ONLY", "h", "u", 0, "x509", "CN=other-ca"),
            AccessLevel::Read
        );
        // mTLS, matching issuer → WRITE granted.
        assert_eq!(
            cfg.check_access_method("TLS_ONLY", "h", "u", 0, "x509", "CN=ops-ca, O=Lab"),
            AccessLevel::ReadWrite
        );
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

    // ----- C-1 / C-2 / C-3: access security must fail CLOSED -----

    /// C-1: an ASG declared with no RULE statements denies every
    /// client. C `asComputePvt` starts `access = asNOACCESS` and only
    /// raises it on a matching RULE — an empty rule list never raises.
    #[test]
    fn empty_rule_asg_denies_access() {
        let config = parse_acf("ASG(LOCKED) { }").unwrap();
        assert_eq!(
            config.check_access("LOCKED", "host", "user"),
            AccessLevel::NoAccess,
            "ASG with no RULE must deny — C asComputePvt fails closed"
        );
    }

    /// C-2: a record whose ASG names a group not in the file resolves
    /// to the always-present (empty) `DEFAULT`, which denies access.
    #[test]
    fn unknown_asg_falls_back_to_empty_default_and_denies() {
        let config = parse_acf("UAG(ops) { alice }").unwrap();
        // `DEFAULT` is auto-synthesised by parse_acf (C asInitialize
        // always calls asAsgAdd("DEFAULT")) and has no rules.
        assert!(config.asg.contains_key("DEFAULT"));
        assert_eq!(
            config.check_access("TYPO", "host", "alice"),
            AccessLevel::NoAccess,
            "unknown ASG must resolve to empty DEFAULT ⇒ NoAccess"
        );
    }

    /// C-2 corner: even `DEFAULT` itself, when never declared with
    /// rules, denies — the auto-synthesised placeholder is empty.
    #[test]
    fn default_asg_without_rules_denies() {
        let config = parse_acf("UAG(ops) { alice }").unwrap();
        assert_eq!(
            config.check_access("DEFAULT", "host", "alice"),
            AccessLevel::NoAccess
        );
    }

    /// C-3: an empty ACF file, or one with only comments / only
    /// UAG/HAG blocks, yields a fail-closed config — every check
    /// denies, matching a C IOC whose only ASG is the empty DEFAULT.
    #[test]
    fn empty_acf_denies_all_access() {
        for acf in ["", "# just a comment\n", "UAG(ops){alice}\nHAG(h){pc1}\n"] {
            let config = parse_acf(acf).unwrap();
            assert_eq!(
                config.check_access("DEFAULT", "host", "alice"),
                AccessLevel::NoAccess,
                "empty/rule-less ACF must deny (input was {acf:?})"
            );
            assert_eq!(
                config.check_access("ANY_GROUP", "host", "alice"),
                AccessLevel::NoAccess,
                "unknown ASG against empty ACF must deny (input was {acf:?})"
            );
        }
    }

    /// A config built by hand (bypassing `parse_acf`) with no
    /// `DEFAULT` and an unknown ASG must still fail closed.
    #[test]
    fn handbuilt_config_missing_default_denies() {
        let config = AccessSecurityConfig {
            uag: HashMap::new(),
            hag: HashMap::new(),
            asg: HashMap::new(),
            unknown_access: AccessLevel::Read,
        };
        assert_eq!(
            config.check_access("WHATEVER", "host", "user"),
            AccessLevel::NoAccess
        );
    }

    // ----- M-3: NONE keyword and unsupported keywords -----

    /// `RULE(0, NONE)` grants asNOACCESS — it must not be treated as a
    /// READ-granting rule. With only a NONE rule, access stays denied.
    #[test]
    fn rule_none_grants_no_access() {
        let config = parse_acf("ASG(N) { RULE(0, NONE) }").unwrap();
        assert_eq!(
            config.check_access("N", "host", "user"),
            AccessLevel::NoAccess
        );
    }

    /// A misspelled access keyword disables the rule (C warns and
    /// drops it) — it must not silently become a READ rule.
    #[test]
    fn rule_unsupported_access_keyword_is_inert() {
        let config = parse_acf("ASG(B) { RULE(0, WRIET) }").unwrap();
        assert_eq!(config.asg["B"].rules.len(), 1);
        assert!(config.asg["B"].rules[0].ignore, "bad keyword ⇒ inert rule");
        assert_eq!(
            config.check_access("B", "host", "user"),
            AccessLevel::NoAccess
        );
    }

    // ----- M-2: RULE level validation -----

    #[test]
    fn rule_negative_level_is_rejected() {
        let err = parse_acf("ASG(X) { RULE(-1, READ) }");
        assert!(err.is_err(), "negative RULE level must fail the parse");
    }

    #[test]
    fn rule_non_numeric_level_is_rejected() {
        let err = parse_acf("ASG(X) { RULE(abc, READ) }");
        assert!(err.is_err(), "non-numeric RULE level must fail the parse");
    }

    // ----- M-4: unknown top-level block tolerated -----

    #[test]
    fn unknown_top_level_block_is_skipped_not_fatal() {
        let acf = r#"
VENDOR(extension) { whatever }
ASG(DEFAULT) { RULE(1, READ) }
"#;
        let config =
            parse_acf(acf).expect("unknown top-level block must not abort the parse");
        assert_eq!(
            config.check_access("DEFAULT", "host", "user"),
            AccessLevel::Read,
            "the ASG after the unknown block must still parse"
        );
    }

    // ----- H-1: HAG host matching is case-insensitive -----

    #[test]
    fn hag_host_match_is_case_insensitive() {
        let acf = r#"
HAG(lab) { LabPC1.invalid }
ASG(C) {
    RULE(1, WRITE) { HAG(lab) }
    RULE(1, READ)
}
"#;
        let config = parse_acf(acf).unwrap();
        // Client reports a differently-cased hostname.
        assert_eq!(
            config.check_access("C", "labpc1.invalid", "user"),
            AccessLevel::ReadWrite,
            "lowercased HAG entry must match a mixed-case client host"
        );
        assert_eq!(
            config.check_access("C", "LABPC1.INVALID", "user"),
            AccessLevel::ReadWrite
        );
        // A genuinely different host still only gets READ.
        assert_eq!(
            config.check_access("C", "other.invalid", "user"),
            AccessLevel::Read
        );
    }

    // ----- H-2: TRAPWRITE / NOTRAPWRITE log option parses -----

    #[test]
    fn rule_trapwrite_log_option_parses() {
        let config =
            parse_acf("ASG(T) { RULE(1, WRITE, TRAPWRITE) RULE(1, READ, NOTRAPWRITE) }")
                .unwrap();
        assert_eq!(config.asg["T"].rules.len(), 2);
        assert_eq!(config.asg["T"].rules[0].access, RuleAccess::Write);
        assert!(
            config.asg["T"].rules[0].trap,
            "TRAPWRITE must set the trap mask"
        );
        assert_eq!(config.asg["T"].rules[1].access, RuleAccess::Read);
        assert!(
            !config.asg["T"].rules[1].trap,
            "NOTRAPWRITE must clear the trap mask"
        );
    }

    #[test]
    fn rule_bad_log_option_is_rejected() {
        assert!(parse_acf("ASG(T) { RULE(1, WRITE, BOGUS) }").is_err());
    }

    // ----- H-3: CALC clause gates (or disables) the rule -----

    /// A CALC condition must never let a rule become unconditional.
    /// This crate cannot resolve INP* link values, so a CALC rule is
    /// disabled (fail closed) — it grants nothing.
    #[test]
    fn calc_rule_is_disabled_when_unevaluable() {
        let config =
            parse_acf(r#"ASG(G) { INPA("ref") RULE(1, WRITE) { CALC("A=1") } }"#).unwrap();
        let rule = &config.asg["G"].rules[0];
        assert!(rule.calc.is_some(), "CALC clause must be parsed and stored");
        assert!(
            rule.ignore,
            "an unevaluable CALC rule must be disabled, not unconditional"
        );
        assert_eq!(
            config.check_access("G", "host", "user"),
            AccessLevel::NoAccess,
            "CALC rule must not silently grant WRITE"
        );
    }

    #[test]
    fn calc_rule_with_bad_expression_is_rejected() {
        assert!(
            parse_acf(r#"ASG(G) { RULE(1, WRITE) { CALC("A=") } }"#).is_err(),
            "syntactically broken CALC must fail the parse"
        );
    }

    // ----- H-4: INP(A..U) link declarations -----

    #[test]
    fn asg_inp_links_are_parsed() {
        let acf = r#"
ASG(G) {
    INPA("rec1.VAL")
    INPC("rec3.VAL")
    RULE(1, READ)
}
"#;
        let config = parse_acf(acf).unwrap();
        let inp = &config.asg["G"].inp;
        assert_eq!(inp.len(), 2);
        assert_eq!(inp[0].index, 0);
        assert_eq!(inp[0].link, "rec1.VAL");
        assert_eq!(inp[1].index, 2);
        assert_eq!(inp[1].link, "rec3.VAL");
    }

    #[test]
    fn asg_inp_bad_selector_is_rejected() {
        // INPZ is out of the A..U range.
        assert!(parse_acf(r#"ASG(G) { INPZ("x") }"#).is_err());
    }

    // ----- L-4: parenthesised name robustness -----

    #[test]
    fn paren_name_rejects_embedded_whitespace() {
        // `UAG(my group)` must NOT silently become `mygroup`.
        assert!(parse_acf("UAG(my group) { x }").is_err());
    }

    #[test]
    fn paren_name_rejects_unterminated() {
        assert!(parse_acf("UAG(unterminated").is_err());
    }

    #[test]
    fn paren_name_accepts_quoted_form() {
        let config = parse_acf(r#"UAG("my group") { x }"#).unwrap();
        assert!(config.uag.contains_key("my group"));
    }

    /// ASL gate still works: a low-level WRITE rule does not apply to
    /// a high-ASL record. C `RULE(N,…)` applies only when ASL ≤ N.
    #[test]
    fn asl_gate_still_honoured_after_fail_closed_rewrite() {
        let config =
            parse_acf("ASG(A) { RULE(0, READ) RULE(1, WRITE) }").unwrap();
        // ASL-0 record: READ rule applies, WRITE rule applies.
        assert_eq!(
            config.check_access_method("A", "h", "u", 0, "", ""),
            AccessLevel::ReadWrite
        );
        // ASL-2 record: both rules require ASL ≤ their level, so
        // neither applies ⇒ denied.
        assert_eq!(
            config.check_access_method("A", "h", "u", 2, "", ""),
            AccessLevel::NoAccess
        );
    }
}
