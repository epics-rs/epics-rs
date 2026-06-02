//! Byte-preserving wire string for EPICS scalar/array string values.
//!
//! Both PVA and CA carry strings as raw bytes with no UTF-8 guarantee:
//! pvxs stores `std::string((char*)buf,len)` straight off the wire with no
//! validation (`pvaproto.h:403`), and CA `DBR_STRING` fields are NUL-padded
//! 40-byte blobs that are historically Latin-1 / arbitrary bytes. Modelling
//! these as Rust `String` forced UTF-8 validation at the decode boundary,
//! which (a) rejected perfectly legal non-UTF-8 wire values and (b) would
//! have corrupted them on re-encode in a gateway pass-through. `PvString`
//! keeps the logical bytes intact so decode → store → re-encode is lossless,
//! matching pvxs.
//!
//! Deliberately **not** `Deref<Target = str>` — that is impossible for
//! non-UTF-8 bytes. Construct from `String` / `&str` / `Vec<u8>` / `&[u8]`;
//! read the bytes with [`PvString::as_bytes`]; render text with
//! [`PvString::as_str_lossy`] (diagnostic) or feed the bytes to a byte-wise
//! escaper (pvxs `util.cpp:210-242`) for CLI output. Compare against `str`,
//! `&str`, and `String` directly via the `PartialEq` shims.

use std::borrow::Cow;
use std::fmt;

/// A wire string: a sequence of bytes that is **not** guaranteed to be valid
/// UTF-8. Backs `EpicsValue::String`/`StringArray` and (in `epics-pva-rs`)
/// `ScalarValue::String`/`TypedScalarArray::String`. See the module docs for
/// why this is a byte newtype rather than `String`.
#[derive(Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PvString(Vec<u8>);

impl PvString {
    /// An empty wire string.
    pub fn new() -> Self {
        PvString(Vec::new())
    }

    /// Wrap raw bytes verbatim (the decode path: bytes straight off the wire).
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        PvString(bytes.into())
    }

    /// The logical bytes, layout-agnostic (an encoder applies CA's 40-byte
    /// NUL padding or PVA's size prefix; this is the unframed content).
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Consume into the owned byte buffer.
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    /// Lossy UTF-8 view for diagnostics / JSON / text comparisons where a
    /// faithful byte rendering is not required. Non-UTF-8 bytes become
    /// `U+FFFD`. For pvxs-faithful CLI output, escape [`Self::as_bytes`]
    /// byte-wise instead.
    pub fn as_str_lossy(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.0)
    }

    /// Number of logical bytes.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// True when the string carries no bytes.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

// --- Construction shims (absorb the ~Vec<u8>/String/&str construction churn) ---

impl From<String> for PvString {
    fn from(s: String) -> Self {
        PvString(s.into_bytes())
    }
}

impl From<&str> for PvString {
    fn from(s: &str) -> Self {
        PvString(s.as_bytes().to_vec())
    }
}

impl From<&String> for PvString {
    fn from(s: &String) -> Self {
        PvString(s.as_bytes().to_vec())
    }
}

impl From<Cow<'_, str>> for PvString {
    fn from(s: Cow<'_, str>) -> Self {
        PvString(s.into_owned().into_bytes())
    }
}

impl From<Vec<u8>> for PvString {
    fn from(b: Vec<u8>) -> Self {
        PvString(b)
    }
}

impl From<&[u8]> for PvString {
    fn from(b: &[u8]) -> Self {
        PvString(b.to_vec())
    }
}

impl From<PvString> for Vec<u8> {
    fn from(s: PvString) -> Self {
        s.0
    }
}

impl AsRef<[u8]> for PvString {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

// --- Comparison shims (absorb the `== "literal"` / `== some_string` churn) ---

impl PartialEq<str> for PvString {
    fn eq(&self, other: &str) -> bool {
        self.0 == other.as_bytes()
    }
}

impl PartialEq<&str> for PvString {
    fn eq(&self, other: &&str) -> bool {
        self.0 == other.as_bytes()
    }
}

impl PartialEq<String> for PvString {
    fn eq(&self, other: &String) -> bool {
        self.0 == other.as_bytes()
    }
}

impl PartialEq<PvString> for str {
    fn eq(&self, other: &PvString) -> bool {
        self.as_bytes() == other.0
    }
}

impl PartialEq<PvString> for &str {
    fn eq(&self, other: &PvString) -> bool {
        self.as_bytes() == other.0
    }
}

impl PartialEq<PvString> for String {
    fn eq(&self, other: &PvString) -> bool {
        self.as_bytes() == other.0
    }
}

// --- Text rendering ---

impl fmt::Display for PvString {
    /// Lossy text rendering for diagnostics and `{}`-formatting. This is the
    /// "diagnostic" policy (pvxs uses a byte-wise escaper for its CLI output;
    /// that lives in the `epics-pva-rs` format layer and consumes
    /// [`Self::as_bytes`] directly).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.as_str_lossy(), f)
    }
}

impl fmt::Debug for PvString {
    /// Quoted, escaped rendering of the lossy text view — keeps `{:?}` output
    /// readable (mirrors `String`'s `Debug` for the common ASCII/UTF-8 case).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.as_str_lossy(), f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_non_utf8_bytes() {
        let raw = vec![0xff, 0x00, 0x80, b'a'];
        let s = PvString::from_bytes(raw.clone());
        assert_eq!(s.as_bytes(), raw.as_slice());
        assert_eq!(s.into_bytes(), raw);
    }

    #[test]
    fn from_str_and_string_preserve_bytes() {
        assert_eq!(PvString::from("abc").as_bytes(), b"abc");
        assert_eq!(PvString::from("abc".to_string()).as_bytes(), b"abc");
        let utf8 = "한글";
        assert_eq!(PvString::from(utf8).as_bytes(), utf8.as_bytes());
    }

    #[test]
    fn compares_against_str_and_string() {
        let s = PvString::from("PV:NAME");
        assert_eq!(s, "PV:NAME");
        assert_eq!(s, "PV:NAME".to_string());
        assert!(s != "OTHER");
        assert_eq!("PV:NAME", s);
        assert_eq!("PV:NAME".to_string(), s);
    }

    #[test]
    fn lossy_view_substitutes_invalid_bytes() {
        let s = PvString::from_bytes(vec![0xff, 0xfe]);
        assert!(s.as_str_lossy().contains('\u{FFFD}'));
        // Debug stays quoted/readable rather than dumping a byte vector.
        assert!(format!("{s:?}").starts_with('"'));
    }

    #[test]
    fn default_is_empty() {
        assert!(PvString::default().is_empty());
        assert_eq!(PvString::default().len(), 0);
    }
}
