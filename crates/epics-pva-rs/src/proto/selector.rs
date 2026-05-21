//! Field selectors used by `pvRequest` to limit which sub-fields the server
//! sends back.
//!
//! At wire level, "selector" really just means a [`BitSet`]
//! over the same field-numbering scheme used for monitor deltas. This module
//! exists as a named type so callers can express intent ("which fields the
//! client asked for") separately from the bit container.

use super::bitset::BitSet;

/// Newtype wrapper around [`BitSet`] for `pvRequest` field selection. The bit
/// numbering must match the structure descriptor's depth-first field order
/// (root = bit 0).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selector {
    bits: BitSet,
}

impl Selector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Selector that selects all fields up to `nbits` (i.e. "no filter").
    pub fn all(nbits: usize) -> Self {
        Self {
            bits: BitSet::all_set(nbits),
        }
    }

    pub fn select(&mut self, field_index: usize) -> &mut Self {
        self.bits.set(field_index);
        self
    }

    pub fn deselect(&mut self, field_index: usize) -> &mut Self {
        self.bits.clear(field_index);
        self
    }

    pub fn is_selected(&self, field_index: usize) -> bool {
        self.bits.get(field_index)
    }

    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.bits.iter()
    }

    pub fn into_bits(self) -> BitSet {
        self.bits
    }

    pub fn as_bits(&self) -> &BitSet {
        &self.bits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_and_iterate() {
        let mut sel = Selector::new();
        sel.select(0).select(3).select(7);
        let collected: Vec<_> = sel.iter().collect();
        assert_eq!(collected, vec![0, 3, 7]);
        assert!(sel.is_selected(0));
        assert!(!sel.is_selected(1));
    }

    #[test]
    fn all_selector_covers_range() {
        let sel = Selector::all(10);
        for i in 0..10 {
            assert!(sel.is_selected(i));
        }
        assert!(!sel.is_selected(10));
    }
}
