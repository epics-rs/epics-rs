//! PVI / `asynParamSet` hierarchical parameter tree (asyn PR #117).
//!
//! C asyn's flat `paramList` (named string → integer index) doesn't
//! scale for instruments with many parameters grouped logically —
//! a 16-channel motor controller has e.g. `axis1:limits:high`,
//! `axis1:limits:low`, `axis1:position`, …, `axis2:limits:high` etc.
//! PR #117 introduced PVI (`PVInterface`) — a tree of named groups
//! containing parameters — and `asynParamSet` to manage groups
//! programmatically without scattered `add_param` calls.
//!
//! This module is a **scaffold**: it provides the name-resolution
//! data structure layered on top of the existing flat
//! [`crate::param::ParamList`], tested in isolation. Wiring it into
//! the port driver and drvInfo resolution is a follow-up — for now
//! ports continue using the flat `ParamList`, and `ParamTree`
//! exists so application code can build a hierarchical naming
//! scheme and resolve `"axis1/limits/high"` to a flat name before
//! calling `add_param`.
//!
//! ## Path syntax
//!
//! Segments are separated by `/` (POSIX-like) for readability.
//! C asyn uses `:` but `:` collides with PV naming conventions in
//! EPICS records — using `/` keeps the tree path distinguishable
//! from EPICS PV names. The resolved flat name uses `_` so it
//! survives EPICS dbd / record-name rules:
//!
//! ```text
//!   axis1/limits/high  →  axis1_limits_high
//! ```

use crate::error::{AsynError, AsynResult, AsynStatus};
use std::collections::HashMap;

/// Path separator in the hierarchical name. Distinguishable from
/// `:` (EPICS PV naming) and `.` (record field), and not a valid
/// character in C asyn parameter names — so a flat name can never
/// collide with a path segment by accident.
pub const PATH_SEP: char = '/';

/// Separator used when flattening a path for the underlying
/// [`crate::param::ParamList`]. Underscore survives EPICS dbd
/// record-name validation and matches existing asyn convention
/// for compound parameter names (e.g. `MOTOR_LIMITS_HIGH`).
pub const FLAT_SEP: char = '_';

/// A node in the parameter tree. Each leaf carries a flat-name
/// alias used to look up the underlying [`crate::param::ParamList`]
/// entry; each interior node owns a `HashMap` of children.
///
/// The tree owns the path strings — caller-supplied `&str`s are
/// always cloned so the tree can outlive temporaries from
/// `path.split('/')`.
#[derive(Debug, Default)]
pub struct ParamTree {
    root: Node,
}

#[derive(Debug, Default)]
struct Node {
    /// Map of child segment → subtree. `None` value at a leaf is
    /// represented by an empty `children` plus the leaf-marker
    /// `flat_name` set on the parent's entry — see `insert`.
    children: HashMap<String, Node>,
    /// When this node is a leaf, the flat-name alias for the
    /// underlying parameter (e.g. `axis1_limits_high`). `None`
    /// means this is an interior grouping node.
    flat_name: Option<String>,
}

impl ParamTree {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a hierarchical path as a leaf. Returns the flat-name
    /// alias that callers should pass to
    /// [`crate::param::ParamList::create_param`].
    ///
    /// Errors if the path is empty, contains an empty segment
    /// (e.g. `"a//b"`), or collides with an existing leaf at the
    /// same path.
    pub fn insert(&mut self, path: &str) -> AsynResult<String> {
        let segments = split_path(path)?;
        let flat = segments.join(&FLAT_SEP.to_string());

        let mut node = &mut self.root;
        let last = segments.len() - 1;
        for (i, seg) in segments.iter().enumerate() {
            // An interior node along the path can never be made
            // into a leaf later — guard against `a/b` then `a`.
            if i == last {
                let entry = node.children.entry(seg.clone()).or_default();
                if entry.flat_name.is_some() {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!("ParamTree: leaf already exists at '{path}'"),
                    });
                }
                if !entry.children.is_empty() {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!(
                            "ParamTree: '{path}' is an existing interior node, cannot make leaf"
                        ),
                    });
                }
                entry.flat_name = Some(flat.clone());
                return Ok(flat);
            }

            // Interior step. Reject if a leaf currently sits here.
            let entry = node.children.entry(seg.clone()).or_default();
            if entry.flat_name.is_some() {
                return Err(AsynError::Status {
                    status: AsynStatus::Error,
                    message: format!(
                        "ParamTree: '{seg}' is an existing leaf, cannot traverse"
                    ),
                });
            }
            node = entry;
        }
        // Unreachable: split_path guarantees non-empty.
        Ok(flat)
    }

    /// Look up a hierarchical path and return the flat-name alias
    /// if a leaf is registered there. Returns `None` for both
    /// missing paths and for interior nodes (you can't read a
    /// "group" as a parameter).
    pub fn resolve(&self, path: &str) -> Option<&str> {
        let segments = split_path(path).ok()?;
        let mut node = &self.root;
        for seg in &segments {
            node = node.children.get(seg)?;
        }
        node.flat_name.as_deref()
    }

    /// Iterate over every leaf in deterministic depth-first order,
    /// yielding `(hierarchical_path, flat_name)` pairs. Used by
    /// follow-up wiring code to walk a `ParamTree` and register
    /// every leaf with a flat [`crate::param::ParamList`] in one
    /// pass.
    pub fn iter_leaves(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        walk(&self.root, &mut Vec::new(), &mut out);
        out
    }

    /// Total number of leaves (parameters) in the tree.
    pub fn len(&self) -> usize {
        self.iter_leaves().len()
    }

    pub fn is_empty(&self) -> bool {
        self.root.children.is_empty()
    }
}

fn walk(node: &Node, stack: &mut Vec<String>, out: &mut Vec<(String, String)>) {
    // Sort children for deterministic iteration — HashMap order
    // would make tests flaky and complicate diffing in tools that
    // dump the tree.
    let mut keys: Vec<&String> = node.children.keys().collect();
    keys.sort();
    for k in keys {
        let child = &node.children[k];
        stack.push(k.clone());
        if let Some(flat) = &child.flat_name {
            out.push((stack.join(&PATH_SEP.to_string()), flat.clone()));
        } else {
            walk(child, stack, out);
        }
        stack.pop();
    }
}

fn split_path(path: &str) -> AsynResult<Vec<String>> {
    if path.is_empty() {
        return Err(AsynError::Status {
            status: AsynStatus::Error,
            message: "ParamTree: empty path".into(),
        });
    }
    let segments: Vec<String> = path
        .split(PATH_SEP)
        .map(|s| s.to_string())
        .collect();
    if segments.iter().any(|s| s.is_empty()) {
        return Err(AsynError::Status {
            status: AsynStatus::Error,
            message: format!("ParamTree: empty segment in '{path}'"),
        });
    }
    Ok(segments)
}

/// `asynParamSet` analog — programmatic builder for groups of
/// parameters that share a common path prefix. Convenience over
/// `ParamTree` for the common pattern where a port driver
/// instantiates the same set of parameters under multiple
/// addresses (`axis1/…`, `axis2/…`).
pub struct ParamGroupBuilder<'a> {
    tree: &'a mut ParamTree,
    prefix: String,
}

impl<'a> ParamGroupBuilder<'a> {
    pub fn new(tree: &'a mut ParamTree, prefix: &str) -> Self {
        Self {
            tree,
            prefix: prefix.to_string(),
        }
    }

    /// Add a parameter under the group prefix. Returns the flat
    /// name (e.g. `axis1_limits_high`) that the caller passes to
    /// [`crate::param::ParamList::create_param`].
    pub fn add(&mut self, leaf_path: &str) -> AsynResult<String> {
        let full = if self.prefix.is_empty() {
            leaf_path.to_string()
        } else {
            format!("{}{}{}", self.prefix, PATH_SEP, leaf_path)
        };
        self.tree.insert(&full)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_resolve_single_leaf() {
        let mut t = ParamTree::new();
        let flat = t.insert("axis1/limits/high").unwrap();
        assert_eq!(flat, "axis1_limits_high");
        assert_eq!(t.resolve("axis1/limits/high"), Some("axis1_limits_high"));
    }

    #[test]
    fn resolve_returns_none_for_interior_node() {
        let mut t = ParamTree::new();
        t.insert("axis1/limits/high").unwrap();
        assert_eq!(t.resolve("axis1"), None);
        assert_eq!(t.resolve("axis1/limits"), None);
    }

    #[test]
    fn resolve_returns_none_for_missing() {
        let mut t = ParamTree::new();
        t.insert("axis1/limits/high").unwrap();
        assert_eq!(t.resolve("axis2/limits/high"), None);
        assert_eq!(t.resolve("axis1/limits/low"), None);
    }

    #[test]
    fn insert_duplicate_leaf_errors() {
        let mut t = ParamTree::new();
        t.insert("axis1/limits/high").unwrap();
        let err = t.insert("axis1/limits/high").unwrap_err();
        matches!(err, AsynError::Status { .. });
    }

    #[test]
    fn insert_collides_with_existing_interior_errors() {
        let mut t = ParamTree::new();
        t.insert("axis1/limits/high").unwrap();
        // `axis1/limits` is interior — cannot also be a leaf.
        assert!(t.insert("axis1/limits").is_err());
    }

    #[test]
    fn insert_traverse_through_leaf_errors() {
        let mut t = ParamTree::new();
        t.insert("axis1/limits").unwrap();
        // `axis1/limits` is a leaf — can't drill into it.
        assert!(t.insert("axis1/limits/high").is_err());
    }

    #[test]
    fn insert_empty_path_errors() {
        let mut t = ParamTree::new();
        assert!(t.insert("").is_err());
    }

    #[test]
    fn insert_empty_segment_errors() {
        let mut t = ParamTree::new();
        assert!(t.insert("a//b").is_err());
        assert!(t.insert("/a").is_err());
        assert!(t.insert("a/").is_err());
    }

    #[test]
    fn iter_leaves_is_sorted_and_complete() {
        let mut t = ParamTree::new();
        t.insert("axis2/position").unwrap();
        t.insert("axis1/limits/high").unwrap();
        t.insert("axis1/limits/low").unwrap();
        t.insert("axis1/position").unwrap();
        let leaves = t.iter_leaves();
        assert_eq!(
            leaves,
            vec![
                ("axis1/limits/high".into(), "axis1_limits_high".into()),
                ("axis1/limits/low".into(), "axis1_limits_low".into()),
                ("axis1/position".into(), "axis1_position".into()),
                ("axis2/position".into(), "axis2_position".into()),
            ]
        );
        assert_eq!(t.len(), 4);
    }

    #[test]
    fn group_builder_prefixes_correctly() {
        let mut t = ParamTree::new();
        {
            let mut g = ParamGroupBuilder::new(&mut t, "axis1");
            assert_eq!(g.add("limits/high").unwrap(), "axis1_limits_high");
            assert_eq!(g.add("position").unwrap(), "axis1_position");
        }
        assert_eq!(t.resolve("axis1/limits/high"), Some("axis1_limits_high"));
        assert_eq!(t.resolve("axis1/position"), Some("axis1_position"));
    }

    #[test]
    fn group_builder_empty_prefix_is_root() {
        let mut t = ParamTree::new();
        {
            let mut g = ParamGroupBuilder::new(&mut t, "");
            assert_eq!(g.add("loose").unwrap(), "loose");
        }
        assert_eq!(t.resolve("loose"), Some("loose"));
    }
}
