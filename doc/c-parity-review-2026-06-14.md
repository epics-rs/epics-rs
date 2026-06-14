# Workspace C-parity review — 2026-06-14 (round 1, churn-led 5)

Codex-style C→Rust output-form parity audit across the five highest-churn
crates. Read-only fan-out (5 parallel general-purpose agents, one per
category), each handed its crate's prior inventory docs so it would not
re-report closed families.

## Parity philosophy (scope filter for this round)

The **only** thing that must match upstream C/C++ parity is the **OUTPUT
FORM** — wire/byte format, DBR/PVA encodings, on-the-wire field values,
externally observable record-field outputs, and monitor-post shape/mask.
Internal design may differ or improve as long as it is functionally
equivalent; a design deviation that produces *identical* observable output
is **not** a finding. Every finding below names a concrete observable
consequence (a caget/pvget/camonitor result, or differing wire bytes).

Reference trees: EPICS base `~/codes/epics-base`, pvxs
`~/codes/epics-modules/pvxs`, motor `~/codes/epics-modules/motor`.

Finding IDs are used **only in this doc** (not in source comments or commit
messages, per the workspace convention). Commits cite the C reference +
rationale.

## Disposition legend

- **fix** — clear output-form divergence, fix to match upstream.
- **fix-low** — real wire/post divergence, narrow or packet-level impact;
  fix for completeness.
- **signoff** — output differs, but closing it requires an architecture /
  semantic change to an intentional port design; surfaced for user decision
  rather than silently changed.
- **verify** — reachability uncertain; confirm an observable divergence
  exists in the Rust model before fixing.

## Open Findings

### BASE-1: Alarm-acknowledge put (ACKT/ACKS) posts wrong monitor mask and skips the record-wide DBE_ALARM event
Severity: High — Disposition: fix
Rust: `crates/epics-ca-rs/src/server/tcp.rs:2955` routes `DBR_PUT_ACKT`/`DBR_PUT_ACKS` to `put_record_field_from_ca_no_notify(name, "ACKT"/"ACKS", Short)` → `crates/epics-base-rs/src/server/database/field_io.rs` `put_record_field_from_ca_inner`. ACKT/ACKS are common (non-pp) fields, so `should_process` is false (`field_io.rs:839-848`) and the only monitor emitted is the written field at `field_io.rs:771-776` with `EventMask::VALUE | EventMask::LOG`. `put_common_field` mutates `acks`/`ackt` (`record_instance.rs:1160-1194`) but signals no side-effect change, and no record-wide alarm event is posted.
C reference: `modules/database/src/ioc/db/dbAccess.c:1285-1315` — `putAckt` posts `&precord->ackt` with `DBE_VALUE | DBE_ALARM` (1293), posts `&precord->acks` with `DBE_VALUE | DBE_ALARM` when it lowers `acks` (1297), then `db_post_events(precord, NULL, DBE_ALARM)` (1299); `putAcks` posts `&precord->acks` `DBE_VALUE | DBE_ALARM` (1311) then the record-wide `db_post_events(precord, NULL, DBE_ALARM)` (1312).
Impact: (1) `camonitor -m a REC.ACKT` (DBE_ALARM-only) gets the ack in C, nothing in Rust. (2) Any `camonitor -m a REC.<field>` alarm-mask subscriber is notified by C's record-wide DBE_ALARM post; Rust fires none. (3) When ACKT→NO lowers `acks` (or ACKS clears it), a client monitoring `REC.ACKS` reads a stale acknowledged-severity in Rust until an unrelated post.

### BASE-2: DBF_CHAR scalar rendered to DBR_STRING is unsigned; C renders it signed
Severity: Medium — Disposition: fix
Rust: `crates/epics-base-rs/src/types/value.rs:85` — `Display for EpicsValue::Char(v)` does `write!(f, "{v}")` on a `u8`, printing 0–255. A DBR_STRING-family GET of a scalar DBF_CHAR reaches this via `types/codec.rs:120` → `value.rs:946` (`format!("{self}")`).
C reference: `modules/database/src/ioc/db/dbConvert.c:417-437` `getCharString` reads `char *psrc` (signed `epicsInt8`) → `cvtCharToString(*psrc,…)` = `cvtInt32ToString((epicsInt32)*psrc,…)` (sign-extends).
Impact: DBR_STRING payload differs for any DBF_CHAR ≥ 128: `0xFF`→C `"-1"`, Rust `"255"`; `0x80`→C `"-128"`, Rust `"128"`. `caget -S` returns the wrong sign. Internally inconsistent: the same `Char(0xFF)` already reads `-1.0` for DBR_DOUBLE (`value.rs:1053`, `(*v as i8) as f64`). (The CharArray→String long-string path at `value.rs:917` is distinct raw-byte; leave it.)

### BASE-3: Integer records defeat the MLST/ALST first-publish sentinel, posting a spurious first-cycle VAL monitor
Severity: Medium — Disposition: fix
Rust: `check_deadband_ext` (`crates/epics-base-rs/src/server/record/record_instance.rs:2145-2198`) reads `mlst = get_field("MLST").to_f64().or(self.common.mlst).unwrap_or(NaN)`; the NaN first-publish sentinel only engages when `get_field` returns `None`. `longin`/`longout`/`int64in`/`int64out` are pure `#[derive(EpicsRecord)]` (no `init_record`) and define `mlst/alst/lalm` default-initialized to `0.0` (e.g. `longout.rs:101-107`), exposed via `get_field("MLST")→Some(Double(0.0))`. So `mlst=0.0`, the NaN fallback never fires, and `check_deadband(val, 0.0, mdel=0)` returns `|val|>0=true` on the first process.
C reference: `modules/database/src/std/rec/longinRecord.c:120-122` (and longout/int64in/int64out init_record) seed `prec->mlst = prec->alst = prec->lalm = prec->val`; `monitor()` then evaluates `DELTA(mlst,val) > mdel` → `0 > 0` → no post on the first unchanged process.
Impact: a `longout`/`longin`/`int64out`/`int64in` initialized to nonzero VAL (constant DOL, initial INP read, or a CA put before first scan) with default `MDEL=0` posts an extra/duplicate VAL camonitor update on first process; C posts none. `ai`/`ao` seed `mlst=val` (`ai.rs:267`, `ao.rs:439-441`), so the four integer records are inconsistent with both C and their analog siblings. Secondary (same root cause): `LALM` defaults `0.0` not `val`, shifting the first-cycle HYST alarm edge when the initial value sits on a threshold with `HYST>0`.

### CA-1: Bad-resource-ID (stale SID) drops the ECA_INTERNAL CA_PROTO_ERROR frame C always emits before disconnect
Severity: High — Disposition: fix
Rust: `crates/epics-ca-rs/src/server/tcp.rs:2472` (READ), `:2873` (WRITE ACKT/ACKS), `:3007` (WRITE), and the same family on WRITE_NOTIFY / CLEAR_CHANNEL / EVENT_CANCEL / EVENT_ADD — every bad-SID branch does `return Err(CaError::Protocol(...))` and tears the circuit down **without writing any frame**. Comments (`:2466`, `:2996`) and tests (`tcp.rs:7631-7637`, `:7770`) encode this as intended.
C reference: `modules/database/src/ioc/rsrv/camessage.c:58` (`logBadId` → `logBadIdWithFileAndLineno`), `:307-320` — calls `send_err(mp, ECA_INTERNAL, client, "Bad Resource ID…")` **before** the handler returns `RSRV_ERROR`; `vsend_err` (`:139-244`) emits a CA_PROTO_ERROR frame (`m_cmmd=11`, `m_cid=0xffffffff`, `status=ECA_INTERNAL`, echoed request header + diagnostic string). `write_action`, `write_notify_action`, `clear_channel_reply`, `event_cancel_reply`, `read_action`, `event_add_action` all route bad SIDs through it.
Impact: on a stale/bad SID C sends a CA_PROTO_ERROR (`m_cid=0xFFFFFFFF`, `m_available=ECA_INTERNAL=0x8E`) + echoed header + string, then closes. Rust sends 0 bytes and closes. A libca peer issuing WRITE/WRITE_NOTIFY/CLEAR_CHANNEL/EVENT_CANCEL after its channel was torn down raises `exceptionRespAction(ECA_INTERNAL)` under C; under Rust the operation silently vanishes.

### CA-2: caget failure via abused-`m_cid` READ_NOTIFY surfaces wrong ECA code (ECA_PUTFAIL)
Severity: Medium — Disposition: fix
Rust: `crates/epics-ca-rs/src/client/transport.rs:1580-1588` — inline READ_NOTIFY handler, on `hdr.cid != ECA_NORMAL`, delivers `CaError::Protocol(format!("server returned ECA error {:#06x}", hdr.cid))`. `CaError::Protocol(_).to_eca_status()` falls to `_ => ECA_PUTFAIL` (`crates/epics-base-rs/src/error.rs:106`), so a structured consumer sees ECA `0xA0` (a *put* error) for a GET failure.
C reference: `modules/ca/src/client/cac.cpp` `readNotifyRespAction` calls `pmiu->exception(hdr.m_cid, "read failed", …)`, propagating the server's exact ECA code. The C server `camessage.c:540-556` sets `cas_set_header_cid(pClient, ECA_GETFAIL)` on a GET failure → arrives as `m_cid = ECA_GETFAIL (0x98)`.
Impact: caget against a C IOC whose record read fails returns `CaError::Protocol("…0x0098")`; `.eca_status()` yields `0xA0 ECA_PUTFAIL` instead of `0x98 ECA_GETFAIL`. Siblings preserve the code — CA_PROTO_ERROR read error (`transport.rs:1789` → `ServerError`) and EVENT_ADD monitor error (`:1669-1672` → `ServerError`). Fix: emit `CaError::ServerError(hdr.cid)` here.

### CA-3: READ_NOTIFY get-failure ships count=0 / zero-byte payload where C ships requested count and a `dbr_size_n`-sized zero body
Severity: Low — Disposition: fix-low
Rust: `crates/epics-ca-rs/src/server/tcp.rs:2563-2599` — on a READ_NOTIFY upstream get failure / None-snapshot, `send_cmd_error(CA_PROTO_READ_NOTIFY, type, ECA_GETFAIL/ECA_BADCHID, ioid)` (`tcp.rs:4972-4988`) sets `count=0`, `cid=status`, and serializes a 16-byte header with **zero payload bytes** regardless of requested count/autosize.
C reference: `camessage.c:540-581` (`read_reply`) — non-autosize keeps header count at requested `m_count` and commits a `dbr_size_n(type, m_count)`-byte zeroed body; autosize resets count to 0 and commits `dbr_size_n(type, 0)` (nonzero for compound DBR_TIME/GR/CTRL) zeroed; a `count>=0xffff` request stays extended (24-byte) form.
Impact: autosize DBR_TIME_DOUBLE failure: C `m_count=0, m_postsize=8` + 8 zero bytes; Rust `m_postsize=0`, no payload. Non-autosize DBR_DOUBLE count=N failure: C `m_count=N, m_postsize=8*N` zeroed; Rust `m_count=0, m_postsize=0`. **Client-observable outcome identical** (libca reads `m_cid != ECA_NORMAL` and raises the exception without inspecting the body) — only a packet-level wire validator sees it. Reachable only via a no-cache CA-gateway shadow PV.

### PVA-1: Monitor overflow squash emits a non-empty overrun BitSet; pvxs always sends `0x00`
Severity: Medium — Disposition: fix
Rust: `crates/epics-pva-rs/src/server_native/tcp.rs:8177` (and `:8238`, `:8290`) — `build_monitor_payload` writes `overrun_bitset(intro, overrun_paths, mask)`. On a server-side monitor-queue overflow, `coalesce_monitor_update` (`:8340-8352`) populates `overrun` with the leaves changed in both the dropped and surviving updates; that set (taken at `:7531`) produces a **non-empty** overrun BitSet on the wire.
C reference: `pvxs src/servermon.cpp:174-176` — after `to_wire_valid`, pvxs always writes `to_wire(R, uint8_t(0u))` (single `0x00` = empty BitSet), `// TODO: placeholder for overrun mask`. The server squash (`:285`) is never signaled.
Impact: under a producer that overflows the queue, the Rust DATA frame carries extra overrun bytes (e.g. `0x01 0x02`) where pvxs emits exactly `0x00`. A pvxs/pvAccessCPP client decodes it (`clientmon.cpp:554-564`), sets `servSquash=true`, and increments the client-visible `nSrvSquash` statistic — a counter that stays 0 against a real pvxs server. Output-form parity requires matching pvxs's empty placeholder (this is a Rust-side "improvement" that nevertheless breaks wire parity).

### PVA-2: Pipelined MONITOR INIT reply echoes the client's `0x80` bit (subcmd `0x88`); pvxs always sends `0x08`
Severity: Low — Disposition: fix-low
Rust: `crates/epics-pva-rs/src/server_native/tcp.rs:6061` — the INIT reply writes `payload.put_u8(subcmd)`, echoing the inbound INIT subcmd. A pipeline MONITOR INIT arrives `0x88` (`0x08 | 0x80`), so the reply subcmd is `0x88`.
C reference: `pvxs src/servermon.cpp:133-135` — `doReply` sets `subcmd = 0x08;` unconditionally for the Creating→INIT reply; pvxs never sets `0x80` on a server→client monitor frame.
Impact: one differing subcommand byte on the INIT reply (`0x88` vs `0x08`) for every pipelined monitor. Both pvxs and Rust clients mask `&0x08` so no behavioral effect; a strict third-party client that exact-matches the reply subcmd sees an unexpected byte. Pure wire-byte divergence; fix: hardcode `0x08`.

### PVA-3: STOP→START coalesces queued posts into one DATA frame; pvxs delivers up to `queueSize` distinct frames
Severity: Medium — Disposition: signoff
Rust: `crates/epics-pva-rs/src/server_native/tcp.rs:7508-7519` — while `monitor_paused` (set on STOP, `:6702-6710`), the subscriber loop coalesces the in-hand value plus the entire `pending` backlog into one `held` update via `coalesce_monitor_update`, then emits a single DATA frame on resume. N distinct posts during the Idle window collapse to one frame.
C reference: `pvxs src/servermon.cpp:251-297` (`doPost`) appends each post as a distinct `queue` entry while `queue.size() < limit`; `doReply` reschedules while `!queue.empty()` (`:211-220`), so after STOP→START pvxs delivers every accumulated distinct post (up to `limit`) as separate DATA frames.
Impact: for a source that keeps posting during STOP, a peer receives 1 coalesced frame from Rust vs up to `queueSize` distinct frames from pvxs — different frame count and intermediate values. When the source honors the start gate (QSRV/gateway `ctl.set(false)`), no posts arrive during Idle and the divergence does not occur. This is the "hold-latest while paused" port design and is a sibling of the SR-19 unbounded-queueSize decision — surfaced for sign-off rather than silently rewritten to pvxs's distinct-frame queue.

### MOT-1: DIFF and RDIF are not re-posted on an unchanged device-callback pass
Severity: Medium — Disposition: fix
Rust: `crates/motor-rs/src/record/status_update.rs:177-184` recomputes `diff`/`rdif` each pass, but `record/mod.rs:504-509` lists DIFF/RDIF only in `alarm_cycle_monitored_fields` and provides no always-mark hook, so on a non-alarm pass DIFF/RDIF post only through generic change-detection (`prev != val`). When DVAL and DRBV are unchanged between two callbacks, neither posts.
C reference: `motorRecord.cc:3764-3767` — `process_motor_info()` does `pmr->diff = …; MARK(M_DIFF); pmr->rdif = …; MARK(M_RDIF);` **unconditionally** every CALLBACK_DATA pass (unlike RRBV/DRBV/RBV, marked only on change); `monitor()` 3522-3532 posts both with `DBE_VAL_LOG` every pass.
Impact: a `camonitor DIFF`/`RDIF` on an axis settled at a constant non-zero error (retries exhausted with MISS=1, or STUP/periodic refresh while parked off-target) gets an event every callback in C, none in Rust.

### MOT-2: `load_pos` posts the new-coordinate RBV one pass earlier than C
Severity: Low — Disposition: fix-low
Rust: `crates/motor-rs/src/record/command_planner.rs:1684` — `load_pos` unconditionally recomputes `self.pos.rbv = dial_to_user(drbv, dir, off)` after the FOFF=Variable offset change (`:1681`), so the load-dispatch monitor pass posts RBV in the new frame while DRBV is still the pre-LOAD_POS readback.
C reference: `motorRecord.cc:3771-3817` — neither `load_pos` branch recomputes or `MARK`s RBV; RBV shifts to the new frame only after the GET_INFO callback re-runs `process_motor_info` (`:3717`).
Impact: during a SET-mode DVAL/RVAL (or SET+TWF) redefine with FOFF=Variable, a `camonitor RBV` observes the new-offset RBV immediately in Rust; C delivers no RBV post on that pass and converges one poll later. One extra/early RBV monitor event, self-correcting.

### MOT-3: Retry give-up / close-enough do not preserve a held jog button (MIP_JOG_REQ)
Severity: Low — Disposition: verify
Rust: `crates/motor-rs/src/record/state_machine.rs:627-630` (give-up) and `:665-669` (close-enough) call `finalize_motion`, which sets `mip = MipFlags::empty()` (`:438`); `MipFlags::JOG_REQ` is never inspected/set on any completion path.
C reference: `motorRecord.cc:1063-1065` — on retry give-up C does `mip = MIP_DONE; if ((jogf && !hls) || (jogr && !lls)) mip |= MIP_JOG_REQ;`; close-enough (`:1088`) and rtry==0 (`:1055`) do `mip &= MIP_JOG_REQ`, preserving a held jog request.
Impact: when a positional move ends with a jog button held, C posts MIP with the 0x1000 (JOG_REQ) bit and resumes the jog; Rust posts MIP=0. Reachability caveat: Rust routes a JOGF/JOGR put through `start_jog`, which stops any in-flight positional move (`command_planner.rs:929`), so C's precondition (an active positional move with `jogf=true` latched) is hard to construct. Verify an observable divergence exists before fixing.

### BR-1: QSRV `+type:"plain"` array member advertises a scalar descriptor but serves a scalar-array value
Severity: Medium — Disposition: fix
Rust: `crates/epics-bridge-rs/src/qsrv/group.rs:1682` — the introspection Plain branch emits `FieldDesc::Scalar(scalar_type)` unconditionally, discarding the array-ness `introspect_member` already resolved (`group.rs:1019` → `NtType::ScalarArray`). The value path (`group.rs:951`, `decode_member` Plain → `convert::epics_to_pv_field`) produces `PvField::ScalarArray` for an array backing field. Descriptor says scalar, value is a length-prefixed array.
C reference: `pvxs ioc/iocsource.cpp:632,640-641` (`getChannelValueType(chan, true)`) returns `valueType.arrayOf()` when `dbChannelFinalElements(chan) != 1`; `ioc/groupconfigprocessor.cpp:886-895` (`addMembersForPlainType`) builds the Plain leaf from that type → pvxs advertises a Plain array member as a scalar-array.
Impact: a PVA client introspecting a group with a `+type:"plain"` array member (e.g. a waveform) receives that field typed scalar (`double`), but every GET/MONITOR reply carries a `double[]` — the introspected-type cache disagrees with the wire bytes, so a conforming client mis-decodes/fails, and the published type differs from pvxs's `double[]`.

### BR-2: PVA gateway decoded monitor path sends a full-mask changedBitSet on every event, dropping upstream's real changed bits
Severity: Medium — Disposition: fix
Rust: the decoded (non-raw) monitor path (field projection / pipeline window / `_filter`, gated at `crates/epics-pva-rs/src/server_native/tcp.rs:6869`) → `apply_monitor_event` (`crates/epics-bridge-rs/src/pva_gateway/channel_cache.rs:347`) decodes the upstream `changed` BitSet but uses it only to merge the delta (`fill_unmarked_from_prior`, `:383`), storing only the merged full value; the upstream bitset is discarded. The gateway fans out `MonitorUpdate::from(val)` with `marked: None` (`source.rs:1882-1889`) and inherits `monitor_emits_partial=false` (`source.rs:1135`); with `marked==None` and `emits_partial==false`, `tcp.rs:7618-7626` falls to `build_monitor_payload(&mask_clone)` = every requested leaf set, on every event.
C reference: `epics-base modules/pva2pva/p2pApp/moncache.cpp:142,189` copy the upstream event's actual `*update->changedBitSet` verbatim into each downstream element.
Impact: a field-projected/pipelined/filtered downstream PVA monitor on a multi-field structure sees `changedBitSet`=all-selected-leaves on every update behind the Rust gateway, vs only the changed leaves behind pva2pva. A client driving logic off "which field changed" gets the wrong answer. Distinct from BR-R41 (which fixed "decoded fallback emits only the initial snapshot"); the raw path (`EPICS_PVA_GW_RAW_FRAMES=YES`) forwards the upstream bitset verbatim and is correct.

### BR-3: CA gateway upstream-disconnect posts an ALARM|LOG event; a DBE_VALUE-only downstream monitor receives nothing
Severity: Medium — Disposition: signoff
Rust: `crates/epics-bridge-rs/src/ca_gateway/upstream.rs:1002-1008` posts `post_alarm(name, 3 /*INVALID*/, LINK_ALARM)` on upstream monitor close → delivered (`crates/epics-base-rs/src/server/pv.rs:723`) with mask `EventMask::ALARM | EventMask::LOG`, filtered per-subscriber by `Subscriber::accepts` (`pv.rs:270-272`). A monitor opened with `DBE_VALUE` only does not intersect `ALARM|LOG`, so it receives no frame; the shadow PV is kept alive so there is no ECA_DISCONN.
C reference: `ca-gateway src/gatePv.cc:600-601` — `gatePvData::death()` on `gatePvActive` does `delete vc`, destroying the downstream casPV; the CA server signals ECA_DISCONN to every downstream client independent of any casEventMask.
Impact: a downstream `camonitor -m v` (DBE_VALUE only) sees, on upstream IOC disconnect: C → ECA_DISCONN (monitor stops, value known stale); Rust → nothing, channel stays "connected" showing the last value. Scope note: camonitor's default mask `DBE_VALUE|DBE_ALARM` does intersect ALARM, so the common case still receives the disconnect alarm; the gap is value-only/log-only subscribers. Closing it fully requires the gateway to tear down the downstream channel (alarm-post-vs-delete-VC architecture, see `pv.rs:710-716`) — surfaced for sign-off.

## Review Log

### Round 1 — 2026-06-14 (churn-led 5: base records, ca-wire, pva-wire, motor, bridge)

15 new output-form findings (2 High, 9 Medium, 4 Low). Each verified at the
cited `file:line` on both the Rust and C/C++ side; design-only deviations
with identical observable output were deliberately excluded (the per-agent
reports name the rejected leads).

Thematic clusters:

- **Monitor-post fidelity (BASE-1, BASE-3, MOT-1, BR-2, BR-3, PVA-1).** The
  dominant cluster: the port repeatedly diverges on *which* monitor events
  fire and *what mask/bitset* they carry, not on the value itself. C posts a
  record-wide DBE_ALARM (BASE-1), suppresses a first-cycle post via a
  value-seeded sentinel (BASE-3), unconditionally re-marks DIFF/RDIF
  (MOT-1), and forwards the upstream changedBitSet verbatim (BR-2). The
  structural through-line is "monitor masks/bitsets are an output-form
  contract, not an internal detail."
- **Error-frame fidelity (CA-1, CA-2, CA-3).** The CA server/client drop or
  mistype the failure frame: a bad-SID emits no CA_PROTO_ERROR at all
  (CA-1), a GET failure surfaces ECA_PUTFAIL (CA-2), and an error reply
  ships a truncated body (CA-3). CA-1+CA-2 are a small family — the structured
  ECA code must survive the failure path.
- **Coordinate/transition timing (MOT-2, MOT-3, PVA-2, PVA-3).** Low-impact,
  mostly self-correcting timing/echo divergences.

Disposition summary: 9 **fix**, 2 **fix-low** (CA-3, PVA-2; MOT-2 fix-low),
2 **signoff** (PVA-3, BR-3), 1 **verify** (MOT-3). Sign-off items differ in
observable output but only close via an intentional-design/architecture
change; presented to the user rather than silently rewritten.
