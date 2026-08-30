# ad-plugins-rs reference pins

This crate cites **five** reference trees — more than any other crate in the
workspace. Every line-numbered C citation in it resolves at the revision named
below for its tree, and must be read against that revision, not against
whatever the local checkout happens to hold.

| tree | pin | checkout on this machine | citations |
|---|---|---|---|
| ADCore | **`6c53844e`** (`R3-14-111-g6c53844e`) | `R3-14-173-g926bb4c8` | 404 |
| ADSupport | **`R1-10`** (`e5be67d7`) | `R1-10-48-gfe23754` | 25 |
| epics-base | **`R7.0.10`** (tag object `e1c98a45`, commit `bf11a0c3`) | `R7.0.10-146-g8f5015b66` | 5 |
| pvxs | **`1.5.2`** | `1.5.2-26-gbd2243d` | 2 |
| asyn | **`e2a281e2`** (`R4-45-19-ge2a281e2`) | `R4-45-74-g731d616e` | 1 |

Recorded 2026-08-25. Measured with the cited line's *text* compared at pin and
checkout — file-level drift is not citation drift, and using the file-level
measure overstates the affected set several-fold.

## ADCore — 404 citations, pin `6c53844e`

294 sit on lines byte-identical at the pin and the checkout. 106 sit on lines
the checkout moved but resolve at the pin and are deliberately **left
drifted**: the pin declaration is what closes drift, and re-anchoring them
onto the worktree is the failure this file exists to prevent. 4 —
`Codec.h:12-18` and `:37-39` in `src/codec.rs` and `src/file_hdf5.rs` — name a
file that exists only at the pin, because `ADApp/ADSrc/Codec.h` was renamed to
`NDCodec.h` by `ace98398` (2026-04-27), a descendant of the pin. At the pin
`Codec.h:12-18` is the `NDCodecCompressor_t` enum and `:37-39` is
`bool empty()`. Re-anchoring them to `NDCodec.h` would move correct citations
onto a file the pin does not carry.

Four citation values that were wrong at the pin were corrected on 2026-08-25
(see `a8266431`), at eight sites across this crate and `ad-core-rs`; three
values at four sites are here:

| was | prose names | now, at `6c53844e` |
|---|---|---|
| `NDFileJPEG.h:326` | `supportsMultipleArrays = 0` | `NDFileJPEG.cpp:326` |
| `NDPluginStats.cpp:526` | `if (bgdPixels < 1) bgdPixels = 1` | `:527` |
| `NDPluginDriver.cpp:1016` | the callback threads' construction | `:1000` |

The JPEG row is a file error rather than a line error: that header is 64 lines
at the pin and at the checkout alike, and the assignment is in the `.cpp`.

## ADSupport — 25 citations, pin `R1-10`

`bitshuffle_core.c` and `bitshuffle.c` under `supportApp/bitshuffleSrc/`,
reached from `src/codec.rs` (13 sites) and a since-deleted workspace
parity-review document (12); 13 distinct citation values.

**`R1-10`** (`e5be67d7`, 2021-05-26) is the pin because it is byte-identical to
the checkout in exactly these files, not because it is a tag. Both files have
had one revision in the tree's entire history — `23b119b`, 2018-12-05, "New
file from bitshuffle 0.3.5" — so `git log R1-10..fe23754 -- supportApp/
bitshuffleSrc/` is empty and the 48 commits of drift never touched them. `R1-10`
is the more stable name for identical bytes. `bitshuffle.c` is 165 lines and
`bitshuffle_core.c` 1862 at every revision ADSupport has ever carried.

`R1-10-48-gfe23754` would have been an equally valid pin: `git merge-base
--is-ancestor fe23754 origin/master` passes, so a `git describe` string 48
commits past a tag is reproducible from a fresh clone and is a perfectly good
pin. That test — ancestry of the upstream default branch — is what separates a
good pin from a bad one, not whether the name is a tag.

### The 25 citations were re-anchored onto the pin (2026-08-25)

They were wrong at every revision ADSupport has ever carried, so no pin
declaration could rescue them: 3 of the 13 values are past EOF at `R1-10` and at
`fe23754` alike, and the other 10 land on a different construct than the prose
names, with deltas from −2 to −185 that no single shift explains. They had been
written against a larger bitshuffle than the 0.3.5 ADSupport vendors.

Re-anchoring them is not the mass re-anchor this file exists to prevent. That
prohibition protects *drifted-but-correct* citations, where the cited text is
still right at the pin and only the line number moved; here nothing that
resolved was rewritten, because none of them resolved.

ADSupport's copy is the right target because it is the C the port is achieving
parity with. At ADCore pin `6c53844e`, `NDPluginCodec.cpp:461` is
`#include <bitshuffle.h>`, and the file calls `bshuf_compress_lz4` at
`NDPluginCodec.cpp:556` and `bshuf_decompress_lz4` at `:596`; those link
`supportApp/bitshuffleSrc`. ADCore never sees upstream
`kiyo-masui/bitshuffle`.

| was | prose names | now, at `R1-10` | delta |
|---|---|---|---|
| `bitshuffle.c:34` | `bshuf_compress_lz4_block` | `:32` | −2 |
| `bitshuffle.c:82` | `bshuf_decompress_lz4_block` | `:78` | −4 |
| `bitshuffle.c:237` | `bshuf_compress_lz4` | `:153` | −84 |
| `bitshuffle.c:244` | `bshuf_decompress_lz4` | `:160` | −84 |
| `bitshuffle_core.c:110` | macro `TRANS_BIT_8X8` | `:89` | −21 |
| `bitshuffle_core.c:166` | `bshuf_trans_byte_elem_scal` | `:174` | +8 |
| `bitshuffle_core.c:205` | `bshuf_trans_bit_byte_scal` | `:219` | +14 |
| `bitshuffle_core.c:280` | `bshuf_trans_bit_elem_scal` | `:256` | −24 |
| `bitshuffle_core.c:306` | `bshuf_trans_byte_bitrow_scal` | `:281` | −25 |
| `bitshuffle_core.c:331` | `bshuf_shuffle_bit_eightelem_scal` | `:308` | −23 |
| `bitshuffle_core.c:373` | `bshuf_untrans_bit_elem_scal` | `:349` | −24 |
| `bitshuffle_core.c:1852` | `bshuf_blocked_wrap_fun` | `:1667` | −185 |
| `bitshuffle_core.c:2009` | `bshuf_default_block_size` | `:1828` | −181 |

### The blosc copy was ruled out first

ADSupport also vendors `supportApp/bloscSrc/blosc/bitshuffle-avx2.c` (248
lines), `bitshuffle-generic.c` (221) and `bitshuffle-sse2.c` (467), so a
citation past the end of a 165-line file could have named one of those instead.
None does. Every symbol above was located in all five candidate files at
`R1-10`: the four `bshuf_*_lz4*` entry points, `TRANS_BIT_8X8`,
`bshuf_blocked_wrap_fun` and `bshuf_default_block_size` exist **only** under
`bitshuffleSrc`. The six `_scal` routines do also exist in
`bitshuffle-generic.c`, at `:44`, `:89`, `:126`, `:145`, `:168` and `:209`
against cited `:166`, `:205`, `:280`, `:306`, `:331` and `:373` — a mean miss of
149 lines against 15 for `bitshuffle_core.c`, and four of the six would be past
the end of that 221-line file. The two specifically at risk are the rows naming
`bshuf_compress_lz4` and `bshuf_decompress_lz4`, functions that appear in no
blosc file at all.

## epics-base — 5 citations, pin `R7.0.10`

Same pin as `crates/std-rs` and `crates/ad-core-rs`: the checkout fails the
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

- `epicsThread.cpp:214-220` (2 citations) — byte-identical at the pin and the
  checkout; revision-independent.
- `iocsh.cpp` (3 citations, `src/time_series.rs:688`, `:724`, `:725`) — were
  **wrong at the pin**, resolving at the checkout only, and were corrected on
  2026-08-25. `iocsh.cpp` shifted by exactly six lines in this band, so
  `:1274-1284` became `:1269-1279` (the `try`/`catch` that swallows a throwing
  iocsh command and prints `C++ error: %s`), `:1001` became `:995` (the
  `onerr(Continue)` default in the `iocshScope` constructor) and the bare
  `:1129` continuations became `:1123` (`if(scope.onerr==Continue)`). The old
  `:1129` is `break;` inside the `Break` arm at the pin — the opposite of the
  "continues by default" the prose asserts, which is why leaving these drifted
  was not an option.

## pvxs — 2 citations, pin `1.5.2`

`nt.cpp:240-247` (`src/pva.rs:290`, `:534`) is byte-identical at the tag and
the checkout, so these citations do not discriminate between them.

## asyn — 1 citation, pin `e2a281e2`

`asynPortDriver.cpp:4036-4040` (`src/time_series.rs:687`) is byte-identical at
the pin and the checkout. The pin is declared because it is the workspace pin
of record for asyn, not because this citation proves it.
