//! # C reference pins
//!
//! Every `file.c:NNN` citation in this crate resolves at the tree and revision
//! below, not at whatever that tree's working copy holds today. These trees are
//! checked out on local branches here and run ahead of their pins.
//!
//! | tree | pinned revision |
//! | --- | --- |
//! | `ADCore` | `R3-14-111-g6c53844e` |
//! | `ADSupport` | *no settled pin* |
//! | `epics-base` | `R7.0.10` |
//! | `pvxs` | *no settled pin* |
//! | `asyn` | `R4-45-19-ge2a281e2` |
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
    clippy::approx_constant,
    clippy::collapsible_if,
    clippy::erasing_op,
    clippy::identity_op,
    clippy::manual_is_multiple_of,
    clippy::manual_range_contains,
    clippy::needless_range_loop,
    clippy::new_without_default,
    clippy::op_ref,
    clippy::type_complexity,
    clippy::too_many_arguments
)]

pub mod attr_plot;
pub mod attribute;
pub mod bad_pixel;
pub mod circular_buff;
pub mod codec;
pub mod color_convert;
pub mod fft;
pub mod file_hdf5;
pub mod file_jpeg;
pub mod file_magick;
pub mod file_netcdf;
pub mod file_nexus;
pub mod file_tiff;
pub mod gather;
pub mod hdf5_layout;
pub mod overlay;
pub mod overlay_font;
pub mod par_util;
pub mod passthrough;
pub mod pos_plugin;
pub mod process;
pub mod roi;
pub mod roi_stat;
pub mod scatter;
pub mod stats;
pub mod std_arrays;
pub mod time_series;
pub mod time_series_plugin;
pub mod transform;

#[cfg(feature = "pva")]
pub mod pva;

// The moved code's own requirement, and nothing else: `ArgValue` comes from
// `epics-base-rs`, an unconditional dependency, and its own gate is the target
// — the iocsh registry is absent on RTEMS and VxWorks, not reactor-dependent.
// So neither `ioc` nor `tokio_backend` belongs here. Carrying `ioc` over from
// the module this came from left the three boundary cases unselected in the
// default configuration, which was the same mistake one layer smaller.
#[cfg(not(epics_embedded_target))]
pub mod attr_plot_args;

// `tokio_backend` as well as the feature: the module stands its IOC up on
// `epics_ca_rs::server::run_ca_ioc_app` and the QSRV runner, and neither
// exists on the reactor-free backend.
#[cfg(all(feature = "ioc", tokio_backend))]
pub mod ioc;
