# Permissive-on-failure audit of the CA and PVA authorization paths

**Measured at** integration worktree `/home/stevek/work/epics-rs/.caucus/worktrees/integration`,
HEAD **`b594b18a7e70905ac607803e8bbc55bc469c166f`** (`fix(rtems): every IOC thread
states a stack class — the first ceiling`), working tree **clean**.

Reading began at `51cef3f2` while the sibling panel's A6 stack-class fix was
still uncommitted (6 modified files). It landed as `b594b18a` mid-audit.
`git diff --stat 51cef3f2..b594b18a` touches six files; two of them carry
citations of mine (`epics-ca-rs/src/bin/rtems-ca-ioc.rs`,
`epics-ca-rs/src/server/blocking.rs`) and **both citations were re-read and
re-verified at `b594b18a`** (`rtems-ca-ioc.rs:140-141`,
`blocking.rs:134`). Every other file cited here is byte-identical across the
two commits. **Nothing in this audit was written; no file was edited, no
commit made.**

**Reference trees** (upstream comparison, read-only):
- EPICS base (C) — `/home/stevek/work/epics-base`
- pvxs (C++) — `/home/stevek/work/epics-modules/pvxs` @ `9348ebc` (`1.5.2-20-g9348ebc`)
- ca-gateway (C++) — `/home/stevek/work/epics-modules/ca-gateway` @ `0666f21`
  (**not** in the CLAUDE.md reference list; found by local search, present)

---

## 0. What was audited, and the one fact that reshapes the question

### 0.1 Scope

Every point at which an *identity* value enters an authorization comparison in
the CA server, the PVA server, and the two gateways that reuse their gates:

| identity | source | consumer |
|---|---|---|
| CA account | `CA_PROTO_CLIENT_NAME` (wire) | ACF UAG |
| CA host | `CA_PROTO_HOST_NAME` (wire) **or** peer IP | ACF HAG, gateway `DENY FROM` |
| CA method/authority | mTLS chain, cap-token | ACF `METHOD`/`AUTHORITY` |
| PVA account | `CONNECTION_VALIDATION` `user` (wire) | ACF UAG |
| PVA host | `CONNECTION_VALIDATION` `host` (**wire**) | ACF HAG, gateway control ACF |
| PVA roles | `getpwnam`/`getgrouplist` on the account | ACF `role/<name>` UAG members |
| PVA method/authority | advertised-method gate, x509 chain | ACF `METHOD`/`AUTHORITY` |
| HAG member set | forward DNS at ACF-parse time | ACF HAG |
| `.pvlist` `DENY FROM` set | forward DNS at pvlist-load time | gateway admission |

### 0.2 The EPICS ACF model has no DENY construct — this corrects my own §A5

`RuleAccess` is `{ None, Read, Write }` with `None` as `#[default]`
(`epics-base-rs/src/server/access_security.rs:440-449`), and `compute_rules`
(`:766-862`) starts at `AccessLevel::NoAccess` and only ever **raises** it:

```rust
let mut access = AccessLevel::NoAccess;          // :766
...
if access == AccessLevel::ReadWrite { break; }   // :778
if rule_rank(rule_access(rule)) <= rule_rank(access) { continue; }  // :781
```

There is no rule form that lowers a granted level. This is faithful to C
`asComputePvt` (cited at `:724-728`).

**Consequence for the sentinel question the task poses:** inside the ACF engine
a non-matching sentinel can only ever *fail to raise* access, i.e. it is
uniformly fail-CLOSED. My Task N §A5 wrote that `unresolved:<host>` means "a
rule meant to grant denies **and a DENY FROM rule fails to deny**" — the second
half is wrong for the ACF engine, because no such rule exists there.

The unsafe half of the asymmetry is real, but it lives in exactly one place:
the **gateway `.pvlist` `DENY FROM`** construct (§2.2, §4). That is the only
DENY primitive in the tree, and §4 treats it in both directions per site as
asked.

### 0.3 Method

Every hit below was read in its file, not classified from a grep line. Verdicts
are **ALLOW** (a failed or degenerate lookup widens access), **DENY** (it
narrows access), **ERROR** (it refuses / propagates), or **NO-OP**.
Upstream comparison is stated per site with the C/C++ file:line, because a
deviation where **we are more permissive than upstream** is a different finding
from one where we merely differ.

---

## 1. ALLOW — ranked

### A1 (worst). The PVA server takes its ACF host identity off the wire; pvxs never does

**Site:** `epics-pva-rs/src/server_native/tcp.rs:2767-2769`

```rust
("host", PvField::Scalar(crate::pvdata::ScalarValue::String(v))) => {
    creds.host = v.as_str_lossy().into_owned();
}
```

That is the **only** write to `ClientCredentials::host` in the crate
(`rg 'host\s*=' server_native/tcp.rs` → one production hit, `:2768`; the other
three are `let cred_host = cred.host.clone()` at `:6649`, `:6801`, `:6953`).
It is never derived from, checked against, or reconciled with the peer address.

It propagates into `ChannelContext.host` at **10** literal sites
(`tcp.rs:4319, 4663, 4897, 5023, 5407, 6429, 6524, 7228, 7314, 7533`) — each of
which builds the context with the real `SocketAddr` sitting in the adjacent
field:

```rust
let ctx = crate::server_native::source::ChannelContext {
    peer,                                 // tcp.rs:4316 — the trustworthy value
    account: cred.account.clone(),
    method:  cred.method.clone(),
    host:    cred.host.clone(),           // tcp.rs:4319 — the wire value
    ...
```

and reaches the ACF gate at **16** production call sites:
`tcp.rs:1991, 4379, 4404, 4702, 5069, 6439, 6695, 6843, 6998, 7039, 7349`;
`server_native/composite.rs:216`; `server_native/source.rs:760`;
`epics-bridge-rs/src/pva_gateway/control.rs:596` and
`pva_gateway/source.rs:519, 2015`.
(`native_source.rs:1260/1269/1280` and `:3120…:3420` are inside
`#[cfg(test)] mod tests` at `native_source.rs:1209` — test-only, excluded.)

**Comparison performed:** `compute_rules` HAG match,
`access_security.rs:820-827` — `members.iter().any(|m| m.eq_ignore_ascii_case(&host_lc))`.

**Verdict: ALLOW.** A PVA client chooses the string that is matched against
every `HOST(...)` HAG. Any rule scoped `HAG(trusted)` is satisfied by sending
`host: "<whatever trusted contains>"` in the `ca` auth body.

**Upstream:** pvxs has **no `host` field in `ClientCredentials` at all**
(`src/pvxs/srvcommon.h:36-55` — `peer`, `iface`, `method`, `account`, `raw`,
`roles()`). QSRV derives the ACF host from the socket:

```cpp
Credentials::Credentials(const server::ClientCredentials& clientCredentials) {
    SockAddr addr(clientCredentials.peer);
    addr.setPort(0);
    host = std::string(SB()<<addr.map6to4());        // ioc/credentials.cpp:27-29
```

and hands exactly that to `asAddClient` (`ioc/securityclient.cpp:25-30`). The
wire auth body is retained only as `C->raw` (`serverconn.cpp:232`) and is never
consulted for authorization. **We are strictly more permissive than pvxs**, and
the thing pvxs makes structurally impossible is what we do by default.

**The code already documents the correct invariant and violates it — twice:**

- `server_native/config.rs:545-548`:
  *"Host name claim from the `ca` auth, when present. **Informational only —
  never trust it for access decisions** over the network hostname /
  mTLS-verified peer."*
- `server_native/source.rs:99-100`:
  *"**Reverse-resolved host name.** Empty when DNS lookup failed."*

Neither is true of the value assigned. There is no reverse DNS anywhere in
either server's identity path — the only DNS call in the whole authorization
surface is the forward `to_socket_addrs` in `hag_members`
(`access_security.rs:1507`), at ACF-parse time (verified by
`rg 'lookup_addr|reverse|gethostbyaddr|to_socket_addrs'` over
`epics-ca-rs/src/server`, `epics-pva-rs/src/server_native`,
`epics-base-rs/src/server`: one hit, `access_security.rs:1507`).

**`asCheckClientIP` does not reach PVA.** It has exactly six references in the
workspace, all in `epics-ca-rs/src/server/tcp.rs` (`:842` production, `:7642-7668`
tests). `rg as_check_client_ip crates/epics-pva-rs` returns nothing. An
operator who hardens an IOC by setting `asCheckClientIP=1` hardens the CA
circuit and leaves the PVA circuit exactly as it was — while simultaneously
switching HAG members to dotted quads, which makes A2 worse.

**Structural fix available in place:** `ChannelContext.peer` is already carried
alongside (`source.rs:92`), so `host` can be derived from it at the 10
construction sites, matching `credentials.cpp:27-29` (strip port, map6to4). The
wire value belongs in the `raw`/diagnostic position pvxs puts it in.

**Reachable on:** every platform. RTEMS adds nothing here — this is a
host-Linux defect first.

---

### A2. The HAG sentinel and the dotted-quad HAG are both typeable by a PVA client

**Sites:** `access_security.rs:1499-1529` (`hag_members`) composed with A1.

Under `asCheckClientIP=1`, `hag_members` resolves each HAG entry at parse time
and stores either a dotted quad (`:1516`) or the literal sentinel
`format!("unresolved:{m}")` (`:1517`, `:1526`) — byte-identical to C
(`asLibRoutines.c:1244`: `static const char unresolved[] = "unresolved:";`).

On the **CA** side the matched identity is then `HostIdentity::Pinned(peer.ip())`
(`epics-ca-rs/src/server/tcp.rs:842-844`), a dotted quad that can never equal
`"unresolved:lab-pc1"` and cannot be chosen by the client. Sound.

On the **PVA** side the matched identity is the wire string (A1). Therefore:

- a client can send `host: "unresolved:lab-pc1"` and **match the sentinel** — a
  HAG whose DNS lookup failed at load time becomes a *password* that anyone who
  can read the `.acf` (or guess the hostname) can type;
- a client can send `host: "192.0.2.7"` and match a HAG entry that the operator
  believed was pinned to one machine's address.

**Verdict: ALLOW**, and specifically an ALLOW created by the *failure* path
(the sentinel exists only because a lookup failed).

**Upstream:** unreachable in pvxs/QSRV — see A1; the host there is the socket
peer, so no sentinel is ever typeable.

**Reachable on:** every platform, but only when `asCheckClientIP=1`. On RTEMS,
where DNS depends on DHCP having completed and on a resolver that
`rtems_init.c` never configures beyond `/etc/dhcpcd.conf`
(`epics-rtems-boot/csrc/rtems_init.c:165-188`), the `unresolved:` branch is the
*expected* branch rather than the exceptional one, so A2 is additionally
reachable there and additionally likely.

---

### A3. A verified mTLS chain whose leaf CN cannot be extracted silently becomes a self-asserted identity

**Sites:**
- `epics-pva-rs/src/auth/tls.rs:1211-1217` — `subject_common_name` returns
  `None` for an absent CN, an empty CN, or one containing NUL.
- `:1330-1342` — `x509_credentials_from_chain` is `Option`-returning and
  short-circuits on that `None` (`let account = subject_common_name(leaf)?;`).
- `server_native/accept.rs:271-283` — the result is stored as
  `ConnInit::x509_identity: Option<X509Credentials>`.
- `server_native/tcp.rs:3094-3098`:

```rust
let x509_locked = x509_identity.is_some();
let mut cred = match x509_identity {
    Some(id) => ClientCredentials::x509(id),
    None     => ClientCredentials::anonymous(),
};
```

`x509_locked == false` is exactly the plaintext path: `process_connection_validation`
takes the `else` branch at `:2595` and **commits the client's
`CONNECTION_VALIDATION` claim** (`:2632-2638`).

**Verdict: ALLOW.** A peer that completed a full mutual-TLS handshake against
the configured trust roots — i.e. one the operator believes is cryptographically
identified — is handed the same self-asserted `method="ca", account=<anything>`
path as an unauthenticated plaintext peer, on a cert-content technicality. The
downgrade is silent: no `warn!` fires on the `None`.

The narrower sibling case is sound and worth stating: when the chain has no
self-signed CA at its end, `authority` is left empty (`:1339-1342`,
`auth/x509.rs:32-34`), and an empty authority fails
`rule.authority.iter().any(...)` at `access_security.rs:836-840` — DENY. Only
the *account* extraction failure escalates, because it takes the whole
credential with it.

**Upstream:** **cannot be compared from this machine.** The local pvxs checkout
(`1.5.2-20-g9348ebc`) has **no TLS support at all** — `ls src` shows no
`ossl.cpp`, and `rg 'fill_credentials|X509'` over `src/*.cpp src/*.h` returns
nothing. The `x509` method, `METHOD(...)`/`AUTHORITY(...)` ACF clauses and
`PeerCredentials` that our port cites are from a pvxs version / branch not
present here. See §7.C1.

**Reachable on:** hosted only in practice — the `tls` feature is not in the
RTEMS build (§6).

---

### A4. A `getpwnam` miss turns the account name into a role of the same name

**Site:** `epics-pva-rs/src/auth/plain.rs:210-214`

```rust
let pw = libc::getpwnam(account_cstr.as_ptr());
if pw.is_null() {
    return vec![account.to_string()];
}
```

and the same fallback on four further degenerate inputs (`:202` embedded NUL,
`:223` non-UTF-8 `pw_name`, `:227` re-`CString` failure, `:264` the
no-local-account-DB target arm).

`roles` reaches `compute_rules` and matches `role/<name>` UAG members:

```rust
m == user || matches!(m.strip_prefix("role/"), Some(role) if roles.iter().any(|r| r == role))
// access_security.rs:805-809
```

**Verdict: ALLOW.** A client claiming `user: "operators"` on a server where no
passwd entry `operators` exists is granted `roles = ["operators"]` and satisfies
`UAG(x) { role/operators }`. The role assertion is self-service whenever the
lookup misses.

**Upstream:** **exact parity — this is pvxs's own fallback.**

```cpp
passwd *user = getpwnam(account.c_str());
if(!user) {
    roles.insert(account);
    return; // don't know who this is        // pvxs src/osgroups.cpp:56-60
}
```

and QSRV pushes it as `role/<role>` (`ioc/credentials.cpp:43-45`). So this is a
finding of the *lower* severity class the task defines — shared with upstream,
not a deviation. It is listed here anyway because RTEMS changes its
probability from "rare" to "always", see below, and because the two-line
structural fix (deny when the account is unknown, rather than trusting it)
would be a deliberate deviation the user has to sign off.

**Reachable on:** every platform. **On RTEMS it is guaranteed for every account
but `root`** — libcsupport synthesizes `/etc/passwd` containing only
`root::0:0::::` before reading it (the box's measurement of `pwdgrp.c:201`
preceding the `fopen` at `:203`), so every non-root `getpwnam` misses. Our
build additionally selects the `#[cfg(not(local_account_db))]` arm on RTEMS
(`plain.rs:253-265`), which reaches the *same* `vec![account]` result by a
different route. Either way, on RTEMS `roles == [account]` for every PVA client,
always, and `role/<name>` UAG rules are pure self-assertion there.

---

### A5. `cap-tokens`: a client opts out of token verification by omitting the prefix

**Site:** `epics-ca-rs/src/server/tcp.rs:2554-2588`

```rust
state.username = match (&state.cap_token_verifier, raw.starts_with("cap:")) {
    (Some(v), true) => match v.verify(&raw, state.tls_channel_binding.as_ref()) {
        Ok(claims)  => { state.auth_method = "cap-token".into(); ... claims.sub }
        Err(e)      => { tracing::warn!(...); "unverified".to_string() }
    },
    _ => raw,                                        // :2587
};
```

The `_ => raw` arm fires whenever the payload does not start with `cap:` — i.e.
whenever the client declines to present a token. Configuring a
`TokenVerifier` (`ca_server.rs:337-342`) therefore adds a credential path
without removing the un-credentialed one, and no "tokens required" mode exists:
`rg 'require_token'` over `epics-ca-rs/src` returns nothing.

**Verdict: ALLOW.** The verifier is advisory. A client that wants
`username = "alice"` sends `alice`; a client that wants it verified sends
`cap:<token>`. Both land in the same `state.username` that
`access_for_asg(..., &self.username, ...)` (`tcp.rs:1008`) matches against UAG.

The failure arm itself is well-built and should not be changed: a rejected
token becomes the fixed literal `"unverified"` rather than folding
attacker-controlled bytes into the ACF identity and the audit log
(`:2573-2585`), and `"unverified"` matches no UAG unless one names it.
Note only that a plaintext client can *also* type `unverified`, so the sentinel
identifies a state, not a client.

**Upstream:** none — `cap-tokens` is an epics-rs extension with no C
counterpart. Not a deviation; an in-house gap.

**Reachable on:** hosted only, and only under `--features cap-tokens` (default
off). `epics-ca-rs/src/server/mod.rs:23` gates part of the surface on
`all(feature = "cap-tokens", not(target_os = "rtems"))`.

---

### A6. No ACF attached ⇒ ReadWrite for everyone — including the state RTEMS boots in

**Sites:** three, one per gate:

- `epics-base-rs/src/server/access_security.rs:314-319` —
  `AccessGateInner::Open => (AccessLevel::ReadWrite, false)` and
  `Required { acf, .. }` with `*guard == None` → the same.
- `epics-ca-rs/src/server/tcp.rs:1055-1059` (simple PV) and `:1104-1106`
  (record field) — `else { (AccessLevel::ReadWrite, false) }`.

**Verdict: ALLOW**, and **exact upstream parity**:

```c
#define asCheckGet(asClientPvt) (!asActive || ((asClientPvt)->access >= asREAD))
#define asCheckPut(asClientPvt) (!asActive || ((asClientPvt)->access >= asWRITE))
/* epics-base modules/libcom/src/as/asLib.h:46-49 */
```

with `int asActive = FALSE;` (`asLibRoutines.c:44`) and
`asAddClient` returning `S_asLib_asNotActive` before doing anything
(`:374`). An IOC with no ACF is unrestricted in C too.

Two things make it worth naming rather than dismissing:

1. **Upstream degrades into this state on an ACF *load failure*.**
   `asInitFile` prints `ERROR asInitFile: Can't open file '%s'` to stderr and
   returns `S_asLib_badConfig` (`asLibRoutines.c:179-183`); `asActive = TRUE`
   is only reached at `:169` on the success path. A C IOC whose `.acf` is
   missing keeps running fully permissive. **Our port is better here and should
   stay that way:** `CaServerBuilder::acf_file` returns `Err(CaError::Io)` on a
   read failure (`epics-ca-rs/src/server/ca_server.rs:181-186`), so the caller
   must decide — see §3.
2. **It is the state the RTEMS image boots in, unconditionally.**
   `epics-ca-rs/src/bin/rtems-ca-ioc.rs:140-141`:

   ```rust
   let acf = Arc::new(tokio::sync::RwLock::new(None));
   let server = match BlockingCaServer::bind((Ipv4Addr::UNSPECIFIED, port), db, acf) {
   ```

   There is no ACF, no path to load one from (the IMFS has no files), and no
   environment to name one with (`rtems_init.c` calls `setenv` zero times —
   Task N §A-fixed). So on target the entire §1 ranking above is moot in one
   direction and sharpened in the other: **today every RTEMS PVA/CA client has
   ReadWrite on everything**, and A1–A4 describe what happens the moment an ACF
   is compiled in.

---

## 2. The DENY construct, and the one place we are more permissive than upstream on it

### 2.1 `.pvlist DENY FROM` — DNS failure is handled correctly (fail-closed)

`epics-bridge-rs/src/ca_gateway/pvlist.rs:190-197`, `:202-243`: a `DENY FROM`
host that fails to resolve is dropped from the rule's host set with a `WARN`;
if **all** of a rule's hosts fail, `from_hosts` ends empty and
`is_global_deny` (`:117`) promotes the rule to a **global** deny. That is
fail-closed in the direction a DENY rule needs, and it is cited to C's own
two-pass parser (`gateAs.cc:504-507`, `:540-556`). IPv6 literals are dropped
the same way with the same collapse, cited to `aToIPAddr` being AF_INET-only.

**Verdict: DENY (sound).** This is the counter-example that proves the sentinel
asymmetry was thought about somewhere: the ACF engine stores a never-matching
sentinel because a non-match is safe there, and the pvlist collapses to a
global deny because a non-match would be unsafe there.

### 2.2 …but the write-side enforcement point feeds it the wrong identity type

**Site:** `epics-bridge-rs/src/ca_gateway/upstream.rs:1603`

```rust
if pvlist.is_host_denied(&pv_name, &ctx.host) {
```

`ctx` is a `WriteContext` (`epics-base-rs/src/server/pv.rs:48-56`) built by the
CA server at `epics-ca-rs/src/server/tcp.rs:4367-4371`:

```rust
let ctx = epics_base_rs::server::pv::WriteContext {
    user: state.username.clone(),
    host: state.hostname.as_str().to_string(),   // the HostIdentity string
    peer: state.peer.clone(),                    // "ip:port" — the real thing
};
```

Under CA's default (`asCheckClientIP == 0`) `state.hostname` is
`HostIdentity::Claimed(...)` — **the name the client sent in
`CA_PROTO_HOST_NAME`** (`tcp.rs:798`, `:847`, claimed at `:2483-2484`).

`is_host_denied` compares that against `from_hosts`, whose documented
post-`resolve_hosts` invariant is *"every non-empty `from_hosts` vec contains
only IP-address strings, so `is_host_denied` can compare them directly against
the TCP peer IP that callers pass"* (`pvlist.rs:199-201`, restated at
`:286-291`, `:411-413`).

Two consequences, both provable without running anything:

1. **Type mismatch.** A dotted quad never equals a claimed host *name*, so under
   the default configuration `DENY FROM` **never fires on the write path** for
   any client that claims a normal hostname.
2. **Client-controlled even when it does.** A client that claims
   `HOST_NAME = "10.0.0.9"` selects which `DENY FROM` row applies to it — and by
   claiming anything else, selects none.

**The same policy is evaluated correctly at the other enforcement point.**
`epics-bridge-rs/src/ca_gateway/server.rs:640`:

```rust
Some(addr) => pvlist.match_name_for_host(&name, &addr.ip().to_string()),
```

— the real peer IP, exactly as the invariant asks. So the two call sites of one
policy disagree about what a host is, and the write side holds the untrusted
one.

**Upstream does the opposite by construction.** ca-gateway `gateServer.cc:1518-1533`:

```cpp
if(getAs()->isDenyFromListUsed()) {
    char hostname[GATE_MAX_HOSTNAME_LENGTH];
    // Get the hostname and check if it is allowed
    //getClientHostName(ctx, hostname, sizeof(hostname));     // <- abandoned
    struct sockaddr_in sockAdd = clientAddress.getSockIP();
    ...
    ipAddrToDottedIP(pSockAdd,hostname,sizeof(hostname));
```

The commented-out `getClientHostName` is the claimed-name path they deliberately
did not take, and `gateAs.cc:456` states the matching half:
*"All deny from rules with host names will be converted to ip addresses."*

**Verdict: ALLOW, and a deviation where we are more permissive than upstream on
the only DENY primitive in the tree.** By the task's own severity rule this
ranks with A1. It is filed here rather than in §1 only because it is in
`epics-bridge-rs` rather than the two servers proper.

**Structural fix available in place:** `WriteContext.peer` (`pv.rs:54-55`) is
already carried; the write path needs the same `addr.ip().to_string()` the
search path uses (port-stripped), not `ctx.host`.

---

## 3. Verified fail-closed / fail-loud — what was checked and found sound

Listed so the scoping is checkable, and because three of these are structural
ports worth not regressing.

| # | site | failure / degeneracy | verdict |
|---|---|---|---|
| S1 | `access_security.rs:766` `compute_rules` | any unmatched identity | **DENY** — starts `NoAccess`, only raises |
| S2 | `access_security.rs:685-691`, `:714-723` | ASG name missing **and** no `DEFAULT` | **DENY** — explicit *"fail CLOSED rather than open"* comment; C always synthesises `DEFAULT` (`asLibRoutines.c:107`) |
| S3 | `access_security.rs:440-449` | `RuleAccess` derive | **DENY** — `#[default] None` ⇒ `RULE(N, NONE)` is `asNOACCESS` |
| S4 | `access_security.rs:850-853` + `epics-ca-rs/.../tcp.rs:994-1003` | CALC-gated rule with a bad input, an uncompilable expression, or **no INP resolver installed** | **DENY** — `unwrap_or(false)` on both eval and compile; `None => false` when no resolver |
| S5 | `epics-ca-rs/src/server/tcp.rs:739-768` | client tries to overwrite a pinned host identity | **NO-OP by type** — `HostIdentity::claim` is a no-op on `Pinned`; the illegal transition is unrepresentable, not runtime-checked. Faithful to `camessage.c:839-843` / `caservertask.c:1425-1437` |
| S6 | `epics-ca-rs/src/server/tcp.rs:2423-2434`, `:2501-2512` | `HOST_NAME`/`CLIENT_NAME` after the first channel | **ERROR** — `ECA_INTERNAL`, claim ignored; matches C `host_name_action`/`client_name_action` |
| S7 | `epics-ca-rs/src/server/tcp.rs:2445-2465`, `:2520-2539` | unterminated or >511-byte name | **ERROR** — reply + `CaError::Protocol` (disconnect), matching C's `RSRV_ERROR` |
| S8 | `epics-pva-rs/.../tcp.rs:2606`, `:2614-2638` | client selects an unadvertised auth method (e.g. claims `x509` over plaintext) | **DENY** — `advertised` is computed from the *effective* method and `*cred` is committed **only** on the advertised path; a rejected re-auth leaves the previous identity in force. Mirrors pvxs `serverconn.cpp:221-241` including the candidate-not-committed shape |
| S9 | `epics-pva-rs/.../tcp.rs:2577`, `:2606` | truncated / undecodable auth body | **ERROR** — `?`-propagated, connection-fatal, mirroring pvxs `bev.reset()` (`serverconn.cpp:211-216`). The comment at `:2512-2520` records the pre-fix behaviour, which was an ALLOW of exactly this audit's shape (`method="ca", account="ca"`) |
| S10 | `epics-pva-rs/.../tcp.rs:2465-2467`, `:2770-2775` | client advertises `groups`/`roles` on the wire | **DENY by construction** — every constructor funnels through `with_server_roles()`, which overwrites `roles` from `osd_get_roles(account)`; the wire field is matched and discarded |
| S11 | `epics-pva-rs/.../tcp.rs:2793-2795` | `method == "ca"` with an empty `user` | **DENY** — `Ok(None)` ⇒ caller keeps `anonymous()`; matches `serverconn.cpp:229-231` |
| S12 | `epics-ca-rs/src/server/ca_server.rs:181-186` | ACF file unreadable | **ERROR** — `Err(CaError::Io)`; **louder than C**, which prints to stderr and runs on permissive (`asLibRoutines.c:179-183`) |
| S13 | `epics-bridge-rs/.../upstream.rs:1619-1625` | CA client never sent `CLIENT_NAME` | **DENY** — empty user refuses the put whenever the ACF has any rule |
| S14 | `epics-bridge-rs/.../control.rs:592-604` | gateway control RPC with no control ACF configured | **DENY** — `None => false` |
| S15 | `auth/tls.rs:1211-1217`, `:1383-1386`, `:1400-1405` | CN empty or containing NUL | **rejected at extraction** — correct in isolation; the *consequence* of the resulting `None` is A3 |
| S16 | `auth/x509.rs:32-34`, `tls.rs:1339-1342` | chain has no self-signed CA at its end | **DENY** — empty `authority` fails `AUTHORITY(...)` at `access_security.rs:836-840` |

---

## 4. The sentinel asymmetry, per site, in both directions

The task asks for this explicitly rather than assumed one way.

| sentinel | where produced | matched against | under a grant rule | under a deny rule |
|---|---|---|---|---|
| `unresolved:<host>` | `access_security.rs:1517`, `:1526` (forward DNS failed at ACF-parse time, `asCheckClientIP=1` only) | HAG member list | **safe** — never matches, rule fails to grant (§0.2: the only outcome the ACF engine has) | **n/a — the ACF engine has no deny rule.** Not "safe by luck": structurally absent |
| `unresolved:<host>` | same | HAG member list, **PVA path** | **unsafe** — the client picks the identity string (A1), so it can type the sentinel and match it (**A2**) | n/a |
| `invalidhost.` | `auth/plain.rs:14`, `:26-28` — **client-side** only, when `gethostname()` fails | the server's HAG, as a claimed host | **safe on CA** (a claimed name matches only a name-form HAG, which is what the operator wrote); **client-controlled on PVA** (A1) — but no worse than any other string it could send | n/a |
| `nobody` | `auth/plain.rs:13`, `:19-21` — client-side, when the user lookup fails | UAG members | as above: safe unless an operator writes `UAG(x) { nobody }`, which would then be satisfiable by anyone | n/a |
| `unverified` | `epics-ca-rs/.../tcp.rs:2584` — cap-token verification failed | UAG members | **safe** — matches nothing unless named; deliberately *designed* to be namable so an ACF can single it out. Caveat: a plaintext client can type it too, so it names a state, not a client | n/a |
| dropped host / empty `from_hosts` | `pvlist.rs:190-197` — `DENY FROM` host failed to resolve | `.pvlist` deny set | n/a | **safe** — the rule is promoted to a **global** deny (`is_global_deny`, `:117`) rather than silently not matching. The one site where the unsafe direction was live, and it is closed |
| `""` (empty host) | `ClientCredentials::anonymous()` / `x509()` (`tcp.rs:2474`, `:2489`); `HostIdentity::Claimed(String::new())` (`epics-ca-rs/.../tcp.rs:798`) | HAG members | **safe** — matches no HAG member; but note a rule with an **empty** `hag` list still applies to it (`access_security.rs:821`, deliberate, C parity, documented at `:730-736`) | n/a |

---

## 5. Upstream comparison summary

**More permissive than upstream (highest severity):**
- **A1** — PVA ACF host is client-asserted; pvxs derives it from the socket
  (`ioc/credentials.cpp:27-29`) and has no wire host field at all
  (`srvcommon.h:36-55`).
- **A2** — follows from A1; unreachable upstream.
- **§2.2** — gateway `DENY FROM` evaluated against the CA-claimed host name;
  ca-gateway explicitly uses `clientAddress.getSockIP()` and left
  `getClientHostName` commented out (`gateServer.cc:1523-1530`).

**Same as upstream (shared exposure, lower severity):**
- **A4** — `getpwnam` miss ⇒ `roles = {account}` is verbatim pvxs
  (`osgroups.cpp:56-60`).
- **A6** — no ACF ⇒ permissive is verbatim base (`asLib.h:46-49`,
  `asLibRoutines.c:44`, `:374`).
- The `unresolved:` sentinel spelling is verbatim base (`asLibRoutines.c:1244`),
  and `asCheckClientIP` defaulting to off is verbatim base
  (`asLibRoutines.c:35` — `int asCheckClientIP;` with no initialiser ⇒ 0; our
  `AtomicBool::new(false)` at `access_security.rs:1462-1463`).

**Stricter than upstream (not defects; listed so a future "parity fix" does not
undo them):**
- **S12** — ACF read failure is an `Err`, not a stderr line.
- **A3's sibling** — an empty/NUL CN is rejected outright rather than
  propagated.
- `composite.rs:216` calls `.check(...)` (which passes `&[]` roles,
  `access_security.rs:293`) while the 11 `tcp.rs` sites and `source.rs:760`
  call `.check_with_roles(...)`. Under `CompositeSource` a `role/<name>` UAG
  member therefore cannot match. Fail-closed, so not a security finding, but
  **it is an inconsistency between two call sites of one policy** — the same
  shape as §2.2, in the safe direction.

**No upstream to compare against on this machine:** A3 and everything else
`x509`/`METHOD`/`AUTHORITY` — see §7.C1.

---

## 6. RTEMS reachability

| finding | hosted | additionally on RTEMS |
|---|---|---|
| A1 | yes | yes — and `asCheckClientIP`, the only mitigation, was already PVA-blind |
| A2 | only when `asCheckClientIP=1` | yes, **and more likely**: the `unresolved:` branch is the expected one when DHCP/resolver state is absent (`rtems_init.c:165-188` writes `/etc/dhcpcd.conf` and nothing else) |
| A3 | yes | **no** — the RTEMS PVA build excludes the TLS surface |
| A4 | on any unknown account | **always, for every non-root account** — libcsupport's synthesized `/etc/passwd` has only `root`, and our `#[cfg(not(local_account_db))]` arm (`plain.rs:253-265`) reaches the same result independently |
| A5 | only under `--features cap-tokens` | **no** — `epics-ca-rs/src/server/mod.rs:23` gates on `not(target_os = "rtems")` |
| A6 | when no ACF is configured | **always** — `rtems-ca-ioc.rs:140` hardcodes `None` and there is no filesystem to load one from and no env to name one with |
| §2.2 | yes | not evaluated — `epics-bridge-rs` is outside the RTEMS dependency closure (Task M §0.1: only `epics-base-rs`, `epics-ca-rs`, `epics-pva-rs` reach an image) |

---

## 7. What I could not establish from this machine

Stated in the terms the previous two audits used, with what would settle each.

**C1 — the entire x509/TLS upstream comparison (blocks A3, S15, S16, and the
`METHOD`/`AUTHORITY` parity claims our own doc comments make).**
The local pvxs checkout at `9348ebc` (`1.5.2-20-g9348ebc`) contains no TLS
support: `ls src` has no `ossl.cpp`, and `rg 'fill_credentials|X509'` over
`src/*.cpp src/*.h` returns nothing. Our port cites
`SSLContext::fill_credentials` and `PeerCredentials` (`auth/x509.rs:16-27`) —
neither exists in the tree on this machine. I did not reconstruct them from
memory. **What would settle it:** a pvxs checkout at a revision with
`PVXS_ENABLE_OPENSSL` / the `tls` work merged, plus the epics-base branches for
PR #563 / #618 that introduce the `METHOD`/`AUTHORITY` ACF clauses. Until then
every "mirrors pvxs" claim on the x509 path in this workspace is unverified.

**C2 — whether A1 is exploitable against a *real* pvxs or pvaPy client, or only
against a crafted one.**
I established that the server *reads and trusts* a wire `host` field. I did not
establish which clients populate it, nor with what. `auth/plain.rs:271-276`
shows our own client sends `EPICS_PVA_AUTH_HOST` or `gethostname()`. Whether
pvAccessCPP / pvaPy send a `host` field at all is not decidable from a tree
whose pvxs never reads one. **What would settle it:** a wire capture of a
`CONNECTION_VALIDATION` reply from pvxs `pvxput` and from pvaPy, or reading the
`buildCAMethod` equivalent in pvAccessCPP (not present locally). This does not
change A1's verdict — the server's trust is the defect, not the clients'
honesty — but it decides whether the fix is a behaviour change for existing
deployments.

**C3 — the actual ACF outcome on target for any of §1.**
No RTEMS image has ever run an ACF: `rtems-ca-ioc.rs:140` hardcodes `None`, so
A1–A4 are unreachable *today* on target and become reachable the moment an ACF
is compiled in. I cannot say what `compute_rules` does with a real
`.acf` on RTEMS because nothing has ever loaded one there. **What would settle
it:** on the bring-up box, an RTEMS image with an ACF compiled into a static
`&str` and passed to `BlockingCaServer::bind`, then a `caput`/`pvput` from the
host against a `HAG`-scoped rule, once with a truthful host claim and once with
a forged one.

**C4 — whether `getpwnam("root")` on RTEMS returns before or after our
`#[cfg(not(local_account_db))]` arm makes the question moot.**
The box established the libcsupport synthesis (`pwdgrp.c:201` before the `fopen`
at `:203`), and our build now selects the no-account-DB arm on RTEMS. Both
routes give `roles = [account]`, so A4's *outcome* is settled. What is **not**
settled is whether any other code path in the image still calls `getpwnam`
directly and would receive the documented POSIX deviation (miss returns `EINVAL`
rather than not-found). **What would settle it:** `rg 'getpwnam|getpwuid|getgrgid|getgrouplist'` over the RTEMS-reachable crates in a build with
`local_account_db` off, plus a target run that calls each survivor.

**C5 — whether §2.2's type mismatch is masked in any existing deployment by a
client that happens to claim its own IP.**
`is_host_denied` compares case-insensitively for exact string equality
(`pvlist.rs:308`), so a client whose `HOST_NAME` claim happens to be its dotted
quad *would* match. Whether any real client does that is not decidable from the
tree. **What would settle it:** a `.pvlist` with a `DENY FROM` row, a gateway
run, and a `caput` from a denied host — once with the default
`CA_PROTO_HOST_NAME` a stock `caput` sends, once with none.

**C6 — the trap/put-logging side.**
`TrapWriteFields.host` is populated from the same `state.hostname`
(`epics-ca-rs/src/server/tcp.rs:4343`), so a forged CA host claim also forges
the *audit record* of the write. I traced the field to its construction but did
not follow it through the listener chain to whatever consumes a put-log line, so
I cannot state what a downstream audit consumer does with a forged host.
**What would settle it:** reading the `TrapWriteListener` implementations and
whatever `emit_putlog` (`epics-bridge-rs/.../upstream.rs:1761`, called at
`:1657` and `:1799`) writes, against
C `asTrapWriteWithData` (`rsrv/camessage.c:768-779`).
