//! The resolver itself, gated like every other test that reads a reference
//! tree.
//!
//! This lives in its own integration binary rather than beside
//! `epics_base_rs::reference` because CI has no reference checkout, and the
//! only lever that can exclude a test there is `binary(...)`. Left in the lib
//! it would take the whole `epics-base-rs` unit-test binary down with it.

use epics_base_rs::reference::{ReferenceTree, reference_path, reference_root};
use epics_base_rs::runtime::sync::CMutex;

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

/// `epicsMutexShowAll` lists what C lists: `PriorityInheritanceMutex::new`
/// takes a [`CMutex`], so every listed lock names the C `epicsMutexCreate` it
/// stands for, and each name is checked here against the C source.
/// Whether `lines[at]` creates `c_name`: an `epicsMutexCreate` call storing
/// into it, or, for `Class::member`, that member of that class declared as an
/// `epicsMutex`, whose constructor makes the call (`epicsMutex.cpp:152`).
fn creates(lines: &[&str], at: usize, c_name: &str) -> bool {
    let source = lines[at].trim();
    match c_name.split_once("::") {
        None => {
            source.contains(c_name)
                && (source.contains("epicsMutexCreate()")
                    || source.contains("epicsMutexMustCreate()"))
        }
        Some((class, member)) => {
            let declares = source.strip_prefix("mutable ").unwrap_or(source)
                == format!("epicsMutex {member};");
            let enclosing = lines[..at]
                .iter()
                .rev()
                .find_map(|l| l.strip_prefix("class "))
                .and_then(|rest| rest.split_whitespace().next());
            declares && enclosing == Some(class)
        }
    }
}

#[test]
fn every_listed_lock_names_its_c_epics_mutex_create() {
    let mut claimed = std::collections::HashSet::new();
    for &c_mutex in CMutex::ALL {
        let path = reference_path(ReferenceTree::Base, c_mutex.c_file());
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        for &line in c_mutex.c_lines() {
            let at = line as usize - 1;
            let source = lines.get(at).copied().unwrap_or("");
            assert!(
                at < lines.len() && creates(&lines, at, c_mutex.c_name()),
                "{c_mutex:?} names {} at {}:{line}, which does not create it: {source:?}",
                c_mutex.c_name(),
                c_mutex.c_file()
            );
            assert!(
                claimed.insert((c_mutex.c_file(), line)),
                "{c_mutex:?} names {}:{line}, which another CMutex already names",
                c_mutex.c_file()
            );
        }
    }
}
