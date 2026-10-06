# source-guard

Test-only. The one production-slice rule the workspace's mechanical guards
share, and the derived file sets they sweep.

Never published (`publish = false`); every consumer takes it as a `path`-only
dev-dependency, which `cargo package` drops from the uploaded manifest.

## What a source guard is

A `#[test]` that reads its own crate's source with `include_str!` and asserts
something about the code that ships: that no thread is created outside its
owner, that no timer is taken from `tokio` directly on a path the RTEMS backend
runs, that a blocking driver names no async socket type.

Such an assertion is only as good as the slice it runs on, and every guard had
grown its own slicer. Fourteen call sites carried three different rules. Eleven
truncated at the first `\n#[cfg(test)]`, which makes the covered set a property
of *where a test item happens to be written*: a `#[cfg(test)]` helper placed
high in a file silently removes everything below it from the guard's view, and
the guard stays green while checking a fraction of its subject.

## `production` — the slice

A file's production slice is the file with every item whose `cfg` predicate
**implies `test`** blanked out, wherever in the file it sits and at whatever
depth; a `#[cfg(test)] mod tests` nested inside `mod ioc` is test code exactly
as a top-level one is.

"Implies test" is decided by reading the predicate, not by matching the string.
`#[cfg(all(test, unix))]` is test-only; `#[cfg(not(test))]` is not; and
`#[cfg(any(target_os = "rtems", test))]` is not — those seven items in
`epics-libcom-rs`'s task seam ship on RTEMS, and a substring rule would have
quietly dropped them from the census that exists to cover them.

Excluded lines are **blanked, not deleted**, so a 1-based line number taken
from the slice still names the same line of the original file. Guards that
report offending line numbers were relying on truncation for that.

`Comments::Strip` is the second half of the rule, because guards forbid *code*
from naming something while the prose beside that code names it constantly —
the `epics-libcom-rs` `try_clone` guard once failed on five prose hits and zero
code hits. Strip removes line, block (nested) and trailing comments, and reads
string and character literals, so `"https://x"` keeps its text and `&'a str`
keeps its lifetime. That is what lets it replace the two rules that were in the
tree: a whole-line rule that missed trailing comments, and a truncate-at-`//`
rule that could not tell a comment from a URL inside a string.

## `sweep` — the subject set

```rust
for (file, text) in source_guard::sweep(concat!(env!("CARGO_MANIFEST_DIR"), "/src/server_native"), &["mod.rs"]) {
    let slice = source_guard::production(text, source_guard::Comments::Strip);
    // assert over `slice`, reporting `file`
}
```

A guard that carries its subjects as a hand-written list is default-out: a file
added to the module is invisible to it and nothing says so. `epics-pva-rs`'s two
`client_native` guards asked one question with two lists that disagreed, between
them naming 7 of 11 files, and two live seam violations sat in the gap. A derived
set is default-in — a new file is swept on the commit that adds it, and a file
that genuinely cannot be swept has to be named with the reason beside it.

Three refusals keep the derivation from reading green over nothing:

- a directory with no `.rs` file at all (a module whose files moved up a level,
  leaving the directory behind);
- an exemption that names no file under the directory, since an exemption list
  goes stale the same way a subject list does;
- nothing surviving the exemptions.

`rust_sources` is `sweep` without the exemption half: every `.rs` file under a
directory, recursively, sorted, both halves `'static` so a derived set drops
straight into a guard that used to carry `[(&str, &str); N]` by hand.
Directories are read once per test binary, and slices are cached by source
address, length and comment policy.

## Why the text is passed in

`include_str!` cannot cross a crate boundary, so each guard keeps its own
`include_str!` and passes the text here — this crate never names a file of its
own. The consequence of `publish = false` is the intended one: the tests inside
a published `.crate` do not compile against a registry checkout. They are
guards over this workspace's source, and this workspace is where they run.

Taken by `epics-base-rs`, `epics-libcom-rs`, `epics-ca-rs`, `epics-pva-rs`,
`epics-bridge-rs`, `epics-rtems-boot`, `asyn-rs`, `ad-core-rs`,
`ad-plugins-rs`, and `tools/record-seam-gate`.

## Testing

```bash
cargo nextest run -p source-guard
```

14 tests, one per boundary of the rule. `tests/slice_rule.rs` keeps the
positional rule it replaced as a named function, so every test that disproves it
fails if `production` ever goes back to it.

## License

[EPICS Open License](LICENSE)
