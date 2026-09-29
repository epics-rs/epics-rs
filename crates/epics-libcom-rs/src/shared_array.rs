//! [`SharedArray`]: the array payload of an epics-base-rs `EpicsValue`, and
//! of an asyn array read, shared between a driver, a record, its monitor
//! snapshots and the wire encoders.

use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

/// A read-mostly array whose clones share one buffer.
///
/// C posts a waveform by reference and copies it into every client's send
/// buffer under the record lock (`dbEvent.c:727`, `dbAccess.c:1020`). Here
/// the record hands its buffer to the snapshot instead: a clone is a
/// reference-count bump, [`head`](Self::head) is a shorter view of the same
/// buffer, and [`make_mut`](Self::make_mut) copies only while another holder
/// still reads it.
pub struct SharedArray<T> {
    buf: Arc<[T]>,
    /// Elements served, `<= buf.len()`; the rest is the record's unused
    /// capacity (`NELM` past `NORD`).
    len: usize,
}

impl<T> SharedArray<T> {
    /// The first `max` elements as a view of the same buffer.
    pub fn head(&self, max: usize) -> Self {
        Self {
            buf: Arc::clone(&self.buf),
            len: self.len.min(max),
        }
    }

    /// Keep the first `n` elements; a no-op when `n >= len`.
    pub fn truncate(&mut self, n: usize) {
        self.len = self.len.min(n);
    }

    /// The served elements.
    pub fn as_slice(&self) -> &[T] {
        self
    }

    /// Whether both views read the same buffer.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.buf, &other.buf)
    }

    /// Whether another holder shares the buffer, so a write would copy.
    pub fn is_shared(&self) -> bool {
        Arc::strong_count(&self.buf) > 1
    }

    /// The whole buffer, unused capacity included.
    pub fn buffer(&self) -> &Arc<[T]> {
        &self.buf
    }
}

impl<T: Clone> SharedArray<T> {
    /// Write access to the elements: in place while this is the only
    /// holder, into a fresh copy otherwise (`Arc::make_mut`).
    pub fn make_mut(&mut self) -> &mut [T] {
        if self.len != self.buf.len() {
            self.buf = self.buf[..self.len].into();
        }
        Arc::make_mut(&mut self.buf)
    }

    /// Resize to `n` elements, filling with `value`; growth copies.
    pub fn resize(&mut self, n: usize, value: T) {
        if n <= self.len {
            self.len = n;
        } else {
            let mut v = Vec::with_capacity(n);
            v.extend_from_slice(self);
            v.resize(n, value);
            *self = v.into();
        }
    }

    /// Append `value`; copies the buffer.
    pub fn push(&mut self, value: T) {
        let mut v = self.to_vec();
        v.push(value);
        *self = v.into();
    }
}

impl<T> Deref for SharedArray<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.buf[..self.len]
    }
}

impl<T> AsRef<[T]> for SharedArray<T> {
    fn as_ref(&self) -> &[T] {
        self
    }
}

impl<T> Clone for SharedArray<T> {
    fn clone(&self) -> Self {
        Self {
            buf: Arc::clone(&self.buf),
            len: self.len,
        }
    }
}

impl<T> Default for SharedArray<T> {
    fn default() -> Self {
        Vec::new().into()
    }
}

impl<T: fmt::Debug> fmt::Debug for SharedArray<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T: PartialEq> PartialEq for SharedArray<T> {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

/// Compares as the slice does against every shape a `Vec<T>` compared to.
impl<T: PartialEq<U>, U> PartialEq<Vec<U>> for SharedArray<T> {
    fn eq(&self, other: &Vec<U>) -> bool {
        **self == other[..]
    }
}

impl<T: PartialEq<U>, U> PartialEq<[U]> for SharedArray<T> {
    fn eq(&self, other: &[U]) -> bool {
        **self == *other
    }
}

impl<T: PartialEq<U>, U> PartialEq<&[U]> for SharedArray<T> {
    fn eq(&self, other: &&[U]) -> bool {
        **self == **other
    }
}

impl<T: PartialEq<U>, U, const N: usize> PartialEq<[U; N]> for SharedArray<T> {
    fn eq(&self, other: &[U; N]) -> bool {
        **self == other[..]
    }
}

impl<T: PartialEq<U>, U, const N: usize> PartialEq<&[U; N]> for SharedArray<T> {
    fn eq(&self, other: &&[U; N]) -> bool {
        **self == other[..]
    }
}

impl<T> From<Vec<T>> for SharedArray<T> {
    fn from(v: Vec<T>) -> Self {
        let len = v.len();
        Self { buf: v.into(), len }
    }
}

impl<T> From<Arc<[T]>> for SharedArray<T> {
    fn from(buf: Arc<[T]>) -> Self {
        let len = buf.len();
        Self { buf, len }
    }
}

impl<T: Clone> From<&[T]> for SharedArray<T> {
    fn from(s: &[T]) -> Self {
        let len = s.len();
        Self { buf: s.into(), len }
    }
}

impl<T: Clone, const N: usize> From<[T; N]> for SharedArray<T> {
    fn from(a: [T; N]) -> Self {
        Self::from(&a[..])
    }
}

/// Collects straight into the shared buffer: an exact-size iterator
/// (a slice map, a `chunks_exact` walk) allocates once, with no `Vec`
/// in between.
impl<T> FromIterator<T> for SharedArray<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self::from(iter.into_iter().collect::<Arc<[T]>>())
    }
}

impl<'a, T> IntoIterator for &'a SharedArray<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<T: Clone> IntoIterator for SharedArray<T> {
    type Item = T;
    type IntoIter = IntoIter<T>;
    fn into_iter(self) -> Self::IntoIter {
        IntoIter { arr: self, at: 0 }
    }
}

/// By-value iteration over a [`SharedArray`]: clones each element out of
/// the shared buffer rather than the buffer itself.
pub struct IntoIter<T> {
    arr: SharedArray<T>,
    at: usize,
}

impl<T: Clone> Iterator for IntoIter<T> {
    type Item = T;
    fn next(&mut self) -> Option<T> {
        let v = self.arr.get(self.at)?.clone();
        self.at += 1;
        Some(v)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.arr.len() - self.at;
        (n, Some(n))
    }
}

impl<T: Clone> ExactSizeIterator for IntoIter<T> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clone_and_a_head_share_the_buffer_and_a_write_copies_it() {
        let mut a: SharedArray<f64> = vec![1.0, 2.0, 3.0].into();
        let b = a.clone();
        let h = a.head(2);
        assert!(a.ptr_eq(&b) && a.ptr_eq(&h));
        assert_eq!(&*h, &[1.0, 2.0]);
        assert_eq!(h.buffer().len(), 3);
        assert!(a.is_shared());
        a.make_mut()[0] = 9.0;
        assert!(!a.ptr_eq(&b));
        assert_eq!(&*a, &[9.0, 2.0, 3.0]);
        assert_eq!(&*b, &[1.0, 2.0, 3.0]);
        drop((b, h));
        assert!(!a.is_shared());
        let p = a.as_ptr();
        a.make_mut()[1] = 8.0;
        assert_eq!(a.as_ptr(), p, "the sole holder writes in place");
    }

    #[test]
    fn a_head_written_to_keeps_only_its_view() {
        let a: SharedArray<i32> = vec![1, 2, 3].into();
        let mut h = a.head(2);
        h.make_mut()[0] = 7;
        assert_eq!(&*h, &[7, 2]);
        assert_eq!(h.buffer().len(), 2);
        assert_eq!(&*a, &[1, 2, 3]);
    }

    #[test]
    fn resize_truncate_and_push_follow_vec() {
        let mut a: SharedArray<u8> = vec![1, 2, 3].into();
        a.truncate(5);
        assert_eq!(a.len(), 3);
        a.resize(2, 0);
        assert_eq!(&*a, &[1, 2]);
        a.resize(4, 9);
        assert_eq!(&*a, &[1, 2, 9, 9]);
        a.push(5);
        assert_eq!(&*a, &[1, 2, 9, 9, 5]);
        assert_eq!(a, vec![1, 2, 9, 9, 5]);
        assert_eq!(a, [1u8, 2, 9, 9, 5]);
        let empty = SharedArray::<u8>::default();
        assert!(empty.is_empty());
    }
}
