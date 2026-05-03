# EPICS Reference Feature Map

Stable inventories of the public API + wire protocol of the two EPICS
network protocols, extracted from their canonical C / C++ reference
implementations. This is **Layer 1**: a "what does a CA / PVA library
need to do?" specification, independent of any particular
implementation.

| Map | Reference source | Revision | Items |
|-----|------------------|----------|-------|
| [`ca.md`](./ca.md) | `epics-base/modules/ca` (libca + rsrv) | `c9817fa59` | 177 |
| [`pva.md`](./pva.md) | `pvxs` | `9beba6b` | 174 |

Total: **351** reference features cataloged.

---

## What this is

Each entry in the maps has:

- **Stable ID** (`CA-NNN` / `PVA-NNN`) — never renumbered, append-only.
- **Symbol** — the function / class / command name.
- **Header:line** — exact provenance in the upstream source.
- **Description** — what it does, in one sentence.

Entries are grouped by **functional area** (channel ops, subscriptions,
wire protocol commands, etc.) so a newcomer can navigate without prior
knowledge.

## What this is NOT

- It is not a coverage map of any specific implementation. The matching
  Layer 2 (`docs/coverage/{ca,pva}.md`) overlays the
  `epics-rs` implementation status on top of these IDs and lives separately.
- It is not exhaustive at the bug level. The
  `archaeology/INDEX/master_index.md` (365 epics-base bug-fix commits)
  fills that role.
- It is not a guide to the protocol byte-for-byte — see `caProto.h` /
  `pvaproto.h` directly for wire format details. The maps cite line
  numbers there but do not duplicate the byte layouts.

## Why bother?

1. **Onboarding**. New contributors can find "where is the file
   handle for a CA channel?" → `CA-020 ca_create_channel` →
   `cadef.h:519` in seconds.
2. **Compatibility audits**. Cross-checking against other EPICS
   client libraries (Java pvAccess, p4p, py-pvxs) becomes a
   row-by-row diff.
3. **Roadmap**. Coverage overlay (Layer 2) makes implementation
   gaps visible per-feature.
4. **Test coverage**. Each ID can be cross-referenced to test cases —
   missing rows are missing tests.
5. **Spec drift detection**. When upstream releases tag a new
   reference revision, `git diff` on the cited headers yields the
   list of rows that need re-inspection.

## Update procedure

When upstream is bumped:

1. Update the `Revision` row at the top of the affected map.
2. `git diff <old>..<new> -- include/...` against the cited headers.
3. For each diff hunk:
   - **Added** symbol → append a new row at the END of its section
     with the next available ID.
   - **Removed** symbol → mark the existing row with `(deprecated)`
     in the description; do not delete or renumber.
   - **Modified** signature → update the row in place but keep the ID.
4. Bump the `audited` date.

Layer 2 (coverage) is updated independently each time `epics-rs`
ships a release affecting CA / PVA.

## File layout

```
docs/reference-feature-map/
├── README.md   # this file
├── ca.md       # 177 CA features (libca + rsrv + caProto)
└── pva.md      # 174 PVA features (pvxs client + server + pvaproto)
```

Future Layer 2 (coverage):

```
docs/coverage/
├── README.md
├── ca.md       # status of each CA-NNN in epics-ca-rs
└── pva.md      # status of each PVA-NNN in epics-pva-rs
```

## Related documents

- [`../../archaeology/INDEX/master_index.md`](../../archaeology/INDEX/master_index.md)
  — bug-level archaeology (365 epics-base commits, 127 apply, 44 partial).
- [`../../archaeology/pvxs/`](../../archaeology/pvxs/) — same for pvxs.
- [`../../CHANGELOG.md`](../../CHANGELOG.md) — release history of `epics-rs`.
- [`../../ROADMAP.md`](../../ROADMAP.md) — forward-looking work plan.
