//! Locating the upstream C/C++ reference trees that parity tests read.
//!
//! Parity tests assert the port against the real EPICS base / synApps /
//! pvxs sources rather than against a transcription of them, so they need
//! those trees on disk. Each such test used to resolve its own path and
//! *skip* when the resolution failed:
//!
//! ```text
//! SKIP stdsupport_device_rows_are_all_accounted_for: stdSupport.dbd not
//! found (set EPICS_MODULES or place the reference checkout).
//! ```
//!
//! nextest reports that test as `PASS`. A skip is indistinguishable from a
//! pass in every report anyone reads, so a guard that cannot find its
//! reference verifies nothing while still reporting green — which is how an
//! epid device-support gap survived eleven review rounds with an allowlist
//! entry that named it.
//!
//! This module is the single resolver, and it has no skip: failing to
//! resolve panics, so the vacuous outcome is unconstructible rather than
//! merely discouraged. Resolution order is an explicit environment override
//! first, then a walk up the ancestors of this crate's manifest directory
//! looking for a conventionally-named sibling checkout — so it works from a
//! git worktree, from a nested checkout, and on any machine, without
//! hard-coding a developer's home directory (the previous
//! `/Users/<name>/codes/...` defaults are exactly what went stale).

use std::path::{Path, PathBuf};

/// One upstream reference tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceTree {
    /// EPICS base — upstream for `epics-base-rs` and `epics-ca-rs`.
    Base,
    /// The synApps module collection (`std`, `calc`, `asyn`, `motor`, ...).
    Modules,
    /// pvxs — upstream for `epics-pva-rs`.
    Pvxs,
}

impl ReferenceTree {
    /// Environment variable that overrides discovery for this tree.
    pub fn env_var(self) -> &'static str {
        match self {
            ReferenceTree::Base => "EPICS_BASE",
            ReferenceTree::Modules => "EPICS_MODULES",
            ReferenceTree::Pvxs => "PVXS_HOME",
        }
    }

    /// Directory names to probe under each ancestor, most specific first.
    fn candidates(self) -> &'static [&'static str] {
        match self {
            ReferenceTree::Base => &["epics-base", "work/epics-base"],
            ReferenceTree::Modules => &["epics-modules", "work/epics-modules"],
            ReferenceTree::Pvxs => &[
                "epics-modules/pvxs",
                "work/epics-modules/pvxs",
                "pvxs",
                "work/pvxs",
            ],
        }
    }

    /// A file that must exist in a correctly-resolved tree, so a directory
    /// that merely has the right name cannot pass for the real checkout.
    fn sentinel(self) -> &'static str {
        match self {
            ReferenceTree::Base => "modules/database/src/ioc/db/dbAccess.c",
            ReferenceTree::Modules => "std/stdApp/src/epidRecord.c",
            ReferenceTree::Pvxs => "src/clientreq.cpp",
        }
    }
}

/// Resolve the root of a reference tree, or panic explaining how to fix it.
///
/// Panics rather than returning `Option` on purpose: every caller is a
/// parity test, and the one outcome that must be impossible is "reported
/// green without reading the reference".
pub fn reference_root(tree: ReferenceTree) -> PathBuf {
    if let Some(root) = try_reference_root(tree) {
        return root;
    }
    panic!(
        "{:?} reference tree not found. This test compares the port against \
         the upstream sources and cannot run without them — it fails rather \
         than skipping, because a skip reports as a pass and verifies \
         nothing.\n  Set {} to the checkout, or place it next to this \
         repository as one of: {}.\n  Searched upward from {}.",
        tree,
        tree.env_var(),
        tree.candidates().join(", "),
        env!("CARGO_MANIFEST_DIR"),
    );
}

/// The non-panicking half, for callers that must distinguish "no checkout"
/// from "checkout present but the artifact inside it is absent".
pub fn try_reference_root(tree: ReferenceTree) -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var(tree.env_var()) {
        let root = PathBuf::from(explicit);
        if root.join(tree.sentinel()).is_file() {
            return Some(root);
        }
        panic!(
            "{} is set to {:?}, but that is not a {:?} checkout — {} is \
             missing. Point it at the real tree or unset it.",
            tree.env_var(),
            root,
            tree,
            tree.sentinel(),
        );
    }
    for ancestor in Path::new(env!("CARGO_MANIFEST_DIR")).ancestors() {
        for candidate in tree.candidates() {
            let root = ancestor.join(candidate);
            if root.join(tree.sentinel()).is_file() {
                return Some(root);
            }
        }
    }
    None
}

/// Resolve one file inside a reference tree, or panic.
///
/// A tree that resolves but does not contain `relative` is just as fatal:
/// it means the test is reading a path that upstream moved or renamed, and
/// silently skipping would hide that.
pub fn reference_path(tree: ReferenceTree, relative: &str) -> PathBuf {
    let path = reference_root(tree).join(relative);
    assert!(
        path.exists(),
        "{relative} is missing from the {tree:?} checkout at {:?}. Upstream \
         may have moved it; this test cannot verify anything without it.",
        reference_root(tree),
    );
    path
}
