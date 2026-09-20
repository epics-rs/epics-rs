//! A string field whose empty value is its common value.

/// Text that is usually absent, holding that absence in a discriminant rather
/// than in an empty allocation.
///
/// Several per-cycle fields are strings that are empty on almost every record:
/// `dbCommon`'s `AMSG`/`NAMSG`, and the cached OUT-link text calcout and swait
/// diff each cycle to notice a runtime re-point. C writes the absence as
/// `namsg[0] == '\0'` — the empty string IS the absence — so a `String` here
/// carries one value with two meanings.
///
/// It also carries a cost. An empty Rust `String` holds the dangling `0x1`
/// pointer, `str` equality compares lengths and then calls `bcmp`, and glibc's
/// AVX-512 `bcmp` resolves its masked load against page zero even for a
/// zero-length compare — a microcode assist measured at 103 ns on this port's
/// reference host against 1.5 ns for the same call on real pointers. One such
/// compare per record per cycle was 10.6% of a 2000-record scan.
///
/// Both go away by construction here. [`Self::set`] is the only writer and maps
/// `""` to `None`, so an empty `Some` is unrepresentable; `is_empty` and every
/// equality then answer an absent side from the discriminant or a length alone,
/// and `bcmp` is reached only with two non-empty operands.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SparseText(Option<Box<str>>);

impl SparseText {
    /// The text, or `""` when there is none — C's `char *` view of the field,
    /// which never distinguishes the two.
    pub fn as_str(&self) -> &str {
        self.0.as_deref().unwrap_or("")
    }

    /// C's `namsg[0] == '\0'`.
    pub fn is_empty(&self) -> bool {
        self.0.is_none()
    }

    /// C's `namsg[0] = '\0'`.
    pub fn clear(&mut self) {
        self.0 = None;
    }

    /// The only writer, and the reason an empty `Some` is unrepresentable.
    pub fn set(&mut self, text: &str) {
        self.0 = (!text.is_empty()).then(|| Box::from(text));
    }

    /// Equality against a plain `str`, answering an absent side by length so
    /// the comparison cannot reach `bcmp` with the other side's dangling
    /// empty-string pointer.
    fn eq_str(&self, other: &str) -> bool {
        match &self.0 {
            None => other.is_empty(),
            Some(text) => **text == *other,
        }
    }
}

impl std::fmt::Debug for SparseText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.as_str(), f)
    }
}

impl std::fmt::Display for SparseText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for SparseText {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl From<&str> for SparseText {
    fn from(text: &str) -> Self {
        Self((!text.is_empty()).then(|| Box::from(text)))
    }
}

impl From<String> for SparseText {
    fn from(text: String) -> Self {
        Self((!text.is_empty()).then(|| text.into_boxed_str()))
    }
}

impl PartialEq<str> for SparseText {
    fn eq(&self, other: &str) -> bool {
        self.eq_str(other)
    }
}

impl PartialEq<&str> for SparseText {
    fn eq(&self, other: &&str) -> bool {
        self.eq_str(other)
    }
}

impl PartialEq<SparseText> for &str {
    fn eq(&self, other: &SparseText) -> bool {
        other.eq_str(self)
    }
}

impl PartialEq<String> for SparseText {
    fn eq(&self, other: &String) -> bool {
        self.eq_str(other)
    }
}

#[cfg(test)]
mod tests {
    use super::SparseText;

    #[test]
    fn an_empty_write_is_stored_as_absence() {
        let mut text = SparseText::from("boom");
        assert!(!text.is_empty());
        text.set("");
        assert!(text.is_empty(), "\"\" must not survive as an empty Some");
        assert_eq!(text, "");
        assert_eq!(text, SparseText::default());
        assert_eq!(text, String::new());
    }

    #[test]
    fn absence_and_text_compare_both_ways() {
        let absent = SparseText::default();
        let present = SparseText::from("field OUT".to_string());
        assert_ne!(absent, present);
        assert_eq!(present, "field OUT");
        assert_ne!(present, "field INP");
        // The empty `String` here is the dangling-pointer operand the type
        // exists to keep away from `bcmp`; the answer must still be right.
        assert_ne!(present, String::new());
        assert_eq!(absent, String::new());
        assert_eq!(absent.as_str(), "");
    }
}
