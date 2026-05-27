//! Copy-on-write array container. Mirrors `pvxs::shared_array<T>`.
//!
//! `SharedArray<T>` wraps `Arc<[T]>` so cloning is a refcount bump
//! (no heap allocation, no element copy). When a writer needs to
//! mutate, [`SharedArray::make_unique`] forks off a private copy
//! the first time the refcount is shared, leaving every other
//! holder pointing at the original buffer.
//!
//! Use cases:
//! - Fan-out a monitor data buffer to N downstream subscribers
//!   without N memcpy's. Each subscriber holds an `Arc<[T]>` clone.
//! - Pass large `ScalarArray<Double>` values around inside the
//!   server without paying the per-clone copy that `Vec<T>` would.
//!
//! This is provided alongside the existing `Vec<T>`-backed
//! `PvField::*Array` variants — not a replacement. Migration of
//! the wire-protocol path to `SharedArray<T>` would be a separate
//! cross-cutting change; this type unblocks new APIs that want
//! pvxs-shape COW semantics today.

use std::ops::Deref;
use std::sync::Arc;

/// Copy-on-write slice. Cheap clone (refcount bump only); mutation
/// forks a unique private copy the first time the refcount is
/// shared.
#[derive(Clone)]
pub struct SharedArray<T> {
    inner: Arc<[T]>,
}

impl<T> SharedArray<T> {
    /// Wrap an `Arc<[T]>` directly — useful when interop already
    /// produced one (e.g. `Bytes::into_arc_slice`).
    pub fn from_arc(inner: Arc<[T]>) -> Self {
        Self { inner }
    }

    /// Build from a `Vec<T>`. Allocates a fresh `Arc<[T]>` and
    /// moves the elements; equivalent to `Arc::from(vec.into_boxed_slice())`.
    pub fn from_vec(v: Vec<T>) -> Self {
        Self {
            inner: Arc::from(v.into_boxed_slice()),
        }
    }

    /// Build from an `&[T]` by cloning each element. O(n) and
    /// requires `T: Clone`.
    pub fn from_slice(slice: &[T]) -> Self
    where
        T: Clone,
    {
        Self::from_vec(slice.to_vec())
    }

    /// Construct an empty array (no allocation beyond the empty
    /// `Arc<[T]>` shell).
    pub fn empty() -> Self {
        Self {
            inner: Arc::from(Vec::<T>::new().into_boxed_slice()),
        }
    }

    /// Element count.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Borrow the underlying slice.
    pub fn as_slice(&self) -> &[T] {
        &self.inner
    }

    /// Number of strong references to the buffer. `1` after
    /// [`Self::make_unique`] (or when this is the only holder).
    /// Useful for tests and for opportunistic in-place mutation
    /// without the make_unique overhead.
    pub fn ref_count(&self) -> usize {
        Arc::strong_count(&self.inner)
    }

    /// True iff this is the only holder. When false, a write would
    /// trigger a copy.
    pub fn is_unique(&self) -> bool {
        Arc::strong_count(&self.inner) == 1
    }

    /// Get a mutable slice. If the underlying `Arc<[T]>` is shared,
    /// this allocates a fresh private copy first (the COW step) so
    /// other clones aren't disturbed.
    pub fn make_unique(&mut self) -> &mut [T]
    where
        T: Clone,
    {
        if Arc::strong_count(&self.inner) > 1 {
            let cloned: Vec<T> = self.inner.iter().cloned().collect();
            self.inner = Arc::from(cloned.into_boxed_slice());
        }
        // By the time we get here, strong_count == 1, so there's no
        // other holder. `Arc::get_mut` is the safe API and returns
        // Some when unique.
        Arc::get_mut(&mut self.inner).expect("shared array must be unique after make_unique")
    }

    /// Immutable iterator. Same as `<&[T] as IntoIterator>::iter`.
    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.inner.iter()
    }

    /// Consume and return a `Vec<T>`. Always allocates fresh storage
    /// (the underlying `Arc<[T]>` cannot be re-interpreted as a
    /// `Vec`). Use sparingly on hot paths.
    pub fn into_vec(self) -> Vec<T>
    where
        T: Clone,
    {
        self.inner.iter().cloned().collect()
    }
}

impl<T> Default for SharedArray<T> {
    fn default() -> Self {
        Self::empty()
    }
}

impl<T> Deref for SharedArray<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.inner
    }
}

impl<T> AsRef<[T]> for SharedArray<T> {
    fn as_ref(&self) -> &[T] {
        &self.inner
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for SharedArray<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedArray")
            .field("len", &self.len())
            .field("ref_count", &self.ref_count())
            .field("data", &self.as_slice())
            .finish()
    }
}

impl<T: PartialEq> PartialEq for SharedArray<T> {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: Eq> Eq for SharedArray<T> {}

impl<T> From<Vec<T>> for SharedArray<T> {
    fn from(v: Vec<T>) -> Self {
        Self::from_vec(v)
    }
}

impl<T: Clone> From<&[T]> for SharedArray<T> {
    fn from(s: &[T]) -> Self {
        Self::from_slice(s)
    }
}

impl<T> From<Arc<[T]>> for SharedArray<T> {
    fn from(a: Arc<[T]>) -> Self {
        Self::from_arc(a)
    }
}

impl<'a, T> IntoIterator for &'a SharedArray<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_construction() {
        let a: SharedArray<i32> = SharedArray::empty();
        assert!(a.is_empty());
        assert_eq!(a.len(), 0);
    }

    #[test]
    fn clone_is_refcount_bump() {
        let a = SharedArray::from_vec(vec![1, 2, 3, 4, 5]);
        assert_eq!(a.ref_count(), 1);
        let b = a.clone();
        assert_eq!(a.ref_count(), 2);
        assert_eq!(b.ref_count(), 2);
        // Both views reflect the same data — same pointer under
        // the hood.
        assert_eq!(a.as_slice(), &[1, 2, 3, 4, 5]);
        assert_eq!(b.as_slice(), &[1, 2, 3, 4, 5]);
        assert_eq!(a.as_ptr(), b.as_ptr());
    }

    #[test]
    fn make_unique_forks_when_shared() {
        let mut a = SharedArray::from_vec(vec![1, 2, 3]);
        let b = a.clone();
        assert_eq!(a.ref_count(), 2);
        // First mutation forks
        let slice = a.make_unique();
        slice[0] = 99;
        // a now sees the change, b still sees the original
        assert_eq!(a.as_slice(), &[99, 2, 3]);
        assert_eq!(b.as_slice(), &[1, 2, 3]);
        // Refcounts split
        assert!(a.is_unique());
        assert!(b.is_unique());
    }

    #[test]
    fn make_unique_when_already_unique_skips_copy() {
        let mut a = SharedArray::from_vec(vec![1, 2, 3]);
        let ptr_before = a.as_ptr();
        let _ = a.make_unique();
        let ptr_after = a.as_ptr();
        // No fork happened — same allocation.
        assert_eq!(ptr_before, ptr_after);
    }

    #[test]
    fn from_slice_clones_elements() {
        let buf = [10u8, 20, 30];
        let a = SharedArray::from_slice(&buf);
        assert_eq!(a.as_slice(), &buf);
    }

    #[test]
    fn deref_to_slice_works_for_iter_and_index() {
        let a = SharedArray::from_vec(vec![5, 6, 7]);
        assert_eq!(a[0], 5);
        let sum: i32 = a.iter().sum();
        assert_eq!(sum, 18);
    }

    #[test]
    fn equality_compares_contents() {
        let a = SharedArray::from_vec(vec![1, 2, 3]);
        let b = SharedArray::from_vec(vec![1, 2, 3]);
        let c = SharedArray::from_vec(vec![1, 2]);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn into_iter_borrowed_yields_refs() {
        let a = SharedArray::from_vec(vec![1, 2, 3]);
        let collected: Vec<i32> = (&a).into_iter().copied().collect();
        assert_eq!(collected, vec![1, 2, 3]);
    }

    #[test]
    fn into_vec_round_trip() {
        let a = SharedArray::from_vec(vec![1, 2, 3]);
        let v = a.into_vec();
        assert_eq!(v, vec![1, 2, 3]);
    }

    #[test]
    fn fan_out_share_pattern_n_cheap_clones() {
        // Common usage: build once, hand to 100 subscribers. Each
        // clone is a refcount bump, no element copies.
        let original = SharedArray::from_vec(vec![0u8; 1024]);
        let mut subscribers: Vec<SharedArray<u8>> = Vec::with_capacity(100);
        for _ in 0..100 {
            subscribers.push(original.clone());
        }
        // 1 original + 100 clones = 101 strong refs.
        assert_eq!(original.ref_count(), 101);
        for sub in &subscribers {
            assert_eq!(sub.as_slice().len(), 1024);
        }
    }
}
