# Parity Review 03 — types/ + server/db_loader/

Rust under review:
- `crates/epics-base-rs/src/types/{codec.rs,dbr.rs,value.rs,mod.rs}`
- `crates/epics-base-rs/src/server/db_loader/{mod.rs,include.rs}`

C reference:
- `modules/ca/src/client/db_access.h` (DBR struct layout)
- `modules/database/src/ioc/dbStatic/{dbLex.l,dbYacc.y,dbLexRoutines.c}`
- `modules/database/src/ioc/dbtemplate/{dbLoadTemplate.y,msi.cpp}`
- `modules/libcom/src/macLib/macCore.c` (`trans`, `refer`)
- `modules/libcom/src/misc/epicsString.c` (`epicsStrnRawFromEscaped`)

---

## CRITICAL

### C-1 — `DBR_STS_DOUBLE` RISC pad is 2 bytes, C uses 4

- Rust: `codec.rs:72` — `sts_pad(DbFieldType::Double) => &[0, 0]` (2 bytes)
- C: `db_access.h:233-238` `struct dbr_sts_double` — `RISC_pad` is `dbr_long_t`
  (`epicsInt32`, **4 bytes**), `db_access.h:45 typedef epicsInt32 dbr_long_t`.

The C struct layout for `DBR_STS_DOUBLE` (type 13) is:
`status(2) + severity(2) + RISC_pad(4) + value(double, 8)` — value at offset 8.
The Rust encoder/decoder produces `status(2) + severity(2) + pad(2) + value(8)` —
value at offset 6.

Both `serialize_sts` (`codec.rs:89`, used by `serialize_dbr`/`encode_dbr`) and
`decode_sts` (`codec.rs:598`, `val_off = 4 + pad_len`, line 602) consume
`sts_pad`, so encode and decode are self-consistent but **both wrong by 2 bytes
against C and against any real CA peer**.

Runtime impact: every `DBR_STS_DOUBLE` reply emitted by the CA server is 2 bytes
short and the double payload is shifted; a real `caget`/CA client parsing the
struct reads garbage (or a misaligned/truncated double). Inbound decode of a
correct `DBR_STS_DOUBLE` frame from a real IOC also misparses. Scalar +
waveform double PVs both affected on the STS layer. The inline comment on
`codec.rs:71-72` even argues itself into the wrong answer
("sts(4)+pad(2) = 6? No.").

Fix: `DbFieldType::Double => &[0, 0, 0, 0]`.

---

## HIGH

### H-1 — `dbr_buffer_size` reports wrong STS size for DOUBLE and CHAR

- Rust: `dbr.rs:168-170` — STS branch (`dbr_type / 7 == 1`) returns a flat
  `meta_size = 4` for all native types.
- C: `db_access.h` — `dbr_sts_char` (218-223) = `status(2)+severity(2)+RISC_pad(1)+value(1)`
  → meta 5; `dbr_sts_double` (233-238) = meta 8 (see C-1).

`dbr_buffer_size(DBR_STS_DOUBLE, Double, n)` returns `4 + 8*n`; C
`dbr_size_n` returns `16 + 8*(n-1)` i.e. `8 + 8*n`. `dbr_buffer_size(DBR_STS_CHAR, Char, n)`
returns `4 + n`; C returns `6 + (n-1)` i.e. `5 + n`.

Runtime impact: any caller that pre-allocates / bounds-checks a CA buffer from
`dbr_buffer_size` for STS_DOUBLE/STS_CHAR under-allocates or mis-bounds. No
in-tree caller was found (`rg dbr_buffer_size` finds only the definition), so
impact today is latent, but the function is `pub` and documented as the
`dbValueSize` equivalent. Note this is a *second, independent* defect from C-1:
even after fixing `sts_pad`, this hard-coded `4` stays wrong.

### H-2 — Quoted-string escape translation diverges from the C dbStatic lexer

- Rust: `mod.rs:374-396` `read_quoted_string` translates `\"`→`"`, `\\`→`\`,
  `\n`→newline; any other `\x` is kept as the 2-char sequence `\x`.
- C: `dbLex.l:90-93` — a `tokenSTRING` quoted string matches
  `{doublequote}({dqschar}|{escape})*{doublequote}` where `escape = {backslash}.`.
  The lexer **keeps the escape bytes raw** (`dbmfStrdup(yytext+1)`, only the
  surrounding quotes are stripped). For ordinary `field(...)` values
  (`dbLexRoutines.c:1398`) escape translation runs **only when the value still
  carries quotes**, which for plain `tokenSTRING` it does not — so a `field`
  value `"a\nb"` stays the literal 4 chars `a\nb` in C.

Runtime impact: a `.db` field value such as `field(DESC, "line1\nline2")`
becomes a string with an embedded newline in the Rust IOC but a literal
backslash-n in a C IOC. `\"` likewise: Rust stores `a"b`, C stores `a\"b`.
DESC / link / format-string fields silently differ between the two
implementations. (Note also the Rust escape set is inconsistent with itself:
`\t` is passed through verbatim while `\n` is translated.) If C-parity is the
goal, `read_quoted_string` should keep escapes raw for `.db` field/tag values.

### H-3 — Newline inside a quoted string is accepted; C aborts the parse

- Rust: `mod.rs:387-390` `read_quoted_string` explicitly handles `'\n'` inside
  a quoted string by pushing the newline and continuing.
- C: `dbLex.l:131-133` — `{doublequote}({dqschar}|{escape})*{newline}` →
  `yyerrorAbort("Newline in string, closing quote missing")`. An unterminated
  quoted string spanning a line break is a **hard parse error** in C.

Runtime impact: a malformed `.db` with a missing closing quote loads silently
in the Rust IOC (the rest of the file is swallowed into one giant field value)
where the C IOC would reject it with a clear error. Masks authoring bugs and
can pull subsequent records' text into a single field.

### H-4 — Cross-type array conversion collapses the whole array to one zero

- Rust: `value.rs:512-557` `convert_to` — documented "scalar only; arrays use
  first element", but `to_f64` (`value.rs:581-604`) has **no array arms**, so
  for any `*Array` variant `to_f64()` returns `None`.
- Path: `codec.rs:58-63` `convert_and_serialize` — when the requested DBR
  native type differs from the value's type, it calls `value.convert_to(native)`.
  For e.g. a `DoubleArray` PV requested as `DBR_SHORT`, `convert_to(Short)` →
  `Short(self.to_f64().unwrap_or(0.0) as i16)` → `Short(0)`.

C: `dbConvert` converts arrays element-by-element with the proper per-type
GET conversion routine.

Runtime impact: a CA client issuing a typed array GET (`DBR_SHORT` against a
native-`DOUBLE` waveform, etc.) receives a single scalar zero instead of the
converted N-element array. Wrong count *and* wrong data. Most server paths
serve the native type so this is an edge path, but it is a definite wrong
encoding when exercised.

---

## MEDIUM

### M-1 — Macro values are not re-expanded (no chained macro expansion)

- Rust: `mod.rs:290-291` — when a macro is found, `result.push_str(val)` inserts
  the value verbatim and parsing resumes at `i = j + 1`; the inserted text is
  never re-scanned for further `$(...)`.
- C: `macCore.c:refer` (790-895) — a found `MAC_ENTRY` is either the
  pre-expanded `value` or, when `dirty`, the raw value re-run through `trans`.
  Macro values that themselves contain `$(...)` are expanded.

Runtime impact: `dbLoadRecords` invoked with `P=$(Q),Q=IOC:` (or
nested-via-template macros) expands `$(P)` to the literal `$(Q)` in the Rust
IOC instead of `IOC:`. Only the *default-value* path is recursively expanded
(`mod.rs:302`); the resolved-macro path is not.

### M-2 — `$(name,sub=val,...)` scoped-macro syntax unsupported; comma in default mis-split

- Rust: `mod.rs:284-288` — `macro_content.find('=')` splits on the **first**
  `=` only; everything after is treated as the default value.
- C: `macCore.c:refer` (776, 819-859) — the name terminates at `=`, `,` or the
  close bracket (`macEnd = "=,)"`); a `,` introduces comma-separated **scoped
  macro definitions** (`$(NAME,A=1,B=2)` defines A and B for the duration of
  the reference).

Runtime impact: (a) the EPICS `$(name,key=val)` scoped-substitution form is not
honored — `A`/`B` are not defined and the inner reference is mis-resolved;
(b) a default value that legitimately contains a comma, e.g.
`$(LIST=a,b,c)`, is parsed by Rust with default `"a,b,c"` whereas C parses
name `LIST`, default `a`, then scoped `b` (no `=`) — different result. Either
way the two implementations disagree.

### M-3 — Macros expanded inside single quotes; C suppresses expansion there

- Rust: `mod.rs:234-317` `substitute_macros` has no quote tracking — `$(X)`
  inside `'...'` is expanded.
- C: `macCore.c:trans` (722-733) — single quotes set `quote='\''` and
  `macRef && quote != '\''` blocks expansion: **macros are not expanded inside
  single quotes**.

Runtime impact: JSON-style link values in `.db` files use single quotes
(`dbLex.l:32 jsonsqstr`). A `'$(X)'` literal intended to survive verbatim is
expanded by the Rust loader. Double-quoted values behave the same in both, so
impact is limited to single-quoted JSON link text.

### M-4 — `substitute` directive value splitting is not quote-aware

- Rust: `include.rs:100-104` — splits the directive payload on `,` then on the
  first `=`; no handling of quoted values.
- C: `msi.cpp` `addMacroReplacements` → `macParseDefns` performs quote-aware
  tokenizing of `name=value,name2=value2`.

Runtime impact: `substitute "MSG=a,b"` (a value containing a comma) is broken
by Rust into `MSG=a` plus a stray `b`. Quoted values with embedded `,` or `=`
are mis-parsed.

### M-5 — Macro reference name is not macro-expanded before lookup

- Rust: `mod.rs:284` — the name slice `&macro_content[..eq_pos]` (or whole
  content) is used for `macros.get(name)` without expansion.
- C: `macCore.c:refer:807` — `trans(... level+1 ...)` is run on the name first,
  so `$($(WHICH))` resolves the inner reference to build the name.

Runtime impact: indirection through a computed macro name (`$($(SEL))`) fails
to resolve in the Rust loader.

### M-6 — `DBR_STSACK_STRING` encodes but cannot decode

- Rust: `codec.rs:234-244` `encode_dbr` handles `DBR_STSACK_STRING` (type 37);
  `decode_dbr` (`codec.rs:585-595`) only matches `0..=34` and returns
  `CaError::UnsupportedType(37)`.
- C: `dbr_stsack_string` (`db_access.h:184-190`) is a normal readable struct.

Runtime impact: a client/forwarder that round-trips type 37 through
`decode_dbr` fails; the encode/decode pair is asymmetric. (Type 38
`DBR_CLASS_NAME` is correctly symmetric.)

---

## LOW

### L-1 — No `.substitutions` / `dbLoadTemplate` support

There is no Rust equivalent of `dbLoadTemplate` / msi substitution-file
parsing (`dbLoadTemplate.y`, `msi.cpp`): the `file {}` / `pattern {} {}` /
`global {}` substitution-file grammar is entirely absent. Only line-oriented
`substitute "..."` / `include "..."` directives inside `.db` files are
supported (`include.rs`). IOCs that load `.substitutions` files cannot be
ported as-is. Feature gap.

### L-2 — `.db` grammar coverage is record-only

`parse_db` (`mod.rs:94-232`) recognizes only `record` / `grecord`. The C
dbStatic grammar (`dbYacc.y`) additionally accepts top-level `path`,
`addpath`, `include`, `menu`, `recordtype`, `device`, `driver`, `link`,
`breaktable`, `registrar`, `function`, `variable`, and the **standalone
2-arg** `alias("record","newname")` (`dbYacc.y:275`). Any of these at file
scope makes `parse_db` fail with `expected 'record'`. In-record-body
`alias("name")` and `info(...)` are supported; the global `alias` form and
`path`/`addpath` directives are not.

### L-3 — `path` / `addpath` directives ignored for include resolution

`DbLoadConfig.include_paths` (`include.rs:11-14`) can only be set
programmatically. C `path "..."` / `addpath "..."` directives inside a file
mutate the search path (`dbLexRoutines.c:433-441`); the Rust loader does not
parse them, so a `.db` that sets its own include path won't resolve includes
the way a C IOC would.

### L-4 — Undefined-macro placeholder text differs from C

- Rust: `mod.rs:306` — an undefined macro with no default is emitted as the
  bare `$(name)`.
- C: `macCore.c:refer:905-919` — emits `$(name,undefined)` when warnings are
  enabled (or `$(name)` when suppressed), and sets the entry error flag /
  prints `macLib: macro X is undefined`.

Runtime impact: cosmetic for the expanded text in the suppressed case; the
Rust loader never raises the "undefined macro" diagnostic that C produces by
default, so genuinely-missing macros pass silently.

### L-5 — Unquoted field/info values accept characters the C lexer rejects

`read_field_value` (`mod.rs:410-432`) for an unquoted value reads everything up
to `,`/`)`, including spaces and arbitrary punctuation. The C lexer restricts
an unquoted `bareword` to `[a-zA-Z0-9_\-+:.\[\]<>;]` (`dbLex.l:21`); an
unquoted value with a space is two tokens / a lexer error in C. The Rust
parser is strictly more permissive — it will accept `.db` text a C IOC
rejects.

### L-6 — `info` command detection vs C msi

`parse_include_directive` / `parse_substitute_directive` (`include.rs:133-185`)
require a strict `starts_with("include"/"substitute")` prefix. C msi
(`msi.cpp:311-315`) uses `strstr` (substring match, last wins). The Rust
behavior is arguably more correct, but it is a documented divergence: a line
like `xinclude "f"` is an include in C msi and inert in Rust.

### L-7 — `resolve_menu_string` is a hard-coded subset of EPICS menus

`value.rs:611-673` resolves a fixed list of menu strings to indices. C resolves
menu strings against the actual menu definitions parsed from `.dbd`. Custom
record/menu choice strings (anything outside the hard-coded list) will not
resolve; `parse`/`convert_to` then fall back to an error or `0`. Feature gap;
relevant when loading `.db` field values that name menu choices for
non-built-in menus.

---

## Items verified correct (no finding)

- TIME-layer RISC pad: `time_pad` (`codec.rs:78-87`) — Short/Enum 2, Char 3,
  Double 4, Float/Long/String 0 — all match `dbr_time_*` in `db_access.h`
  (251-301).
- STS pad for Char: `sts_pad(Char) => &[0]` matches `dbr_sts_char` RISC_pad
  (`db_access.h:221`).
- GR/CTRL layouts for Short/Long/Float/Double/Char/Enum/String match the
  `dbr_gr_*` / `dbr_ctrl_*` structs (`db_access.h:309-517`), including the
  trailing 1-byte `RISC_pad` on GR/CTRL char and the precision+pad(2+2) prefix
  on float/double.
- `DBR_CLASS_NAME` (38): 40-byte fixed string, symmetric encode/decode, strict
  inbound length check (`codec.rs:25-33,205-213,567-583`).
- DBF_CHAR limits encoded/decoded as signed `epicsInt8`
  (`codec.rs:435-447,759-775`) and `Char` widening to f64 via `i8`
  (`value.rs:593`) — correct per epics-base treating DBF_CHAR as signed.
- DBR type-code constants and `native_type_for_dbr` ranges (`dbr.rs:9-224`)
  match `db_access.h`.
- `from_bytes_array` count=0 → typed empty array, count=1 → scalar
  (`value.rs:284-309`) and the allocation cap against a hostile `m_count`
  (`value.rs:310`) — correct hardening.
- Backslash-escapes-dollar in `substitute_macros` (`mod.rs:250-255`) matches
  `macCore.c:trans` level-0 semantics (`macLib.plt:52`).
