//! The resolver itself, gated like every other test that reads a reference
//! tree.
//!
//! This lives in its own integration binary rather than beside
//! `epics_base_rs::reference` because CI has no reference checkout, and the
//! only lever that can exclude a test there is `binary(...)`. Left in the lib
//! it would take the whole `epics-base-rs` unit-test binary down with it.

use epics_base_rs::reference::{ReferenceTree, reference_root};

/// Every tree this workspace's parity tests read must resolve. Failing here
/// means the machine is missing a checkout — the condition that used to be
/// reported as a pass.
#[test]
fn every_reference_tree_resolves() {
    for tree in [
        ReferenceTree::Base,
        ReferenceTree::Modules,
        ReferenceTree::Pvxs,
    ] {
        reference_root(tree);
    }
}
