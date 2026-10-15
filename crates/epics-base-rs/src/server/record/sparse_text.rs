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
/// with: a third word in `amsg`/`namsg` cost +11..+22 instr/cycle. pvalink's
/// `"%s Disconn"` (`pvalink_lset.cpp:267`) is held the same way, under a
/// second tag bit.
///
/// Owned text is a shared `Arc<str>`, so copying a message between records
/// — an MSS link's `recGblInheritSevrMsg`, an lset's cached remote
/// `alarm.message` — is a refcount, not an allocation. C copies into a fixed
/// `char[40]`; here a store allocates only when the text itself changes, an
/// event bounded by the source's changes ([`Self::store`]).
#[derive(Default)]
pub struct SparseText(Option<Raw>);

/// A non-empty `Arc<str>`'s data pointer and length, or a `&'static` link
/// field name.
struct Raw {
    ptr: std::ptr::NonNull<u8>,
    len: usize,
}

/// Set in `Raw::len` for a link field name. No allocation reaches it.
const LINK_FIELD: usize = 1 << (usize::BITS - 1);
/// Set in `Raw::len` for a link field name rendered `"%s Disconn"`.
const LINK_DISCONN: usize = 1 << (usize::BITS - 2);
const TAGS: usize = LINK_FIELD | LINK_DISCONN;

const LINK_FIELD_PREFIX: &str = "field ";
const LINK_DISCONN_SUFFIX: &str = " Disconn";

// SAFETY: `Raw` is either one strong count of an `Arc<str>` or a
// `&'static str`, both `Send + Sync`.
unsafe impl Send for SparseText {}
// SAFETY: as above.
unsafe impl Sync for SparseText {}

enum View<'a> {
    Owned(&'a str),
    /// `"field "` followed by the name.
    LinkField(&'static str),
    /// The name followed by `" Disconn"`.
    LinkDisconn(&'static str),
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

    /// pvalink `pvaGetValue`'s message on a link that is not valid,
    /// `"%s Disconn"` of the link field's name (`pvalink_lset.cpp:267`).
    pub fn link_disconn(field: &'static str) -> Self {
        let ptr = std::ptr::NonNull::from(field.as_bytes()).cast::<u8>();
        Self(Some(Raw {
            ptr,
            len: field.len() | LINK_DISCONN,
        }))
    }

    fn owned(text: std::sync::Arc<str>) -> Self {
        if text.is_empty() {
            return Self(None);
        }
        let len = text.len();
        let ptr = std::ptr::NonNull::new(std::sync::Arc::into_raw(text).cast::<u8>().cast_mut())
            .expect("an Arc's data pointer is non-null");
        Self(Some(Raw { ptr, len }))
    }

    /// The same text object: the one `Arc`, or the same tagged `'static`.
    #[inline]
    fn same(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (None, None) => true,
            (Some(a), Some(b)) => a.ptr == b.ptr && a.len == b.len,
            _ => false,
        }
    }

    /// C's `strcpy` into the record's message: `src` is stored unless this
    /// already holds the same object (a pointer compare, no refcount write),
    /// so a steady message costs nothing per cycle.
    #[inline]
    pub fn store(&mut self, src: &SparseText) {
        if !self.same(src) {
            *self = src.clone();
        }
    }

    fn view(&self) -> Option<View<'_>> {
        let raw = self.0.as_ref()?;
        let len = raw.len & !TAGS;
        // SAFETY: `ptr`/`len` came from an `Arc<str>` this value holds a
        // count of (`owned`) or from a `&'static str` (`link_field`,
        // `link_disconn`); neither is mutated after.
        let text = unsafe {
            std::str::from_utf8_unchecked(std::slice::from_raw_parts(raw.ptr.as_ptr(), len))
        };
        // SAFETY (both tagged arms): a tag is set only by `link_field` or
        // `link_disconn`, over a `'static`.
        Some(match raw.len & TAGS {
            0 => View::Owned(text),
            LINK_FIELD => View::LinkField(unsafe { &*std::ptr::from_ref::<str>(text) }),
            _ => View::LinkDisconn(unsafe { &*std::ptr::from_ref::<str>(text) }),
        })
    }

    /// The text as a prefix, a middle and a suffix, all `""` when there is
    /// none — C's `char *` view of the field, which never distinguishes them.
    pub(crate) fn parts(&self) -> (&'static str, &str, &'static str) {
        match self.view() {
            None => ("", "", ""),
            Some(View::Owned(text)) => ("", text, ""),
            Some(View::LinkField(field)) => (LINK_FIELD_PREFIX, field, ""),
            Some(View::LinkDisconn(field)) => ("", field, LINK_DISCONN_SUFFIX),
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
    /// Text equal to what is held is left in place: it allocates only when
    /// the text changes.
    pub fn set(&mut self, text: &str) {
        if !self.eq_str(text) {
            *self = Self::from(text);
        }
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
            Some(View::LinkDisconn(field)) => other
                .strip_suffix(LINK_DISCONN_SUFFIX)
                .is_some_and(|rest| rest == field),
        }
    }

    /// Both sides present: C's `strcmp`, whichever form holds each.
    #[cold]
    #[inline(never)]
    fn eq_present(&self, other: &Self) -> bool {
        if self.same(other) {
            return true;
        }
        match (self.view(), other.view()) {
            (Some(View::LinkField(a)), Some(View::LinkField(b)))
            | (Some(View::LinkDisconn(a)), Some(View::LinkDisconn(b))) => a == b,
            (Some(View::Owned(text)), _) => other.eq_str(text),
            (_, Some(View::Owned(text))) => self.eq_str(text),
            _ => false,
        }
    }
}

/// `namsg` is cloned and dropped every cycle (`LinkAlarm::pending`) and is
/// almost always absent, so only that test is inline, as in `eq_present`: an
/// inline present arm merged its tag test into a branchless compare that
/// every absent message paid.
impl Drop for SparseText {
    fn drop(&mut self) {
        if let Some(raw) = &self.0 {
            raw.release();
        }
    }
}

impl Raw {
    fn as_str_ptr(&self) -> *const str {
        std::ptr::slice_from_raw_parts(self.ptr.as_ptr().cast_const(), self.len) as *const str
    }

    #[cold]
    #[inline(never)]
    fn share(&self) -> Self {
        if self.len & TAGS == 0 {
            // SAFETY: an untagged `Raw` holds a strong count of a live
            // `Arc<str>`; the copy below owns the one taken here.
            unsafe { std::sync::Arc::increment_strong_count(self.as_str_ptr()) };
        }
        Raw {
            ptr: self.ptr,
            len: self.len,
        }
    }

    #[cold]
    #[inline(never)]
    fn release(&self) {
        if self.len & TAGS == 0 {
            // SAFETY: an untagged `Raw` is the strong count `owned` took
            // out of its `Arc<str>`.
            drop(unsafe { std::sync::Arc::from_raw(self.as_str_ptr()) });
        }
    }
}

impl Clone for SparseText {
    fn clone(&self) -> Self {
        Self(self.0.as_ref().map(Raw::share))
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
        let (prefix, text, suffix) = self.parts();
        f.write_str(prefix)?;
        f.write_str(text)?;
        f.write_str(suffix)
    }
}

impl From<&SparseText> for String {
    fn from(text: &SparseText) -> Self {
        let (prefix, middle, suffix) = text.parts();
        let mut out = String::with_capacity(prefix.len() + middle.len() + suffix.len());
        out.push_str(prefix);
        out.push_str(middle);
        out.push_str(suffix);
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
        if text.is_empty() {
            return Self(None);
        }
        Self::owned(std::sync::Arc::from(text))
    }
}

impl From<String> for SparseText {
    fn from(text: String) -> Self {
        Self::from(text.as_str())
    }
}

/// Shares the text: no copy is made.
impl From<std::sync::Arc<str>> for SparseText {
    fn from(text: std::sync::Arc<str>) -> Self {
        Self::owned(text)
    }
}

/// A message `recGblSetSevrMsg` stores into `namsg`
/// ([`crate::server::recgbl::rec_gbl_set_sevr_msg`]): a held text through
/// [`SparseText::store`], plain text through [`SparseText::set`], so a
/// message equal to the one stored allocates nothing.
pub trait AlarmText {
    fn store_into(self, slot: &mut SparseText);
}

impl AlarmText for &SparseText {
    #[inline]
    fn store_into(self, slot: &mut SparseText) {
        slot.store(self);
    }
}

impl AlarmText for SparseText {
    #[inline]
    fn store_into(self, slot: &mut SparseText) {
        if !slot.same(&self) {
            *slot = self;
        }
    }
}

impl AlarmText for &str {
    fn store_into(self, slot: &mut SparseText) {
        slot.set(self);
    }
}

impl AlarmText for &String {
    fn store_into(self, slot: &mut SparseText) {
        slot.set(self);
    }
}

impl AlarmText for String {
    fn store_into(self, slot: &mut SparseText) {
        slot.set(&self);
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
    fn a_link_disconn_equals_its_rendered_text() {
        let disconn = SparseText::link_disconn("INP");
        assert_eq!(String::from(&disconn), "INP Disconn");
        assert_eq!(disconn, SparseText::from("INP Disconn"));
        assert_eq!(SparseText::from("INP Disconn"), disconn);
        assert_eq!(disconn, "INP Disconn");
        assert_ne!(disconn, SparseText::link_field("INP"));
        assert_ne!(disconn, SparseText::link_disconn("INPA"));
        assert_ne!(disconn, "Disconn");
        assert_eq!(disconn.clone(), disconn);
    }

    #[test]
    fn the_field_stays_two_words() {
        assert_eq!(
            std::mem::size_of::<SparseText>(),
            2 * std::mem::size_of::<usize>()
        );
    }
}
