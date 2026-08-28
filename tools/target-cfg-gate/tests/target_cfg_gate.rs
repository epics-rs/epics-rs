//! The workspace-wide half of the rule. The derivation's own boundary cases
//! ride in `src/lib.rs`; this is the one that reads the tree.

#[test]
fn every_target_declares_the_cfg_its_imports_require() {
    target_cfg_gate::assert_workspace_targets_declare_their_cfg();
}
