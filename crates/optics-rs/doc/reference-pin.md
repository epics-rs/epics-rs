# optics-rs reference pin

`optics = R2-14-15-g3def19d`. Every C citation in this crate — source, tests
and dbd alike — resolves at that revision and must be read against it, not
against whatever the local checkout happens to hold.

Recorded 2026-08-25, after two `tableRecord.c` ranges were listed as verified
only at their anchor line in the workspace round of 2026-08-23. Both were
opened at the pin; both resolve.

## Measured pin-to-checkout drift

The checkout on this machine has moved to `R2-14-29-ga750b95`, 14 commits
ahead. Across `crates/optics-rs` there are 56 line-numbered C citations, 52 of
them naming a file optics carries at the pin — `tableRecord.c` (42),
`orient.c` (3), `orient.h` (3), `chantler.c` (2), `chantler.h` (2). The other
4 are epics-base: `dbLink.c` (3) and `recGbl.c` (1).

**All five cited optics files are byte-identical between the pin and the
checkout**, so none of the 52 citations is affected by the 14 commits of
drift. That is the opposite of what the AD and std crates measured, and it is
why the pin still has to be written down: without it, "identical" is an
accident nobody can check next time the tree moves.

To re-measure one file:

```
git -C $EPICS_MODULES/optics diff --quiet 3def19d HEAD -- opticsApp/src/<file>
```

## The two `checkLinks` ranges

`tableRecord.c` is 2355 lines at the pin, `checkLinks` is defined at `:2305`
(`static void`) / `:2306` (the name line), and its body runs to `:2353`.

- **`tableRecord.c:2306-2352`** (`src/records/table.rs:2225`, "C `checkLinks`
  — which axes the table may …") — resolves. `:2306` is the definition's name
  line and `:2352` closes the `for (i=0; i<6; i++)` loop that is the whole
  body; only the two closing braces fall outside.
- **`tableRecord.c:2318-2352`** (`src/records/table.rs:164`, "the only two
  things `checkLinks` ever asks a link") — resolves exactly. `:2318` is
  `for (i=0; i<6; i++) {` and `:2352` its closing `}`, and every
  `plink->type ==` and `dbCaIsLinkConnected(…)` test in the function lies
  inside it, at `:2321`-`:2351`.

Both also resolve at the checkout, since the file did not drift.
