# `libc` RTEMS filing kit

Everything needed to file the two `libc` defects found during RTEMS bring-up.
Nothing here has been filed. The box has no `gh` and no credentials.

A third gap (missing PI mutex API — `pthread_mutexattr_setprotocol` +
`PTHREAD_PRIO_*` constants, found 2026-07-22 during the priority-inheritance
design) is documented in `pthread-prio-protocol.md`. No patch prepared yet;
it follows the same one-PR-against-`main` + `stable-nominated` routing when
filed.

## Routing — CORRECTED 2026-07-22 against the project's own CONTRIBUTING.md

An earlier version of this file said to file **four** PRs (each change against
both `main` and `libc-0.2`). **That is wrong**, and it is wrong in the
direction that creates work for maintainers. `rust-lang/libc`'s
`CONTRIBUTING.md` says:

> `main` is for active development of the upcoming v1.0 release, and should be
> the target of all pull requests.

> Once a `stable-nominated` PR targeting `main` has merged, it can be cherry
> picked to the `libc-0.2` branch. A maintainer will likely do these cherry
> picks in a batch before a release, **so there is no need for any action as a
> PR author.**

So: **two PRs, both against `main`, each with a `@rustbot label
stable-nominated` comment.** The 0.2 backport is what actually matters to us —
`library/std/Cargo.toml` depends on `libc 0.2.x`, so a `main`-only fix changes
nothing for `-Zbuild-std` — but the nomination label is the whole mechanism,
not a second PR.

| # | what | target | body / patch |
|---|---|---|---|
| 1 | socket address types + 27 constants | `main` | `PR-1-sockaddr-main.md` / `rtems-sockaddr-len.patch` |
| 2 | scalar type widths (6 types) | `main` | `PR-2-typewidths-main.md` / `rtems-type-widths.patch` |
| — | `@rustbot label stable-nominated` | comment on each of 1 and 2 | — |
| 3 | measured data + `dev_t`/`ino_t` correction | **comment** on rust-lang/libc#5132 | `COMMENT-5132.md` |

The `-0.2.md` bodies and `-0.2.patch` diffs are kept but are **not to be
filed**. Their content is identical to the `main` pair (the diffs differ only
in a blob-hash line), so they are useful only if a maintainer asks for a manual
backport or a rebase.

Why 1 and 2 stay split:

- (1) and (2) are unrelated defects — one is the FreeBSD length byte in socket
  addresses, the other is scalar widths. They touch mostly different files and
  reviewers will want them separately. They *do* both add to
  `src/unix/newlib/rtems/mod.rs`; whichever lands second needs a one-hunk
  context rebase there. Nothing else conflicts.
- (3) is a comment, **not** a competing PR. #5132 is an open PR already in
  review that restructures exactly the `time_t` cfg block PR (2) touches.
  Opening a second PR over the same lines would fight it. The comment
  contributes the measured data, confirms its `time_t = i64` for RTEMS, and
  corrects its `dev_t`/`ino_t`, which it leaves at `u32`.
- Reaching 0.2 is load-bearing, not optional: `library/std/Cargo.toml` depends
  on `libc 0.2.x`, so a fix landing only on `main` changes nothing for anyone
  building `std` with `-Zbuild-std`. **The mechanism for that is the
  `stable-nominated` label on the `main` PR, not a second PR** — see the
  correction above. Do not skip the label.

### Which of our four type findings #5132 already covers

| finding | #5132 | verdict |
|---|---|---|
| `time_t` 4→8 | yes — sets `time_t = i64` for the non-espidf/vita branch | **already covered** |
| `dev_t` 4→8  | touches the same cfg block, leaves `dev_t = u32` | **new — and it is a correction to that PR** |
| `ino_t` 4→8  | touches the same cfg block, leaves `ino_t = u32` | **new — same** |
| `rlim_t` 4→8 | not touched | **new** |
| `clock_t` 4→8 | not touched | **new** (found after the table was first written) |
| `clockid_t` signedness | not touched | **new** |
| sockaddr `sin_len` | not touched at all | **new, unrelated** |

`off_t` is *not* one of our findings — it is already `i64` and correct, and
#5132 keeps it that way.

### #5132 state, as read on 2026-07-21

Open **pull request** (not an issue), "newlib: fix definition of `time_t` and
`off_t`", opened 2026-06-01, last updated 2026-07-21, author `dybucc`,
reviewers `tgross35` / `pheki`, **changes requested**.

**Re-verified 2026-07-22 by the main worker, direct from the PR page.** Every
claim above holds: open, changes requested by `tgross35` and `pheki`, sets
`time_t = i64` for RTEMS on ARM and `off_t` for vita, and does **not** touch
`dev_t`, `ino_t`, `rlim_t`, `clock_t` or `clockid_t`.

The `core::compile_error!("unsupported target")` for RTEMS **is** really in the
diff — the doubt above was warranted as a doubt but the code is there, so it is
a legitimate point to raise in the comment rather than a misreading to drop:
`armv7-rtems-eabihf` is `target_arch = "arm"`, so ask the author which RTEMS
configuration that arm is meant to catch.

## Files

| file | what |
|---|---|
| `PR-1-sockaddr-main.md` | PR body, socket address types, `main` |
| `PR-2-typewidths-main.md` | PR body, scalar type widths, `main` |
| `COMMENT-5132.md` | text to post as a comment on #5132 |

The two `-0.2.md` bodies were **deleted** — there is no 0.2 PR to file (see the
routing correction above). The `-0.2.patch` diffs are kept only in case a
maintainer asks for a manual backport.

> The last paragraph of `COMMENT-5132.md` asks the author about a
> `compile_error!` on an arm RTEMS can reach. That line rests on a *summarised*
> read of the diff, not the raw patch — it is phrased as a question for exactly
> that reason. Drop it if you would rather not ask from a second-hand reading.
| `type-widths.md` | the measured type/struct width table |
| `pthread-prio-protocol.md` | gap 3: missing PI mutex API (documented only, no patch yet) |
| `repro-timespec/` | standalone maintainer reproduction (see its README) |
| `rtems-sockaddr-len.patch` | **the diff for PR 1**, against `origin/main` |
| `rtems-sockaddr-len-0.2.patch` | the diff for PR 1b, against `origin/libc-0.2` |
| `rtems-type-widths.patch` | **the diff for PR 2**, against `origin/main` |
| `rtems-type-widths-0.2.patch` | the diff for PR 2b, against `origin/libc-0.2` |

The four `.patch` files were generated on this machine with
`git diff <base>...<branch>` from the box's clone, so filing needs no access to
the box: `git apply` one onto a fresh `rust-lang/libc` checkout of the matching
base, commit, push, and paste the corresponding `PR-*.md` as the body. The
clone at `~/rtems-bringup/libc` on the box remains the authority if a rebase is
needed.

## Branches in `~/rtems-bringup/libc`

```
rtems-sockaddr-len       e381fa90f  newlib: give RTEMS its own socket address types
                         83dabf2df  newlib: correct RTEMS socket and file constants
rtems-sockaddr-len-0.2   25e25fe86 / 7af649b22   (same two, on libc-0.2)
rtems-type-widths        57ecb059f  newlib: correct RTEMS scalar type widths
rtems-type-widths-0.2    0730fb752  (cherry-pick -x of 57ecb059f onto libc-0.2)
bringup                  f4451c609  = rtems-sockaddr-len-0.2 + type widths
                                      (what the box builds against; currently
                                      checked out; carries no workaround commit)
bringup-workaround       a50792990  superseded — the old "do not upstream"
                                      time_t hack, kept only for history
```

`git format-patch main..rtems-type-widths` / `main..rtems-sockaddr-len` produces
the patch files if filing by email is preferred.
