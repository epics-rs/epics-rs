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
///
/// A link failure's `"field %s"` (`dbLink.c:320`) is held as the link field's
/// `&'static` name and rendered on read: C formats it into the record's fixed
/// `namsg` without allocating, and the failure is raised under the hold. The
/// two forms share one pointer and length, the length's top bit naming the
/// form, so the field stays the two words `dbCommon`'s hot path was laid out
/// with: a third word in `amsg`/`namsg` cost +11..+22 instr/cycle.
#[derive(Default)]
pub struct SparseText(Option<Raw>);

/// A non-empty `Box<str>` the field owns, or a `&'static` link field name.
struct Raw {
    ptr: std::ptr::NonNull<u8>,
    len: usize,
}

/// Set in `Raw::len` for a link field name. No allocation reaches it.
const LINK_FIELD: usize = 1 << (usize::BITS - 1);

const LINK_FIELD_PREFIX: &str = "field ";

// SAFETY: `Raw` is either a uniquely owned `Box<str>` or a `&'static str`,
// both `Send + Sync`.
unsafe impl Send for SparseText {}
// SAFETY: as above.
unsafe impl Sync for SparseText {}

enum View<'a> {
    Owned(&'a str),
    /// `"field "` followed by the name.
    LinkField(&'static str),
}

impl SparseText {
    /// C `setLinkAlarm`'s message, `"field %s"` of the link field's name.
    pub fn link_field(field: &'static str) -> Self {
        let ptr = std::ptr::NonNull::from(field.as_bytes()).cast::<u8>();
        Self(Some(Raw {
            ptr,
            len: field.len() | LINK_FIELD,
        }))
    }

    fn owned(text: Box<str>) -> Self {
        if text.is_empty() {
            return Self(None);
        }
        let len = text.len();
        let ptr = std::ptr::NonNull::from(Box::leak(text)).cast::<u8>();
        Self(Some(Raw { ptr, len }))
    }

    fn view(&self) -> Option<View<'_>> {
        let raw = self.0.as_ref()?;
        let len = raw.len & !LINK_FIELD;
        // SAFETY: `ptr`/`len` came from a `str` this value owns (`owned`) or
        // from a `&'static str` (`link_field`); neither is mutated after.
        let text = unsafe {
            std::str::from_utf8_unchecked(std::slice::from_raw_parts(raw.ptr.as_ptr(), len))
        };
        Some(if raw.len & LINK_FIELD == 0 {
            View::Owned(text)
        } else {
            // SAFETY: the tag is set only by `link_field`, over a `'static`.
            View::LinkField(unsafe { &*std::ptr::from_ref::<str>(text) })
        })
    }

    /// The text as a prefix and a remainder, `("", "")` when there is none —
    /// C's `char *` view of the field, which never distinguishes the two.
    fn parts(&self) -> (&'static str, &str) {
        match self.view() {
            None => ("", ""),
            Some(View::Owned(text)) => ("", text),
            Some(View::LinkField(field)) => (LINK_FIELD_PREFIX, field),
        }
    }

    /// C's `namsg[0] == '\0'`.
    pub fn is_empty(&self) -> bool {
        self.0.is_none()
    }

    /// C's `namsg[0] = '\0'`.
    pub fn clear(&mut self) {
        *self = Self(None);
    }

    /// The only writer, and the reason an empty `Some` is unrepresentable.
    pub fn set(&mut self, text: &str) {
        *self = Self::from(text);
    }

    /// Equality against a plain `str`, answering an absent side by length so
    /// the comparison cannot reach `bcmp` with the other side's dangling
    /// empty-string pointer.
    fn eq_str(&self, other: &str) -> bool {
        match self.view() {
            None => other.is_empty(),
            Some(View::Owned(text)) => text == other,
            Some(View::LinkField(field)) => other
                .strip_prefix(LINK_FIELD_PREFIX)
                .is_some_and(|rest| rest == field),
        }
    }

    /// Both sides present: C's `strcmp`, whichever form holds each.
    #[cold]
    #[inline(never)]
    fn eq_present(&self, other: &Self) -> bool {
        match (self.view(), other.view()) {
            (Some(View::LinkField(a)), Some(View::LinkField(b))) => a == b,
            (Some(View::Owned(text)), _) => other.eq_str(text),
            (_, Some(View::Owned(text))) => self.eq_str(text),
            _ => false,
        }
    }
}

impl Drop for SparseText {
    fn drop(&mut self) {
        if let Some(raw) = &self.0
            && raw.len & LINK_FIELD == 0
        {
            // SAFETY: an untagged `Raw` is the `Box<str>` `owned` leaked.
            drop(unsafe {
                Box::from_raw(
                    std::ptr::slice_from_raw_parts_mut(raw.ptr.as_ptr(), raw.len) as *mut str,
                )
            });
        }
    }
}

impl Clone for SparseText {
    fn clone(&self) -> Self {
        match self.view() {
            None => Self(None),
            Some(View::Owned(text)) => Self::owned(Box::from(text)),
            Some(View::LinkField(field)) => Self::link_field(field),
        }
    }
}

/// C's `strcmp`: two texts are equal when they render the same, whichever
/// form holds them.
impl PartialEq for SparseText {
    /// Every cycle's `recGblResetAlarms` compares `namsg` with `amsg`, and
    /// almost always both are absent: that answer stays inline.
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (None, None) => true,
            (Some(_), Some(_)) => self.eq_present(other),
            _ => false,
        }
    }
}

impl Eq for SparseText {}

impl std::fmt::Debug for SparseText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&String::from(self), f)
    }
}

impl std::fmt::Display for SparseText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (prefix, rest) = self.parts();
        f.write_str(prefix)?;
        f.write_str(rest)
    }
}

impl From<&SparseText> for String {
    fn from(text: &SparseText) -> Self {
        let (prefix, rest) = text.parts();
        let mut out = String::with_capacity(prefix.len() + rest.len());
        out.push_str(prefix);
        out.push_str(rest);
        out
    }
}

impl From<&SparseText> for SparseText {
    fn from(text: &SparseText) -> Self {
        text.clone()
    }
}

impl From<&str> for SparseText {
    fn from(text: &str) -> Self {
        Self::owned(Box::from(text))
    }
}

impl From<String> for SparseText {
    fn from(text: String) -> Self {
        Self::owned(text.into_boxed_str())
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
        assert_eq!(String::from(&absent), "");
    }

    /// The `&'static` form and the owned form of one text are one value, as
    /// C's `strcmp` sees them; a shared prefix or a shared name is not.
    #[test]
    fn a_link_field_equals_its_rendered_text() {
        let field = SparseText::link_field("INP");
        assert_eq!(String::from(&field), "field INP");
        assert_eq!(field.to_string(), "field INP");
        assert_eq!(field, SparseText::from("field INP"));
        assert_eq!(SparseText::from("field INP"), field);
        assert_eq!(field, "field INP");
        assert_ne!(field, SparseText::link_field("INPA"));
        assert_ne!(field, SparseText::from("field INPA"));
        assert_ne!(field, SparseText::from("INP"));
        assert_ne!(field, "field ");
        assert_ne!(field, SparseText::default());
        assert!(!field.is_empty());
        let owned = SparseText::from("field INP");
        assert_eq!(owned.clone(), field.clone());
    }

    #[test]
    fn the_field_stays_two_words() {
        assert_eq!(
            std::mem::size_of::<SparseText>(),
            2 * std::mem::size_of::<usize>()
        );
    }
}
