# C reference pins for the `epics-bridge-rs` review docs

Every review doc in this directory cites its upstream by env-var path —
`$PVXS_HOME`, `$EPICS_BASE`, `$EPICS_MODULES/ca-gateway`,
`$EPICS_BASE/modules/pva2pva`. Those name working checkouts, which run ahead
of the revision the review was written against. A `file.cpp:NNN` checked
against one of them can be graded wrong while being right, or graded right
after drifting into a neighbouring construct — the failure is silent either
way. This file gives the revisions those citations resolve at, and the rules
for resolving them. Each doc repeats the rows it needs in its own
`## C reference pins` section, so a reader who opens one doc alone still has
its pins.

## Pinned revisions

| tree | checkout | pinned revision |
| --- | --- | --- |
| `pvxs` | `$PVXS_HOME` | `1.5.1-42-gb568e93` |
| `epics-base` | `$EPICS_BASE` | `R7.0.10` |
| `ca-gateway` | `$EPICS_MODULES/ca-gateway` | `R2-1-3-0-54-g0666f21` |
| `pva2pva` | `$EPICS_BASE/modules/pva2pva` | `1.4.1` (`3a08da44`) |

The first three rows are the ones `crates/epics-bridge-rs/src/lib.rs` already
declares, so the docs and the crate cannot drift apart. `pva2pva` is absent
from that table — the crate's own source cites it, but only through
`epics-base`, so the pin is not free-standing: `3a08da44` is the submodule
gitlink `epics-base` `R7.0.10` carries
(`git rev-parse R7.0.10:modules/pva2pva`), which is pva2pva's own tag `1.4.1`.

Each pin passes `git merge-base --is-ancestor <pin> <default branch>` in its
own tree (`origin/7.0` for `epics-base`, `origin/master` for the other three),
which is the test a pin has to meet: a revision reachable only from a fork
branch or an unmerged PR names nothing a reader outside this workspace can
fetch.

## Resolve by symbol at the pin; the line is a hint

Find the named function, struct, macro or field first, and treat the line
number as a hint that has to land inside that construct.

1. Construct at the pin, line lands in it — the citation is exact. A
   reference checkout ahead of the pin will disagree; that disagreement is
   the checkout's, not the citation's.
2. Construct at the pin, line lands outside it — line drift. Keep the symbol
   and move the line to the pin's.
3. Construct absent at the pin — the citation means code added after it, and
   is NOT moved onto the pin, where it would point at lines that do not
   exist. It names the revision it means inline, beside the line span.

Resolve each citation on its own. One sentence can cite two lines that are
right at different revisions, and a check run at either revision then reports
a single tidy error while vouching for the very citation the other condemns.

## A bare basename is not a path

`server.cpp`, `channel.cpp`, `pvalink.cpp`, `pvalink.h`, `testpvalink.cpp`
and every `pvalink_*.cpp` exist in both `pva2pva` and `pvxs`. A citation
naming only the basename therefore resolves in the wrong file without failing
— the worse outcome, because nothing flags it. pvxs `src/server.cpp` is 860
lines and pva2pva `p2pApp/server.cpp` is 316, so the same number picks out
unrelated code in each.

Every citation to one of those carries its in-tree path:
`p2pApp/server.cpp:218`, `ioc/pvalink_lset.cpp:199`. The pva2pva copies live
under `p2pApp/` (gateway) and `pdbApp/` (pvalink); the pvxs copies under
`ioc/` (`src/` for `server.cpp`). `chancache.cpp`, `chancache.h`,
`moncache.cpp` and `gwmain.cpp` are pva2pva-only and carry no ambiguity.

## Rust citations carry no pin

A `foo.rs:NNN` citation is in-repo. It has no external revision to resolve
at and no pin can be given for it: it resolves at the current worktree, not
at the commit the review was written on. Where the reviewed code has since
been fixed, moved, or replaced, the line is moved onto the construct that now
carries the behaviour and the sentence says so, rather than being left
pointing at whatever now occupies the old number.
