# mca-rs reference pins

This crate cites **two** reference trees. Every line-numbered C citation in it
resolves at the revision named below for its tree, and must be read against
that revision, not against whatever the local checkout happens to hold.

| tree | pin | checkout on this machine | citations |
|---|---|---|---|
| mca | **`687d563`** (tree carries no tags) | `687d563` — same commit | 64 |
| epics-base | **`R7.0.10`** (tag object `e1c98a45`, commit `bf11a0c3`) | `R7.0.10-146-g8f5015b66` | 10 |

Recorded 2026-08-26. Counted with `doc/parity-instruments/span-census.py`,
which detects the full `file.ext:payload` form; a bare `:NNN` continuation is
not counted again. The counts are of the crate **without this file**: the
epics-base list below quotes five of its own citations, so a re-run over
`crates/mca-rs` now reports 15 rather than 10 for those basenames.

Until this file existed the mca pin was declared only at `src/lib.rs:20`. That
in-crate table is what a reader of the code sees and it stays; this file is
where an auditor checking the crate's citations looks, and a pin that is only
in a doc comment is auditable but not findable.

## mca — 64 citations, pin `687d563`

`mcaRecord.c` (50), `mcaRecord.dbd` (6), `devMCA_soft.c` (5), `mca.h` (2) and
`devMcaAsyn.c` (1), all under `mcaApp/mcaSrc/`. Every cited line lands inside
the file at the pin: the files are 1180, 2049, 162, 50 and 417 lines there
against highest cited lines of 1180, 395, 161, 48 and 388.

The pin is the raw hash because the tree carries **no tags at all** — `git
describe` fails outright on it, so no `R…-g…` name exists to prefer. It still
passes the test that separates a good pin from a bad one: `git merge-base
--is-ancestor 687d563 origin/master` exits zero.

This checkout sits exactly on the pin — `687d563206d59de9097e28e95e32ad09ebcc2522`,
2025-08-25, "Remove Form Feed characters, screws up PermaLink references", and
`git rev-list --count 687d563..origin/master` is 0. So the declaration buys
nothing today; it is what keeps these 64 citations auditable the first time
upstream moves, which is the only moment at which writing it down is no longer
possible from the checkout alone.

## epics-base — 10 citations, pin `R7.0.10`

`longinRecord.c:413-416` and `:414` (3), `dbAccess.c:1365-1368` and
`:1370-1373` (2), `dbLink.c:316-321` and `:319-320` (2),
`oldChannelNotify.cpp:287` (2, at `modules/ca/src/client/`) and
`recGbl.c:135-139` (1).

Same pin and same reason as `crates/ad-core-rs` and `crates/ad-plugins-rs`:
the checkout fails the ancestry test.

```
git -C $EPICS_BASE merge-base --is-ancestor HEAD origin/7.0     # exits non-zero
git -C $EPICS_BASE merge-base --is-ancestor R7.0.10 origin/7.0  # exits zero
```

`R7.0.10-146-g8f5015b66` carries the unmerged PR #944, so it is reachable only
from this machine. The distance from the tag is not what disqualifies it —
being off the branch is.

Measured at the cited lines' *text*, not at file level: 6 of the 10 are
byte-identical at the pin and the checkout (`longinRecord.c`, `recGbl.c`,
`oldChannelNotify.cpp`) and do not discriminate between them. The 4 on
`dbAccess.c` and `dbLink.c` resolve **only at the pin** — both files moved in
this band, so reading them against the checkout gives different text. The
file-level measure would have called 7 of the 10 drifted — it counts
`longinRecord.c`'s 3 as well, because the file moved elsewhere while the cited
`case menuYesNoYES:` block did not — and overstated the affected set by 75%.
