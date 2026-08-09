# Upstream-issue audit — 2026-08-09

Sweep of the epics-base and pvxs GitHub issue trackers for problems that
could also exist in epics-rs. Surface: 149+37 open and 139+52
recently-closed issues; ~80 triaged as plausibly applicable (the rest
are C-toolchain/build/docs/platform-specific) and checked against the
port by six parallel read-only agents (areas: CA, PVA wire,
QSRV/pvalink/discovery, DB links/events, record types,
libcom/access-security/iocsh). Closed upstream issues were checked
against the upstream FIX (did we port the corrected behavior?); open
ones against the reported defect. Baseline for already-known deliberate
deviations: doc/upstream-c-bugs.md.

Verdict legend: SAME-PROBLEM (defect exists here), NOT-PRESENT (area
implemented, defect absent — with evidence), NOT-APPLICABLE (feature
not ported / C-specific), SUSPECTED (not fully proven), PARITY
(deliberate byte/behavior parity with an upstream wart that upstream
has not fixed).

## Open Findings

### UI-100 iocsh `asInit` never reaches the live AccessGate — enforcement silently fails open — CLEARED
Severity: HIGH. epics-rs: `crates/epics-base-rs/src/server/iocsh/access_commands.rs:220`. Upstream: adjacent to epics-base#667.
**Cleared**: one `AcfCell` per IOC, created by `IocApplication::run`
before the startup script, administered by every shell
(`IocShell::new_with_acf`) and adopted (never re-wrapped) by
`CaServer::from_parts` / `PvaServer::from_parts` /
`build_qsrv_mount`; `AcfCell` is now a newtype whose `store` fires
`notify_asg_field_changed`, so a post-boot `asInit` also re-gates
live connections and drops the gate/grant caches. Same family closed
alongside: the bridge runner previously wrapped `config.acf` into
THREE independent cells, and the QSRV `AcfAccessControl` froze a
boot-time snapshot no reload could reach. Regression tests:
`crates/epics-base-rs/tests/asinit_reaches_live_gate.rs`.
A script-driven IOC doing `asSetFilename` + `asInit` gets a success
message and working `astac`/`asdbdump`, but the parsed config lands only
in `as_state()` — nothing bridges it to the servers' `AcfCell`, which
stays `None` = permissive. Access security is silently OFF while
appearing loaded. The live cell is fed exclusively by the programmatic
`IocApplication::acf()` path.

### UI-20 Server outbound queue bounded in frames, not bytes — CLEARED
Severity: HIGH. epics-rs: `crates/epics-pva-rs/src/server_native/config.rs:414`, `tcp.rs:3110`, `tcp.rs:2997-3003`. Upstream: pvxs#161.
Per-connection writer queue admits by frame count (default 1024) with no
byte watermark; ~10 MB NTNDArray monitor frames to a slow client can pin
~10 GiB per connection before `send_timeout` evicts. pvxs fixed this
with a byte-denominated `tcp_tx_limit` (2 × socket send buffer). The
per-op squash FIFO bounds staleness, not writer-queue bytes. Static
analysis; not measured live.
CLEARED: `SrvTx` is now a struct carrying a `TxBudget` semaphore of
`SO_SNDBUF × 2` bytes (pvxs `tcp_tx_limit`, `serverconn.cpp:20,61`),
read per-connection by both drivers (`accept.rs` via socket2,
`blocking.rs` via raw libc) and threaded through `ConnInit`. Every send
acquires `clamp(len, 1..=limit)` permits; the writer task releases them
after the frame is written and closes the budget on exit so parked
senders fail instead of waiting forever. Boundary tests in
`tx_byte_budget_tests`.

### UI-1 EPICS_CA_NAME_SERVERS entries never re-resolve DNS — CLEARED
Severity: MED. epics-rs: `crates/epics-ca-rs/src/client/mod.rs:805-806`, `client/search.rs:1147-1191`. Upstream: epics-base#488 (partial).
CLEARED: `parse_nameserver_list` now yields `AddrEntry` (the #488
primitive) and the entry rides to `run_nameserver_connection`, which
calls `refresh_dns()` before every dial — same keep-cached-IP-on-failure
policy as the ADDR_LIST refresh. Regression test
`a_nameserver_dial_uses_the_fresh_dns_resolution`. Residual closed:
the `experimental-rust-tls` SNI override map was keyed by the
startup-resolved `SocketAddr` (a moved nameserver lost its SNI
override until restart); `SniOverrides` now keeps nameserver rows
keyed by hostname and `AddrEntry::refresh_dns` — the one resolution
owner — rewrites the row, so the stale-key state is unconstructable
(`a_moved_nameserver_keeps_its_sni_override`).
The `EPICS_CA_ADDR_LIST` half of #488 is deliberately fixed (periodic
`refresh_dns`), but name-server entries are resolved once at startup and
the per-nameserver task redials the same stale `SocketAddr` forever
(hostname survives only into the TLS SNI map). A nameserver/gateway that
moves behind a stable DNS name is lost until client restart; worst on
`name_servers_only` embedded search and the ca_gateway.

### UI-21 QSRV single-record path admits unresolvable fields as a fabricated NTScalar double — CLEARED
Severity: MED. epics-rs: `crates/epics-base-rs/src/server/database/mod.rs:1853-1879` (`has_name_no_resolve`), `crates/epics-bridge-rs/src/qsrv/channel.rs:625-655`, `qsrv/pvif.rs:421`. Upstream: pvxs#193 (server half).
CLEARED: `BridgeChannel::new` refuses an unresolvable field with
`FieldNotFound` (C `S_dbLib_fieldNotFound`) instead of fabricating an
NTScalar double, and `has_name_no_resolve` validates an explicit field
suffix the way `dbChannelTest` does — declared-name existence, `$`
eligibility, plus the `dbCommon` `DBF_NOACCESS` names
(`DBCOMMON_NOACCESS`) that C resolves and pvxs answers SEARCH for
(measured `pvxget ORACLE:AI.MLOK` → `Refused to create Channel`).
Residual closed: record-own `DBF_NOACCESS` names (`BPTR` family) were
dropped by dbd-codegen with their descs, so a SEARCH for them stayed
silent where C answers then refuses the create. The generator now
keeps the dropped names (`record_noaccess_fields`, per-target), the
gate consults them through one predicate
(`RecordInstance::resolves_noaccess_name`, which also carries the
`dbCommon` list — now generated as `DB_COMMON_NOACCESS` instead of
hand-kept), and downstream record types route theirs via
`Record::declared_noaccess_fields` (motor `CBAK`, mca, scaler, epid,
throttle).
The search/CREATE gate tests only the record name (field suffix
discarded) and `BridgeChannel::new` backstops an unresolvable field
(`REC.TIME`, any typo) with `DbFieldType::Double`/NTScalar — the client
connects and is taught a fantasy prototype, then every GET fails at op
level (debug-only log). C refuses the channel. The group path already
has the gate (`resolve_db_channel`); the single-record path never calls
it.

### UI-22 Client retries a refused CREATE_CHANNEL at ~1 s forever — CLEARED
Severity: MED. epics-rs: `crates/epics-pva-rs/src/client_native/channel.rs:987-989`, `client_native/search_engine.rs:1251-1256`, `:1514-1525`. Upstream: pvxs#193 (client half; fixed upstream in 084336bb).
Refusal → re-search → current bucket (≤1 s) → refusal, indefinitely,
attempt counter reset each pass — the pre-fix pvxs loop. Upstream now
drops the channel into the furthest search bucket (~30 s).
CLEARED: new `SearchReason::CreateRefused` selected when
`researched_after_refusal`; `placement_bucket` parks it at
`(current + N_SEARCH_BUCKETS - 1) % N_SEARCH_BUCKETS` (pvxs
`laterBucket`), and the immediate-broadcast fast path stays gated on
`SearchReason::Initial`, so a refused channel waits one full ring
revolution (~30 s) by construction. Placement unit test covers the
wrap-at-0 boundary and every non-zero bucket.

### UI-63 Enum/menu sources read as string over links deliver index digits, not labels — CLEARED
Severity: MED. epics-rs: `crates/epics-base-rs/src/server/database/links.rs:1353-1357`, `processing.rs:2517-2536`, `types/value.rs:99,1150`; CA half `crates/epics-ca-rs/src/calink/resolver.rs:553`. Upstream: epics-base#183 (closed-fixed), #855 (mechanism).
`stringin`/`lsi` INP at a DBF_MENU/DBF_ENUM field stores `"2"` where
fixed C stores `"INVALID"`. The correct renderer exists
(`field_as_dbr_string` → choice tables) but only sseq requests
`LinkReadAs::String`; the single-INP soft path fetches native and
coerces via field-blind `convert_to`. Same outcome over CA links (calink
subscribes native; no string subscription).
CLEARED: every framework input read (single-INP soft, DOL, multi-input
loop, SIOL) now consults `Record::input_link_read_as` through one
conversion owner (`apply_link_read_as`); stringin/stringout declare
`DBR_STRING` for INP/SIOL/DOL, lsi/lso the `dbGetLinkLS` source switch,
printf the per-conversion request (`%s` → String, FMT-walked). printf's
A..J slots store raw (they are C locals, not DBF fields — the previous
Double-typed coercion turned EVERY string delivery into `atof` = 0.0).
External half: `LinkMetadata::enum_choices` carries the label table
(calink fills it from the `DBR_CTRL_ENUM` attribute fetch — C `dbCa`'s
`pgetString` monitor equivalent; pvalink values already arrive as
`EnumWithChoices`), rendered in `dbr_string_of`'s external arm.
Residuals closed as deliberate deviations, documented at the owning
code: printf's fetch-time slot↔conversion map stays unshifted — after a
failed `*`-width link C's `goto bad_format` skips the conversion's
`plink++` so later directives re-read shifted slots under their own DBR
types; `apply_fmt` reproduces the slot shift, and correcting the
shifted slot's fetch conversion would need a second fetch (double-
processing a PP source) or C's lazy read-during-format model
(`printf.rs::plain_string_slots`). lsi keeps its failed-read LINK alarm
where C's `dbGetLinkLS` silently succeeds without writing on a
disconnected source (`dbLink.c:504-505`; `lsi.rs::input_link_read_as`).

### UI-64 DB-link puts to link fields are accepted where C's dbPut refuses — CLEARED
Severity: MED. epics-rs: `crates/epics-base-rs/src/server/database/field_io.rs:653-707` (`put_pv_body`), reached from `links.rs:1504`. Upstream: epics-base#876 (the C guard is dbAccess.c:1340).
C's `dbPut` refuses writes to DBF_INLINK/OUTLINK/FWDLINK
(`field_type > DBF_DEVICE` → S_db_badDbrtype); only `dbPutField`
(CA/dbpf) routes link fields through `dbPutFieldLink`. Our DB OUT-link
write path lands in `put_pv_body` with no such guard: a local DB link
can silently rewire another record's link field every process — a
semantics divergence and lock-model hazard C deliberately forbids.

CLEARED: `check_not_link_field` (new `CaError::BadDbrType`, C
S_db_badDbrtype) refuses link fields in both `dbPut`-analogue bodies
(`put_pv_body`, `put_pv_and_post_with_origin`); the OUT-link route now
lands the refusal on the writer as LINK/INVALID exactly like C
`setLinkAlarm`. The dbPutField-analogue paths keep their link writes:
`put_record_field_from_ca*` (re-parse via `put_common_field`),
`put_pv_no_process` (autosave; C `reboot_restore` = dbPutField), and
autosave's Process mode routes link entries through the no-process
write. QSRV single-source Force/Inhibit puts re-route link fields down
the Passive/CA path per pvxs `doDbPut`'s per-field split
(iocsource.cpp:451-458, singlesource.cpp:374-383);
`PvDatabase::is_dbf_link_field` is the one classification owner. QSRV
GROUP puts keep refusing link members in the preparation pass ("Links
not supported for put", groupsource.cpp:603-606) — already implemented
and tested, unchanged. Boundary tests:
`tests/dbput_refuses_link_fields.rs` (6),
`testqsingle.rs::link_field_put_succeeds_in_every_process_mode`.

### UI-101 `iocshLoad`/`<` recursion has no depth guard — self-including script aborts the IOC — CLEARED
Severity: MED. epics-rs: `crates/epics-base-rs/src/server/iocsh/mod.rs:103-113`, `:84-88`. Upstream: epics-base#499 (worse than C).
C survives via the incidental fd limit; our `read_to_string` closes the
fd, so recursion is bounded only by the thread stack → Rust
stack-overflow abort at boot. The only depth guard in iocsh
(`max_include_depth: 32`) covers DB-file includes, not scripts.
CLEARED: `IocShell` now carries a `script_depth` cell; both script
executors (`execute_script`, `execute_script_with_macros`) take a
Drop-released `ScriptDepthGuard` via `enter_script`, refusing past
`MAX_SCRIPT_DEPTH = 32` (matching the db_loader `max_include_depth`
convention). At the cap the include line errors and unwinds through
normal `on error` semantics — the explicit form of C's fd-exhaustion
failure. Tests:
`iocsh::tests::self_including_script_errors_at_the_depth_cap` (both
`<` and `iocshLoad` entries, ticket unwinds to 0),
`iocsh::tests::nested_include_under_the_cap_still_runs`.

### UI-104 procserv-rs allocates and locks stdio between forkpty and exec — CLEARED
Severity: MED. epics-rs: `crates/epics-tools-rs/src/procserv/child.rs:309-384`. Upstream: epics-base#211 (fork-safety class).
`in_child_setup_and_exec` builds `CString`s/`Vec` (malloc) and uses
`eprintln!` (stdio lock) after fork; respawns run from the live tokio
supervisor (multi-threaded), so an arena/stdio lock held by another
thread at fork time deadlocks the child pre-exec. The SAFETY comment at
child.rs:110-112 claims async-signal-safe-only. (The daemonize fork is
safe: pre-runtime, single-threaded.)
CLEARED: `ChildExecImage::prepare` builds every allocation before
`forkpty` — cwd/exec `CString`s, the NULL-terminated `argv_ptrs`
vector, and pre-formatted failure lines; NUL validation now surfaces
as a parent-side `ProcServError::Config` instead of a child exit. The
post-fork path is syscall-only: `libc::chdir`/`libc::execvp` on the
pre-built pointers, `write_child_failure` (raw `write(2)` + stack
errno digits) instead of `eprintln!`, and `libc::_exit` instead of
`std::process::exit` (no atexit/stdio flush in the forked image).
Existing regressions `missing_child_binary_exits_255_not_127` /
`failed_chdir_exits_255_not_126` exercise the new failure path.

### UI-24 Tree formatter drops the member name for `any`/union inline fields — CLEARED
Severity: LOW. epics-rs: `crates/epics-pva-rs/src/format.rs:1376-1377`, `:1411-1417`. Upstream: pvxs#46 residue.
The #46 fix proper is ported, but upstream's inline branch emits
`' ' << member` and ours never does: `pvinfo-rs` prints `any` instead of
`any parameters`; value mode omits the member path prefix. No test
covers union/any/struct[] tree rendering.
CLEARED: `tree_show_inline` now takes `member` and emits it first,
before the union `.MEM` selector and value (datafmt.cpp:224-230); the
misnamed `member_already_emitted` fmt parameter that documented the
wrong assumption is gone. Test
`format::tests::tree_inline_branch_keeps_the_member_name` covers the
inline boundaries: `any`/`any[]` describe mode, valued `any`, valued
`union u.i`, and the null union.

### UI-65 DBF_ULONG string puts lack C's via-double fallback — CLEARED
Severity: LOW. epics-rs: `crates/epics-base-rs/src/types/c_parse.rs:107-135,180-259`. Upstream: epics-base#564 (the fallback is quoted in its comments; dbConvert.c:1044-1057).
C re-parses via double when the integer parse stops at `.`/`e`/`E`:
`1.0e3` → 1000, `".5"` → 0. Ours stores 1 for `1.0e3` (longest integer
prefix — accepted, wrong value) and refuses `".5"`. GET direction
unaffected. The negative-wrap behavior #564 complained about is
correctly reproduced (upstream wontfix).
CLEARED: `parse_ulong_via_double` ports `putStringUlong` exactly, keyed
on `scan_int`'s new `end` index (`strtoul`'s `*endp`): fallback on
noConversion or a `.`/`e`/`E` stop, double stored only inside
`0..=UINT_MAX`, out-of-band double keeps the integer prefix, integer
overflow gets no fallback. All expectations measured via `dbpf B.SVAL`
on the reference softIoc (`1.0e3`→1000, `.5`→0, `1.5e20`→1, `-1.5`→
4294967295, `1e999`→error, `-.5`→C silent no-write). Two documented
deviations, both from the port's atomic-put shape: `-.5`-class inputs
(no digits, double out of band) are refused where C reports success
writing nothing, and `1e999` refuses without C's partial prefix write.
Test `c_parse::tests::ulong_string_put_falls_back_via_double`
(includes the UInt64 no-fallback control).

### UI-80 lso/lsi `field(VAL, ...)` load failure carries a misleading diagnostic — CLEARED
Severity: LOW. epics-rs: `crates/epics-base-rs/src/server/db_loader/mod.rs:1529-1560`. Upstream: epics-base#548.
Load is refused as in C, but for an accidental reason: the loader parses
VAL as scalar `Char` and reports a value-syntax error, steering the
operator toward "malformed string" instead of "field not settable from a
.db". `Record::long_string_fields()` exists but the loader never
consults it.
CLEARED: the owned-field arm asks `Record::long_string_fields` before
parsing and refuses with C's real constraint — "can't set array field
before iocInit()" (measured on the reference softIoc: `Can't set
'L.VAL' to 'hello' Can't set array field before iocInit() : Bad Field
value`). Tests in `db_load_refuses_long_string_val.rs` cover lso VAL,
printf VAL, and the ordinary-field control (lso SIZV still loads).

### UI-103 Out-of-quote backslash diverges from C `split()`; `lint_line` disagrees with the splitters — CLEARED
Severity: LOW. epics-rs: `crates/epics-base-rs/src/server/iocsh/registry.rs:418-420` vs `:449-456`. Upstream: adjacent to epics-base#362 (the reported in-quote defect is fixed here).
C consumes `\` as an escape outside quotes (`echo \"hello\"` →
`"hello"`); our splitters only honor backslash inside quotes (→
`\hello"`), while `lint_line` honors it everywhere — the lint passes
lines the splitters then mis-parse.
CLEARED: `split_space_args`, `split_comma_args`, and
`find_closing_paren` now apply lint_line's rules — out-of-quote `\X`
is a literal X (neither separator, quote, nor closing paren), and the
paren scanner tracks `'` quotes like the rest. Two family members
found by the sweep and fixed in the same change: the comma splitter's
second pass re-processed escapes (`"a\\\\b"` collapsed twice) — it is
now trim+strip only, gated on a functional-outer-quote flag so an
escape-produced quote is never stripped — and `find_closing_paren`
took an escaped `\)` or a `)` inside single quotes as the call's end.
All expectations measured on the reference softIoc (`echo \"hello\"`→
`"hello"`, `a\ b`→`a b`, `echo(a\,b)`→`a,b`, `echo(\"hi\")`→`"hi"`,
`echo(a\))`→`a)`, `echo('a)b')`→`a)b`). Tests
`registry::tests::out_of_quote_backslash_escapes_like_c_split`,
`registry::tests::call_syntax_escapes_are_not_double_processed`.

### UI-25 Same-host/same-process discovery (pvxs#200) not live-verified
Severity: LOW/SUSPECTED. Upstream: pvxs#200 (open, cause undiagnosed upstream).
Structurally our port lacks the suspected upstream hazard (separate
client search socket; lo-mcast re-forward ported), but no live
same-process repro under the issue's exact env config was run.

### UI-105 iocsh db* failure paths return Ok — invisible to `on error` — CLEARED
Severity: MED. epics-rs: `crates/epics-base-rs/src/server/iocsh/commands.rs:795-810`, `:1222-1227`, `:1278`; port-original family `commands.rs:92`, `:667-716`, `crates/mqtt-rs/src/z2m.rs:73-94`. Upstream: epics-base#498 (db* family worse than current C).
Current C base routes exactly these failures through `iocshSetError`
(`dbStaticIocRegister.c:282-310`, `dbIocRegister.c:56,73`): a bad
record name, duplicate record, unknown type, or dbLoadRecords parse
failure makes `on error break|halt|wait` fire. The port's handlers
print the failure and return `Ok(Continue)`, so a startup script sails
past a database that did not load. Same print-then-Ok shape in the
port-original commands with no C constraint (dbDeleteRecord,
pushd/popd, mqttZ2m*). Module commands whose C callFuncs are void
(autosave, asyn, areaDetector, astac) mirror C and stay as parity.
CLEARED: every reachable failure arm in the two families now returns
Err — dbCreateRecord's four rejections, dbDeleteRecord's missing
record, all pushd/popd failures — and the two non-fatal dbLoadRecords
loops (alias reject, merge put_common_field) accumulate into a final
Err after the load completes, matching C's parse-recover-then-return
non-zero. mqtt z2m's `load_records` returns Result with the same
accumulation; all six mqttZ2m* commands propagate. Unreachable
missing-arg arms (parse_args rejects first) left as-is. Tests: the
three dbCreateRecord rejection tests now assert Err, and
`on_error_break_stops_at_a_failed_db_command` pins that a real command
failure (not just an unknown command) trips `on error break`.

### UI-106 qsrv serves an always-empty `display.description` — pvxs fills it from DESC — CLEARED
Severity: MED. epics-rs: `crates/epics-base-rs/src/server/record/record_instance.rs` (`populate_display_info`), `crates/epics-bridge-rs/src/qsrv/pvif.rs:763-776`, `:1275-1278`. Upstream: epics-base#785 (the port is behind the pvxs half).
The NTScalar/NTEnum builders emit the `display.description` leaf and
`qsrv_marks::property_leaves` marks it, but no producer ever assigns
`DisplayInfo::description`, so the served description is always the
empty string. pvxs QSRV fills it from dbCommon DESC at channel
initialize (`iocsource.cpp:307-310`). The C-defect half of #785 — no
DBE_PROPERTY on a DESC write because DESC lacks `prop(YES)` — is exact
parity (pinned at `record_instance.rs:5523`) and stays: posting it
would be a wire-visible divergence upstream has not decided to make.
CLEARED: `populate_display_info` ends with a uniform epilogue filling
`display.description` from `common.desc` for every record type (the
builders always emit the leaf, so a newly-Some display changes leaf
values, never wire shape). `DisplayInfo::description` became
`PvString` for the same byte-preservation reason as `units`. Cache
freshness is owned by the DESC arm of `put_common_field` — the single
writer of `common.desc` — which invalidates the metadata cache on a
real change WITHOUT posting DBE_PROPERTY, exactly pvxs's fresh-on-
rebuild/no-event behavior; DESC stays out of `is_metadata_field`
(both invariant doc comments updated). Field channels inherit the
record's DESC through the shared per-record cache, matching pvxs's
`dbChannelRecord(pChannel)->desc`. Three tests that pinned
`display.is_none()` for no-display-arm record types now assert
empty-units/default limits; new boundary tests
`desc_reaches_display_description_and_a_write_refreshes_it` and
`an_idempotent_desc_put_keeps_the_cache`.

### UI-107 HAG hostnames frozen at ACF parse under asCheckClientIP — stale after DNS change
Severity: LOW. epics-rs: `crates/epics-base-rs/src/server/access_security.rs:1717-1748`, `:1518`. Upstream: epics-base#863 (AS half; C parity; upstream PR #862 in flight).
With `EPICS_CA_AS_CHECK_CLIENT_IP` set, `hag_members` resolves each
HAG hostname exactly once during `parse_acf` and stores the frozen
dotted quad; a DNS move leaves stale IPs until an operator re-runs
`asInit`. Byte-for-byte the C behavior (`asLibRoutines.c:1227-1265`),
and the sibling of the CA half this port already closed as the UI-1
family deviation (60 s `refresh_dns` cadence). Fix direction: periodic
re-resolution that rebuilds the hag map and republishes through
`AcfCell::store` — the same notification path `cmd_as_init` uses, so
connected clients re-evaluate automatically. Default mode (claimed
HOST_NAME string match) involves no DNS and is unaffected.

— CLEARED. `AccessSecurityConfig` now keeps the raw HAG spellings
(`hag_raw`; the resolved `hag` stores `hag_members` *output*, which a
DNS move makes unre-resolvable), `with_refreshed_hags` re-runs
`hag_members` — still the single resolution owner — and returns a
refreshed config only on change, and `spawn_hag_refresh` (wired in
`ioc_app` right after `new_acf_cell`) republishes it every 60 s
(`HAG_DNS_REFRESH`, the CA-side `refresh_dns` cadence) via
`AcfCell::store`, the same client-notification path `asInit` uses.
Gated on `asCheckClientIP`; the task holds a `Weak` and ends with the
IOC. Tests: stale-quad re-resolve, unchanged → `None`, name-mode
no-op.

### UI-108 dbLoadTemplate silently ignores `file "x.template" {}`
Severity: LOW. epics-rs: `crates/epics-base-rs/src/server/db_loader/substitution.rs:285-289`, `:449-476`. Upstream: epics-base#666 (C dbLoadTemplate half reproduced; msi's parse-error half absent).
The docs call the subs block optional, but an empty body loads zero
instances with no diagnostic — pinned as intended by
`parse_empty_file_body`. Upstream is undecided between expand-once and
a doc change, so semantics stay; the missing piece is the diagnostic:
warn when a `file` entry contributes zero loads, the same move
upstream made for CP/CPP-on-OUTLINK discards.

### UI-109 `IOCSH_STARTUP_SCRIPT` never set
Severity: LOW. epics-rs: `crates/epics-base-rs/src/server/iocsh/mod.rs` (no producer). Upstream: epics-base#469 (C's setter leaks scope; the port lacks the variable entirely).
C sets `IOCSH_STARTUP_SCRIPT` when a script starts; #469's bug is that
`iocshLoad` overwrites the global value without restoring it, so after
st.cmd finishes the variable names the last nested script. The port
never sets it at all, so a C-compatible st.cmd reading the variable
gets nothing. Port it with the corrected scoping upstream wants: set
on script entry, restore the outer value when a nested script exits,
keep the top-level value after boot. Verify the exact C set sites
against the reference before implementing.

## Parity findings (upstream wart reproduced deliberately — document, do not fix)

- **UI-2** camonitor-rs `-S` renders an emptied char waveform as `0`
  (`cli.rs:620-628`; epics-base#829 open, tied to CA's
  scalar-vs-length-1 ambiguity; byte-parity with C tool_lib).
- **UI-23** Q:group members sorted putorder-then-alphabetical,
  declaration order destroyed (`group_config.rs:536-553`; pvxs#87 open;
  parity with groupconfigprocessor.cpp; `+putorder` is the workaround).
- **UI-60** SDIS read wraps to i16 → unexpected disable every 65536
  counts (`processing.rs:1660-1668`; epics-base#906 open; C dbGetLink
  DBR_SHORT parity).
- **UI-61** Event-queue replace threshold copies C's disputed
  `rngSpace <= EVENTSPERQUE` condition (`event_queue.rs:449`;
  epics-base#868 open; upstream itself split on code-vs-comment).
- **UI-62** calc/calcout go INVALID when an UNUSED input link is broken
  (`calc.rs:93-98,406`; epics-base#823 open; upstream declined to
  change — existing sites rely on it).
- **UI-102** Trailing `#` comments parsed as command arguments, extra
  args silently dropped (`iocsh/mod.rs:79`, `registry.rs:579-606`;
  epics-base#414 open; faithful C parity).
- **UI-81** seq record: all-zero-delay link groups run synchronously
  under the record gate (C uses the callback task per group) —
  deliberate deviation, previously documented only in a code comment
  (`links.rs:2528-2578`; epics-base#784 context).

## Classification (per issue)

### CA (epics-base) — agent A
- #943 — NOT-PRESENT — server accepts libca's truncated scalar DBR_STRING put, matching upstream PR #944's corrected behavior; bounds-checked (`types/value.rs:249-266`, `server/tcp.rs:4337-4396`).
- #936 — NOT-PRESENT — teardown aborts tasks; blocking driver shuts the socket down before join (`blocking_io.rs:626-644`).
- #426 — NOT-PRESENT — circuit minor version set only by the TCP VERSION frame; zero-count requests gated on CA_V413(peer); unsolicited VERSION greeting on accept (`transport.rs:1995-1999`, `server/tcp.rs:1795-1802`).
- #402 — NOT-PRESENT — every reply datagram reseeds a VERSION placeholder patched at flush (`server/udp.rs:694-711`, `:936-948`).
- #266 — NOT-PRESENT — extended-header threshold matches libca implementation (`protocol.rs:699-737`); upstream resolved as spec-doc error.
- #488 — SAME-PROBLEM (partial) — UI-1.
- #455 — NOT-PRESENT — pre-1990 clock saturates to 0 (`types/codec.rs:48-53`); monotonic timers; no warning-printer spin.
- #477 — NOT-PRESENT — no 30 s exitWait analog; socket shutdown precedes the only I/O-thread join.
- #515 — NOT-APPLICABLE — no RSRV_SERVER_PORT export; our env parsing is full-width with range check (`runtime/env.rs:145-162`).
- #128 — NOT-PRESENT — port-scan garbage refused via ECA_DEFUNCT with no console noise, rate-limited (`server/tcp.rs:2045-2128`).
- #829 — PARITY — UI-2.
- #190 — NOT-APPLICABLE — upstream closed as local misconfiguration; same echo design as C (`transport.rs:2000-2016`).
- #223 — NOT-PRESENT — infallible chrono rendering with explicit fallback (`cli.rs:352-358`).
- #554 — NOT-PRESENT — loopback appended only when broadcast enumeration is empty, as upstream by-design (`client/mod.rs:4906-4928`).
- #209 — NOT-APPLICABLE — upstream closed as documented flush behavior; our client frames subscribe immediately (`transport.rs:1718-1760`).

### PVA (pvxs) — agent B
- #200 — SUSPECTED — UI-25.
- #193 — SAME-PROBLEM — UI-21 (server half) + UI-22 (client half); the CRIT+backtrace log itself is absent (refusal is quiet wire-parity, `tcp.rs:3441-3459`).
- #161 — SAME-PROBLEM — UI-20.
- #156 — NOT-PRESENT — parser accepts both dialects, normalizes identically (`pv_request.rs:1092-1176`); pvDataCPP getField/putField sections unsupported same as pvxs.
- #136 — NOT-PRESENT — read side never gates on TX watermark; stuck client evicted by send_timeout (`tcp.rs:3094-3117`) — post-4249885 design.
- #135 — NOT-PRESENT — DESTROY_CHANNEL removes channel + report entry through the single teardown owner (`tcp.rs:5701,5717`).
- #119 — NOT-PRESENT — every data-phase exit reports through RAII ExecFinishGuard → apply_exec_finish (`tcp.rs:1053-1141`).
- #93 — NOT-PRESENT — nameserver dial failure enters the same 10 s retry arm as disconnect (`search_engine.rs:2916-2984`).
- #84 — NOT-PRESENT — no persistent per-channel attempt counter; fresh Pending per search with regression test (`search_engine.rs:1514-1525`, `:4920-4980`).
- #32 — NOT-PRESENT — type cache stores decoded subtrees per 0xFD key, resolved in wire order in the reader task (`pvdata/encode.rs:760-774`, `decode.rs:362-391`).
- #192 — NOT-PRESENT — callbacks moved into owning task or Arc-cloned out of locks before invocation (`shared_pv.rs:725-1112`).
- #44 — NOT-PRESENT — pvput-rs special-cases enum_t via choices with integer fallback (`ops_v2.rs:5133-5227`).
- #46 — NOT-PRESENT for the cited defect (fix ported, `format.rs:1366-1374`); residue filed as UI-24.
- #87 — PARITY — UI-23.
- #55 — NOT-PRESENT — display.precision from PREC served (`record_instance.rs:2007-2028`, `pvif.rs:1279-1286`); adjacent nesting defect is CBUG-G1.
- #69 — NOT-PRESENT — scalar-vs-array keys on FTVL-pinned storage variant, not count (`pvif.rs:401-423`); NELM=1 waveform stays NTScalarArray (wire-visible divergence from C QSRV's max-count==1 rule; the stale doc comment at pvif.rs:496 now states the port's rule and the divergence — fixed in this fix phase).

### QSRV / pvalink / discovery (pvxs) — agent C (all clean)
- #148 — NOT-PRESENT — pruned-delta presence ≡ fixed isMarked(true,true) (`group.rs:702-707`, `encode.rs:2316-2374`); no-putorder warning also present (`group.rs:1640-1644`).
- #105 — NOT-PRESENT — `record._options.block` parsed; single-record PUT awaits put-notify completion (`channel.rs:350-368`, `:828-883`); group not waiting matches pvxs.
- #97 — NOT-PRESENT — DBE value-class mask keeps ARCHIVE|LOG; post-#98 event promotion (`channel.rs:69-77`, `monitor.rs:201-217`).
- #177 — NOT-PRESENT — time=true adopts userTag (`pvalink/link.rs:1441-1446`, `processing.rs:3214-3230`; regression test).
- #107 — NOT-PRESENT — no cached field reference; every read re-selects and derefs Union/Variant (`link.rs:1871-1904`).
- #187 — NOT-PRESENT — no update-seq barrier exists; cache written before scan trigger enqueue (`link.rs:428-437`, `integration.rs:1010-1104`).
- #152 — NOT-PRESENT — remote String into CHAR-array dest exempted from scalar parse, carried via CharArray (`record_trait.rs:3606-3610`, `value.rs:1168-1178`).
- #59 — NOT-PRESENT — self-trigger default applied silently, no per-group WARN (`group_config.rs:136-165`).
- #137 — NOT-PRESENT — pvput-rs JSON-parses bracketed values against the descriptor (pvAccessCPP pvput semantics, `ops_v2.rs:5530-5670`).
- #138 — NOT-APPLICABLE — upstream not-a-bug; our config carries auto_beacon with env override (`config/env.rs:740-757`).
- #139 — NOT-PRESENT — tools don't share the UDPManager refactor that broke mcast; origSrc/replyDest distinction ported (`udp.rs:1348-1385`, `pvxvct-rs.rs:71-79`).
- #31 — NOT-PRESENT — dedicated AF_INET lo-mcast socket, join failure degrades to local answering instead of black-holing (`loopback_mcast.rs:54-93`, `search_engine.rs:273-399`).

### DB links / events (epics-base) — agent D
- #906 — PARITY — UI-60.
- #868 — PARITY — UI-61.
- #867 — NOT-PRESENT — documented deviation keeps the incoming event on collapse (newest survives; `event_queue.rs:440-448`, module doc `:133-141`).
- #855 — NOT-PRESENT — calink opens exactly one native subscription (`resolver.rs:553`); the label-delivery gap is UI-63.
- #823 — PARITY — UI-62.
- #657 — NOT-PRESENT — link re-classification re-reads the stored string post-put; no late-installed lset to lie (`calcout.rs:1321-1354`).
- #692 — NOT-PRESENT — raw-soft mbbi converts INP through the ULong coercion owner; 0xFFFFFFFF preserved (`mbbi.rs:378-384`).
- #564 — NOT-PRESENT for the reported wrap (reproduced as upstream intends); adjacent gap is UI-65.
- #521 — NOT-PRESENT — matches C's current sdef/RVAL-as-index/65535 behavior (`mbbi.rs:183-215`, `mbbo.rs:198-312`).
- #183 — SAME-PROBLEM (variant) — UI-63.
- #567 — NOT-PRESENT — upstream fix d0cf47cd6 ported: MSS propagates stat+sevr+AMSG (`links.rs:198-219`).
- #442 — NOT-PRESENT — UTAG kept out of the timestamp as upstream decided; separate u64 carried through (`common_fields.rs:81-85`).
- #324 — NOT-PRESENT — no eventsRemaining-deferred flush; outbox drains independent of callbacks (`monitor.rs:52-66`).
- #423 — NOT-PRESENT — cancel is idempotent Drop, no flush-semaphore analog (`event_queue.rs:600-610`).
- #557 — NOT-PRESENT — upstream fix a4bc0db6e ported: CP target mid-process sets RPRO (`processing.rs:5158-5166`).
- #876 — SAME-PROBLEM (inverted: the C guard is missing) — UI-64.

### Record types (epics-base) — agent E
- #548 — SAME-PROBLEM — UI-80.
- #485 — NOT-PRESENT — SIZV clamped [16, 0x7fff] mirroring upstream e5b4829 (`lsi.rs:262`, `lso.rs:260`, `printf.rs:589`).
- #187 — NOT-PRESENT — PACT framework-owned; visited-set + PACT guards (`record_instance.rs:1330-1345`, `processing.rs:1359-1623`).
- #174 — NOT-APPLICABLE — upstream closed without change; port matches current C init-alarm behavior.
- #97 — NOT-PRESENT — aai init loads constant links only; compress reads are nuse-clamped Vec indexing (`compress.rs:497-514`).
- #9 — NOT-PRESENT — rem_euclid LIFO / modulo FIFO with the fixed read-out formula (`compress.rs:257-273`, `:504-510`).
- #258 — NOT-PRESENT — posts built after apply_timestamp; snapshot reads common.time at post time.
- #280 — NOT-PRESENT — one framework epilogue; single NORD post owner per path (`field_io.rs:426-461`).
- #361 — NOT-PRESENT — `%%` emits literal `%` (`printf.rs:235-238`).
- #555 — NOT-PRESENT — `%s` stringifies CharArray (deliberate improvement; upstream wontfix).
- #846 — NOT-APPLICABLE — open feature request; port matches current C (32 bit fields, no names).
- #874 — NOT-APPLICABLE — open enhancement; current MS/MSI/MSS/NMS semantics matched (`links.rs:198-219`).
- #784 — SUSPECTED/PARITY-deviation — UI-81.

### libcom / access security / iocsh (epics-base) — agent F
- #865 — NOT-APPLICABLE — no signal handler triggers an event (only SIG_IGN + tokio self-pipe).
- #495 — NOT-APPLICABLE — priority applied by the thread itself at band entry; no cross-thread cell.
- #667 — NOT-PRESENT for the reported stall; adjacent HIGH defect UI-100.
- #438 — NOT-PRESENT — no asCaTask worker; per-gate async resolver into the local DB.
- #328 — NOT-PRESENT — HAG resolves to IPv4 at parse and peer IP is the identity (`access_security.rs:1688-1719`, `server/tcp.rs:821`).
- #474 — NOT-PRESENT — typed immutable trap-write messages, Drop-handle unregister; all four upstream complaints structurally absent.
- #362 — NOT-PRESENT for the reported defect (in-quote escapes work; C still broken); adjacent divergence UI-103.
- #414 — PARITY — UI-102.
- #499 — SAME-PROBLEM (worse than C) — UI-101.
- #369 — NOT-APPLICABLE — no errPrintf port; single_line owns framing.
- #106 — NOT-PRESENT — monotonic deadlines, no quantum/2 adjustment (`delayed_timer.rs:94-163`).
- #241 — NOT-APPLICABLE — no foreign-thread OSD attach registry.
- #596 — NOT-PRESENT — stats are instance-owned atomics; pre-start reads are zeros (`ca_server.rs:780`).
- #709 — NOT-APPLICABLE — no epicsThreadOSD::attr analog.
- #718 — NOT-PRESENT — pool count usize clamped `.max(1)` (`callback_executor.rs:272-274`).
- #211 — SAME-PROBLEM (scoped to procserv-rs) — UI-104.

### epics-base uncovered delta (second sweep — agents G–J + inline)
- #498 — SAME-PROBLEM (db* family worse than current C) — UI-105.
- #785 — SAME-PROBLEM (port behind pvxs on the description half) — UI-106; the missing-prop(YES) half is parity.
- #863 — split verdict: CA half NOT-PRESENT — already closed as the UI-1 family deviation (60 s `refresh_dns` cadence, per-redial nameserver re-resolution; `search.rs:1040-1117`, `client/mod.rs:4838-4863`); AS half SAME-PROBLEM — UI-107.
- #666 — SAME-PROBLEM (silent zero-load `file` entry, pinned by test) — UI-108.
- #469 — PARITY-GAP — UI-109 (variable absent entirely; C's setter has the scope leak).
- #378 — NOT-PRESENT — the repeater-registered socket discards everything but beacons (`beacon_monitor.rs:546-559`) and the search socket never registers (`search.rs:626`), so a forwarded SEARCH request cannot reach the response parser. Caveat: the parse itself, like C, sanity-checks nothing (`search.rs:1784-1800`) — reachable only by direct unicast to the ephemeral search port.
- #372 — NOT-PRESENT — HashMap channel tables + 30-bucket retry ring touch only due entries per tick (`search.rs:112`, `:383-460`, `:1981-2049`).
- #576 — NOT-PRESENT — single-coordinator subscribe; registry insert precedes EVENT_ADD; every disconnect path flags `needs_restore`, every connect restores (`client/mod.rs:3458-3567`, `:3943-4033`, `:4408-4477`).
- #380 — NOT-PRESENT — `cmd_astac` is a stateless query over the loaded config; no client-registration surface exists to leak (`access_commands.rs:468-503`).
- #333 — NOT-PRESENT — no narrow-integer timestamp diff anywhere; camonitor's `secs_between` goes through `Duration` (`camonitor-rs.rs:695-702`), wire u32 widened before arithmetic (`codec.rs:47-54`).
- #368 — NOT-APPLICABLE — mingw-libc defect; the port formats in Rust with `%F`/`%lx` mapped (`printf.rs:248`, `:399-415`, `:510`).
- #549 — NOT-PRESENT — load errors are values printed once (`commands.rs:1062`, `softioc-rs.rs:340`); latent double-print only for an external binary that prints both the script-line echo and `run()`'s Err (`iocsh/mod.rs:366`, `ioc_app.rs:740`).
- #683 — NOT-APPLICABLE — no lockset machinery; per-record gates in a leaked static registry (`record_lock.rs:30-58`, `:244-255`).
- #529 — NOT-PRESENT — template resolver bypasses the include-path search only on `is_absolute()`, matching the current C fix (`substitution.rs:480-511`).
- #537 — NOT-APPLICABLE — open enhancement; `-m p` parsed but the subscription type stays TIME-class, matching current C camonitor (`camonitor-rs.rs:518-552`, `client/mod.rs:203-216`).
- #428 — NOT-PRESENT — modern dbParseLink parity including the CP/CPP discard warning (`link.rs:1153-1258`, `record_instance.rs:2872-2879`).
- #836 — NOT-PRESENT — no pool-space probe in either search-reply driver (`server/udp.rs:643-700`, `blocking.rs:707-724` records the deliberate admission design).
- #643 — NOT-PRESENT — filter chains are Arc-owned by channel/monitor objects; the only global is a leaked static; no teardown ordering exists to violate (`filters/mod.rs:119`, `qsrv/channel.rs:489`).
- #256 — NOT-PRESENT — parse errors propagate as Results; in-parse warnings go through the synchronous console subscriber; both end at line-atomic `eprintln!` from the loading thread (`db_loader/mod.rs:77-82`, `log.rs:106-108`).
- #911 — NOT-PRESENT — `erl_warning()` gates ANSI on isatty + non-empty TERM (`log.rs:332-340`); no unconditional escapes in port output.
- #858 — NOT-PRESENT — `on error wait` parses through a checked Result (`iocsh/mod.rs:534-539`); the C inversion has no counterpart.
- #761 — NOT-APPLICABLE — no cantProceed/epicsEventMustTrigger suspension model in the port.
- #416 — NOT-APPLICABLE — no errlog localEcho/atExit console gate; port logging is the tracing pipeline.
- #775 — NOT-APPLICABLE — unmerged upstream proposal (bi/bo ZRVL/ONVL); pre-adoption would be a DBD-visible deviation.

The remaining 91 uncovered open issues were title-triaged as
toolchain/build/CI/docs/platform items with no port surface.

## Review Log

- 2026-08-09 — initial sweep, 6 agents, ~80 issues. 17 findings: 2 HIGH
  (UI-100 asInit unwired, UI-20 byte-unbounded TX queue), 7 MED (UI-1,
  UI-21, UI-22, UI-63, UI-64, UI-101, UI-104), 8 LOW/PARITY. Thematic
  clusters: (1) "the port fixed the famous half and missed the sibling
  half" — UI-1 (ADDR_LIST fixed, NAME_SERVERS not), UI-63 (renderer
  exists, only sseq uses it), UI-100 (programmatic ACF path works, iocsh
  path unwired) — suggests auditing every dual-entry feature for the
  second entry point; (2) deliberate C-parity reproductions of open
  upstream warts were undocumented outside code comments (UI-2, UI-23,
  UI-60, UI-61, UI-62, UI-81, UI-102) — this doc now records them;
  (3) resource-exhaustion bounds denominated in the wrong unit (UI-20
  frames-vs-bytes).
- 2026-08-09 — second sweep over the 115 open epics-base issues the
  initial round only surface-classified: title triage → 24 candidates
  → 4 read-only agents + inline checks. 5 findings: UI-105..UI-109
  (2 MED, 3 LOW), 19 clean verdicts with evidence. Themes: (1) round
  1's "fixed half / missed sibling half" recurs — #863's CA DNS
  refresh was already shipped, its access-security sibling was not
  (UI-107); (2) signals dropped while porting the mechanism — error
  status to the shell (UI-105), the zero-load warning (UI-108), the
  startup-script env var (UI-109); (3) metadata leaf emitted but never
  produced (UI-106, empty display.description).
