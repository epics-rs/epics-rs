//! C's `$` modifier on a database address (`dbAccess.c:682-701`) and what
//! `dbGet` / `dbPut` then do with the `DBR_CHAR` view it makes. One owner for
//! every front end that resolves a `REC.FIELD$` name — the iocsh `dbTest.c`
//! commands and the CA server — so they cannot disagree about which fields
//! take the modifier, how many elements the view has, or what a put stores.

use crate::server::database::filters::FilterChain;
use crate::server::record::{
    FieldDeclaration, FieldDesc, OwnRecord, Record, RecordInstance, Special, served_element_size,
};
use crate::types::{DbFieldType, DbfCode, EpicsValue};

/// C `dbEntryToAddr`'s two type words for one field (`dbAccess.c:627-649`):
/// `paddr->field_type`, the `DBF_*` token the `.dbd` declared except on a
/// `special(SPC_DBADDR)` row where the record's `cvt_dbaddr` overwrites it,
/// and `paddr->dbr_field_type`, that token through `mapDBFToDBR`.
///
/// **The single owner of "what type is this field read and labelled as".**
/// Every `dbTest.c` command that prints a `DBF_<T>:` header — `dbgf`, `dbpf`,
/// `dbtgf`, `dbtpf` — and `dba`, which prints both words, asks here, so the
/// answer cannot be read off the [`EpicsValue`] the record happens to hold.
/// It cannot be, in either direction: `DBF_ENUM`, `DBF_MENU` and `DBF_DEVICE`
/// all collapse onto one served variant and one `mapDBFToDBR` code, so the
/// variant cannot name the token, while a `DBF_MENU` index is stored in a
/// short and the variant names the wrong code entirely. Reading it off the
/// value is what made `dbgf REC.PRIO` answer `DBF_SHORT: 0 = 0x0` where C
/// answers `DBF_STRING: "LOW"`.
///
/// The one place the stored value IS the answer is a
/// [`runtime_typed`](FieldDesc::runtime_typed) row, where C's `cvt_dbaddr`
/// re-types the address from the record's own state — `waveform.VAL` to `FTVL`
/// (`waveformRecord.c::cvt_dbaddr`), `mbbo.VAL` down to `DBF_USHORT` when no
/// state string is defined (`mbboRecord.c::cvt_dbaddr`) — and the port's
/// answer to that same question is the variant the record holds. `value` is
/// therefore consulted for those rows and only those; `None` leaves the
/// declaration standing, which is what a caller with no value in hand wants.
pub(crate) fn field_addr_types(fd: &FieldDesc, value: Option<&EpicsValue>) -> (DbfCode, DbfCode) {
    let field_type = match value {
        Some(v) if fd.runtime_typed => v.db_field_type().dbf_code(),
        _ => fd.addressed_dbf(),
    };
    (field_type, field_type.dbr_code())
}

/// The record a `$` view reads: an instance under its lock set, or the record
/// the caller is inside, which C's `dbGet` reads the same way.
pub(crate) trait ViewedRecord {
    fn record(&self) -> &dyn Record;
    /// [`RecordInstance::attribute`].
    fn attribute(&self, field: &str) -> Option<String>;
    /// [`RecordInstance::resolve_field`].
    fn resolve_field(&self, field: &str) -> Option<EpicsValue>;
}

impl ViewedRecord for RecordInstance {
    fn record(&self) -> &dyn Record {
        &*self.record
    }
    fn attribute(&self, field: &str) -> Option<String> {
        RecordInstance::attribute(self, field)
    }
    fn resolve_field(&self, field: &str) -> Option<EpicsValue> {
        RecordInstance::resolve_field(self, field)
    }
}

impl ViewedRecord for OwnRecord<'_> {
    fn record(&self) -> &dyn Record {
        OwnRecord::record(*self)
    }
    fn attribute(&self, field: &str) -> Option<String> {
        OwnRecord::attribute(*self, field)
    }
    fn resolve_field(&self, field: &str) -> Option<EpicsValue> {
        OwnRecord::resolve_field(*self, field)
    }
}

/// Which of `dbGet`'s routes reads `inst.field` (`dbAccess.c:944-952`), or
/// `None` where the record has no such field.
pub fn read_route(inst: &RecordInstance, field: &str) -> Option<CharSource> {
    route_at(inst, field, inst.field_desc(field))
}

/// [`read_route`] with `desc` = [`RecordInstance::field_desc`]`(field)`.
fn route_at<R: ViewedRecord + ?Sized>(
    inst: &R,
    field: &str,
    desc: Option<&FieldDesc>,
) -> Option<CharSource> {
    match desc {
        Some(desc) if crate::types::link_class_of(desc).is_some() => Some(CharSource::Link),
        Some(_) => Some(CharSource::Field),
        // `dbGetAttributePart`'s synthesised `DBF_STRING` descriptor.
        None => inst.attribute(field).map(|_| CharSource::Attribute),
    }
}

/// The byte width of one element of `code`, as C's `dbFldDes.size` carries
/// it — `sizeof(prec->field)`, stamped per field by the `GEN_SIZE_OFFSET`
/// block of the generated `xxxRecord.h`, except for `DBF_STRING` where it is
/// the `.dbd` `size(N)` attribute (`dbLexRoutines.c:663`, required non-zero
/// at `:755`).
///
/// `None` where the C number is a fact about a C struct this port does not
/// have: the three link types are `sizeof(DBLINK)` — measured as **80** on
/// `softIoc` linux-x86_64, and platform-dependent. Printing a width for those
/// would be inventing an ABI, so `dba` prints `(none)` instead.
///
/// A `DBF_NOACCESS` row reaches the last arm only when `field_addr_types` left
/// the declaration standing, i.e. when no `cvt_dbaddr` re-typed it — and those
/// are exactly the rows the generator carries a width for, taken from the C
/// type its `extra(...)` names. `dbCommon.TIME` is the case in hand: C prints
/// `Field Size: 8`, `sizeof(epicsTimeStamp)`. A width of zero still means the
/// port has no number, so it keeps `(none)`.
pub(crate) fn c_field_size(code: DbfCode, declared_size: u16) -> Option<u16> {
    Some(match code {
        DbfCode::String => declared_size,
        DbfCode::NoAccess if declared_size > 0 => declared_size,
        DbfCode::Char | DbfCode::UChar => 1,
        // `epicsEnum16` for all three (`epicsTypes.h`).
        DbfCode::Short | DbfCode::UShort | DbfCode::Enum | DbfCode::Menu | DbfCode::Device => 2,
        DbfCode::Long | DbfCode::ULong | DbfCode::Float => 4,
        DbfCode::Double | DbfCode::Int64 | DbfCode::UInt64 => 8,
        DbfCode::Inlink | DbfCode::Outlink | DbfCode::Fwdlink | DbfCode::NoAccess => return None,
    })
}

/// The view of `inst.field` with `$`, or `None` where C refuses the modifier.
/// It holds the resolved value and the counts only, so a caller under the
/// record's lock pays one field read; the character image is built from it
/// after the lock is gone.
pub fn char_view(inst: &RecordInstance, field: &str) -> Option<CharView> {
    char_view_of(inst, field)
}

/// [`char_view`] of either record a `dbGet` reads ([`ViewedRecord`]).
pub(crate) fn char_view_of<R: ViewedRecord + ?Sized>(inst: &R, field: &str) -> Option<CharView> {
    let desc = crate::server::record::record_instance::field_desc_of(inst.record(), field);
    let desc = match route_at(inst, field, desc)? {
        CharSource::Attribute => {
            return Some(CharView {
                source: CharSource::Attribute,
                value: inst
                    .attribute(field)
                    .map(|text| EpicsValue::String(text.into())),
                count: MAX_STRING_SIZE,
                no_elements: MAX_STRING_SIZE,
                field_type: DbfCode::Char,
                truncate: false,
            });
        }
        CharSource::Link => {
            return Some(CharView {
                source: CharSource::Link,
                value: inst.resolve_field(field),
                count: PVLINK_STRINGSZ,
                no_elements: PVLINK_STRINGSZ,
                field_type: desc?.declared_dbf,
                truncate: false,
            });
        }
        CharSource::Field => desc?,
    };
    let value = inst.resolve_field(field);
    if field_addr_types(desc, value.as_ref()).0 != DbfCode::String {
        return None;
    }
    // `no_elements = field_size`, and `dbGet`'s count is
    // `get_array_info`'s where the field is `SPC_DBADDR`.
    let (count, capacity) = match value.as_ref()? {
        // A `SPC_DBADDR` string element serves `get_array_info`'s count
        // (aSub `NOB`) in a `field_size` buffer; a plain string field's count
        // is its own width.
        EpicsValue::String(_) => {
            let width = usize::from(served_element_size(inst.record(), desc, DbfCode::String));
            match desc.declared_special {
                Special::DbAddr => (
                    inst.record().dbaddr_count(desc.name).unwrap_or(1) as usize,
                    width,
                ),
                _ => (width, width),
            }
        }
        EpicsValue::CharArray(_) | EpicsValue::UCharArray(_) => {
            (long_string_len(inst), long_string_sizv(inst))
        }
        EpicsValue::StringArray(v) => (v.len(), MAX_STRING_SIZE),
        _ => return None,
    };
    Some(CharView {
        source: CharSource::Field,
        value,
        count,
        no_elements: capacity,
        field_type: DbfCode::Char,
        truncate: desc.declared_dbf == DbfCode::String,
    })
}

/// The text of `inst.field`'s `$` view up to its NUL, as a string:
/// [`RecordInstance::resolve_string_view_field`].
pub(crate) fn string_view_value<R: ViewedRecord + ?Sized>(
    inst: &R,
    field: &str,
) -> Option<EpicsValue> {
    let mut text = char_view_of(inst, field)?.bytes();
    text.truncate(text.iter().position(|&c| c == 0).unwrap_or(text.len()));
    Some(EpicsValue::String(crate::types::PvString::from_bytes(text)))
}

/// The address [`char_view`] reads through, as `dbChannelCreate` makes it
/// (`dbChannel.c:486-503`): the route, `no_elements` and the cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CharAddr {
    pub(crate) source: CharSource,
    pub(crate) no_elements: usize,
    truncate: bool,
}

impl CharAddr {
    /// `field`'s `$` address with no lock, as C's takes none: from the
    /// declaration and what `cell` settled. `None` where C refuses `$`.
    pub(crate) fn of(
        db: &crate::server::database::PvDatabase,
        cell: &crate::server::record::RecordCell,
        field: &str,
    ) -> Option<Self> {
        let desc = crate::server::record::record_instance::field_desc_in(cell.rdes().fields, field);
        let Some(desc) = desc else {
            return (!cell.declares_name(field)
                && db.has_record_type_attribute(cell.rdes().record_type, field))
            .then_some(Self {
                source: CharSource::Attribute,
                no_elements: MAX_STRING_SIZE,
                truncate: false,
            });
        };
        if crate::types::link_class_of(desc).is_some() {
            return Some(Self {
                source: CharSource::Link,
                no_elements: PVLINK_STRINGSZ,
                truncate: false,
            });
        }
        if !cell.takes_string_view(desc) {
            return None;
        }
        let no_elements = if desc.long_string {
            cell.long_string_size()
        } else {
            usize::from(desc.element_size(DbfCode::String))
        };
        Some(Self {
            source: CharSource::Field,
            no_elements,
            truncate: desc.declared_dbf == DbfCode::String,
        })
    }

    fn of_view(view: &CharView) -> Self {
        Self {
            source: view.source,
            no_elements: view.no_elements,
            truncate: view.truncate,
        }
    }

    /// Which of `dbGet`'s routes reads the field.
    pub fn source(&self) -> CharSource {
        self.source
    }

    /// `paddr->no_elements`: the string's `field_size`, or `PVLINK_STRINGSZ`
    /// for a link.
    pub fn no_elements(&self) -> usize {
        self.no_elements
    }

    /// Whether pvxs re-views the channel WITHOUT `$` as this char array, so
    /// it serves a long string and puts through `putLongString`: a link, or
    /// a scalar string with room past `MAX_STRING_SIZE + 1`
    /// (`ioc/channel.cpp:56-72`).
    pub fn re_views_plain_channel(&self) -> bool {
        match self.source {
            CharSource::Link => true,
            CharSource::Field => self.no_elements > MAX_STRING_SIZE + 1,
            CharSource::Attribute => false,
        }
    }

    /// [`CharView::nul_at_count`].
    pub(crate) fn nul_at_count(&self) -> bool {
        self.source != CharSource::Field || self.truncate
    }

    /// [`DollarView`]'s `pad`: a `DBF_STRING` field's `field_size`.
    fn pad(&self) -> usize {
        if self.source == CharSource::Field && self.truncate {
            self.no_elements
        } else {
            0
        }
    }
}

/// `info(Q:form, "String")` on `inst`.
pub(crate) fn form_string_of(inst: &RecordInstance) -> bool {
    inst.get_info("Q:form") == Some("String")
}

/// C's `MAX_STRING_SIZE` (`epicsTypes.h:59`), the `field_size` of a
/// `DBF_STRING` array element and of an attribute.
pub(crate) const MAX_STRING_SIZE: usize = 40;

/// C's `PVLINK_STRINGSZ` (`dbDefs.h:79`), the char-array length `$` gives a
/// link field.
pub(crate) const PVLINK_STRINGSZ: usize = 1024;

/// lsi/lso/printf `LEN`, which their `get_array_info` reports as VAL's
/// element count (`lsiRecord.c:154`, `lsoRecord.c:196`, `printfRecord.c:432`):
/// 0 until VAL is first written, `strlen + 1` after.
pub(crate) fn long_string_len<R: ViewedRecord + ?Sized>(inst: &R) -> usize {
    match inst.resolve_field("LEN") {
        Some(EpicsValue::ULong(n)) => n as usize,
        _ => 0,
    }
}

/// lsi/lso/printf `SIZV`, the buffer their `cvt_dbaddr` serves VAL in.
pub(crate) fn long_string_sizv<R: ViewedRecord + ?Sized>(inst: &R) -> usize {
    match inst.resolve_field("SIZV") {
        Some(EpicsValue::UShort(n)) => usize::from(n),
        Some(v) => v.to_f64().map_or(0, |n| n as usize),
        None => 0,
    }
}

/// Where a `$` address's characters come from — `dbGet`'s three routes
/// (`dbAccess.c:944-952`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CharSource {
    /// `pfield` itself, converted element by element.
    Field,
    /// `getLinkValue`: the link's `dbGetString` text.
    Link,
    /// `getAttrValue`: the attribute's text.
    Attribute,
}

impl CharSource {
    /// The text `dbGet` reads in place of converting the field, `None` for
    /// [`CharSource::Field`].
    pub fn text_route(self) -> Option<TextRoute> {
        match self {
            CharSource::Field => None,
            CharSource::Link => Some(TextRoute::Link),
            CharSource::Attribute => Some(TextRoute::Attribute),
        }
    }
}

/// A field `dbGet` reads as text whatever the request type, through
/// `getLinkValue` or `getAttrValue` (`dbAccess.c:817-895`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextRoute {
    Link,
    Attribute,
}

impl TextRoute {
    /// The read made for a `target` request, `None` for `S_db_badDbrtype`.
    pub fn read(self, target: crate::types::DbFieldType) -> Option<TextRead> {
        use crate::types::DbFieldType as T;
        match (self, target) {
            (_, T::String) => Some(TextRead::String),
            // "Needed for dbCa links" (`dbAccess.c:838-841`).
            (TextRoute::Link, T::Double) => Some(TextRead::Nan),
            (_, T::Char | T::UChar) => Some(TextRead::Chars(target)),
            _ => None,
        }
    }
}

/// One request type [`TextRoute::read`] accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextRead {
    /// `DBR_STRING`: the text in `MAX_STRING_SIZE` with its NUL, one element.
    String,
    /// A link's `DBR_DOUBLE`: one NaN.
    Nan,
    /// `DBR_CHAR` / `DBR_UCHAR`: the text in `nRequest` chars with its NUL,
    /// `strlen + 1` elements.
    Chars(crate::types::DbFieldType),
}

impl TextRead {
    /// The elements read of `text` for a request of `n_req`.
    pub fn get(self, text: &[u8], n_req: usize) -> EpicsValue {
        let upto = |max: usize| -> Vec<u8> {
            let end = text.len().min(max);
            let mut t: Vec<u8> = text[..end]
                .iter()
                .copied()
                .take_while(|&c| c != 0)
                .collect();
            t.push(0);
            t
        };
        match self {
            TextRead::String => {
                let mut t = upto(MAX_STRING_SIZE - 1);
                t.pop();
                EpicsValue::String(crate::types::PvString::from_bytes(t))
            }
            TextRead::Nan => EpicsValue::Double(f64::NAN),
            // `dbGet` returns before the read for `nRequest == 0`.
            TextRead::Chars(target) if n_req == 0 => char_elements(target, &[]),
            TextRead::Chars(target) => char_elements(target, &upto(n_req - 1)),
        }
    }
}

/// The characters `value` holds: a string's, a char array's, the first
/// element of a string array's.
pub(crate) fn text_of(value: &EpicsValue) -> &[u8] {
    match value {
        EpicsValue::String(s) => s.as_bytes(),
        EpicsValue::CharArray(b) | EpicsValue::UCharArray(b) => b,
        EpicsValue::StringArray(v) => v.first().map_or(&[], |s| s.as_bytes()),
        _ => &[],
    }
}

/// C's `DBADDR` after the `$` modifier (`dbAccess.c:682-701`) and what
/// `dbGet` / `dbPut` do with it.
pub struct CharView {
    pub source: CharSource,
    /// The field's value as the record resolves it; a link's or attribute's
    /// text.
    value: Option<EpicsValue>,
    /// `dbGet`'s element count: `get_array_info`'s for an `SPC_DBADDR`
    /// field, else `no_elements`.
    pub count: usize,
    /// `paddr->no_elements`: the string's `field_size`, or
    /// `PVLINK_STRINGSZ` for a link.
    pub no_elements: usize,
    /// `paddr->field_type`: `DBF_CHAR`, or a link's own type.
    pub field_type: DbfCode,
    /// `pfldDes->field_type == DBF_STRING`, so a `DBR_CHAR` read ends at the
    /// first NUL (`dbAccess.c:1037-1045`).
    pub truncate: bool,
}

impl CharView {
    /// [`CharAddr::re_views_plain_channel`].
    pub fn re_views_plain_channel(&self) -> bool {
        self.addr().re_views_plain_channel()
    }

    /// The address this view reads through.
    pub fn addr(&self) -> CharAddr {
        CharAddr::of_view(self)
    }

    /// The characters `pfield` holds, zero-padded to `no_elements`; a link's
    /// or attribute's text.
    pub fn bytes(&self) -> Vec<u8> {
        let mut bytes = self
            .value
            .as_ref()
            .map(text_of)
            .unwrap_or_default()
            .to_vec();
        if self.source == CharSource::Field {
            bytes.resize(self.no_elements, 0);
        }
        bytes
    }

    /// A `DBR_CHAR` read of `n` elements ends with a NUL at `n - 1`:
    /// `dbGet`'s rule for a `DBF_STRING` field (`dbAccess.c:1037-1045`) and
    /// the text a link or attribute read copies. A long-string record's
    /// `SPC_DBADDR` buffer is read raw.
    pub fn nul_at_count(&self) -> bool {
        self.addr().nul_at_count()
    }

    /// `dbGet(paddr, target, …, &n_req)`: the elements read, counted as C
    /// counts them, or `None` for a refused request type.
    pub fn get(&self, target: crate::types::DbFieldType, n_req: usize) -> Option<EpicsValue> {
        use crate::types::DbFieldType as T;
        if n_req == 0 {
            return Some(char_elements(target, &[]));
        }
        let bytes = self.bytes();
        if let Some(route) = self.source.text_route() {
            return Some(route.read(target)?.get(&bytes, n_req));
        }
        let n = field_count(self.count, n_req).min(bytes.len());
        let mut b = bytes[..n].to_vec();
        if self.count != 1 && target == T::Char && self.truncate && n > 0 {
            b[n - 1] = 0;
            let len = b.iter().position(|&c| c == 0).unwrap_or(n);
            b.truncate(len + 1);
        }
        Some(char_elements(target, &b))
    }

    /// [`Self::get`]`(DBR_CHAR, count)` — every element of the view, as a
    /// `$` link reads it — built in one allocation from the value the view
    /// holds, or none where a long string's own buffer already is the image.
    pub fn into_chars(self) -> EpicsValue {
        use std::iter::{once, repeat_n};
        let count = self.count;
        if count == 0 {
            return EpicsValue::CharArray(Vec::new().into());
        }
        let src: &[u8] = self.value.as_ref().map_or(&[], text_of);
        // Text up to the first NUL within `max` bytes, then the NUL.
        let text = |max: usize| -> crate::types::SharedArray<u8> {
            let end = src.len().min(max);
            let len = src[..end].iter().position(|&c| c == 0).unwrap_or(end);
            src[..len]
                .iter()
                .copied()
                .chain(once(0))
                .collect::<std::sync::Arc<[u8]>>()
                .into()
        };
        let chars = match self.source {
            CharSource::Field if count == 1 => [src.first().copied().unwrap_or(0)][..].into(),
            CharSource::Field => {
                let n = count.min(self.no_elements);
                if n == 0 {
                    Vec::new().into()
                } else if self.truncate {
                    text(n - 1)
                } else if let Some(EpicsValue::CharArray(b)) = &self.value
                    && b.len() >= n
                {
                    b.head(n)
                } else {
                    let m = src.len().min(n);
                    src[..m]
                        .iter()
                        .copied()
                        .chain(repeat_n(0, n - m))
                        .collect::<std::sync::Arc<[u8]>>()
                        .into()
                }
            }
            CharSource::Link | CharSource::Attribute => text(count - 1),
        };
        EpicsValue::CharArray(chars)
    }

    /// `dbPut`'s conversion of `written` through the `DBR_CHAR` address into
    /// the value the field stores, or `None` where C refuses it. Only
    /// `db_put_at`'s `$` arm calls it, under the put's hold.
    pub(crate) fn put_value(&self, written: CharWrite) -> Option<EpicsValue> {
        let inst_value = self.value.as_ref();
        use crate::types::DbFieldType as T;
        match self.source {
            // `dbPutField` returns `S_db_noMod` (`dbAccess.c:1254`).
            CharSource::Attribute => None,
            // `dbPutFieldLink` takes a string, or a char array whose last
            // element is its NUL (`dbAccess.c:1087-1099`).
            CharSource::Link => {
                let text = match written {
                    CharWrite::Chars(b) => b.split(|&c| c == 0).next().unwrap_or(&[]).to_vec(),
                    CharWrite::Elements(T::String, EpicsValue::String(s)) => s.as_bytes().to_vec(),
                    CharWrite::Elements(T::Char | T::UChar, v) if v.to_f64() == Some(0.0) => {
                        Vec::new()
                    }
                    CharWrite::Elements(..) => return None,
                };
                Some(EpicsValue::String(crate::types::PvString::from_bytes(text)))
            }
            CharSource::Field => {
                let dbaddr = !matches!(inst_value, Some(EpicsValue::String(_)));
                let chars: Vec<u8> = match written {
                    CharWrite::Chars(b) => b,
                    // `dbFastPutConvertRoutine` for one element into a plain
                    // string field, else `dbPutConvertRoutine` element by
                    // element (`dbAccess.c:1350`).
                    CharWrite::Elements(T::String, v) => {
                        use crate::types::c_parse::{Converted, NumericField};
                        let stored = match v {
                            EpicsValue::String(s) if !dbaddr => crate::types::c_parse::put_string(
                                "VAL",
                                NumericField::Char,
                                &s.as_str_lossy(),
                            ),
                            EpicsValue::String(s) => crate::types::c_parse::put_string_elements(
                                "VAL",
                                NumericField::Char,
                                &[s],
                            ),
                            EpicsValue::StringArray(a) => {
                                crate::types::c_parse::put_string_elements(
                                    "VAL",
                                    NumericField::Char,
                                    &a,
                                )
                            }
                            _ => return None,
                        };
                        match stored {
                            Ok(Converted::Stored(v)) => char_bytes(&v),
                            // Nothing is written and the put succeeds.
                            Ok(Converted::Unchanged) => return inst_value.cloned(),
                            Err(_) => return None,
                        }
                    }
                    CharWrite::Elements(_, v) => char_bytes(&v.convert_to(T::Char)),
                };
                let n = chars.len().min(self.no_elements);
                let mut buf = self.bytes();
                buf[..n].copy_from_slice(&chars[..n]);
                Some(match inst_value {
                    Some(EpicsValue::StringArray(old)) => {
                        // waveform `put_array_info` makes NORD the count
                        // just written.
                        let mut elems: Vec<crate::types::PvString> = old.to_vec();
                        elems.resize(n.max(1), crate::types::PvString::from_bytes(Vec::new()));
                        elems.truncate(n);
                        if let Some(first) = elems.first_mut() {
                            *first = crate::types::PvString::from_bytes(nul_text(&buf));
                        }
                        EpicsValue::StringArray(elems.into())
                    }
                    // lsi/lso `put_array_info` terminates at the count
                    // written, a full buffer one short (`lsoRecord.c:206-209`).
                    // Never a `String`: the record takes that as a
                    // `DBR_STRING` put and caps it at 40.
                    _ if dbaddr => {
                        if let Some(end) = buf.len().checked_sub(1) {
                            buf[n.min(end)] = 0;
                        }
                        EpicsValue::CharArray(buf.into())
                    }
                    _ => EpicsValue::String(crate::types::PvString::from_bytes(nul_text(&buf))),
                })
            }
        }
    }
}

/// What a `$` put writes.
pub(crate) enum CharWrite {
    /// `dbpf`'s `DBR_CHAR` array, NUL included.
    Chars(Vec<u8>),
    /// Elements of a request type: `dbtpf`'s one, a CA put's `count`.
    Elements(crate::types::DbFieldType, EpicsValue),
}

/// A PVA put's `dbPut(DBR_CHAR)` request through a `$` address: a string is
/// `putLongString`, its bytes and NUL (`ioc/iocsource.cpp:511`); an
/// `int8_t[]` is `putArray`, its elements. `None` for any other payload.
pub fn pva_char_request(value: &EpicsValue) -> Option<EpicsValue> {
    match value {
        EpicsValue::String(s) => {
            let mut chars = s.as_bytes().to_vec();
            chars.push(0);
            Some(EpicsValue::CharArray(chars.into()))
        }
        EpicsValue::CharArray(_) | EpicsValue::UCharArray(_) => Some(value.clone()),
        _ => None,
    }
}

impl CharWrite {
    /// A put's request buffer: a char array is `DBR_CHAR` elements, any
    /// other value elements of its own type.
    pub(crate) fn of(value: EpicsValue) -> Self {
        match value {
            EpicsValue::CharArray(b) | EpicsValue::UCharArray(b) => Self::Chars(b.to_vec()),
            v => Self::Elements(v.db_field_type(), v),
        }
    }
}

/// A `DBR_CHAR` conversion's elements as the bytes `epicsInt8` stores.
fn char_bytes(v: &EpicsValue) -> Vec<u8> {
    match v {
        EpicsValue::CharArray(b) | EpicsValue::UCharArray(b) => b.to_vec(),
        v => vec![v.to_f64().map_or(0, |n| n as i64 as u8)],
    }
}

/// How many elements `dbGet` reads from a `$` field whose `get_array_info`
/// count is `count`, for an `n_req`-element request. One element is the
/// scalar path, with no terminator rule (`dbAccess.c:963`).
fn field_count(count: usize, n_req: usize) -> usize {
    if count == 1 { 1 } else { n_req.min(count) }
}

/// The bytes before the first NUL.
pub(crate) fn nul_text(b: &[u8]) -> Vec<u8> {
    b.iter().copied().take_while(|&c| c != 0).collect()
}

/// `dbGetConvertRoutine[DBF_CHAR][target]` over `chars`, each an
/// `epicsInt8`.
pub(crate) fn char_elements(target: crate::types::DbFieldType, chars: &[u8]) -> EpicsValue {
    use crate::types::DbFieldType as T;
    let i = |c: &u8| *c as i8;
    match target {
        T::String => EpicsValue::StringArray(
            chars
                .iter()
                .map(|c| crate::types::PvString::from_bytes(i(c).to_string().into_bytes()))
                .collect::<Vec<_>>()
                .into(),
        ),
        T::Char => EpicsValue::CharArray(chars.to_vec().into()),
        T::UChar => EpicsValue::UCharArray(chars.to_vec().into()),
        T::Short => EpicsValue::ShortArray(
            chars
                .iter()
                .map(|c| i16::from(i(c)))
                .collect::<Vec<_>>()
                .into(),
        ),
        T::UShort => {
            EpicsValue::UShortArray(chars.iter().map(|c| i(c) as u16).collect::<Vec<_>>().into())
        }
        T::Long => EpicsValue::LongArray(
            chars
                .iter()
                .map(|c| i32::from(i(c)))
                .collect::<Vec<_>>()
                .into(),
        ),
        T::ULong => {
            EpicsValue::ULongArray(chars.iter().map(|c| i(c) as u32).collect::<Vec<_>>().into())
        }
        T::Int64 => EpicsValue::Int64Array(
            chars
                .iter()
                .map(|c| i64::from(i(c)))
                .collect::<Vec<_>>()
                .into(),
        ),
        T::UInt64 => {
            EpicsValue::UInt64Array(chars.iter().map(|c| i(c) as u64).collect::<Vec<_>>().into())
        }
        T::Float => EpicsValue::FloatArray(
            chars
                .iter()
                .map(|c| f32::from(i(c)))
                .collect::<Vec<_>>()
                .into(),
        ),
        T::Double => EpicsValue::DoubleArray(
            chars
                .iter()
                .map(|c| f64::from(i(c)))
                .collect::<Vec<_>>()
                .into(),
        ),
        T::Enum => {
            EpicsValue::EnumArray(chars.iter().map(|c| i(c) as u16).collect::<Vec<_>>().into())
        }
    }
}

/// A long-string field's `LEN` elements ([`FieldDesc::long_string`]) as its
/// plain `DBF_STRING` address reads them: the text before the first NUL. Any
/// other value is returned unchanged.
///
/// [`FieldDesc::long_string`]: crate::server::record::FieldDesc::long_string
pub fn long_string_text(value: EpicsValue) -> EpicsValue {
    match value {
        EpicsValue::CharArray(b) | EpicsValue::UCharArray(b) => {
            EpicsValue::String(crate::types::PvString::from_bytes(nul_text(&b)))
        }
        value => value,
    }
}

/// A long-string field's buffer as its plain address reads it: `cvt_dbaddr`
/// makes it one `DBF_STRING` of `field_size = SIZV` (`lsiRecord.c:122`,
/// `lsoRecord.c:164`, `printfRecord.c:410`), so the text runs to the
/// buffer's NUL, past `LEN` where a device scanned into VAL and then failed
/// (devlsiStream.c). Only the char view is `LEN` elements.
pub fn long_string_field_bytes(b: &crate::types::SharedArray<u8>) -> &[u8] {
    let buf = &b.buffer()[..];
    &buf[..buf.iter().position(|&c| c == 0).unwrap_or(buf.len())]
}

/// [`long_string_field_bytes`] as the one `DBF_STRING` element; any other
/// value is returned unchanged.
pub fn long_string_field_text(value: EpicsValue) -> EpicsValue {
    match value {
        EpicsValue::CharArray(b) | EpicsValue::UCharArray(b) => EpicsValue::String(
            crate::types::PvString::from_bytes(long_string_field_bytes(&b)),
        ),
        value => value,
    }
}

/// Whether pvxs reads the plain channel on a field `desc` routed `source`
/// for a reshape at all: a link, or a field declared `DBF_STRING`.
fn re_view_candidate(source: CharSource, desc: Option<&FieldDesc>) -> bool {
    match source {
        CharSource::Link => true,
        CharSource::Field => desc.is_some_and(|desc| {
            !desc.runtime_typed && field_addr_types(desc, None).0 == DbfCode::String
        }),
        CharSource::Attribute => false,
    }
}

/// A record channel's address as `dbChannelCreate` makes it
/// (`dbChannel.c:470-505`) and pvxs's `Channel` re-views it
/// (`ioc/channel.cpp:40-73`), from the declaration and what the cell settled
/// at load, init and each retyping `special`. C takes no lock for either, so
/// a channel classified from this never waits out a holder of its record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelAddr {
    /// The declared type, or the one a runtime-typed field settled.
    field_type: DbFieldType,
    /// `no_elements` without `$`.
    no_elements: usize,
    long_string: bool,
    /// `info(Q:form, "String")` on the record.
    q_form: bool,
    /// The `$` address; `None` where `$` is `S_dbLib_fieldNotFound`.
    dollar: Option<CharAddr>,
    /// The char address pvxs reshapes the plain channel into
    /// (`ioc/channel.cpp:56-73`).
    re_view: Option<CharAddr>,
}

impl ChannelAddr {
    /// Of `field` on `cell`; `None` where no channel reads it: a name the
    /// type does not declare and no attribute answers, or a field no channel
    /// may read.
    pub fn of(
        db: &crate::server::database::PvDatabase,
        cell: &crate::server::record::RecordCell,
        field: &str,
    ) -> Option<Self> {
        let desc = crate::server::record::record_instance::field_desc_in(cell.rdes().fields, field);
        let dollar = CharAddr::of(db, cell, field);
        let (source, field_type) = match desc {
            // `dbGetAttributePart`'s `DBF_STRING`.
            None => (dollar?.source, DbFieldType::String),
            Some(desc) if desc.unreadable() => return None,
            Some(desc) => {
                let source = match crate::types::link_class_of(desc) {
                    Some(_) => CharSource::Link,
                    None => CharSource::Field,
                };
                (
                    source,
                    cell.addressed_field_type(field, None)?.served_type()?,
                )
            }
        };
        Some(Self {
            field_type,
            no_elements: cell.no_elements(field).unwrap_or(1) as usize,
            long_string: desc.is_some_and(|desc| desc.long_string),
            q_form: cell.form_string(),
            dollar,
            re_view: dollar
                .filter(|addr| re_view_candidate(source, desc) && addr.re_views_plain_channel()),
        })
    }

    /// The declared type, or the one a runtime-typed field settled.
    pub fn field_type(&self) -> DbFieldType {
        self.field_type
    }

    /// The type the plain channel's value comes in: a long string's
    /// `DBF_CHAR` buffer, else [`Self::field_type`].
    pub fn value_type(&self) -> DbFieldType {
        if self.long_string {
            DbFieldType::Char
        } else {
            self.field_type
        }
    }

    /// `no_elements` without `$`.
    pub fn no_elements(&self) -> usize {
        self.no_elements
    }

    /// [`FieldDesc::long_string`].
    pub fn long_string(&self) -> bool {
        self.long_string
    }

    /// The char address pvxs re-views the plain channel as.
    pub fn re_view(&self) -> Option<&CharAddr> {
        self.re_view.as_ref()
    }

    /// The `$` channel on `field` through `filters`, `None` where `$` is
    /// `S_dbLib_fieldNotFound`.
    pub fn dollar_view(&self, field: &str, filters: &FilterChain) -> Option<DollarView> {
        Some(DollarView::of_addr(
            self.dollar?,
            self.q_form,
            field,
            filters,
        ))
    }

    /// pvxs `dbChannelFinalFieldType` before the filters' probe, which C
    /// seeds from `addr.dbr_field_type` (`dbChannel.c:579`): `DBR_CHAR` for a
    /// char address — `$` (`dbChannel.c:486-501`, a link's included) or
    /// pvxs's reshape without it (`ioc/channel.cpp:58-71`) — else the
    /// field's type.
    pub fn channel_field_type(&self, string_view: bool) -> DbFieldType {
        if string_view || self.re_view.is_some() {
            DbFieldType::Char
        } else {
            self.field_type
        }
    }

    /// [`pvxs_form_string`] for this channel on `field` read through `view`.
    pub fn form_string(&self, field: &str, view: Option<&CharAddr>, dollar: bool) -> bool {
        pvxs_form_string(field, self.q_form, view, dollar)
    }
}

/// pvxs's `chan.format() == "String"` (`ioc/channel.cpp:40-73`) for a channel
/// on `field` whose char address is `view`, on a record with
/// `info(Q:form, "String")` when `q_form`: VAL's tag, or the reshape of a
/// link or, without `$`, of a string wider than `MAX_STRING_SIZE + 1`. `$`
/// keeps a link's type (`dbChannel.c:488-493`), so only a link reshapes
/// under it.
pub fn pvxs_form_string(field: &str, q_form: bool, view: Option<&CharAddr>, dollar: bool) -> bool {
    (crate::server::database::is_value_field(field) && q_form)
        || view.is_some_and(|v| {
            if dollar {
                v.source == CharSource::Link
            } else {
                v.re_views_plain_channel()
            }
        })
}

/// What pvxs serves a `$` channel as, from its `getChannelValueType`
/// (`ioc/iocsource.cpp:619-643`). The native PVA source and QSRV both ask
/// it, so the two servers cannot part on a `$` channel's type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PvaDollar {
    /// The view's text: its format is `"String"` ([`pvxs_form_string`]).
    Text,
    /// The `DBR_CHAR` image, `int8_t[]`: `dbChannelCreate` made the field
    /// `DBF_CHAR` (`dbChannel.c:488-493`), so the format stays `Default`.
    Chars,
}

impl PvaDollar {
    /// Of a `$` channel on `field` addressed `addr`, on a record with
    /// `info(Q:form, "String")` when `form_string`: [`pvxs_form_string`]'s
    /// `$` answer.
    fn of_addr(addr: &CharAddr, form_string: bool, field: &str) -> Self {
        if (crate::server::database::is_value_field(field) && form_string)
            || addr.source == CharSource::Link
        {
            Self::Text
        } else {
            Self::Chars
        }
    }
}

/// A `$` channel's read, split where C's filters run: [`Self::render`] is the
/// `DBR_CHAR` image a field log holds, [`Self::finish`] what `dbGet` and
/// pvxs then make of the log a filter chain handed back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DollarView {
    /// What pvxs serves the channel as.
    pub serve: PvaDollar,
    /// `field_size` of a `DBF_STRING` field, which `dbGet` reads as that
    /// many chars and then cuts at the NUL (`dbAccess.c:1037-1045`); 0 for
    /// a link or an attribute, which `dbGet` does not cut.
    pad: usize,
    source: CharSource,
    /// `dbChannelFinalElements`: the `nRequest` pvxs reads with.
    final_elements: usize,
}

impl DollarView {
    /// Of a `$` channel on `field` whose filter chain is `filters`, or
    /// `None` where `$` is `S_dbLib_fieldNotFound`.
    pub fn of(inst: &RecordInstance, field: &str, filters: &FilterChain) -> Option<Self> {
        let view = char_view(inst, field)?;
        Some(Self::of_addr(
            CharAddr::of_view(&view),
            form_string_of(inst),
            field,
            filters,
        ))
    }

    /// [`Self::of`] the channel addressed `addr`, on a record with
    /// `info(Q:form, "String")` when `form_string`.
    pub(crate) fn of_addr(
        addr: CharAddr,
        form_string: bool,
        field: &str,
        filters: &FilterChain,
    ) -> Self {
        Self {
            serve: PvaDollar::of_addr(&addr, form_string, field),
            pad: addr.pad(),
            source: addr.source,
            final_elements: filters.final_element_count(addr.no_elements),
        }
    }

    /// Of a channel pvxs serves as text without `$` (`ioc/iocsource.cpp:633`)
    /// and `final_elements` long: read through the char address pvxs
    /// reshaped it to, `view` ([`ChannelAddr::re_view`]), which is the one
    /// `dbChannelCreate` makes for `$`, or through a `DBF_CHAR` field's own.
    pub fn of_text_channel(view: Option<&CharAddr>, final_elements: usize) -> Self {
        Self {
            serve: PvaDollar::Text,
            pad: view.map_or(0, CharAddr::pad),
            source: view.map_or(CharSource::Field, |v| v.source),
            final_elements,
        }
    }

    /// Of a `$` channel on a `DBF_STRING` value `field_size` chars wide that
    /// no record holds, whose filter chain is `filters`.
    pub fn of_string(field_size: usize, filters: &FilterChain) -> Self {
        Self {
            serve: PvaDollar::Chars,
            pad: field_size,
            source: CharSource::Field,
            final_elements: filters.final_element_count(field_size),
        }
    }

    /// Whether the channel serves one `int8_t` instead of an array: pvxs's
    /// `getScalarValue` for a final count of 1.
    pub fn scalar(self) -> bool {
        self.final_elements == 1
    }

    /// Which of `dbGet`'s routes reads the field.
    pub fn source(self) -> CharSource {
        self.source
    }

    /// The field log's image of the field's value — read, or posted: a
    /// `DBF_STRING` field's `field_size` chars, else the posted chars.
    pub fn render(self, value: EpicsValue) -> EpicsValue {
        match value {
            EpicsValue::String(text) if self.pad > 0 => {
                let text = text.as_bytes();
                let len = text.iter().position(|&c| c == 0).unwrap_or(text.len());
                let mut image = Vec::with_capacity(self.pad.max(len + 1));
                image.extend_from_slice(&text[..len]);
                image.resize(self.pad.max(len + 1), 0);
                EpicsValue::CharArray(image.into())
            }
            value => posted_char_view(value, usize::MAX, false),
        }
    }

    /// What is served of the `image` [`Self::render`] made, given the `log`
    /// a filter chain made of it: [`Self::read`], then [`Self::text`].
    pub fn finish(self, image: EpicsValue, log: EpicsValue) -> EpicsValue {
        self.text(self.read(image, log))
    }

    /// `dbGet`'s chars from the `log` a filter chain made of `image`. A
    /// field's are the log, cut (`Self::cut`). A link's or an attribute's
    /// are its text afresh: `getLinkValue`/`getAttrValue` ignore the log and
    /// keep only its count, `nRequest` (`dbAccess.c:817-895,945-952`).
    pub fn read(self, image: EpicsValue, log: EpicsValue) -> EpicsValue {
        self.read_request(image, log, usize::MAX)
    }

    /// [`Self::read`] for a `dbGet` of `n_request` chars, a CA client's
    /// count: it bounds the copy before the last char becomes the NUL.
    pub fn read_request(self, image: EpicsValue, log: EpicsValue, n_request: usize) -> EpicsValue {
        if self.source == CharSource::Field {
            drop(image);
            // `getCharChar` NULs char `nRequest - 1` of a log of more than
            // one element, whatever `nRequest`; a one-element log takes the
            // fast copy (`dbAccess.c` `dbGet`, `dbConvert.c` `getCharChar`).
            let log = match log {
                EpicsValue::CharArray(mut b)
                    if self.pad > 0 && n_request < b.len() && b.len() > 1 =>
                {
                    b.truncate(n_request.max(1));
                    let last = b.len() - 1;
                    if let Some(chars) = b.head_mut() {
                        chars[last] = 0;
                    } else {
                        b.make_mut()[last] = 0;
                    }
                    EpicsValue::CharArray(b)
                }
                log => log,
            };
            return self.cut(log);
        }
        let final_elements = self.final_elements.min(n_request);
        drop(log);
        match image {
            // `strncpy` into `nRequest` chars, the last a NUL, then
            // `strlen + 1`; nothing at all for `nRequest == 0`.
            EpicsValue::CharArray(mut b) => {
                let len = b.iter().position(|&c| c == 0).unwrap_or(b.len());
                let Some(keep) = final_elements.checked_sub(1) else {
                    return EpicsValue::CharArray(Default::default());
                };
                let keep = keep.min(len);
                if keep < b.len() {
                    if let Some(chars) = b.head_mut() {
                        chars[keep] = 0;
                    } else {
                        b.make_mut()[keep] = 0;
                    }
                    b.truncate(keep + 1);
                } else {
                    b.push(0);
                }
                EpicsValue::CharArray(b)
            }
            value => value,
        }
    }

    /// `dbGet`'s tail on a `DBF_STRING` field's read: a NUL in the last
    /// element and a cut to `strlen + 1` (`dbAccess.c:1037-1045`), except a
    /// one-element read, which takes `dbGet`'s scalar path.
    fn cut(self, value: EpicsValue) -> EpicsValue {
        match value {
            EpicsValue::CharArray(mut b) if self.pad > 0 && b.len() > 1 => {
                let last = b.len() - 1;
                if b[last] != 0 {
                    // An `arr` prefix is a view of the render's buffer, which
                    // nothing else holds by now.
                    if let Some(chars) = b.head_mut() {
                        chars[last] = 0;
                    } else {
                        b.make_mut()[last] = 0;
                    }
                }
                let len = b.iter().position(|&c| c == 0).unwrap_or(last);
                b.truncate(len + 1);
                EpicsValue::CharArray(b)
            }
            value => value,
        }
    }

    /// The text of a [`PvaDollar::Text`] array channel: the image up to its
    /// NUL (`ioc/iocsource.cpp:133-136`).
    ///
    /// Not pvxs's `buf[size-1]=0` (`:134`), which drops the last char of a
    /// full buffer: a C defect, and this read is bounded by the image's
    /// length, not by a NUL.
    pub fn text(self, value: EpicsValue) -> EpicsValue {
        match self.serve {
            PvaDollar::Text if !self.scalar() => long_string_text(value),
            _ => value,
        }
    }
}

/// `dbGet`'s `DBR_CHAR` answer for a `$` address, taken from the value a post
/// carries instead of from the record, for an `n`-element request: the text
/// and its NUL, NUL at element `n - 1`, where [`CharView::nul_at_count`]
/// holds; else a long string's `LEN` elements read raw, as
/// [`CharView::get`] reads them, so `LEN` 0 posts none. A delivery path that
/// holds no record lock reads this. Any other value is returned unchanged.
pub fn posted_char_view(value: EpicsValue, n: usize, nul_at_count: bool) -> EpicsValue {
    let mut chars = match value {
        EpicsValue::CharArray(mut b) | EpicsValue::UCharArray(mut b) if !nul_at_count => {
            b.truncate(field_count(b.len(), n));
            return EpicsValue::CharArray(b);
        }
        // An `FTVL=STRING` buffer: `NORD` elements of its first string's
        // 40 bytes, raw, as [`CharView::get`] reads them.
        EpicsValue::StringArray(v) if !nul_at_count => {
            let mut b = v.first().map(|s| s.as_bytes().to_vec()).unwrap_or_default();
            b.resize(field_count(v.len(), n).min(MAX_STRING_SIZE), 0);
            return EpicsValue::CharArray(b.into());
        }
        EpicsValue::String(s) => nul_text(s.as_bytes()),
        EpicsValue::CharArray(b) | EpicsValue::UCharArray(b) => nul_text(&b),
        value => return value,
    };
    if nul_at_count {
        chars.truncate(n.saturating_sub(1));
        chars.push(0);
    } else {
        chars.push(0);
        chars.truncate(n);
    }
    EpicsValue::CharArray(chars.into())
}

#[cfg(test)]
mod tests {
    use super::{CharSource, CharView, DollarView, PvaDollar};

    /// `FS.VAL${"arr":{"s":0,"e":2}}` on `"hello"`: the prefix view is cut
    /// in the render's own buffer.
    #[test]
    fn cut_writes_a_prefix_slice_in_place() {
        let view = DollarView {
            serve: PvaDollar::Chars,
            pad: 40,
            source: CharSource::Field,
            final_elements: 40,
        };
        let EpicsValue::CharArray(image) = view.render(EpicsValue::String("hello".into())) else {
            panic!("a char image");
        };
        let prefix = image.head(3);
        drop(image);
        let buf = std::sync::Arc::as_ptr(prefix.buffer());
        let EpicsValue::CharArray(cut) = view.cut(EpicsValue::CharArray(prefix)) else {
            panic!("a char image");
        };
        assert_eq!(&*cut, b"he\0");
        assert_eq!(std::sync::Arc::as_ptr(cut.buffer()), buf, "no copy");
    }

    use crate::types::{DbFieldType, DbfCode, EpicsValue, PvString};

    fn view(
        source: CharSource,
        value: EpicsValue,
        count: usize,
        cap: usize,
        cut: bool,
    ) -> CharView {
        CharView {
            source,
            value: Some(value),
            count,
            no_elements: cap,
            field_type: DbfCode::Char,
            truncate: cut,
        }
    }

    #[test]
    fn into_chars_is_the_whole_char_get_at_every_boundary() {
        let s = |t: &str| EpicsValue::String(PvString::from(t));
        let c = |b: &[u8]| EpicsValue::CharArray(b.to_vec().into());
        let long = [b'x'; 45];
        let cases = [
            view(CharSource::Field, s("train"), 40, 40, true),
            view(CharSource::Field, c(&long), 40, 40, true),
            view(CharSource::Field, c(b"0123456789\0"), 11, 100, false),
            view(CharSource::Field, c(&long), 20, 20, false),
            view(CharSource::Field, c(b"abc\0"), 20, 20, false),
            view(CharSource::Field, c(b"abc"), 30, 20, false),
            view(CharSource::Field, s("q"), 1, 40, true),
            view(CharSource::Field, s("q"), 0, 40, true),
            view(CharSource::Field, s("q"), 5, 0, true),
            view(
                CharSource::Field,
                EpicsValue::StringArray(vec![PvString::from("ab"), PvString::from("cd")].into()),
                2,
                40,
                true,
            ),
            view(CharSource::Link, s("SRC.VAL NPP NMS"), 80, 80, false),
            view(CharSource::Link, s("SRC.VAL NPP NMS"), 5, 80, false),
            view(CharSource::Attribute, s("ai"), 40, 40, false),
        ];
        for v in cases {
            let want = v.get(DbFieldType::Char, v.count);
            let label = format!("{:?}/{}/{}", v.value, v.count, v.no_elements);
            assert_eq!(Some(v.into_chars()), want, "{label}");
        }
    }
}
