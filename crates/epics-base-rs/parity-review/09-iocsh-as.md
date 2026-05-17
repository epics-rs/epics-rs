# Parity Review 09 — iocsh + Access Security

Rust files reviewed:
- `crates/epics-base-rs/src/server/iocsh/mod.rs`
- `crates/epics-base-rs/src/server/iocsh/registry.rs`
- `crates/epics-base-rs/src/server/iocsh/commands.rs`
- `crates/epics-base-rs/src/server/access_security.rs`

C reference:
- `modules/libcom/src/iocsh/iocsh.cpp`, `registry.c`
- `modules/libcom/src/as/asLib.y`, `asLib_lex.l`, `asLibRoutines.c`, `asTrapWrite.c`, `asLib.h`
- `modules/database/src/ioc/db/dbIocRegister.c`, `modules/database/src/ioc/as/asIocRegister.c`

---

## CRITICAL

### C-1. Empty-rule ASG grants ReadWrite — C grants NoAccess (security bypass)

- Rust: `access_security.rs:391-393` (`check_access_method`)
- C: `asLibRoutines.c:983-1053` (`asComputePvt`)

The Rust code:
```rust
if asg.rules.is_empty() {
    return AccessLevel::ReadWrite;
}
```

C `asComputePvt` initializes `access = asNOACCESS` and only ever raises it
when a `RULE` matches. An ASG block declared with **no `RULE` statements**
therefore yields `asNOACCESS` for every client — i.e. all access denied.
The Rust port returns `ReadWrite` (full write access) for the identical
configuration.

Runtime impact: an operator who writes `ASG(LOCKED) { }` (or whose RULE
lines were stripped by a bad macro substitution) believes the group is
locked down. Under the Rust IOC every channel in that group becomes
world-writable. This is the opposite of the C decision and is a direct
write-access bypass.

### C-2. Unknown ASG with no `DEFAULT` grants ReadWrite — C falls back to `DEFAULT` (which is empty ⇒ NoAccess)

- Rust: `access_security.rs:380-386` (`check_access_method`)
- C: `asLibRoutines.c:893-928` (`asAddMemberPvt`), `asLibRoutines.c:107` (`asInitialize` always `asAsgAdd("DEFAULT")`)

Rust:
```rust
let asg = match self.asg.get(asg_name) {
    Some(a) => a,
    None => match self.asg.get("DEFAULT") {
        Some(a) => a,
        None => return AccessLevel::ReadWrite,   // <-- bypass
    },
};
```

In C, `asInitialize` *always* creates a `DEFAULT` ASG (line 107) before
parsing the ACF. A record whose `ASG` field names a group not present in
the file is silently reassigned to `DEFAULT` by `asAddMemberPvt`. If the
ACF never declares `DEFAULT` with rules, `DEFAULT` has an empty rule list
⇒ `asNOACCESS` ⇒ deny.

The Rust port: (a) `parse_acf` never synthesizes a `DEFAULT` group, and
(b) when neither the named ASG nor `DEFAULT` exists, it returns
`ReadWrite`. So a record with `field(ASG,"TYPO")` against an ACF that has
no `DEFAULT` becomes fully writable instead of fully denied.

### C-3. Parsing an empty / rule-less ACF file yields a permissive config

- Rust: `access_security.rs:466-509` (`parse_acf`) + `check_access_method:380-393`
- C: `asLibRoutines.c:88-171` (`asInitialize`)

When an operator loads an ACF file that is empty, contains only comments,
or only declares `UAG`/`HAG` with no `ASG`, the Rust `parse_acf` returns a
config with an empty `asg` map. Every subsequent check then hits C-2's
`return AccessLevel::ReadWrite`. In C the same file produces an active
security layer with only the auto-created empty `DEFAULT` ⇒ everything
denied. A site that loads a half-finished ACF gets the inverse of the C
fail-safe posture.

---

## HIGH

### H-1. HAG hostname matching is case-sensitive — C lowercases both sides

- Rust: `access_security.rs:410-416` (`host_match`, `m == host`), `expand_hag_members:527-559`
- C: `asLibRoutines.c:1218-1256` (`asHagAddHost`, `tolower` each char), `asLibRoutines.c:377-380` and `402-405` (`asAddClient`/`asChangeClient` lowercase the client host)

C stores every HAG host name lowercased (`phagname->host[i] = tolower(...)`)
and lowercases the connecting client's host before `asComputePvt`. Host
matching is therefore case-insensitive in C. The Rust port stores HAG
entries verbatim and compares with `m == host` (exact, case-sensitive).

Runtime impact: a HAG entry `HAG(lab){ LabPC1 }` will not match a peer
that reports `labpc1` (or vice versa). DNS / NetBIOS names routinely vary
in case, so a rule intended to *grant* write access silently fails to
match (operator loses access) and, more dangerously, a rule intended to
*restrict* by host can be evaded by presenting a different-cased hostname.
Wrong access decision either direction → HIGH.

### H-2. `TRAPWRITE` / `asTrapWrite` entirely missing

- Rust: `access_security.rs` — `AccessRule` (lines 319-336) has no `trap` field; no `asTrapWrite` machinery anywhere in the crate.
- C: `asLib.y:272-283` (`rule_log_option`), `asLibRoutines.c:1326-1331` (`asAsgAddRuleOptions`, `AS_TRAP_WRITE`), `asLib.h:46-62` (`asTrapWrite*` macros), `asTrapWrite.c`

The C ACF grammar accepts a per-RULE log option: `RULE(1,WRITE,TRAPWRITE)`
/ `RULE(1,WRITE,NOTRAPWRITE)`. A matching rule sets `pasgclient->trapMask`,
and every CA/PVA put on a trapped channel is routed through
`asTrapWriteBeforeWithData` / `asTrapWriteAfterWrite` so listeners (the
`caPutLog` audit logger) record who wrote what.

The Rust `parse_rule` (lines 682-768) parses only `level` and the
`READ`/`WRITE` keyword from `RULE(...)`; a third `TRAPWRITE` token in the
header is silently dropped (the `,` then `)` handling at lines 712-715
stops after the access word). There is no `trap`/`trapMask` field, no
`asTrapWrite` listener interface, and no put-logging hook.

Runtime impact: sites that rely on `caPutLog`-style write auditing get no
audit trail at all. Security-relevant feature gap; also a parse divergence
(the option is accepted-but-ignored rather than honored).

### H-3. `CALC` rule clause unsupported — rules silently lose their condition

- Rust: `access_security.rs:736-755` (`parse_rule` RULE-body loop)
- C: `asLib.y:294-299` (`tokenCALC` in `rule_list_item`), `asLibRoutines.c:1385-1424` (`asAsgRuleCalc`), `asLibRoutines.c:1038-1043` (CALC gating in `asComputePvt`)

C `RULE` bodies may contain `CALC("<expression>")`; the rule only grants
access when the expression (evaluated against the ASG's `INP*` link
values) returns 1. The Rust RULE-body loop handles `UAG`, `HAG`,
`METHOD`, `AUTHORITY` and explicitly states "Unknown alphanumeric keywords
are silently ignored" (lines 752-753). A `CALC(...)` clause is therefore
dropped: `read_word` reads `CALC`, no branch matches, the `(`...`)` is
consumed as "unknown punctuation" one char at a time.

Runtime impact: the conditional gate is lost. In C a rule like
`RULE(1,WRITE){ CALC("A=1") }` only grants write while input A==1; in Rust
the rule becomes unconditional `WRITE`. This *grants* access C would deny
whenever the calc condition is false → wrong access decision. (C, on a
truly unsupported keyword, calls `asAsgRuleDisable` — see `asLib.y:300-306`
— i.e. it *disables* the rule; Rust keeps it active. Rust does neither the
disable-on-unknown nor the CALC evaluation.)

### H-4. `INP(A..U)` ASG input links unsupported

- Rust: `access_security.rs:648-680` (`parse_asg_body`) — only `RULE` is recognized inside an ASG.
- C: `asLib.y:234-243` (`inp_config`: `tokenINP '(' tokenSTRING ')'`), `asLibRoutines.c:1297-1310` (`asAsgAddInp`)

An ASG body in C may declare `INPA("link")` ... `INPU("link")` — the
database links whose values feed `CALC` expressions. The Rust
`parse_asg_body` only matches the keyword `RULE`; any `INPA(...)` line is
swallowed by the `kw.is_empty()` "skip unknown char" branch (line 671-673).
Combined with H-3 this means the entire calc-based access-security feature
is non-functional. Feature gap that also changes ACF parse behavior.

### H-5. Several standard iocsh commands are missing

- Rust: `commands.rs:10-32` (`register_builtins`)
- C: `iocsh.cpp:1601-1614` (core), `libComRegister.c:483-514+`, `dbIocRegister.c`, `asIocRegister.c`

The Rust registry registers ~22 commands. Standard commands a real
`st.cmd` / operator session relies on that are **absent**:

Core iocsh (`iocsh.cpp` / `libComRegister.c`):
- `#` (comment command — registered so it shows in `help`)
- `iocshCmd`, `iocshRun` (single-command execution from vxWorks/RTEMS startup)
- `on` (`on error continue|break|halt|wait` — error-handling control)
- `var` (read/set registered iocsh variables)
- `chdir`, `pwd` (Rust has `pushd`/`popd`/`dirs` but not the base `cd`/`pwd`)
- `epicsEnvUnset`, `epicsEnvShow`, `epicsParamShow`, `epicsPrtEnvParams`
- `registryDump`
- `errlog`, `eltc`, `iocLogInit`, `iocLogShow`, `iocLogPrefix`
- `epicsThreadSleep`, `epicsThreadShowAll`, `thread`, `epicsMutexShowAll`
- `date`, `echo`

Database (`dbIocRegister.c`):
- `dba`, `dbap`, `dbstat`, `dbnr`, `dbli`, `dbla`, `dbcar`, `dbjlr`,
  `dbel`, `dbtr`, `dbtgf`, `dbtpf`, `dbior`, `dbhcr`, `gft`, `pft`,
  `tpn`, `dbtpn`, `dblsr`, `dbPutAttribute`, `dbNotifyDump`, `dbb`,
  `dbd`, `dbc`, `dbs`
- `scanpel`, `scanpiol`, `postEvent`, `scanOnceSetQueueSize`,
  `callbackSetQueueSize`, `dbStateCreate`/`Set`/`Clear`/`Show`

Access security (`asIocRegister.c`):
- `asSetFilename`, `asSetSubstitutions`, `asInit`, `asdbdump`,
  `aspuag`, `asphag`, `asprules`, `aspmem`, `astac`, `ascar`,
  `asDumpHash` — **none** are registered. There is no way to load or
  inspect an ACF from the Rust iocsh at all.

Runtime impact: an unmodified production `st.cmd` will error out on the
first unknown command (`execute_command_inner` returns
`Err("unknown command")`). The complete absence of the `as*` command
family means access security cannot be enabled through the shell.

### H-6. `dbsr` is implemented with the wrong semantics

- Rust: `commands.rs:487-506` (`cmd_dbsr` → `dbsr_handler`)
- C: `dbIocRegister.c:142-144` — `dbsr` = "Database Server Report"
  (prints CA server status and connected-client count).

In C `dbsr` reports the **CA server** state. The Rust `dbsr` is a
record-name glob search (`dbsr_handler`). The record-name glob command in
C is `dbgrep` / `dbglob` (`dbIocRegister.c:245-257`). The Rust port maps
all three names (`dbsr`, `dbglob`, `dbgrep`) to the same name-search
handler, so `dbsr` produces a record list instead of server status.
Operator-visible wrong behavior.

---

## MEDIUM

### M-1. Single-quote (`'`) quoting not supported in the tokenizer

- Rust: `registry.rs:279-404` (`split_comma_args`, `split_space_args` — only `"` toggles quote state)
- C: `iocsh.cpp:307-348` (`split()` — `if ((c == '"') || (c == '\'')) quote = c;`)

C `iocsh` accepts both double and single quotes as string delimiters;
`quote` is set to whichever opening char was seen and only the matching
char closes it. The Rust tokenizer treats `'` as an ordinary character.
A command line such as `dbpf REC:VAL 'hello world'` tokenizes in C as
two args (`REC:VAL`, `hello world`) but in Rust as three
(`REC:VAL`, `'hello`, `world'`). Parse divergence on a documented syntax.

### M-2. `RULE` level parse: missing/garbage level silently defaults

- Rust: `access_security.rs:688-699` (`parse_rule`) — `level_str.parse().unwrap_or(1)`
- C: `asLib.y:253-258` — the grammar requires `tokenINT64` and rejects a
  negative level with `yyerror` ("RULE: LEVEL must be positive"); a
  non-numeric token is a syntax error that fails the whole parse.

Rust reads only ASCII digits; if the level field is empty or malformed it
silently becomes `1`. A `RULE` with a typo'd level that C would reject
(failing the ACF load, which fails safe) is accepted by Rust with
`level = 1`. Edge-case divergence that can change which rules apply to a
given record ASL.

### M-3. `RULE` access keyword: anything not `WRITE` is treated as READ

- Rust: `access_security.rs:709-710` — `let write = access_str.eq_ignore_ascii_case("WRITE");`
- C: `asLib.y:259-267` — explicit `NONE` → `asNOACCESS`, `READ` → `asREAD`,
  `WRITE` → `asWRITE`; anything else triggers
  `yywarn("Ignoring RULE that contains an unsupported keyword")`.

Rust collapses the three-way C enum into a bool: `NONE` and any garbage
keyword become a READ-granting rule. C's `RULE(0,NONE)` explicitly grants
`asNOACCESS`. Because `asComputePvt` raises `access` monotonically a
`NONE` rule in C is effectively inert (it can't lower access), so the
practical risk is low — but a `RULE(0,NONE)` in Rust *grants READ* where
C grants nothing, and a misspelled keyword (`WRIET`) becomes a READ rule
instead of being warned-and-ignored. Edge-case wrong decision.

### M-4. ACF case-sensitivity / keyword recognition is stricter than C lexer

- Rust: `access_security.rs:482-506` (`parse_acf`) — matches `"UAG"`,
  `"HAG"`, `"ASG"` exactly; any other top-level word is a hard
  `Err("unexpected keyword")`.
- C: `asLib_lex.l:41-45` — keyword tokens; `asLib.y:88-103` — an
  unrecognized `tokenSTRING` at top level produces a *warning*
  (`yywarn "Ignoring unsupported TOP LEVEL block"`) and parsing
  continues.

C tolerates unknown top-level blocks (forward-compat); Rust aborts the
whole ACF parse. A future-/vendor-extended ACF that loads fine on a C IOC
fails to load entirely on the Rust IOC — and per C-3 a failed load that is
not caught leaves the IOC permissive. Behavioral divergence.

### M-5. ACF comment handling differs — `#` only as line start vs anywhere

This one is actually OK in Rust (`skip_ws_comments` treats `#` anywhere as
a line comment, matching `asLib_lex.l:94` `{comment}.*`). No defect — noted
to show it was checked.

### M-6. iocsh `on error` semantics absent

- Rust: `mod.rs:190-211` (`execute_script`) records `last_err` and keeps
  going, always returning `Err` at end if any line failed.
- C: `iocsh.cpp:1127-1149`, `1248`, `onCallFunc:1525-1577` — the `on`
  command lets a script choose `continue` (default), `break` (stop on
  first error), `halt`, or `wait <delay>`.

Rust hardcodes "continue, report at end". A script that does
`on error break` to abort on the first failure has no equivalent — and
since `on` is unregistered (H-5) the line itself errors. Behavioral gap.

---

## LOW

### L-1. `?` glob wildcard matches exactly one char; C `dbglob` matches "0 or one"

- Rust: `commands.rs:777-782` (`glob_match`) — `?` consumes exactly one char.
- C: `dbIocRegister.c:246-248` help text — `"?", which matches 0 or one characters`.

`dbglob "REC?"` in C matches both `REC` and `RECX`; in Rust only `RECX`.
Minor pattern-match divergence.

### L-2. `post_event` command name mismatch

- Rust: `commands.rs:736-747` registers `post_event`.
- C: `dbIocRegister.c` registers `postEvent` (camelCase).

A script calling the documented `postEvent` hits "unknown command".
(Subsumed by H-5 but called out as a naming bug.)

### L-3. fd-numbered redirection only plumbs stdout

- Rust: `mod.rs:89-124`, `407-475` — `2>file` is parsed but the capture
  still routes to the stdout sink (a diagnostic is printed).
- C: `iocsh.cpp:401-451` — `startRedirect`/`stopRedirect` actually swap
  `epicsSetThreadStderr` / `epicsSetThreadStdin` for fds 0/1/2.

`2>/dev/null` in a Rust iocsh script does not actually suppress stderr.
Self-documented as a known approximation; feature gap.

### L-4. `read_paren_name` silently strips internal whitespace and accepts unterminated input loosely

- Rust: `access_security.rs:590-608` — drops any whitespace inside the
  parens and never errors on EOF-before-`)` (the `while let` just ends).
- C: lexer requires a `tokenSTRING` then `')'`; malformed input is a parse
  error.

`UAG(my group)` becomes the name `mygroup` in Rust instead of failing.
Minor robustness divergence.

### L-5. iocsh tokenizer does not reject unbalanced quotes / trailing backslash

- Rust: `registry.rs:358-404` — an unterminated `"` just consumes to EOL;
  a trailing `\` is handled only at the line-continuation stage.
- C: `iocsh.cpp:362-371` — `split()` explicitly reports
  `"Unbalanced quote."` and `"Trailing backslash."` and marks the line
  errored.

Rust silently accepts malformed lines that C flags. Diagnostic gap.

---

## Summary of counts

- Critical: 3
- High: 6
- Medium: 5 (M-5 is a no-defect note → 4 real)
- Low: 5

## Worst findings (priority order)

1. **C-1 / C-2 / C-3 (Critical):** the Rust access-security check fails
   *open* in three independent places — an ASG with no rules, an unknown
   ASG with no `DEFAULT`, and an empty/rule-less ACF file all return
   `AccessLevel::ReadWrite`. C fails *closed* (`asNOACCESS`) in every one
   of these cases because `asComputePvt` only ever raises access from
   `asNOACCESS` and `asInitialize` always creates an empty `DEFAULT`.
   Any misconfiguration or partial ACF makes the Rust IOC world-writable.
2. **H-1 (High):** HAG host matching is case-sensitive; C lowercases both
   the stored host and the client host, so case-varying hostnames either
   lose intended write access or evade intended host restrictions.
3. **H-2/H-3/H-4 (High):** `TRAPWRITE` write-auditing, `CALC` conditional
   rules, and `INP(A..U)` link inputs are completely unimplemented — and
   `CALC` clauses are silently dropped, turning a conditional WRITE rule
   into an unconditional one (grants access C would deny).
4. **H-5 (High):** the entire `as*` iocsh command family
   (`asSetFilename`, `asInit`, `aspuag`, ...) plus dozens of standard
   `db*`/core commands are unregistered, so a stock `st.cmd` errors out
   and access security cannot be loaded from the shell at all.
