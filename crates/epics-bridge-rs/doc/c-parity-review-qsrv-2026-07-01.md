# epics-bridge-rs `qsrv` — Codex C-Parity Review — 2026-07-01

Scope: `crates/epics-bridge-rs/src/qsrv/` only (the Record ↔ pvAccess
bridge / QSRV2). Reference implementation is the pvxs IOC PVA server at
`~/codes/epics-modules/pvxs/ioc/` (group/single sources, `iocsource`,
`typeutils`, subscription contexts) with `~/codes/epics-modules/pvxs/src/nt.cpp`
for NT type layout. The `qsrv/mod.rs` doc-comment cites `pva2pva/pdbApp` but
every real divergence maps to `pvxs/ioc/*.cpp` (this port tracks QSRV2).

Methodology: **Codex-style C-parity sweep** — the first such sweep of this
subsystem. `qsrv` had prior *Rust-side reviewer* rounds to convergence
(finding IDs BR-R13..R65, "Round 5"/"Round 6", BRIDGE-RS-2026-05-28-NN, and
the four `doc/` review files) but never the Codex negative-space methodology
(C call-graph routing, silent-failure paths, wire-byte count/shape parity,
test-comment skepticism). This round is that pass. Findings already covered by
the prior rounds are excluded.

Round 1 ran five parallel opus auditors by category:
A = group config parse/definition, B = value marshalling / pvif / NT type,
C = single-record channel / provider / access-security, D = monitor / dbEvent
subscription, E = group get/put / trap-write. IDs are range-assigned per
category (A: Q1–Q12, B: Q13–Q24, C: Q25–Q36, D: Q37–Q48, E: Q49–Q60);
unused slots are gaps, not omissions.

## Open Findings

### Q1: Malformed group field-name path silently normalized, not rejected
Severity: Medium — CLEARED
Resolution: the pvxs `FieldName` grammar is now enforced at group-build time
from a single canonical parser. `group.rs::parse_field_path_checked` is the one
source of truth for the grammar and returns `Err` on pvxs's throw set
(empty leading/interior component; a `]`-terminated component with no `[` or a
non-integer subscript) PLUS three intentionally-stricter subscript rejections
(see the divergence note below), while dropping a single trailing `.` at EOF
like `getline`. The infallible `parse_field_path` (navigation) is now a thin
`parse_field_path_checked(..).unwrap_or_default()` wrapper, so build-time
validation and runtime navigation can never diverge (this also fixes the
navigation-side normalization: `value[x]`→`value`, `value[`→`value`). The
config layer calls it via `group_config::validate_field_name` at the top of
`parse_member`, so a malformed member fails `parse_member` →
`raw_to_group_def`, whose caller already skips just that group
(`tracing::warn!("ignoring invalid QSRV group")`) and preserves siblings —
matching pvxs's per-group `try` (`groupconfigprocessor.cpp:431-446`). The false
`group.rs` comment claiming the old `.filter(!is_empty)` "matches pvxs
validation (fieldname.cpp:35-36)" is removed. Degenerate divergences documented
(intentional, stricter-than-pvxs; we do NOT replicate the `strtol` accidents):
the empty subscript `a[]` (pvxs `strtol("]")` reads element 0), a
whitespace/sign-padded subscript `a[ 5]` (pvxs `strtol` skips leading ws → 5),
and a negative/`u32`-overflowing subscript (pvxs `strtol` accepts then fails at
navigation) are all rejected at build. Group indices are non-negative and
bounded, so none could navigate to a real element and none touches a real
config. Round-2 verify (config panel) confirmed the earlier
"exactly pvxs's throw set" claim was false — `a[]`/`a[ N]` were undocumented
stricter rejections; they are now documented, not silent. Regression: `group.rs::parse_field_path_checked_{empty_component,
bad_subscript,empty_ok}`, `group_config.rs::{malformed_member_field_name_skips_
only_that_group,trailing_dot_member_name_is_accepted}`.
Rust: `crates/epics-bridge-rs/src/qsrv/group.rs:44-50` (`parse_field_path`);
`group_config.rs:882-890` (`parse_member` stores the field name verbatim, no
grammar check).
C ref: `pvxs/ioc/fieldname.cpp:35-36` and `:41-53` (invoked from
`field.cpp:21` `fieldName(def.name)`, inside the `createGroups` per-group
`try` at `groupconfigprocessor.cpp:431-446`).
Impact: pvxs's `FieldName` ctor **throws** on an empty path component
(`value..index`, leading/trailing dot) and on a malformed array subscript
(`value[x]`, `value[`), aborting that group's build (logged, group not
served). Rust's `parse_field_path` does `.split('.').filter(!is_empty)`
(silently drops empty components) and `strip_suffix(']').and_then(parse::<u32>().ok())`
(a bad subscript yields `index=None`, discarding `[x]` and renaming
`value[x]`→`value`). A config typo pvxs refuses is served by Rust as a
well-formed-but-different structure. The `group.rs:37-38` comment claiming
this "matches pvxs validation (fieldname.cpp:35-36)" is false — that C line
throws, it does not filter.

### Q2: Member-level `+id`/`+putorder`/`+channel` accept only their canonical JSON type
Severity: Low — CLEARED
Resolution: every member-level scalar annotation now coerces through the same
`as<T>()`-equivalent helpers the group-level fix uses. `parse_member` routes
`+type`/`+channel`/`+trigger`/`+id` through `json_value_as_string` (bool/number
→ string) and `+putorder` through the new `json_value_as_i64` (bool/real/string
→ int, whose string branch is `parse_stoll_base0` mirroring pvxs
`parseTo<int64_t>` = `std::stoll(s,_,0)`: base auto-detect, sign,
leading/trailing whitespace). So `+id:5`→`"5"`, `+putorder:"2"`→`2`,
numeric `+channel`→`"5"` all coerce (pvxs behavior) instead of being silently
dropped; a `bool`/`number` `+type` coerces then warns-and-defaults to Scalar.
A present-but-non-coercible value (array/object/unparsable string) returns
`Err` → this group is dropped.
Divergence correction (round-2 verify, config panel): the earlier claim that
this per-group drop "matches pvxs's `NoConvert` throw" is FALSE — pvxs's
`as<T>()` NoConvert is raised inside a yajl SAX callback, and its wrapper
`yajlProcess` (groupconfigprocessor.cpp:1048-1059) returns -1, which yajl's
`_CC_CHK` (`libcom/yajl/yajl_parser.c:175-181`, cancels only on a FALSY 0) does
NOT treat as a cancel. pvxs therefore SWALLOWS the coercion error and serves
the group with the annotation defaulted (and for an array `+id:[1,2]`,
decomposed into scalar callbacks with no array boundary, keeps the FIRST
element `structureId="1"`). Rust's hard per-group skip is INTENTIONALLY
stricter — we surface the malformed annotation as a dropped group rather than
copy pvxs's swallow-and-serve (a `_CC_CHK` return-value bug), per the campaign
rule to match pvxs without copying its bugs. Degenerate-config-only. All the
in-source "NoConvert → per-group skip matching pvxs" comments and the
`config_bad_*`/`member_noncoercible_*` test comments are corrected to describe
this stricter divergence; the coercion values themselves (the actual Q2
finding) are unchanged and correct.
Defect-family: the finding cited `+id`/`+putorder`/`+channel`; `+type` and
`+trigger` were the same-anchor member-level `.as_str()` reads and are fixed in
the same pass. Regression: `parse_stoll_base0_matches_pvxs`,
`member_putorder_coerces_numeric_string`, `member_id_and_channel_coerce_numeric`,
`member_noncoercible_annotation_skips_only_that_group`.
Rust: `crates/epics-bridge-rs/src/qsrv/group_config.rs:877-880` (member `+id`
via `.as_str()`), `:872-875` (`+putorder` via `.as_i64()`).
C ref: `pvxs/ioc/groupprocessorcontext.cpp:66-82` (`assign` depth==3:
`+channel`/`+id`/`+trigger` = `value.as<std::string>()`, `+putorder` =
`value.as<int64_t>()`).
Impact: pvxs coerces every field-level scalar annotation through
`Value::as<T>()` (bool/int/double/string all convert) — the same rule the
prior reviewers applied to the *group-level* `+atomic`/`+id`
(`json_value_as_bool`/`json_value_as_string`) — but the *member-level*
annotations still use type-exact serde accessors. A `+id:5` gives struct-id
`"5"` in pvxs but `None` in Rust; a `+putorder:"2"` coerces to 2 in pvxs but
`None` in Rust (silently dropping the member from PUT ordering); a numeric
`+channel` builds `"5"` in pvxs but trips Rust's "missing +channel" and drops
the whole group. Low real-world hit rate (non-canonical inputs), but a genuine
missed sibling of the group-level coercion fix.

### Q13: NTScalar/NTScalarArray metadata sub-structs carry non-empty type-IDs pvxs leaves anonymous
Severity: Medium — CLEARED (39e7f9e3)
Rust: `crates/epics-bridge-rs/src/qsrv/pvif.rs:801` (`build_display` →
`PvStructure::new("display_t")`), `:832` (`build_control` → `"control_t"`),
`:924` (`build_value_alarm` → `"valueAlarm_t"`); mirrored in descriptor
builders `display_desc`/`control_desc`/`value_alarm_desc` (`:898`/`:905`/`:970`).
C ref: `pvxs/src/nt.cpp:60` (`Struct("display", {…})`), `:89`
(`Struct("control", {…})`), `:99` (`Struct("valueAlarm", {…})`) — all the
2-arg `members::Struct(name, children)` form, which per `src/pvxs/data.h:296-304`
sets `id = std::string()` (empty). Only `alarm`/`timeStamp`/`form`/NTEnum-`value`
use the 3-arg form with an explicit id; `pvif.rs` matches those correctly.
Impact: pvxs emits `display`/`control`/`valueAlarm` as anonymous structs
(id = ""); Rust advertises `display_t`/`control_t`/`valueAlarm_t`.
`encode_structure_body` (`encode.rs:413`) serializes the id as a
length-prefixed string, so every NTScalar/NTScalarArray introspection carries
extra bytes and a different type-id than pvxs. Values still decode, but
byte-exact introspection parity breaks and any client/gateway keying a type
cache on the sub-struct id sees a mismatch. NTEnum `display` already correct
(empty id at `pvif.rs:330`/`615`) — defect confined to the NTScalar family.
Resolution: all 6 qsrv sites (`build_display`/`build_control`/`build_value_alarm`
+ the `display_desc`/`control_desc`/`value_alarm_desc` descriptors) set an empty
id. Defect-family search found the identical bug in the PVA native source
(`crates/epics-pva-rs/src/server/native_source.rs`), whose NTScalar builders and
descriptors carried the same `display_t`/`control_t`/`valueAlarm_t` ids — fixed
in the same commit (6 sites). `timeStamp`/`alarm`/NTEnum-`value` keep their ids
(3-arg form, correct). No test asserted the old ids; typed_nt descriptor tests
and qsrv introspection tests stay green.

### Q14: `FTVL=UCHAR` waveforms served as signed `byte[]` (Int8) instead of pvxs `ubyte[]` (UInt8)
Severity: High — CLEARED (11c8798e)

Resolution: added first-class `DbFieldType::UChar` (=11, promotes to DBR_CHAR
over CA per `db_convert.h`) and `EpicsValue::{UChar(u8), UCharArray(Vec<u8>)}`,
mirroring the DBF_USHORT/DBF_ULONG precedent (1cdd4319) but with UChar's
divergent semantics: Char-like over CA (DBR_CHAR, raw 1-byte wire), unsigned
`ubyte` over PVA (pvxs `typeutils.cpp:34-35`), and unsigned-numeric in the
value accessors (INCLUDED in `as_f64/as_int_i64/as_*_array`, unlike the signed
CHAR carrier's string special-cases). Root cause closed at
`waveform.rs` `new()`/`reallocate_val`/`resize_val_preserving`/`put_field`
(FTVL=UCHAR index 2 → `UCharArray`, split from the `1 | 2 => CharArray`
collapse); the qsrv boundary at `pvif.rs` `value_scalar_type`
(`UChar/UCharArray → ScalarType::UByte`), `nt_type_for_field`, `is_empty_array`;
`convert.rs` `epics_to_scalar`/`epics_to_pv_field`/`scalar_to_epics_typed`
(UByte array explicit, no scalar-collapse), plus the context-free
`scalar_to_epics` `UByte → UChar` (was widen-to-Short workaround, now the exact
unsigned-8 carrier, restoring round-trip symmetry — see commit). Threaded
through `codec.rs` (DBR_CHAR wire layout), `value.rs` (Display unsigned,
`dbr_type→Char`, `db_field_type→UChar`), `link_status.rs`, `iocsh`, `autosave`,
`menu_choices.rs`, and the CA/PVA/motor consumers. Tests: `q14_*` in
`convert.rs` + `pvif.rs` (element 200 stays 200, not −56).

Distinct siblings NOT in this fix (separate findings): (a) `waveform.rs`
`reallocate_val` still collapses `3 | 4 => ShortArray` / `5 | 6 => LongArray`
for USHORT/ULONG (a 1cdd4319 record-layer gap); (b) `native_source.rs`
CHAR → UByte (should be Byte/Int8 per pvxs `typeutils.cpp:32-33`) — now
tracked and CLEARED as Q52 (de500251).
Rust: `crates/epics-bridge-rs/src/qsrv/pvif.rs:159` (`value_scalar_type`:
`CharArray => ScalarType::Byte`, no UInt8 arm); `crates/epics-bridge-rs/src/convert.rs:9-25`
(`dbf_to_scalar_type` has no `UChar` arm — `DbFieldType` lacks the variant,
`dbr.rs:73-104`). Root cause: `FTVL=UCHAR` (menuFtype index 2) is allocated as
`EpicsValue::CharArray` at `crates/epics-base-rs/src/server/records/waveform.rs:242`
(`1 | 2 => CharArray`), collapsing UCHAR into the signed CHAR carrier before
marshalling.
C ref: `pvxs/ioc/typeutils.cpp:34-35` (`case DBR_UCHAR: return TypeCode::UInt8;`)
vs `:32-33` (`DBR_CHAR → Int8`). pvxs keeps CHAR and UCHAR distinct.
Impact: a `waveform`/`aai`/`aao` with `FTVL=UCHAR` (the common byte/image-buffer
shape) is advertised over PVA as signed `byte[]` with signed display/control
limits, where pvxs advertises `ubyte[]` (UInt8). Introspection typecode differs
and every element ≥128 is reinterpreted (pixel 200 → −56). Data-fidelity
divergence on the wire, not just metadata. Fix spans `DbFieldType` +
`waveform.rs` allocation + `convert`/`pvif` mapping (sibling of the modeled
DBF_USHORT/DBF_ULONG work, commit 1cdd4319, which omitted UCHAR).

### Q15: `alarm.message` never reflects a record AMSG string
Severity: Low — DEFERRED (latent; blocked on AMSG modeling in epics-base-rs)
Disposition: no qsrv divergence exists for any state Rust can currently
represent — `AlarmInfo` has no `amsg` carrier, so `build_alarm` can only emit the
condition string, which is exactly C's fallback when `meta.amsg` is empty
(`iocsource.cpp:230-237`). The finding is a forward-looking marker: when AMSG is
modeled end-to-end in `epics-base-rs` (an `amsg` field on `AlarmInfo` fed by a
`recGblSetSevrMsg` equivalent, EPICS ≥7.0.6), `build_alarm` must prefer it under
`DBR_AMSG`. That is a base-rs record-layer feature, not a qsrv boundary fix —
outside this gap's scope. Tracked here so the qsrv `build_alarm` prefer-AMSG
branch is added at the same time. No action in the qsrv batch.
Rust: `crates/epics-bridge-rs/src/qsrv/pvif.rs:674-681` (`build_alarm`
unconditionally sets `message = alarm_condition_string(status)`); source
`AlarmInfo` (`crates/epics-base-rs/src/server/snapshot.rs:5-14`) has only
`status`/`severity`/`ackt`/`acks` — no `amsg`.
C ref: `pvxs/ioc/iocsource.cpp:230-237` — C prefers the record's alarm-message
string when present (`if((options & DBR_AMSG) && meta.amsg[0]) node["alarm.message"] = meta.amsg; else … condition string`).
Impact: pvxs sets `alarm.message` to the record AMSG override (EPICS ≥7.0.6,
e.g. `recGblSetSevrMsg`) when non-empty, else the condition string. Rust always
emits the condition string. The condition-string fallback itself matches C
exactly, so this is currently **latent** — `AlarmInfo` cannot carry a custom
message — but when AMSG is modeled, `build_alarm` must prefer it. Deferred:
closing requires modeling AMSG end-to-end.

### Q25: DISP `S_db_putDisabled` (and SPC_NOMOD read-only) gate bypassed for `process=true`/`process=false` single-record puts
Severity: Blocker — CLEARED (56cc44e4)
Resolution: a shared `PvDatabase::check_external_put_preconditions` gate
(DISP≠DISP-field → PutDisabled; read-only field → ReadOnlyField; silent on
missing record so the downstream put reports not-found inside its own
`asTrapWrite` bracket) now runs at the top of `channel.rs::put_with_options`
before the ACF grant — mirroring C's `doPreProcessing` order — so all three
process modes enforce it. Regression:
`testqsingle.rs::disp_disabled_record_rejects_put_in_every_process_mode`.
Rust: `crates/epics-bridge-rs/src/qsrv/channel.rs:688` (Inhibit `put_pv`) and
`:722` (Force `put_pv`).
C ref: `pvxs/ioc/iocsource.cpp:367` (`doPreProcessing`: `precord->disp &&
pfield != &precord->disp` → `S_db_putDisabled`), invoked from
`singlesource.cpp:356` before the `doWait` branch.
Impact: C runs `doPreProcessing` on **every** put regardless of process mode,
rejecting any write to a `DISP=1` record (except the DISP field itself).
Rust's `put_with_options` routes `ProcessMode::Passive` through
`put_record_field_from_ca*` (which enforces DISP at `field_io.rs:665`) but
routes `Inhibit` and `Force` through `put_pv`/`put_pv_inner`, which has no DISP
gate (and no SPC_NOMOD read-only gate). A PVA client sending
`record._options.process=true`/`=false` to a record an operator froze with
`DISP=1` writes/processes it anyway — a safety-interlock bypass wire-reachable
via a standard pvRequest option. Family: shared with Q49 (group path). The
prior review confirmed three-way routing "faithful"
(`crates/epics-bridge-rs/doc/pvxs-functional-security-review-2026-05-18.md:43`) but not that the DISP
precondition applies to all three routes.

### Q26: Read access-security enforced on GET/MONITOR, which pvxs QSRV2 single-source never applies
Severity: Medium — CLEARED (disposition: KEEP the stricter Rust behavior; user sign-off 2026-07-02)
Disposition: KEEP Rust's `can_read` gate on group/single GET and monitor-create;
no code change. Read access-security is a legitimate, long-standing EPICS control
— classic RSRV (the CA server) enforces it via `asCheckGet`
(`epics-base/modules/database/src/ioc/rsrv/camessage.c`), and an `.acf` author
writing a READ rule reasonably expects it to be honored on the pvAccess side
too. pvxs QSRV2 omitting the read check (verified: `SecurityClient` exposes only
`canWrite`/`asCheckPut`; no `canRead`/`asCheckGet` anywhere in `pvxs/ioc/`) is a
security gap, not a behavior to copy — the campaign's rule is "find divergences
from C but do not copy C's bugs." Distinguished from Q51 (also reverse-direction,
but FIXED toward pvxs): Q51's per-member write-ACF on a `proc` member was a
category error (`dbProcess` is not a value write, so write-ACF simply does not
apply); Q26's read-ACF is a genuine security control whose enforcement loses
nothing and matches classic CA. Relaxing to pvxs would remove a real control and
diverge from RSRV, so the stricter, security-positive behavior stands. Reversible
by the user if exact pvxs read-open parity is later required.
Rust: `crates/epics-bridge-rs/src/qsrv/channel.rs:748` (`get` `can_read`) and
`:828` (`create_monitor_with_value_mask` `can_read`).
C ref: `pvxs/ioc/securityclient.cpp:42` (`SecurityClient` exposes only
`canWrite()`/`asCheckPut`; no read check in `ioc/`), and `singlesource.cpp:278-296`
(`singleGet` reads and replies with no ASG gate).
Impact: pvxs QSRV2 single source never enforces read access-security — GET and
MONITOR are served regardless of ASG READ rules. Rust adds `can_read` gates in
both `get` and monitor-create. With an `.acf` whose ASG grants only WRITE (a
shape relying on QSRV's read-open behavior), Rust denies reads/monitors pvxs
would serve. **Reverse-direction** (Rust stricter), kept as security-positive
(matches classic RSRV `asCheckGet`).

### Q27: `block=true` completion barrier skipped when combined with `process=true` (Force)
Severity: Medium — CLEARED
Resolution: the Force arm now honors `opts.block`. When `block` is set it routes
the unconditional process through the new `PvDatabase::process_record_with_notify`
— which mints a put-notify wait-set, registers it into the record's `notify`
slot, runs the full `process_record_with_links` cycle (Force = unconditional
`dbProcess`), and returns a completion receiver only when the chain went async;
a synchronous chain drains the wait-set inside processing and returns `Ok(None)`.
The Force arm awaits that receiver, withholding the put reply until processing —
including async (PACT) device completion — finishes, matching pvxs routing a
`doWait` forced put through `dbProcessNotify(putProcessRequest)`
(`singlesource.cpp:360-369`). Non-blocking Force keeps the fire-and-forget
`process_record_with_links` path. Group Force is DISTINCT and unchanged: pvxs
`putGroupField` → `doPostProcessing` → `dbProcess` directly (no `dbProcessNotify`,
no wait), so a group put never establishes a completion barrier — Rust's group
Force (non-blocking) already matches. Regression:
`epics-base-rs/tests/force_block_process_notify.rs` (sync → `Ok(None)` + OUT
driven; async ODLY-PACT → `Ok(Some(_))` receiver withheld, DLYA armed, OUT
deferred). Bridge-level end-to-end twin added (136e7d72,
`channel.rs::tests::put_force_block_{sync_drives_out_link_before_returning,
async_holds_barrier_until_processing_done}`): drives the real
`BridgeChannel::put_with_options(Force, block=true)` PUT path — the sync case
returns only after the OUT link drove `TGT0.VAL=42`; the async case asserts a
200 ms `tokio::time::timeout` fires (the put future must not resolve while the
record is PACT), the discriminator a bare-`process_record_with_links`
regression would fail. The primitive test proves the DB method; this proves the
bridge Force arm actually calls it.
Rust: `crates/epics-bridge-rs/src/qsrv/channel.rs:721` (Force arm ignores
`opts.block`).
C ref: `pvxs/ioc/singlesource.cpp:348-368` (`doWait` cleared only for
`forceProcessing==False`; a `doWait` Force put goes through `dbProcessNotify`
and replies on completion).
Impact: C honors `block` for both Passive and Force puts — a
`record[process=true,block=true]` put routes through `dbProcessNotify` and the
reply is withheld until processing (incl. async device completion) finishes.
Rust's Force arm never consults `opts.block` and establishes no put-notify wait
(`put_pv` + `process_record_with_links`), so a blocking forced put to an async
(PACT) record returns success before processing completes — the
block/put-completion contract is broken for that combination. Passive+block is
handled correctly.

### Q37: Single-record monitor PROPERTY subscription is unfiltered — filtered array/value monitors emit unfiltered values on metadata events
Severity: High — CLEARED
Resolution: the PROPERTY subscription now carries an INDEPENDENT re-parse
of the same `PV.VAL{...}` suffix (`BridgeChannel::new` parses `channel_filters`
and `property_filters` separately; `BridgeMonitor::with_property_filters`
attaches the second chain via `subscribe_with_mask_and_filters`). Mirrors
pvxs building `pPropertiesChannel(dbChannelName(sInfo->chan))` from the same
filtered name (`singlesrcsubscriptionctx.cpp:24`), with `dbChannelCreate`
re-parsing the suffix per channel (`dbChannel.c:471`) for independent filter
state — so a stateful `dbnd`/`dec` on the value stream is never perturbed by
a DBE_PROPERTY event (which would else move the deadband baseline and drop
value events). `arr` slices unconditionally, so a metadata event now ships
the correctly-sliced value. Regression:
`monitor.rs::tests::property_event_delivers_filtered_slice`.
Rust: `crates/epics-bridge-rs/src/qsrv/monitor.rs:202-205` (`property_sub` via
`subscribe_with_mask`, no filter chain; deliberate per comment `monitor.rs:180-185`
"Property subscription stays unfiltered").
C ref: `pvxs/ioc/singlesrcsubscriptionctx.cpp:24`
(`pPropertiesChannel(dbChannelName(sInfo->chan))`) + `singlesource.cpp:162-167`.
Impact: pvxs builds `pPropertiesChannel` from the *full* channel name —
`record.FIELD{filter-JSON}` — so both value and property dbChannels carry the
client's channel filter (independent state per the C comment). Rust attaches
`filters_opt` to the value subscription only. On any DBE_PROPERTY event
(EGU/HOPR/LOPR/PREC/enum-strings change), `poll()` rebuilds the full NT from the
**unfiltered** property snapshot with `marked:None`; for an `arr`-sliced (or
`ts`/`dec`/`dbnd`) array monitor the wire diff against the sliced previous value
marks `value` and ships the entire un-sliced array. The client applies it,
silently corrupting its cached slice with a wrong-length value.

### Q38: Non-atomic group monitor composes the snapshot with non-atomic per-member reads while stamping `atomic=true`
Severity: Medium — CLEARED
Resolution: the monitor entry point `read_group()` now forces the atomic read
path — `read_group_atomic(self.monitor_stamp || self.def.atomic)` — so a
`+atomic:false` group's MONITOR composes its snapshot under the `lock_records`
`DBManyLock`-equivalent gate over the member set, exactly the lock its atomic
GET/PUT holds. This makes the unconditional `atomic=true` monitor stamp
(`monitor_stamp` → `stamp_atomic=true`, `:880`) truthful by construction rather
than a runtime claim about a sequentially-sampled snapshot. It mirrors pvxs
locking the fired field's whole trigger-target set (`DBManyLocker G(field.lock)`,
`groupsource.cpp:326`) for every value callback and stamping `atomic=true`
unconditionally (`:401-405`) regardless of the group's `+atomic` setting. Only
the two monitor callers (`seed()` INIT frame at `:2030`, `poll()` at `:2503`)
route through `read_group()`; a plain GET keeps `read_group_atomic(operation
atomic)` and a `+atomic:false` group's non-atomic reads remain reachable only
there. Rust locks all members (superset of pvxs's per-field trigger targets) —
consistent with the existing atomic GET/PUT and still atomic over the trigger
set. Regression: `q38_nonatomic_group_monitor_read_composes_atomically` (a
monitor-stamped read blocks on a held member gate while a plain non-atomic GET
does not).
Rust: `crates/epics-bridge-rs/src/qsrv/group.rs:697` (`read_group` →
`read_group_atomic(self.def.atomic)`), `poll()` at `group.rs:2403`; monitor
`atomic` forced true at `group.rs:808` (`stamp_atomic = if self.monitor_stamp { true }`).
C ref: `pvxs/ioc/groupsource.cpp:326` (`DBManyLocker G(field.lock)`) + `:401-405`
(`record._options.atomic = true` set unconditionally in `onSubscribe`).
Impact: pvxs's group value callback locks the fired field's entire
trigger-target record set together (`field.lock`, built unconditionally in
`initialiseTriggers`, `groupconfigprocessor.cpp:561`) while refreshing the
marked leaves, and stamps every monitor value `atomic=true`. For a
`+atomic:false` group Rust reads members through `read_group_atomic(false)` —
sequentially, each under its own momentary lock — yet still stamps
`atomic=true`. A multi-target trigger (`*` or named set) whose targets update
concurrently between the per-member reads produces marked leaves sampled at
different instants: a torn snapshot the wire advertises as atomic.

### Q39: Group monitor `poll()` turns a per-event read failure into MONITOR FINISH instead of skipping the event
Severity: Medium — CLEARED (4cdfd20a)

Resolution: `GroupMonitor::poll()` now matches on `read_group()` and, on
`Err`, logs via `tracing::warn!` (the `log_exc_printf` analogue) and
`continue`s the event loop instead of `.ok()?` → `None`. A per-event member
read/conversion failure drops a single update and keeps the subscription alive,
mirroring pvxs's per-callback try/catch (`groupsource.cpp:350-352`). Defect-family
audit (anchor: per-event read failure mapped to a stream-ending `None` in a
monitor poll): the single-record `BridgeMonitor::poll` `snap?`/`recv_snapshot()?`
are legitimate channel-close (the snapshot is pre-materialised in the
subscription; `snapshot_to_pv_structure` is infallible) — DISTINCT; `seed()`
`.ok()` is the MONITOR-INIT frame whose `None` falls back to `get_value_checked`
(no teardown) — DISTINCT; `event_rx`/`recv()`/`group_channel.as_ref()?` are
teardown detection (stop() cleared the field) — DISTINCT. Regression:
`q39_group_monitor_member_read_error_skips_event_keeps_open` (remove the member
record mid-stream, inject an event, assert poll parks not FINISH).
Rust: `crates/epics-bridge-rs/src/qsrv/group.rs:2402-2403` (`read_group().await.ok()?`)
→ `pva_adapter.rs:324-325` (`let Some(poll) = poll else { break }` →
`monitor.stop()` → stream end → wire FINISH).
C ref: `pvxs/ioc/groupsource.cpp:350-352` (`catch(std::exception& e) {
log_exc_printf(...) }` — logs, no post, subscription left open).
Impact: pvxs wraps each group value/property refresh in try/catch; a read or
conversion failure on one event is logged and the callback returns without
posting, leaving the subscription alive. Rust maps any `read_group()` `Err`
(member record gone, value-conversion failure, mid-stream ACL revocation on a
member) to `None` via `.ok()?`, which the forward task reads as source-close
and tears the whole group monitor down with a MONITOR FINISH. One member-level
read error kills the entire subscription rather than dropping a single update.

### Q49: Group PUT force/inhibit modes bypass the DISP (S_db_putDisabled) and SPC_NOMOD put gates
Severity: High — CLEARED (family with Q25)
Resolution: the group PUT preparation pass (`group.rs`
`put_with_options`) now runs the shared
`PvDatabase::check_external_put_preconditions` gate over every channeled
member before marked/putable filtering — mirroring pvxs's
`groupsource.cpp:596-609` prep loop that calls `doPreProcessing` on every
`field.value` unconditionally. An unmarked DISP=1 member now rejects the
whole group PUT in Passive/Force/Inhibit alike. Regression:
`testqgroup.rs::group_put_rejected_when_unmarked_member_is_disp_disabled`.
Rust: `crates/epics-bridge-rs/src/qsrv/group.rs:1191-1229` (`apply_member_value`
`Inhibit`/`Force` arms → `put_pv`/`put_pv_already_locked`); gate absent in
`crates/epics-base-rs/src/server/database/field_io.rs:96-220` (`put_pv_inner`).
C ref: `pvxs/ioc/iocsource.cpp:365-369` (`doPreProcessing`: SPC_ATTRIBUTE→
S_db_noMod, DISP→S_db_putDisabled), invoked from `pvxs/ioc/groupsource.cpp:599-602`.
Impact: pvxs's prep pass runs `doPreProcessing` on every channeled member
unconditionally (all modes, marked or not), so a group PUT touching a `DISP=1`
record — or an `SPC_ATTRIBUTE`/read-only field — is rejected with
`putOperation->error(...)`. In Rust the DISP/read-only checks live only in
`put_record_field_from_ca_inner` (`field_io.rs:665`, `:704`), used by the
default `Passive` mode; `Force`/`Inhibit` route through `put_pv` (no check), so
a forced group PUT writes and processes a DISP-disabled record and replies OK.
Additionally, because Rust has no prep-pass DISP check, an *unmarked* DISP=1
member never rejects the PUT in any mode, whereas pvxs rejects the whole
operation.

### Q50: Atomic group GET does not share the DBManyLock gate the atomic PUT holds — torn snapshot
Severity: High — CLEARED (GET-side twin of BR-R15)
Resolution: `read_group_atomic`'s atomic branch now takes
`PvDatabase::lock_records` over the member records — the same
`DBManyLock`-equivalent gate the atomic PUT holds — before the per-record
read guards, so an atomic GET is mutually exclusive with a concurrent
atomic PUT and with any plain single-record write (both take the same gate
via `lock_record`). Mirrors pvxs `onGet`'s `DBManyLocker G(group.value.lock)`
(`groupsource.cpp:492`). Every writer takes the advisory gate before its
`RwLock` write guard, so ordering stays advisory→RwLock everywhere (no
inversion, no deadlock). Regression:
`group.rs::tests::q50_atomic_get_blocks_on_member_record_gates`.
Rust: `crates/epics-bridge-rs/src/qsrv/group.rs:742-763` (`read_group_atomic`
uses `lock_group_records_read`, `group.rs:565-591` — incremental
`RwLock::read_owned`, never `lock_records`).
C ref: `pvxs/ioc/groupsource.cpp:492` (onGet `DBManyLocker G(group.value.lock)`)
vs `:621` (onPutGroup, same `group.value.lock`).
Impact: pvxs's GET and PUT take the identical `DBManyLock`, so an atomic GET can
never observe a half-applied atomic PUT. In Rust the atomic PUT holds the
`lock_records` advisory gate for the whole transaction but takes each member's
`RwLock` write guard only briefly per member (`field_io.rs:128`, released
between members), while the atomic GET takes per-record `RwLock` read guards
one-at-a-time in sorted order and never touches `lock_records`. When a
concurrent atomic PUT writes members in a putorder that inverts the GET's
record-name sort order, the GET can read(A), the PUT writes B, the GET then
read(B) — a snapshot with B updated and A stale. Defeats the atomicity the
`atomic` flag exists to provide; BR-R15's gate only closed PUT-vs-plain-write,
not GET-vs-PUT.

Pre-existing residual (TRACKED, not fixed this round — symmetric with accepted
BR-R15): the advisory `lock_records`/`lock_record` gate serializes the atomic
GET/PUT against other qsrv group PUTs, qsrv single PUTs, and plain CA/PVA
single-record writes (all take the gate via `lock_record`,
`field_io.rs:682-683`). It does NOT serialize against a DB-internal
output-link write to a group member: `write_db_link_value` writes the OUT-link
target through `put_pv_already_locked` (`links.rs:742-750`), which by design
does not acquire the target's advisory gate (a self-referencing `SELF PP` link
would else deadlock on the entry record's non-reentrant gate). So a record
whose processing drives an OUT/`dbPutLink` into a group member takes only that
member's `RwLock` write guard, not the group advisory gate, and can still land
between the atomic GET's incremental per-member reads → the same torn snapshot,
via a different writer class. In pvxs this is closed because the DB-link
lockset merges the source and target records, and the group's `DBManyLock`
holds every member's per-record scan lock simultaneously, subsuming the link
writer's `dbScanLock`. Closing it in Rust requires either the group GET/PUT to
hold all member `RwLock` guards simultaneously (a true `DBManyLock`, replacing
the incremental read guards) or every DB-link writer to acquire the group
advisory gate. This is exactly the class BR-R15 accepted on the PUT side (its
fix direction — "route direct member writes through the same lock" — was
deferred; `pvxs-functional-security-review-2026-05-18.md:445-469`); the Q50
GET-side gate inherits the identical limitation. Recorded here so the residual
divergence is not lost behind the CLEARED label.

### Q51: Group PUT enforces per-member write ACF on proc members; pvxs never checks canWrite for proc
Severity: Medium — CLEARED
Resolution: the group PUT's per-member write-ACF loop now skips `proc` members.
That loop mirrors pvxs `doFieldPreProcessing` (`canWrite`, iocsource.cpp:382),
which pvxs runs only for a `changing` field — `marked && putable` with a
`field.value` (groupsource.cpp:557,564). A proc member has no value field, so it
is never `changing` and `canWrite` is never checked for it, while its record is
still processed unconditionally (`doPostProcessing`, :568). Gating a processing
trigger on write access is a category error (`dbProcess` is not a `dbPutField`),
so a client with group-PUT rights but no write permission on the proc member's
backing record now triggers processing and gets a normal reply, matching pvxs
(and correcting a reverse divergence where Rust wrongly rejected). The DISP/
read-only prep gate (`doPreProcessing`, groupsource.cpp:599-602) still runs for
every channeled member including proc — unchanged. Regression:
`q51_group_put_does_not_write_acf_check_proc_member` (a write-denied proc member
does not block the PUT and its record is still processed, INIT 0→1).
Rust: `crates/epics-bridge-rs/src/qsrv/group.rs:1376-1397` (per-member
`write_grant` check runs for every active channeled member, including
`FieldMapping::Proc`, always active per `:1327`).
C ref: `pvxs/ioc/groupsource.cpp:564-571` (`doFieldPreProcessing`/canWrite
gated by `if (changing)`; a proc member is never `changing` — unmarked +
non-putable — yet `doPostProcessing` still runs it).
Impact: a proc member requires a `+channel` in Rust (`group_config.rs:779-785`),
so the ACF loop resolves a `write_grant` for it and a single denial fails the
whole group PUT with "write denied". pvxs enforces `canWrite` only for
marked+putable value members; a proc member's `SecurityClient` is built
(`groupsource.cpp:219-221`) but `doFieldPreProcessing` is never reached for it,
so a client with read/process rights but no write permission on the proc
member's backing record triggers processing and gets a normal `reply()` in
pvxs, while Rust rejects — an observable PUT-error-vs-success divergence for
facilities using per-record ASG.

### Q52: PVA-native server serves `DBF_CHAR` as `ubyte` (UInt8) instead of pvxs `byte` (Int8)
Severity: Medium — CLEARED (de500251 + f9812674), CONVERGED Round 3
Origin: first surfaced as the deferred distinct sibling (b) of Q14; promoted to
its own finding when fixed.
Resolution: the PVA-native server has **two** hand-written `EpicsValue`↔`PvField`
converters, and both mapped `DBF_CHAR` through the unsigned `UByte`, where pvxs
`fromDbrType` maps `DBR_CHAR -> TypeCode::Int8` (signed) and `DBR_UCHAR ->
TypeCode::UInt8` (unsigned) (`ioc/typeutils.cpp:32-35`). The finding spans **8
sites across 2 files**:

- **Serving path** (`native_source.rs`, de500251) — 4 sites: scalar value
  (`:143`), array value (`:173`), scalar descriptor (`:233`), array descriptor
  (`:249`). Now `ScalarValue::Byte(*v as i8)` / `ScalarType::Byte` (bit-preserving
  cast: the wire byte is identical, only the typed interpretation flips from
  unsigned to signed); the genuinely-unsigned `UChar`/`UCharArray` twins stay
  `ubyte`.
- **Monitor-filter bridge** (`server_native/tcp.rs`, f9812674) — 4 sites: the
  forward `pv_value_leaf_to_epics` scalar + array helpers, the backward
  `epics_value_to_pv_field` scalar + array arms. This is a *separate* converter
  pair the de500251 sweep missed: after de500251 flipped the descriptor to
  `byte`/`byte[]`, the forward bridge had no `Byte` arm — a `Byte` leaf returned
  `None` → `DescriptorMismatch`, terminating **every** filtered `DBF_CHAR`
  monitor — and the backward bridge still emitted `UByte`, which no longer fit
  the `byte` descriptor. de500251 alone thus turned "filtered `DBF_CHAR` monitor
  delivers a frame" into "monitor errors and dies", a client-reachable
  regression the fix itself introduced; the Round-2 convergence review caught it
  (defect citation = sample not population). f9812674 maps forward `Byte -> Char`
  / `UByte -> UChar` (distinct carriers so the backward bridge stays
  unambiguous; the filter engine reads `UChar` via the Q14 accessors) and
  backward `Char -> Byte` / `UChar -> UByte`.

**Structural cause (triplication):** `native_source.rs`, `server_native/tcp.rs`,
and the qsrv `convert.rs`/`pvif.rs` each hand-map every `EpicsValue` variant to a
`PvField`. The qsrv bridge already mapped `DBF_CHAR -> pvByte (signed)` correctly
(`convert.rs:44,61`; `pvif.rs:160`, both with typeutils citations) — only the two
native converters diverged, and their `Char` lines carried no citation while the
`UChar` twins did, the tell of the oversight. A single shared value-leaf
converter to close the triplication structurally is proposed as a follow-up (its
own change; see Round 2 note).

Distinct / not in family (verified, both Round-3 panels): the PUT-decode path
(`native_source.rs::scalar_to_epics:1085 Byte=>Char`, `:1089 UByte=>Char`;
`typed_array_to_epics:998/:999 Byte`/`UByte=>CharArray`) folds *both*
signednesses into the signed `Char` carrier — DISTINCT because that `EpicsValue`
is a bit-preserving intermediate `put_pv` (`:748`) re-coerces to the record's
declared DBF type and which never reaches a served descriptor (terminal write,
not a round-trip). `pvalink/integration.rs:1842` is a *consumer* that
deliberately widens `Byte -> ShortArray(i16)` "so the negative range survives the
DBF_CHAR-as-signed gap" (documented, pre-existing, and in a different crate); the
CA-protocol paths (`ca_gateway`, `ca` cli) are a different type system.
`EpicsValue::Char` is stored as `u8` in `epics-base-rs` (`types/value.rs:30`), so
the signed mapping needs the `as i8` cast. Regressions:
`native_source.rs::tests::dbf_char_serves_as_signed_byte_uchar_stays_unsigned`
(value + descriptor, scalar + array; `Char(200) → Byte(-56)`, `UChar(200)` stays
`UByte(200)`), plus `server_native/tcp.rs` fail-closed suite updates
(`forward_char_and_uchar_bytes_carry_faithfully`,
`owner_char_array_under_arr_filter_is_not_descriptor_mismatch`).
Rust: `crates/epics-pva-rs/src/server/native_source.rs:143,173,233,249`;
`crates/epics-pva-rs/src/server_native/tcp.rs` (forward+backward bridges).
C ref: `pvxs/ioc/typeutils.cpp:32-33` (`case DBR_CHAR: return TypeCode::Int8;`).
Impact: a scalar/array `DBF_CHAR` record served over the PVA-native server (not
the qsrv bridge) advertised `ubyte`/`ubyte[]` with a value where every element
≥128 is reinterpreted — a `DBF_CHAR` of 200 reaches the client as 200 where it
should read −56 — and, with any client filter chain applied, the monitor
terminated with `DescriptorMismatch`. Introspection typecode and data fidelity
both diverge on the wire, matching the Q14 UCHAR waveform bug in the opposite
signedness direction.

## Review Log

### Round 1 (2026-07-01) — first Codex-methodology sweep of `qsrv`
Five parallel opus auditors (categories A–E) against `pvxs/ioc` + `pvxs/src/nt.cpp`.
14 findings: 1 Blocker (Q25), 3 High (Q14, Q37, Q49, Q50 — Q50 High), 7 Medium
(Q1, Q13, Q26, Q27, Q38, Q39, Q51), 3 Low (Q2, Q15). Zero re-reports of the
prior Rust-side rounds (BR-R13..R65, Round 5/6, 2026-05-28 series). Candidates
checked-and-rejected by the auditors: unquoted-key JSON leniency (correct
YAJL parity), group/record name-conflict gate (present), `record._options`
shape (correct), trailing-comma/trailing-content (both reject), field
put-order `stable_sort` (matches), GET/PUT sharing `group.value.lock` on the
PUT side (BR-R15 done).

Thematic clusters:
1. **`doPreProcessing` never ported to the qsrv put boundary.** C runs
   `doPreProcessing` (DISP `S_db_putDisabled`, SPC_ATTRIBUTE `S_db_noMod`) on
   *every* channeled put in *every* process mode, before any marked/putable
   filtering. Rust only enforces DISP/read-only inside the `Passive`
   `put_record_field_from_ca_inner` path; the `Force`/`Inhibit` routes through
   `put_pv` skip it entirely, on both the single (Q25, Blocker) and group (Q49)
   paths. Structural fix: a single qsrv-side preprocessing gate applied to all
   three modes, single + group — the flagship of this round.
2. **Group atomicity gaps.** The `atomic` flag's guarantee leaks on both the
   read side (Q50: GET doesn't share the PUT's DBManyLock → GET-vs-PUT tear)
   and the monitor side (Q38: non-atomic group monitor stamps `atomic=true`
   over sequentially-sampled members).
3. **Monitor lifecycle / filter fidelity.** Property subscription drops the
   client channel-filter (Q37 → un-sliced array corrupts a sliced client
   cache); a single member read error tears down the whole group monitor
   instead of skipping one event (Q39).
4. **Type/introspection byte-parity.** Metadata sub-structs carry non-anonymous
   type-ids pvxs leaves empty (Q13); UCHAR waveforms collapse to signed Int8
   (Q14, cross-crate).
5. **Config-parse strictness.** Field-name path grammar is normalized where
   pvxs throws (Q1); member annotations aren't type-coerced like the group-level
   ones (Q2).

Dispositions requiring judgment (not blind parity fixes) — RESOLVED
(this list was written at Round 1; updated 2026-07-02):
- Q26 — reverse divergence (Rust read-gates QSRV where pvxs never does).
  **KEEP the stricter, security-positive behavior — user sign-off 2026-07-02.**
  No code change; documented at the Q26 entry; reversible if exact pvxs
  read-open parity is later required.
- Q14 — cross-crate structural (`DbFieldType` UChar). **FIXED `11c8798e`**
  (first-class `DbFieldType::UChar` served as unsigned `ubyte[]`); distinct
  sibling (b) cleared as Q52. Sibling (a) USHORT/ULONG `reallocate_val`
  collapse remains a separate tracked record-layer gap (1cdd4319), out of
  Q14 scope.
- Q15 — latent (AMSG unmodeled). Deferred until AMSG is modeled end-to-end.

### Round 2 (2026-07-01) — fix-verification pass (four opus panels)
Four parallel opus reviewers verified the Round-1 fix commits against pvxs
(categories: config-parse, group-runtime, single-record, type/wire). Every
load-bearing reviewer claim was ground-verified against the actual
pvxs/epics-base source before acting — all four held:

1. **Q1 (config-parse) — false-parity artifacts corrected, divergence
   documented.** The Rust field-path parser rejects `value[]`, `value[ 5]`,
   negative, and overflow subscripts that pvxs's `strtol`-based
   `fieldName.cpp:29-67` accepts (`[]` → element 0; `[ 5]` → 5; negative/
   overflow accepted then failing at navigation). These are intentionally
   stricter — the "don't copy C's bugs" steer applies, behavior kept — so the
   fix was documentation-only: removed the false "matches strtol" claim, added
   an authoritative divergence note (d62d063a / doc corrections).
2. **Q2 (config-parse) — false NoConvert-throw claim corrected.** The
   annotation-coercion divergence: pvxs's yajl `_CC_CHK` cancels only on a
   falsy 0, so the `yajlProcess` `-1` return on an `as<T>()` NoConvert
   exception is SWALLOWED (a pvxs bug — `groupconfigprocessor.cpp:1048-1059` +
   `yajl_parser.c:175-181`), and the group is served with the annotation
   defaulted. Rust's per-group skip is intentionally stricter; documented, not
   changed (d5a63519 / doc corrections).
3. **Q27 (single-record) — genuine test-coverage gap closed, then the barrier
   *release* path completed.** The Force+block fix had only a primitive test on
   `process_record_with_notify`; nothing exercised the
   `BridgeChannel::put_with_options` Force arm that wires into it. Added the
   bridge end-to-end twin (136e7d72), then a single-record panel noted the twin
   proved the barrier *holds* (async pending) but not that it *releases* — so a
   third test (`put_force_block_async_releases_after_delay_completes`,
   ODLY=0.05 s) asserts the awaited `put` returns `Ok(Ok(()))` after the delayed
   processing completes and the OUT-link target is driven (03633c3f).
4. **Q52 (type/wire) — genuine distinct bug fixed, then completed across the
   second native converter.** de500251 fixed `native_source.rs`'s serving path
   (4 sites) where pvxs maps `DBR_CHAR -> Int8` (the Q14 sibling (b) deferred
   earlier); the qsrv bridge already had it right, only the native path diverged.
   The type/wire panel then caught that de500251 was **incomplete**: the
   monitor-filter bridge in `server_native/tcp.rs` is a *separate* converter pair
   the sweep missed, and de500251's descriptor flip turned it into a
   client-reachable regression (`Byte` leaf → `None` → `DescriptorMismatch` kills
   every filtered `DBF_CHAR` monitor). Completed in f9812674 (4 more sites; 8
   total across 2 files). This is the round's defect-family lesson: the original
   `Char.*=> .*UByte` search shape missed `tcp.rs` because its converters use
   different function names (`pv_value_leaf_to_epics` /
   `epics_value_to_pv_field`) — a type-mapping bug lives in *every* hand-written
   `EpicsValue`↔`PvField` converter, enumerated by locating every fn that
   pattern-matches the variants, not by one regex shape. See the Q52 entry's
   structural-triplication note.

**Structural follow-up surfaced for sign-off (not yet done):** the three
hand-written `EpicsValue`↔`PvField` converters in `epics-pva-rs`
(`native_source.rs` serve, `server_native/tcp.rs` filter-bridge) plus the qsrv
`convert.rs` are the structural cause of this defect family — the same type
mapping is transcribed three times, so a signedness fix must be applied (and can
be forgotten) in each. A single shared value-leaf converter would close the
family by construction. It is large enough to be its own change and is deferred
pending user sign-off.

Also recorded as a tracked residual (not fixed, symmetric with accepted
BR-R15): the Q50 atomic-GET advisory gate does not cover DB-internal
output-link writes to a group member (`write_db_link_value` uses
`put_pv_already_locked`, no gate), so that writer class can still tear the
snapshot — closed in pvxs by the DB-link lockset + `DBManyLock`, deferred here
exactly as BR-R15 deferred the PUT-side twin.

### Round 3 (2026-07-01) — convergence pass (two opus panels), CONVERGED
Two opus reviewers (type/wire, single-record) verified the two follow-up
commits from Round 2 — Q52 completion (f9812674) and Q27c release-path
(03633c3f). Both ground-verified against source + pvxs and both returned
**PASS**; the round converged with zero DEFECT/CONCERN.

1. **Q52 tcp.rs filter-bridge (f9812674) — PASS, family closed.** The
   bidirectional mapping is bijective and bit-preserving (`Byte↔Char`,
   `UByte↔UChar` as distinct carriers; wire byte `0xC8` round-trips
   `Byte(-56)→Char(200)→Byte(-56)`), matches pvxs `typeutils.cpp:32-35`, and a
   filtered `DBF_CHAR[]` monitor no longer `DescriptorMismatch`es. Both panels
   independently enumerated every `EpicsValue`/`ScalarValue` byte-family
   converter in the crate and **confirmed only two serve/monitor converters
   exist** (`native_source.rs` serve + `server_native/tcp.rs` filter-bridge) —
   no third. Family closed at 8 sites / 2 files.
   - **Refinement to the completeness note:** the Round-2 write-up cited only
     PUT-decode `scalar_to_epics:1085` (`Byte=>Char`); its siblings `:1089`
     (`UByte=>Char`) and `typed_array_to_epics:998/:999` (`Byte`/`UByte=>
     CharArray`) fold *both* signednesses into the signed `Char` carrier. Both
     panels ruled this **DISTINCT, not a missed defect**: the PUT-decode
     `EpicsValue` is a bit-preserving intermediate that `put_pv` (`:748`)
     re-coerces to the record's declared DBF field type and which never reaches
     a served descriptor — so the `UByte→Char` fold is benign there. It is
     logged below as a backlog cleanliness item (inbound decode), orthogonal to
     the DBF_CHAR-signedness serve family closed here.
2. **Q27c release-path (03633c3f) — PASS.** `scalcout.rs:634` guarantees the
   async arm for `odly > 0.0` (keys on presence, not magnitude), so ODLY=0.05 s
   reliably takes the `Ok(Some(rx))→rx.await` release path the hold-only test
   (ODLY=100 s, drops the future at 200 ms) never observes. The real ~50 ms
   tokio timer fires well within the 5 s guard; `matches!(outcome, Ok(Ok(())))`
   + `TGT2.VAL==42` pin release-after-OUT-completion ordering. Not flaky (100×
   margin; a hung barrier trips the 5 s timeout). The three tests cleanly
   partition the sync-drain (`Ok(None)`), hold (pending), and release
   (`Some(rx)`) arms.

**Structural follow-up — both panels AGREE, with a scope boundary.** The shared
value-leaf converter is the right (and minimal) structural close for the
triplication; a smaller const-table fix is strictly worse (leaves ≥2
hand-maintained copies that drift — the type/wire panel noted the `UInt→Int64` /
`ULong→UInt64` arms were each already patched piecemeal, the same recurring
divergence family). **Scope boundary (must be stated in that change):** the
shared converter owns only the **serve/monitor `EpicsValue`↔`PvField` leaf
direction** (both ways, the 8 sites, type-preserving `byte↔Char`/`ubyte↔UChar`).
It must **not** absorb the PUT-decode path (`scalar_to_epics`/
`typed_array_to_epics`), whose `UByte→Char` single-carrier collapse is a
deliberately different rule (re-coerced by `put_pv`, never round-tripped);
merging it would break PUT re-coercion or wrongly force distinct carriers where
the fold is correct.

**DONE (79888ef5, signed off).** Implemented as `crate::leaf_convert` in
`epics-pva-rs`: `epics_value_to_pv_leaf` (EpicsValue→PvField value leaf, shared
by `snapshot_to_pv_field` + the monitor backward bridge),
`epics_value_to_field_desc_leaf` (its type-only projection, for
`snapshot_to_field_desc`), and `pv_leaf_to_epics_value` (the partial inverse for
the monitor forward bridge, fail-closed `None` for the uncarried `ushort`/`uint`
gap preserved). The two serve converters and the two filter-bridge converters
now delegate; the DBF_CHAR signedness lives once per direction. Module doc
states the scope boundary; the PUT-decode path is untouched. Round-trip and
gap-pinning unit tests own the correspondence; the redundant tcp.rs
direct-mapping test was dropped and the end-to-end filtered-monitor wiring test
kept. No behavior change — 1196 `epics-pva-rs` tests pass, clippy `-D warnings`
clean.

**Backlog (out-of-family, not blocking):** inbound PUT-decode maps `UByte→Char`
(`native_source.rs:1089`) / `UByte[]→CharArray` (`:999`) into the *signed* CHAR
carrier rather than `UChar`/`UCharArray`. Benign today (the record re-coerces to
its FTVL regardless), but a UCHAR inbound-decode cleanliness item for a later
Q14/UCHAR pass.

### Review closed — 2026-07-02

QSRV parity review (Q1..Q52) closed. Rounds 1–3 converged with zero
DEFECT/CONCERN (see the Round 3 section); every defect was FIXED and verified
there. Workspace locked at close: `cargo clippy --workspace --all-targets
-D warnings` clean and `cargo nextest run --workspace` 7420 passed / 0 failed.

The two items left in the Round-1 "Dispositions requiring judgment" list are
resolved by user sign-off (2026-07-02):

- **Q26** (read-ACF gate stricter than pvxs QSRV2) — **KEEP** as
  security-positive; no code change, reversible on explicit request.
- **Q14** (UCHAR waveform served as signed `byte[]`) — already **FIXED
  `11c8798e`**; no action owed.

Deferred / tracked, documented and not blocking (reopen on demand):

- **Q15** — `alarm.message` / AMSG latent, blocked on AMSG modeling in
  epics-base-rs.
- **Q50** — atomic-GET advisory gate does not cover DB-internal output-link
  writes (symmetric with the deferred BR-R15 PUT-side twin).
- Q14 sibling (a): USHORT/ULONG `reallocate_val` collapse (record-layer,
  `1cdd4319`); and the inbound PUT-decode `UByte→Char` cleanliness backlog
  item above.
