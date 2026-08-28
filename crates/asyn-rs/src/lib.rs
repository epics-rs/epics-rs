//! Rust port of EPICS **asyn**.
//!
//! # C reference pins
//!
//! Every `file.c:NNN` citation in this crate resolves at the tree and revision
//! below, not at whatever that tree's working copy holds today. These trees are
//! checked out on local branches here and run ahead of their pins.
//!
//! The pin is not academic. `drvAsynIPPort.c` is 34 lines longer at the asyn
//! checkout (`731d616e`) than at the pin, so `setNonBlock(fd, 1)` reads as
//! `:511` there and `:536` here. A citation checked against the wrong tree
//! looks wrong while being right, and one written against the wrong tree is
//! wrong here — both happened this round.
//!
//! | tree | pinned revision |
//! | --- | --- |
//! | `asyn` | `R4-45-19-ge2a281e2` |
//! | `epics-base` | `R7.0.10` |
//! | `modbus` | `R3-4-10-gb1009d0` |
//! | `motor` | `R7-4-5-g78b474cd` |
//! | `ADCore` | `R3-14-111-g6c53844e` |
//! | `busy` | `R1-7-4-6-g2dfe92d` |
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

#![allow(
    unused_imports,
    clippy::approx_constant,
    clippy::collapsible_if,
    clippy::derivable_impls,
    clippy::if_same_then_else,
    clippy::manual_range_contains,
    clippy::single_match,
    clippy::unnecessary_map_or
)]

pub mod drivers;
pub mod error;
pub(crate) mod escape;
// Public: `PortManager::exception_manager` / `PortServices::exceptions` hand
// out an `Arc<ExceptionManager>`, and a caller registering an exception
// callback needs to name `AsynException` to match on it.
pub mod exception;
pub mod interfaces;
pub mod interpose;
pub mod interrupt;
pub mod manager;
pub mod param;
pub mod port;
pub(crate) mod port_actor;
pub mod port_handle;
pub(crate) mod protocol;
pub mod registry;
pub mod request;
pub mod runtime;
pub mod services;
pub mod sync_io;
pub mod timestamp;
pub mod trace;
pub(crate) mod transport;
pub mod user;

#[cfg(feature = "epics")]
pub mod adapter;
#[cfg(feature = "epics")]
pub mod asyn_record;
/// The asyn device-support DTYP menus, generated from the vendored asyn `.dbd`
/// by `tools/dbd-codegen` — the same path base and every other downstream crate
/// use. `crate::adapter::register_asyn_device_menus` hands each entry to base's
/// `register_device_menu` so a client reading e.g. `mbbo.DTYP` sees the asyn
/// choices a C fat softIoc lists. Gated on `epics` because it names
/// `epics_base_rs` types.
#[cfg(feature = "epics")]
pub mod dbd_generated;
#[cfg(feature = "epics")]
pub mod iocsh;
