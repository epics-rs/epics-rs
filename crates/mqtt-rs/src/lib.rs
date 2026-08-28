//! # C reference pins
//!
//! Every `file.c:NNN` citation in this crate resolves at the tree and revision
//! below, not at whatever that tree's working copy holds today. These trees are
//! checked out on local branches here and run ahead of their pins.
//!
//! | tree | pinned revision |
//! | --- | --- |
//! | `mqtt` | `278a6a4` (tree carries no tags) |
//! | `autoparamDriver` | `v2.1.0` |
//! | `asyn` | `R4-45-19-ge2a281e2` |
//! | `epics-base` | `R7.0.10` |
//!
//! **Resolve by symbol at the pin; the line is a hint.** Find the named
//! function, struct, macro or field first, and treat the line number as a hint
//! that has to land inside that construct. Three cases follow:
//!
//! 1. Construct at the pin, line lands in it — the citation is exact. A
//!    reference checkout ahead of the pin will disagree; that disagreement is
//!    the checkout's, not the citation's.
//! 2. Construct at the pin, line lands outside it — line drift. Keep the
//!    symbol and move the line to the pin's.
//! 3. Construct absent at the pin — the citation means code added after it,
//!    and is NOT moved onto the pin, where it would point at lines that do not
//!    exist. It names the revision it means inline, beside the line span: the
//!    upstream PR and commit, and that both are later than the pin this table
//!    gives. `epics-libcom-rs` already carries that form.
//!
//! Every pin above passes `git merge-base --is-ancestor <pin> origin/<default>`
//! in its own tree, which is the test a pin has to meet. A `git describe`
//! string names an exact commit and is worth as much as a tag; what
//! disqualifies a revision is being reachable only from a fork branch or an
//! unmerged PR, because then it names nothing a reader outside this workspace
//! can fetch.
//!
//! Resolve each citation on its own. One sentence can cite two lines that are
//! right at different revisions, and a check run at either revision then
//! reports a single tidy error while vouching for the very citation the other
//! condemns.
//!
//! A row reading *no settled pin* means no revision has been agreed for that
//! tree: say which revision you read, and do not take its `HEAD` for the pin.
//! Citations into non-EPICS sources (libc, RTEMS, `rtems-libbsd`, VxWorks,
//! vendored third-party) are outside this table and carry no pin.

pub mod address;
pub mod config;
#[cfg(feature = "ioc")]
mod db_gen;
pub mod driver;
pub mod error;
pub mod event_loop;
#[cfg(feature = "ioc")]
pub mod ioc;
pub mod payload;
#[cfg(feature = "ioc")]
pub mod z2m;
