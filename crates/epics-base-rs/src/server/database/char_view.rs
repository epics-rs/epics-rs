//! C's `$` modifier on a database address (`dbAccess.c:682-701`) and what
//! `dbGet` / `dbPut` then do with the `DBR_CHAR` view it makes. One owner for
//! every front end that resolves a `REC.FIELD$` name — the iocsh `dbTest.c`
//! commands and the CA server — so they cannot disagree about which fields
//! take the modifier, how many elements the view has, or what a put stores.

use crate::server::record::{FieldDesc, RecordInstance, Special};
use crate::types::{DbfCode, EpicsValue};

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
        // C tests `paddr->special`, which `dbEntryToAddr` has just copied from
        // `pflddes->special` (`dbAccess.c:638-641`) — the DECLARED code, not
        // the one `cvt_dbaddr` then raises. `lsi.VAL` is the case in hand: it
        // declares `SPC_DBADDR` and resolves to `SPC_MOD`, so reading the
        // resolved code skipped the arm and labelled it `DBF_NOACCESS` where C
        // says `DBF_STRING` (`lsiRecord.c::cvt_dbaddr`).
        _ if fd.declared_special == Special::DbAddr => fd.dbf_type.dbf_code(),
        _ => fd.declared_dbf,
    };
    (field_type, field_type.dbr_code())
}

/// The view of `inst.field` with `$`, or `None` where C refuses the modifier.
/// It holds the resolved value and the counts only, so a caller under the
/// record's lock pays one field read; the character image is built from it
/// after the lock is gone.
pub fn char_view(inst: &RecordInstance, field: &str) -> Option<CharView> {
    let Some(desc) = inst.field_desc(field) else {
        // `dbGetAttributePart`'s synthesised `DBF_STRING` descriptor.
        return match inst.get_virtual_field(field)? {
            text @ EpicsValue::String(_) => Some(CharView {
                source: CharSource::Attribute,
                value: Some(text),
                count: MAX_STRING_SIZE,
                no_elements: MAX_STRING_SIZE,
                field_type: DbfCode::Char,
                truncate: false,
            }),
            _ => None,
        };
    };
    let value = inst.resolve_field(field);
    if crate::types::link_class_of(desc).is_some() {
        return Some(CharView {
            source: CharSource::Link,
            value,
            count: PVLINK_STRINGSZ,
            no_elements: PVLINK_STRINGSZ,
            field_type: desc.declared_dbf,
            truncate: false,
        });
    }
    if field_addr_types(desc, value.as_ref()).0 != DbfCode::String {
        return None;
    }
    // `no_elements = field_size`, and `dbGet`'s count is
    // `get_array_info`'s where the field is `SPC_DBADDR`.
    let (count, capacity) = match value.as_ref()? {
        EpicsValue::String(_) => (usize::from(desc.size), usize::from(desc.size)),
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

/// C's `MAX_STRING_SIZE` (`epicsTypes.h:59`), the `field_size` of a
/// `DBF_STRING` array element and of an attribute.
pub(crate) const MAX_STRING_SIZE: usize = 40;

/// C's `PVLINK_STRINGSZ` (`dbDefs.h:79`), the char-array length `$` gives a
/// link field.
pub(crate) const PVLINK_STRINGSZ: usize = 1024;

/// lsi/lso/printf `LEN`, which their `get_array_info` reports as VAL's
/// element count (`lsiRecord.c:154`, `lsoRecord.c:196`, `printfRecord.c:432`):
/// 0 until VAL is first written, `strlen + 1` after.
pub(crate) fn long_string_len(inst: &crate::server::record::RecordInstance) -> usize {
    match inst.resolve_field("LEN") {
        Some(EpicsValue::ULong(n)) => n as usize,
        _ => 0,
    }
}

/// lsi/lso/printf `SIZV`, the buffer their `cvt_dbaddr` serves VAL in.
pub(crate) fn long_string_sizv(inst: &crate::server::record::RecordInstance) -> usize {
    match inst.resolve_field("SIZV") {
        Some(EpicsValue::UShort(n)) => usize::from(n),
        Some(v) => v.to_f64().map_or(0, |n| n as usize),
        None => 0,
    }
}

/// Where a `$` address's characters come from — `dbGet`'s three routes
/// (`dbAccess.c:944-952`).
#[derive(Clone, Copy, PartialEq)]
pub enum CharSource {
    /// `pfield` itself, converted element by element.
    Field,
    /// `getLinkValue`: the link's `dbGetString` text.
    Link,
    /// `getAttrValue`: the attribute's text.
    Attribute,
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

    /// The characters `pfield` holds, zero-padded to `no_elements`; a link's
    /// or attribute's text.
    pub fn bytes(&self) -> Vec<u8> {
        let mut bytes = match &self.value {
            Some(EpicsValue::String(s)) => s.as_bytes().to_vec(),
            Some(EpicsValue::CharArray(b) | EpicsValue::UCharArray(b)) => b.to_vec(),
            Some(EpicsValue::StringArray(v)) => {
                v.first().map(|s| s.as_bytes().to_vec()).unwrap_or_default()
            }
            _ => Vec::new(),
        };
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
        self.source != CharSource::Field || self.truncate
    }

    /// `dbGet(paddr, target, …, &n_req)`: the elements read, counted as C
    /// counts them, or `None` for a refused request type.
    pub fn get(&self, target: crate::types::DbFieldType, n_req: usize) -> Option<EpicsValue> {
        use crate::types::DbFieldType as T;
        if n_req == 0 {
            return Some(char_elements(target, &[]));
        }
        let bytes = self.bytes();
        let text = |max: usize| -> Vec<u8> {
            let end = bytes.len().min(max);
            let mut t: Vec<u8> = bytes[..end]
                .iter()
                .copied()
                .take_while(|&c| c != 0)
                .collect();
            t.push(0);
            t
        };
        match (self.source, target) {
            (CharSource::Field, _) => {
                // `dbAccess.c:963`: one element is the scalar path, with no
                // terminator rule.
                let n = if self.count == 1 {
                    1
                } else {
                    n_req.min(self.count).min(bytes.len())
                };
                let mut b = bytes[..n].to_vec();
                if self.count != 1 && target == T::Char && self.truncate && n > 0 {
                    b[n - 1] = 0;
                    let len = b.iter().position(|&c| c == 0).unwrap_or(n);
                    b.truncate(len + 1);
                }
                Some(char_elements(target, &b))
            }
            (_, T::String) => {
                let mut t = text(MAX_STRING_SIZE - 1);
                t.pop();
                Some(EpicsValue::String(crate::types::PvString::from_bytes(t)))
            }
            (CharSource::Link, T::Double) => Some(EpicsValue::Double(f64::NAN)),
            (_, T::Char | T::UChar) => Some(char_elements(target, &text(n_req - 1))),
            _ => None,
        }
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
        let src: &[u8] = match &self.value {
            Some(EpicsValue::String(s)) => s.as_bytes(),
            Some(EpicsValue::CharArray(b) | EpicsValue::UCharArray(b)) => b,
            Some(EpicsValue::StringArray(v)) => v.first().map_or(&[], |s| s.as_bytes()),
            _ => &[],
        };
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

    /// `dbPutField(paddr, dbrType, …)` of `written` as the value the record's
    /// put path stores, or `None` where C refuses it.
    pub fn put_value(&self, written: CharWrite) -> Option<EpicsValue> {
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
pub enum CharWrite {
    /// `dbpf`'s `DBR_CHAR` array, NUL included.
    Chars(Vec<u8>),
    /// Elements of a request type: `dbtpf`'s one, a CA put's `count`.
    Elements(crate::types::DbFieldType, EpicsValue),
}

/// A `DBR_CHAR` conversion's elements as the bytes `epicsInt8` stores.
fn char_bytes(v: &EpicsValue) -> Vec<u8> {
    match v {
        EpicsValue::CharArray(b) | EpicsValue::UCharArray(b) => b.to_vec(),
        v => vec![v.to_f64().map_or(0, |n| n as i64 as u8)],
    }
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

/// `dbGet`'s `DBR_CHAR` answer for a `$` address, taken from the value a post
/// carries instead of from the record, for an `n`-element request: the text
/// and its NUL, NUL at element `n - 1` where
/// [`CharView::nul_at_count`] holds, else cut at `n`. That is
/// [`CharView::get`] for every value a post can carry, so a delivery path
/// that holds no record lock reads this. Any other value is returned
/// unchanged.
pub fn posted_char_view(value: EpicsValue, n: usize, nul_at_count: bool) -> EpicsValue {
    let mut chars = match &value {
        EpicsValue::String(s) => nul_text(s.as_bytes()),
        EpicsValue::CharArray(b) | EpicsValue::UCharArray(b) => nul_text(b),
        _ => return value,
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
    use super::{CharSource, CharView};
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
