# ad-core-rs reference pins

This crate cites **three** reference trees. Every line-numbered C citation in
it resolves at the revision named below for its tree, and must be read against
that revision, not against whatever the local checkout happens to hold.

| tree | pin | checkout on this machine | citations |
|---|---|---|---|
| ADCore | **`6c53844e`** (`R3-14-111-g6c53844e`) | `R3-14-173-g926bb4c8` | 171 |
| epics-base | **`R7.0.10`** (tag object `e1c98a45`, commit `bf11a0c3`) | `R7.0.10-146-g8f5015b66` | 5 |
| asyn | **`e2a281e2`** (`R4-45-19-ge2a281e2`) | `R4-45-74-g731d616e` | 1 |

Recorded 2026-08-25. Measured with the cited line's *text* compared at pin and
checkout — file-level drift is not citation drift, and using the file-level
measure overstates the affected set several-fold.

## ADCore — 171 citations, pin `6c53844e`

112 sit on lines byte-identical at the pin and the checkout, so no revision
choice can affect them. 58 sit on lines the checkout moved but resolve at the
pin and are deliberately **left drifted**: the pin declaration is what closes
drift, and re-anchoring them onto the worktree is the failure this file
exists to prevent. 1 — `Codec.h:12-18` in `src/codec.rs` — names a file that
exists only at the pin.

`ADApp/ADSrc/Codec.h` was renamed to `NDCodec.h` by `ace98398` (2026-04-27),
which is a descendant of the pin. At the pin `Codec.h:12-18` is exactly the
`NDCodecCompressor_t` enum. Re-anchoring it to `NDCodec.h` would move a
correct citation onto a file the pin does not carry.

Eleven citations that were wrong at the pin were corrected on 2026-08-25 (see
`a8266431`); the largest was `NDPluginDriver.cpp:1016`, which is
`deleteCallbackThreads`' opening brace at the pin while the prose names
`createCallbackThreads` building `new epicsThread(...)`, at `:1000`.

To re-measure one file:

```
git -C $EPICS_MODULES/ADCore diff --quiet 6c53844e HEAD -- ADApp/<path>
```

## epics-base — 5 citations, pin `R7.0.10`

Same pin as `crates/std-rs`, and for the same reason: the checkout fails the
ancestry test that separates a good pin from a bad one:

```
git -C $EPICS_BASE merge-base --is-ancestor HEAD origin/7.0   # exits non-zero
git -C $EPICS_BASE merge-base --is-ancestor R7.0.10 origin/7.0 # exits zero
```

`R7.0.10-146-g8f5015b66` carries the unmerged PR #944, so it is reachable only
from this machine. Its distance from the tag is not the problem — a `git
describe` string 146 commits past a tag is a perfectly good pin when it is an
ancestor of the upstream default branch, and several of this workspace's pins
are exactly that. Being off the branch is the problem.

- `epicsThread.cpp:214-220` (1 citation) — byte-identical at the pin and the
  checkout; revision-independent.
- `iocsh.cpp` (4 citations, `src/plugin/runtime.rs:1715`, `:1716`, `:2401`,
  `:2402`) — were **wrong at the pin**, resolving at the checkout only, and were
  corrected on 2026-08-25. `iocsh.cpp` shifted by exactly six lines in this
  band, so `:1274-1284` became `:1269-1279` (the `try`/`catch` that swallows a
  throwing iocsh command and prints `C++ error: %s`), `:1001` became `:995` (the
  `onerr(Continue)` default in the `iocshScope` constructor) and the bare
  `:1129` continuations became `:1123` (`if(scope.onerr==Continue)`). The old
  `:1129` is `break;` inside the `Break` arm at the pin — the opposite of the
  "continues by default" the prose asserts, which is why leaving these drifted
  was not an option.

## asyn — 1 citation, pin `e2a281e2`

`asynPortDriver.cpp:4036-4040` (`src/plugin/runtime.rs:1714`) is byte-identical
at the pin and the checkout, so this crate's single citation does not
discriminate between them. The pin is declared because it is the workspace pin
of record for asyn, not because this citation proves it.
