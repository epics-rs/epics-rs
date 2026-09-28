//! Reference-counted, read-only element buffer behind every
//! [`TypedScalarArray`](super::TypedScalarArray) variant.
//!
//! pvxs `shared_array<const T>` can wrap memory it does not own — ADCore's
//! NTNDArray converter hands it the detector frame with a deleter that
//! releases the `NDArray` (ntndArrayConverter.cpp:425-429), so publishing a
//! frame copies nothing. `Arc<[T]>` cannot express that: turning a `Vec<T>`
//! or a foreign buffer into one is always a copy. [`PvArray`] keeps the
//! `Arc<[T]>` form for buffers built here and adds an owner-backed form
//! that borrows its elements from any `Arc` that can show them as a slice.

use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

/// A `[T]` that lives as long as any clone of the `PvArray` holding it.
enum Inner<T: 'static> {
    Slice(Arc<[T]>),
    Owner(Arc<dyn AsRef<[T]> + Send + Sync>),
}

/// Cheaply clonable, read-only `[T]`. Deref to the slice for reads.
pub struct PvArray<T: 'static> {
    inner: Inner<T>,
}

impl<T: 'static> PvArray<T> {
    /// Borrow the elements from `owner` for as long as this array (or any
    /// clone of it) lives; nothing is copied.
    pub fn from_owner(owner: Arc<dyn AsRef<[T]> + Send + Sync>) -> Self {
        Self {
            inner: Inner::Owner(owner),
        }
    }

    /// The elements.
    pub fn as_slice(&self) -> &[T] {
        match &self.inner {
            Inner::Slice(a) => a,
            Inner::Owner(o) => o.as_ref().as_ref(),
        }
    }

    /// The `Arc<[T]>` behind a slice-backed array, `None` for an
    /// owner-backed one; lets another shared-buffer type adopt the
    /// elements without copying them.
    pub fn as_arc(&self) -> Option<&Arc<[T]>> {
        match &self.inner {
            Inner::Slice(a) => Some(a),
            Inner::Owner(_) => None,
        }
    }

    /// Whether both arrays read the same memory.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.as_slice(), other.as_slice())
    }
}

impl<T: 'static> Deref for PvArray<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T: 'static> AsRef<[T]> for PvArray<T> {
    fn as_ref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T: 'static> Clone for PvArray<T> {
    fn clone(&self) -> Self {
        Self {
            inner: match &self.inner {
                Inner::Slice(a) => Inner::Slice(Arc::clone(a)),
                Inner::Owner(o) => Inner::Owner(Arc::clone(o)),
            },
        }
    }
}

impl<T: fmt::Debug + 'static> fmt::Debug for PvArray<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_slice(), f)
    }
}

impl<T: PartialEq + 'static> PartialEq for PvArray<T> {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: 'static> From<Arc<[T]>> for PvArray<T> {
    fn from(a: Arc<[T]>) -> Self {
        Self {
            inner: Inner::Slice(a),
        }
    }
}

impl<T: 'static> From<Box<[T]>> for PvArray<T> {
    fn from(b: Box<[T]>) -> Self {
        Self::from(Arc::<[T]>::from(b))
    }
}

/// Takes the vector as it is; the elements are not copied.
impl<T: Send + Sync + 'static> From<Vec<T>> for PvArray<T> {
    fn from(v: Vec<T>) -> Self {
        Self::from_owner(Arc::new(v))
    }
}

impl<T: Clone + Send + Sync + 'static> From<&[T]> for PvArray<T> {
    fn from(s: &[T]) -> Self {
        Self::from(s.to_vec())
    }
}

impl<T: Clone + Send + Sync + 'static, const N: usize> From<[T; N]> for PvArray<T> {
    fn from(a: [T; N]) -> Self {
        Self::from(a.to_vec())
    }
}

impl<T: Send + Sync + 'static> FromIterator<T> for PvArray<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self::from(iter.into_iter().collect::<Vec<T>>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vec_is_taken_without_copying() {
        let v = vec![1u16, 2, 3];
        let ptr = v.as_ptr();
        let a = PvArray::from(v);
        assert_eq!(a.as_ptr(), ptr);
        assert_eq!(&*a, &[1, 2, 3]);
    }

    #[test]
    fn an_owner_is_borrowed_and_kept_alive_by_every_clone() {
        struct Frame(Vec<u8>);
        impl AsRef<[u8]> for Frame {
            fn as_ref(&self) -> &[u8] {
                &self.0
            }
        }
        let frame = Arc::new(Frame(vec![7, 8, 9]));
        let a = PvArray::from_owner(Arc::clone(&frame) as Arc<dyn AsRef<[u8]> + Send + Sync>);
        let b = a.clone();
        assert!(a.ptr_eq(&b));
        assert_eq!(a.as_ptr(), frame.0.as_ptr());
        assert_eq!(Arc::strong_count(&frame), 3);
        drop(a);
        drop(b);
        assert_eq!(Arc::strong_count(&frame), 1);
    }

    #[test]
    fn equality_is_by_element() {
        let a: PvArray<f64> = vec![1.0, 2.0].into();
        let b = PvArray::from(Arc::<[f64]>::from(vec![1.0, 2.0]));
        assert_eq!(a, b);
        assert!(!a.ptr_eq(&b));
    }
}
