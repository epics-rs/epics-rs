//! A record or alias name, as the bytes C keeps in `dbCommon::name`.
//!
//! C's database is byte-keyed: `dbLoadRecords` lexes bytes, `dbPvdLib`
//! hashes them, and rsrv looks a channel up by the bytes the client sent.
//! A name is therefore not guaranteed to be UTF-8, and a `str`-keyed
//! database could not hold one that C loads. Like [`super::PvString`] this
//! is deliberately not `Deref<Target = str>`; `Display` is lossy and for
//! diagnostics only.

use std::borrow::{Borrow, Cow};
use std::fmt;
use std::sync::Arc;

/// One allocation of a record's name, shared by every holder (the records
/// map key, the cell, a scan key, a process chain) as `Arc<str>` was.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RecordName(Arc<[u8]>);

impl RecordName {
    /// The name's bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// The name as text, U+FFFD for a non-UTF-8 sequence: for printing.
    pub fn to_str_lossy(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.0)
    }

    /// The name as `&str` when it is UTF-8 — for an API that has no byte
    /// form, which therefore cannot name a record that is not.
    pub fn to_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::ops::Deref for RecordName {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl Borrow<[u8]> for RecordName {
    fn borrow(&self) -> &[u8] {
        &self.0
    }
}

impl AsRef<[u8]> for RecordName {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl From<&[u8]> for RecordName {
    fn from(name: &[u8]) -> Self {
        Self(Arc::from(name))
    }
}

impl From<Vec<u8>> for RecordName {
    fn from(name: Vec<u8>) -> Self {
        Self(Arc::from(name))
    }
}

impl From<&str> for RecordName {
    fn from(name: &str) -> Self {
        Self(Arc::from(name.as_bytes()))
    }
}

impl From<String> for RecordName {
    fn from(name: String) -> Self {
        Self(Arc::from(name.into_bytes()))
    }
}

impl From<&RecordName> for RecordName {
    fn from(name: &RecordName) -> Self {
        name.clone()
    }
}

impl From<&super::PvString> for RecordName {
    fn from(name: &super::PvString) -> Self {
        Self(Arc::from(name.as_bytes()))
    }
}

impl PartialEq<str> for RecordName {
    fn eq(&self, other: &str) -> bool {
        *self.0 == *other.as_bytes()
    }
}

impl PartialEq<&str> for RecordName {
    fn eq(&self, other: &&str) -> bool {
        *self.0 == *other.as_bytes()
    }
}

impl PartialEq<String> for RecordName {
    fn eq(&self, other: &String) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl PartialEq<[u8]> for RecordName {
    fn eq(&self, other: &[u8]) -> bool {
        *self.0 == *other
    }
}

impl fmt::Display for RecordName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.to_str_lossy(), f)
    }
}

impl fmt::Debug for RecordName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.to_str_lossy(), f)
    }
}
