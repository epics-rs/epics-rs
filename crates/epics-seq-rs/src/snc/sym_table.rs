//! Symbol table (C `sym_table.c`).
//!
//! A finite map from `(name, type)` keys to values. C keys the second half
//! by an object's address (`void *type`); here it is any hashable `K`. C
//! gives zero values no meaning, so inserting zero is a no-op there; Rust
//! values cannot be zero and that case does not arise.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::hash::Hash;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct SymTable<K, V> {
    table: HashMap<K, HashMap<Arc<str>, V>>,
}

impl<K: Hash + Eq, V: Copy> Default for SymTable<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Hash + Eq, V: Copy> SymTable<K, V> {
    /// C `sym_table_create`.
    pub fn new() -> Self {
        SymTable {
            table: HashMap::new(),
        }
    }

    /// C `sym_table_lookup`: the value under `(name, ty)`, if any.
    pub fn lookup(&self, name: &str, ty: K) -> Option<V> {
        self.table.get(&ty)?.get(name).copied()
    }

    /// C `sym_table_insert`: insert `value` under `(name, ty)` unless the
    /// key is already bound. Returns whether it was inserted.
    pub fn insert(&mut self, name: &Arc<str>, ty: K, value: V) -> bool {
        match self.table.entry(ty).or_default().entry(name.clone()) {
            Entry::Occupied(_) => false,
            Entry::Vacant(e) => {
                e.insert(value);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_refuses_bound_key() {
        let mut st = SymTable::new();
        let n: Arc<str> = Arc::from("x");
        assert!(st.insert(&n, 1, 'a'));
        assert!(!st.insert(&n, 1, 'b'));
        assert!(st.insert(&n, 2, 'c'));
        assert_eq!(st.lookup("x", 1), Some('a'));
        assert_eq!(st.lookup("x", 2), Some('c'));
        assert_eq!(st.lookup("y", 1), None);
    }
}
