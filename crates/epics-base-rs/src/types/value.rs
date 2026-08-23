use crate::error::{CaError, CaResult};
use std::fmt;

use super::DbFieldType;
use super::PvString;
use super::c_cast;

/// `sizeof(union native_value)` (C `dbFldTypes.h`) — the widest member is
/// `char [MAX_STRING_SIZE]`, so 40 bytes. The cut-off C uses to decide whether
/// a monitor event queue may hold a value inline; see
/// [`EpicsValue::queues_by_value`].
pub const NATIVE_VALUE_BYTES: usize = 40;

/// Runtime value from an EPICS PV
#[derive(Debug, Clone, PartialEq)]
pub enum EpicsValue {
    String(PvString),
    Short(i16),
    Float(f32),
    Enum(u16),
    /// An NTEnum value that still carries its `choices` labels, so a
    /// later type-aware `put_field` can pick the label-vs-index form
    /// the target dbrType requires (pvxs `pvalink_lset.cpp:344-356`:
    /// a `DBR_STRING` target copies `choices[index]`, a numeric target
    /// takes the index). This is a TRANSIENT carrier produced only on
    /// the pvalink NTEnum link-resolver path (`pvfield_to_epics_value`);
    /// the dbrType-blind [`ExternalPvResolver`](crate::server::database)
    /// boundary cannot resolve the label itself, so the choices must
    /// survive to the record's `put_field`. Everywhere else it behaves
    /// exactly like [`Self::Enum`]: read the public `index` field for the
    /// choice index. It is never stored in a record field nor serialized
    /// to the wire (those paths normalize it to the index).
    EnumWithChoices {
        index: u16,
        choices: Vec<PvString>,
    },
    Char(u8),
    Long(i32),
    Double(f64),
    /// Internal int64 storage for int64in/int64out records.
    /// Over CA these appear as DBR_DOUBLE; over PVA they are native int64.
    Int64(i64),
    /// Internal unsigned int64 storage for C `DBF_UINT64` fields.
    /// Over CA these appear as DBR_DOUBLE (precision lost above 2^53);
    /// over PVA they are native `ulong`.
    UInt64(u64),
    /// Unsigned 16-bit storage for C `DBF_USHORT` fields (e.g. `seq`/
    /// `fanout`/`sel` `SELN`). Over CA promoted to `DBR_LONG` (u16 fits in
    /// i32); over PVA served natively as `ushort`. See [`DbFieldType::UShort`].
    UShort(u16),
    /// Unsigned 32-bit storage for C `DBF_ULONG` fields (e.g. `ts` filter
    /// `num=sec|nsec`, `printf.LEN`). Over CA promoted to `DBR_DOUBLE`
    /// like `UInt64`; over PVA served natively as `uint`. See
    /// [`DbFieldType::ULong`].
    ULong(u32),
    /// Unsigned 8-bit storage for C `DBF_UCHAR` fields (`waveform`
    /// `FTVL=UCHAR` scalar element). Distinct from the signed `Char`
    /// (epicsInt8): over CA promoted to `DBR_CHAR` (identical 1-byte wire
    /// form as `Char`); over PVA served natively as `ubyte` (UInt8, not the
    /// signed `byte`). See [`DbFieldType::UChar`].
    UChar(u8),
    // Array variants
    ShortArray(Vec<i16>),
    FloatArray(Vec<f32>),
    EnumArray(Vec<u16>),
    DoubleArray(Vec<f64>),
    LongArray(Vec<i32>),
    CharArray(Vec<u8>),
    Int64Array(Vec<i64>),
    /// Unsigned int64 array storage for `waveform` records with
    /// `FTVL = UINT64`. Over CA served as DBR_DOUBLE; over PVA `ulong[]`.
    UInt64Array(Vec<u64>),
    /// Unsigned 16-bit array storage (`DBF_USHORT` waveform / filter
    /// result). Over CA promoted to `DBR_LONG[]`; over PVA `ushort[]`.
    UShortArray(Vec<u16>),
    /// Unsigned 32-bit array storage (`DBF_ULONG` waveform / `ts`
    /// `num=ts` two-element result). Over CA promoted to `DBR_DOUBLE[]`;
    /// over PVA `uint[]`.
    ULongArray(Vec<u32>),
    /// Unsigned 8-bit array storage (`DBF_UCHAR` waveform / `aai` / `aao`,
    /// the common image/byte-buffer shape). Distinct from the signed
    /// `CharArray` (epicsInt8): over CA promoted to `DBR_CHAR[]` (identical
    /// raw bytes); over PVA `ubyte[]` (UInt8), so element 200 stays 200 not
    /// −56. See [`DbFieldType::UChar`].
    UCharArray(Vec<u8>),
    /// DBR_STRING with `count > 1`. Each element is at most 40 bytes
    /// per the DBR_STRING spec; the wire layout is `count * 40` bytes
    /// of NUL-padded strings. Used by `mbbo`/`mbbi` choice arrays
    /// (ZNAM..FFNAM as a single read), NTNDArray dim labels, etc.
    StringArray(Vec<PvString>),
}

impl fmt::Display for EpicsValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::String(s) => write!(f, "{s}"),
            Self::Short(v) => write!(f, "{v}"),
            Self::Float(v) => write!(f, "{v}"),
            Self::Enum(v) => write!(f, "{v}"),
            // Transient NTEnum carrier: render the index, identical to
            // `Enum`. The labels are only consumed by a string-target
            // `put_field`, never by Display.
            Self::EnumWithChoices { index, .. } => write!(f, "{index}"),
            // DBF_CHAR is epicsInt8 (signed); C `getCharString`
            // (dbConvert.c) renders it via cvtCharToString → signed
            // cvtInt32ToString, so 0xFF -> "-1". Render the storage byte as
            // i8 to match — consistent with the signed `to_f64` view below.
            Self::Char(v) => write!(f, "{}", *v as i8),
            Self::Long(v) => write!(f, "{v}"),
            Self::Double(v) => write!(f, "{v}"),
            Self::Int64(v) => write!(f, "{v}"),
            Self::UInt64(v) => write!(f, "{v}"),
            Self::UShort(v) => write!(f, "{v}"),
            Self::ULong(v) => write!(f, "{v}"),
            // DBF_UCHAR is epicsUInt8: render the storage byte unsigned
            // (0xFF -> "255"), the counterpart of `Char`'s signed render.
            Self::UChar(v) => write!(f, "{v}"),
            Self::ShortArray(arr) => {
                let parts: Vec<_> = arr.iter().map(|v| v.to_string()).collect();
                write!(f, "[{}]", parts.join(", "))
            }
            Self::FloatArray(arr) => {
                let parts: Vec<_> = arr.iter().map(|v| v.to_string()).collect();
                write!(f, "[{}]", parts.join(", "))
            }
            Self::EnumArray(arr) => {
                let parts: Vec<_> = arr.iter().map(|v| v.to_string()).collect();
                write!(f, "[{}]", parts.join(", "))
            }
            Self::DoubleArray(arr) => {
                let parts: Vec<_> = arr.iter().map(|v| v.to_string()).collect();
                write!(f, "[{}]", parts.join(", "))
            }
            Self::LongArray(arr) => {
                let parts: Vec<_> = arr.iter().map(|v| v.to_string()).collect();
                write!(f, "[{}]", parts.join(", "))
            }
            Self::Int64Array(arr) => {
                let parts: Vec<_> = arr.iter().map(|v| v.to_string()).collect();
                write!(f, "[{}]", parts.join(", "))
            }
            Self::UInt64Array(arr) => {
                let parts: Vec<_> = arr.iter().map(|v| v.to_string()).collect();
                write!(f, "[{}]", parts.join(", "))
            }
            Self::UShortArray(arr) => {
                let parts: Vec<_> = arr.iter().map(|v| v.to_string()).collect();
                write!(f, "[{}]", parts.join(", "))
            }
            Self::ULongArray(arr) => {
                let parts: Vec<_> = arr.iter().map(|v| v.to_string()).collect();
                write!(f, "[{}]", parts.join(", "))
            }
            // DBF_UCHAR[] is a numeric unsigned-byte buffer (image/waveform
            // data), not text — render as a numeric array, unlike the signed
            // `CharArray` which decodes as a UTF-8 long string.
            Self::UCharArray(arr) => {
                let parts: Vec<_> = arr.iter().map(|v| v.to_string()).collect();
                write!(f, "[{}]", parts.join(", "))
            }
            Self::CharArray(arr) => match std::str::from_utf8(arr) {
                Ok(s) => write!(f, "{s}"),
                Err(_) => write!(f, "{arr:?}"),
            },
            Self::StringArray(arr) => {
                write!(f, "[")?;
                for (i, s) in arr.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{s:?}")?;
                }
                write!(f, "]")
            }
        }
    }
}

impl EpicsValue {
    /// The label for an enum `index` against its `choices`:
    /// `choices[index]` when in range, else the decimal index — pvxs
    /// `pvalink_lset.cpp:347-354` (`strncpy(choices[index])` with an
    /// `epicsSnprintf("%u", index)` out-of-range fallback). Used at the
    /// link-write boundary to render an NTEnum onto a string target.
    ///
    /// Returns the choice as a `PvString` so the raw label bytes are copied
    /// verbatim, matching C's `strncpy(choice.c_str(), …)`. Rendering an
    /// in-range choice through `String`/`as_str_lossy` here would rewrite a
    /// non-UTF-8 label with `U+FFFD` before it reached `EpicsValue::String`;
    /// the byte-faithful return keeps decode→store→re-encode lossless. The
    /// out-of-range decimal fallback is pure ASCII, so it is lossless too.
    pub fn enum_index_label(choices: &[PvString], index: u16) -> PvString {
        choices
            .get(index as usize)
            .cloned()
            .unwrap_or_else(|| PvString::from(index.to_string()))
    }

    /// Render for audit / log paths with array truncation. The full
    /// `Display` impl allocates `Vec<String>` of length N and joins —
    /// for a peer-controlled `LongArray` of millions of elements that
    /// is tens of MB of churn before any post-truncation. This helper
    /// caps array element count to `max_elems`, appending `, …+K
    /// more` on overflow.
    pub fn display_truncated(&self, max_elems: usize) -> String {
        fn render<T: fmt::Display>(arr: &[T], max: usize) -> String {
            if arr.len() <= max {
                let parts: Vec<String> = arr.iter().map(|v| v.to_string()).collect();
                format!("[{}]", parts.join(", "))
            } else {
                let parts: Vec<String> = arr[..max].iter().map(|v| v.to_string()).collect();
                format!("[{}, …+{} more]", parts.join(", "), arr.len() - max)
            }
        }
        match self {
            Self::ShortArray(arr) => render(arr, max_elems),
            Self::FloatArray(arr) => render(arr, max_elems),
            Self::EnumArray(arr) => render(arr, max_elems),
            Self::DoubleArray(arr) => render(arr, max_elems),
            Self::LongArray(arr) => render(arr, max_elems),
            Self::Int64Array(arr) => render(arr, max_elems),
            Self::UInt64Array(arr) => render(arr, max_elems),
            Self::UShortArray(arr) => render(arr, max_elems),
            Self::ULongArray(arr) => render(arr, max_elems),
            // Numeric unsigned-byte preview (not the binary/text CharArray
            // path below): DBF_UCHAR carries image/waveform numbers.
            Self::UCharArray(arr) => render(arr, max_elems),
            Self::StringArray(arr) => {
                if arr.len() <= max_elems {
                    let parts: Vec<String> = arr.iter().map(|s| format!("{s:?}")).collect();
                    format!("[{}]", parts.join(", "))
                } else {
                    let parts: Vec<String> =
                        arr[..max_elems].iter().map(|s| format!("{s:?}")).collect();
                    format!("[{}, …+{} more]", parts.join(", "), arr.len() - max_elems)
                }
            }
            Self::CharArray(arr) if arr.len() > max_elems * 4 => {
                // 4× because chars are bytes; let scalar+small-array
                // CharArray fall through to Display.
                format!("<binary {} bytes>", arr.len())
            }
            // Scalars + short CharArray: full Display is bounded.
            other => format!("{other}"),
        }
    }

    /// Deserialize a value from raw bytes based on DBR type
    pub fn from_bytes(dbr_type: DbFieldType, data: &[u8]) -> CaResult<Self> {
        match dbr_type {
            DbFieldType::String => {
                // DBR_STRING is fixed-width 40 bytes (MAX_STRING_SIZE). A
                // peer-overflowed buffer with NUL past byte 40 must
                // still produce a ≤40-byte string per spec; otherwise
                // a downstream consumer that assumes the bound breaks.
                let bounded = &data[..data.len().min(40)];
                let end = bounded
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(bounded.len());
                // Preserve the bytes verbatim — no UTF-8 validation. CA
                // DBR_STRING fields are historically Latin-1 / arbitrary
                // bytes and pvxs stores them raw, so rejecting non-UTF-8
                // here would drop legal wire values.
                Ok(Self::String(PvString::from_bytes(&bounded[..end])))
            }
            DbFieldType::Short => {
                if data.len() < 2 {
                    return Err(CaError::Protocol("short data too small".into()));
                }
                Ok(Self::Short(i16::from_be_bytes([data[0], data[1]])))
            }
            DbFieldType::Float => {
                if data.len() < 4 {
                    return Err(CaError::Protocol("float data too small".into()));
                }
                Ok(Self::Float(f32::from_be_bytes([
                    data[0], data[1], data[2], data[3],
                ])))
            }
            DbFieldType::Enum => {
                if data.len() < 2 {
                    return Err(CaError::Protocol("enum data too small".into()));
                }
                Ok(Self::Enum(u16::from_be_bytes([data[0], data[1]])))
            }
            DbFieldType::Char => {
                if data.is_empty() {
                    return Err(CaError::Protocol("char data empty".into()));
                }
                Ok(Self::Char(data[0]))
            }
            DbFieldType::Long => {
                if data.len() < 4 {
                    return Err(CaError::Protocol("long data too small".into()));
                }
                Ok(Self::Long(i32::from_be_bytes([
                    data[0], data[1], data[2], data[3],
                ])))
            }
            DbFieldType::Double => {
                if data.len() < 8 {
                    return Err(CaError::Protocol("double data too small".into()));
                }
                Ok(Self::Double(f64::from_be_bytes([
                    data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
                ])))
            }
            DbFieldType::Int64 => {
                if data.len() < 8 {
                    return Err(CaError::Protocol("int64 data too small".into()));
                }
                Ok(Self::Int64(i64::from_be_bytes([
                    data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
                ])))
            }
            DbFieldType::UInt64 => {
                if data.len() < 8 {
                    return Err(CaError::Protocol("uint64 data too small".into()));
                }
                Ok(Self::UInt64(u64::from_be_bytes([
                    data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
                ])))
            }
            // DBF_USHORT/DBF_ULONG have no CA wire type — over the network
            // they arrive promoted (DBR_LONG / DBR_DOUBLE) and decode through
            // those arms. These native-width decoders exist for the internal
            // round-trip (`to_bytes`/`from_bytes` symmetry, tests) so the
            // match stays total over `DbFieldType` rather than swallowing the
            // unsigned widths in a catch-all.
            DbFieldType::UShort => {
                if data.len() < 2 {
                    return Err(CaError::Protocol("ushort data too small".into()));
                }
                Ok(Self::UShort(u16::from_be_bytes([data[0], data[1]])))
            }
            DbFieldType::ULong => {
                if data.len() < 4 {
                    return Err(CaError::Protocol("ulong data too small".into()));
                }
                Ok(Self::ULong(u32::from_be_bytes([
                    data[0], data[1], data[2], data[3],
                ])))
            }
            // DBF_UCHAR promotes to DBR_CHAR over CA (identical 1-byte wire
            // form), so on the network it decodes through the `Char` arm; this
            // native decoder keeps the match total and preserves the unsigned
            // interpretation for the internal round-trip.
            DbFieldType::UChar => {
                if data.is_empty() {
                    return Err(CaError::Protocol("uchar data too small".into()));
                }
                Ok(Self::UChar(data[0]))
            }
        }
    }

    /// Serialize a value to bytes for writing.
    ///
    /// Thin wrapper over [`EpicsValue::write_into`], which is the single
    /// owner of the wire layout. Prefer `write_into` on any path that
    /// already has a destination buffer: C's `dbGet` converts the field
    /// straight into the reply buffer (`dbAccess.c:1020`) and never
    /// materialises a standalone payload, so a wide array should be written
    /// once rather than built here and copied.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        self.write_into(&mut buf);
        buf
    }

    /// Append this value's CA wire bytes to `dst`.
    ///
    /// The port of C `convert(paddr, pbuf, ...)` writing into the payload
    /// space `cas_copy_in_header` reserved inside the client's send buffer
    /// (`camessage.c:516` → `dbAccess.c:1020`): one materialisation of the
    /// value, in the buffer that will be written to the socket.
    pub fn write_into(&self, dst: &mut Vec<u8>) {
        match self {
            Self::String(s) => {
                let mut buf = [0u8; 40];
                let bytes = s.as_bytes();
                let len = bytes.len().min(39);
                buf[..len].copy_from_slice(&bytes[..len]);
                dst.extend_from_slice(&buf);
            }
            Self::Short(v) => dst.extend_from_slice(&v.to_be_bytes()),
            Self::Float(v) => dst.extend_from_slice(&v.to_be_bytes()),
            Self::Enum(v) => dst.extend_from_slice(&v.to_be_bytes()),
            Self::EnumWithChoices { index, .. } => dst.extend_from_slice(&index.to_be_bytes()),
            Self::Char(v) => dst.push(*v),
            Self::Long(v) => dst.extend_from_slice(&v.to_be_bytes()),
            Self::Double(v) => dst.extend_from_slice(&v.to_be_bytes()),
            // Over CA, Int64 is served as Double (8-byte f64 big-endian).
            // Precision is lost for |v| > 2^53; that is a CA protocol limitation.
            Self::Int64(v) => dst.extend_from_slice(&(*v as f64).to_be_bytes()),
            // UInt64 mirrors Int64: no CA wire type, served as DBR_DOUBLE.
            Self::UInt64(v) => dst.extend_from_slice(&(*v as f64).to_be_bytes()),
            // DBF_USHORT promotes to DBR_LONG on the wire
            // (db_convert.h `dbDBRnewToDBRold[DBR_USHORT] = DBR_LONG`); a u16
            // fits losslessly in i32. Emit 4 promoted big-endian bytes to
            // match `dbr_type()` == Long.
            Self::UShort(v) => dst.extend_from_slice(&(*v as i32).to_be_bytes()),
            // DBF_ULONG promotes to DBR_DOUBLE on the wire
            // (db_convert.h `dbDBRnewToDBRold[DBR_ULONG] = DBR_DOUBLE`),
            // mirroring UInt64. Emit 8 promoted big-endian bytes.
            Self::ULong(v) => dst.extend_from_slice(&(*v as f64).to_be_bytes()),
            // DBF_UCHAR promotes to DBR_CHAR on the wire (db_convert.h
            // `dbDBRnewToDBRold[DBR_UCHAR] = DBR_CHAR`); the raw byte is
            // identical to `Char`, only the interpretation is unsigned.
            Self::UChar(v) => dst.push(*v),
            Self::ShortArray(arr) => {
                dst.reserve(arr.len() * 2);
                for v in arr {
                    dst.extend_from_slice(&v.to_be_bytes());
                }
            }
            Self::FloatArray(arr) => {
                dst.reserve(arr.len() * 4);
                for v in arr {
                    dst.extend_from_slice(&v.to_be_bytes());
                }
            }
            Self::EnumArray(arr) => {
                dst.reserve(arr.len() * 2);
                for v in arr {
                    dst.extend_from_slice(&v.to_be_bytes());
                }
            }
            Self::DoubleArray(arr) => {
                dst.reserve(arr.len() * 8);
                for v in arr {
                    dst.extend_from_slice(&v.to_be_bytes());
                }
            }
            Self::LongArray(arr) => {
                dst.reserve(arr.len() * 4);
                for v in arr {
                    dst.extend_from_slice(&v.to_be_bytes());
                }
            }
            Self::Int64Array(arr) => {
                dst.reserve(arr.len() * 8);
                for v in arr {
                    dst.extend_from_slice(&(*v as f64).to_be_bytes());
                }
            }
            Self::UInt64Array(arr) => {
                dst.reserve(arr.len() * 8);
                for v in arr {
                    dst.extend_from_slice(&(*v as f64).to_be_bytes());
                }
            }
            // DBF_USHORT[] promotes element-wise to DBR_LONG[] (4 bytes each).
            Self::UShortArray(arr) => {
                dst.reserve(arr.len() * 4);
                for v in arr {
                    dst.extend_from_slice(&(*v as i32).to_be_bytes());
                }
            }
            // DBF_ULONG[] promotes element-wise to DBR_DOUBLE[] (8 bytes each).
            Self::ULongArray(arr) => {
                dst.reserve(arr.len() * 8);
                for v in arr {
                    dst.extend_from_slice(&(*v as f64).to_be_bytes());
                }
            }
            Self::CharArray(arr) => dst.extend_from_slice(arr),
            // DBF_UCHAR[] promotes to DBR_CHAR[] over CA (identical raw bytes,
            // same as CharArray); the unsigned interpretation is carried by
            // the type, not the wire bytes.
            Self::UCharArray(arr) => dst.extend_from_slice(arr),
            Self::StringArray(arr) => {
                let start = dst.len();
                dst.resize(start + arr.len() * 40, 0);
                for (i, s) in arr.iter().enumerate() {
                    let bytes = s.as_bytes();
                    let len = bytes.len().min(39);
                    let at = start + i * 40;
                    dst[at..at + len].copy_from_slice(&bytes[..len]);
                }
            }
        }
    }

    /// Deserialize an array value from raw bytes.
    ///
    /// `count` comes directly from the wire (CA `m_count`,
    /// 16-bit native or 32-bit "extended"). A malicious peer can send
    /// `m_count = 0xFFFF_FFFF` with a tiny payload, and a naive
    /// `Vec::with_capacity(count)` allocates ~8 GiB for shorts /
    /// ~16 GiB for doubles before the bounds check inside the loop
    /// can short-circuit. Cap the allocation at `data.len() / size`
    /// so the capacity tracks the bytes that actually arrived.
    pub fn from_bytes_array(dbr_type: DbFieldType, data: &[u8], count: usize) -> CaResult<Self> {
        // P-1 (BUG_ARCHAEOLOGY libca 8cc20393f / a7bf59079): count=0
        // is a legitimate empty-array round-trip and must NOT collapse
        // to scalar decoding. The previous `count <= 1` short-circuit
        // (a) read garbage scalar from an empty payload on GET (raised
        // CaError::Protocol "char data empty" / "...too small") and
        // (b) accepted scalar bytes for an array WRITE with count=0
        // when the server should reject. Treat count=0 as the typed
        // empty-array variant; count=1 still falls through to the
        // scalar decoder (the legitimate "scalar shaped as array of
        // one" case in CA).
        if count == 0 {
            return Ok(match dbr_type {
                DbFieldType::String => Self::StringArray(Vec::new()),
                DbFieldType::Short => Self::ShortArray(Vec::new()),
                DbFieldType::Float => Self::FloatArray(Vec::new()),
                DbFieldType::Enum => Self::EnumArray(Vec::new()),
                DbFieldType::Char => Self::CharArray(Vec::new()),
                DbFieldType::Long => Self::LongArray(Vec::new()),
                DbFieldType::Double => Self::DoubleArray(Vec::new()),
                DbFieldType::Int64 => Self::Int64Array(Vec::new()),
                DbFieldType::UInt64 => Self::UInt64Array(Vec::new()),
                DbFieldType::UShort => Self::UShortArray(Vec::new()),
                DbFieldType::ULong => Self::ULongArray(Vec::new()),
                DbFieldType::UChar => Self::UCharArray(Vec::new()),
            });
        }
        if count == 1 {
            return Self::from_bytes(dbr_type, data);
        }
        let cap_for = |elem_size: usize| count.min(data.len() / elem_size.max(1));
        match dbr_type {
            DbFieldType::Short => {
                let mut arr = Vec::with_capacity(cap_for(2));
                for i in 0..count {
                    let offset = i * 2;
                    if offset + 2 > data.len() {
                        break;
                    }
                    arr.push(i16::from_be_bytes([data[offset], data[offset + 1]]));
                }
                Ok(Self::ShortArray(arr))
            }
            DbFieldType::Float => {
                let mut arr = Vec::with_capacity(cap_for(4));
                for i in 0..count {
                    let offset = i * 4;
                    if offset + 4 > data.len() {
                        break;
                    }
                    arr.push(f32::from_be_bytes([
                        data[offset],
                        data[offset + 1],
                        data[offset + 2],
                        data[offset + 3],
                    ]));
                }
                Ok(Self::FloatArray(arr))
            }
            DbFieldType::Enum => {
                let mut arr = Vec::with_capacity(cap_for(2));
                for i in 0..count {
                    let offset = i * 2;
                    if offset + 2 > data.len() {
                        break;
                    }
                    arr.push(u16::from_be_bytes([data[offset], data[offset + 1]]));
                }
                Ok(Self::EnumArray(arr))
            }
            DbFieldType::Double => {
                let mut arr = Vec::with_capacity(cap_for(8));
                for i in 0..count {
                    let offset = i * 8;
                    if offset + 8 > data.len() {
                        break;
                    }
                    arr.push(f64::from_be_bytes([
                        data[offset],
                        data[offset + 1],
                        data[offset + 2],
                        data[offset + 3],
                        data[offset + 4],
                        data[offset + 5],
                        data[offset + 6],
                        data[offset + 7],
                    ]));
                }
                Ok(Self::DoubleArray(arr))
            }
            DbFieldType::Long => {
                let mut arr = Vec::with_capacity(cap_for(4));
                for i in 0..count {
                    let offset = i * 4;
                    if offset + 4 > data.len() {
                        break;
                    }
                    arr.push(i32::from_be_bytes([
                        data[offset],
                        data[offset + 1],
                        data[offset + 2],
                        data[offset + 3],
                    ]));
                }
                Ok(Self::LongArray(arr))
            }
            DbFieldType::Int64 => {
                let mut arr = Vec::with_capacity(cap_for(8));
                for i in 0..count {
                    let offset = i * 8;
                    if offset + 8 > data.len() {
                        break;
                    }
                    arr.push(i64::from_be_bytes([
                        data[offset],
                        data[offset + 1],
                        data[offset + 2],
                        data[offset + 3],
                        data[offset + 4],
                        data[offset + 5],
                        data[offset + 6],
                        data[offset + 7],
                    ]));
                }
                Ok(Self::Int64Array(arr))
            }
            DbFieldType::UInt64 => {
                let mut arr = Vec::with_capacity(cap_for(8));
                for i in 0..count {
                    let offset = i * 8;
                    if offset + 8 > data.len() {
                        break;
                    }
                    arr.push(u64::from_be_bytes([
                        data[offset],
                        data[offset + 1],
                        data[offset + 2],
                        data[offset + 3],
                        data[offset + 4],
                        data[offset + 5],
                        data[offset + 6],
                        data[offset + 7],
                    ]));
                }
                Ok(Self::UInt64Array(arr))
            }
            // Native-width decoders (see the scalar `from_bytes` note): over
            // CA these arrive promoted as DBR_LONG[]/DBR_DOUBLE[]; these arms
            // keep the match total over `DbFieldType`.
            DbFieldType::UShort => {
                let mut arr = Vec::with_capacity(cap_for(2));
                for i in 0..count {
                    let offset = i * 2;
                    if offset + 2 > data.len() {
                        break;
                    }
                    arr.push(u16::from_be_bytes([data[offset], data[offset + 1]]));
                }
                Ok(Self::UShortArray(arr))
            }
            DbFieldType::ULong => {
                let mut arr = Vec::with_capacity(cap_for(4));
                for i in 0..count {
                    let offset = i * 4;
                    if offset + 4 > data.len() {
                        break;
                    }
                    arr.push(u32::from_be_bytes([
                        data[offset],
                        data[offset + 1],
                        data[offset + 2],
                        data[offset + 3],
                    ]));
                }
                Ok(Self::ULongArray(arr))
            }
            DbFieldType::Char => {
                let len = count.min(data.len());
                Ok(Self::CharArray(data[..len].to_vec()))
            }
            // DBF_UCHAR[] decodes 1 byte per element like Char (it promotes to
            // DBR_CHAR[] over CA); the bytes are copied verbatim, unsigned.
            DbFieldType::UChar => {
                let len = count.min(data.len());
                Ok(Self::UCharArray(data[..len].to_vec()))
            }
            DbFieldType::String => {
                // DBR_STRING is fixed-width 40 bytes per element. The
                // wire delivers `count * 40` bytes; each slot is
                // NUL-padded. Walk in 40-byte slots and strip at the
                // first NUL (if any).
                let mut arr = Vec::with_capacity(cap_for(40));
                for i in 0..count {
                    let start = i * 40;
                    let end = start + 40;
                    if end > data.len() {
                        break;
                    }
                    let slot = &data[start..end];
                    let nul = slot.iter().position(|&b| b == 0).unwrap_or(slot.len());
                    // Bytes preserved verbatim — see the scalar DBR_STRING
                    // branch in `from_bytes`.
                    arr.push(PvString::from_bytes(&slot[..nul]));
                }
                Ok(Self::StringArray(arr))
            }
        }
    }

    /// Get the DBR type for this value (CA wire type).
    /// Int64 has no CA wire type — it appears as Double over Channel Access.
    pub fn dbr_type(&self) -> DbFieldType {
        match self {
            Self::String(_) | Self::StringArray(_) => DbFieldType::String,
            Self::Short(_) | Self::ShortArray(_) => DbFieldType::Short,
            Self::Float(_) | Self::FloatArray(_) => DbFieldType::Float,
            Self::Enum(_) | Self::EnumWithChoices { .. } | Self::EnumArray(_) => DbFieldType::Enum,
            Self::Char(_) | Self::CharArray(_) => DbFieldType::Char,
            Self::Long(_) | Self::LongArray(_) => DbFieldType::Long,
            Self::Double(_) | Self::DoubleArray(_) => DbFieldType::Double,
            Self::Int64(_) | Self::Int64Array(_) => DbFieldType::Double,
            // UInt64 has no CA wire type; served as DBR_DOUBLE like Int64.
            Self::UInt64(_) | Self::UInt64Array(_) => DbFieldType::Double,
            // DBF_USHORT promotes to DBR_LONG (u16 fits in i32);
            // db_convert.h `dbDBRnewToDBRold[DBR_USHORT] = DBR_LONG`.
            Self::UShort(_) | Self::UShortArray(_) => DbFieldType::Long,
            // DBF_ULONG promotes to DBR_DOUBLE like UInt64;
            // db_convert.h `dbDBRnewToDBRold[DBR_ULONG] = DBR_DOUBLE`.
            Self::ULong(_) | Self::ULongArray(_) => DbFieldType::Double,
            // DBF_UCHAR promotes to DBR_CHAR (identical 1-byte wire form);
            // db_convert.h `dbDBRnewToDBRold[DBR_UCHAR] = DBR_CHAR`.
            Self::UChar(_) | Self::UCharArray(_) => DbFieldType::Char,
        }
    }

    /// True iff this value is an array variant, whatever its length.
    ///
    /// The port's stand-in for C's `no_elements > 1` destination test
    /// (`dbAccess.c:1345`): a record's array-valued field reads back as an
    /// array variant, a scalar field as a scalar variant.
    pub fn is_array(&self) -> bool {
        matches!(
            self,
            Self::ShortArray(_)
                | Self::FloatArray(_)
                | Self::EnumArray(_)
                | Self::DoubleArray(_)
                | Self::LongArray(_)
                | Self::Int64Array(_)
                | Self::UInt64Array(_)
                | Self::UShortArray(_)
                | Self::ULongArray(_)
                | Self::UCharArray(_)
                | Self::CharArray(_)
                | Self::StringArray(_)
        )
    }

    /// C `db_add_event`'s `useValque` test (`dbEvent.c:492-500`): may a monitor
    /// event queue hold this value *by value*, or must it keep only the latest?
    ///
    /// ```c
    /// if (dbChannelElements(chan) == 1 &&
    ///     dbChannelSpecial(chan) != SPC_DBADDR &&
    ///     dbChannelFieldSize(chan) <= sizeof(union native_value)) {
    ///     pevent->useValque = TRUE;
    /// }
    /// else {
    ///     pevent->useValque = FALSE;
    /// }
    /// ```
    ///
    /// C's `union native_value` (`dbFldTypes.h`) is at most one
    /// `char[MAX_STRING_SIZE]`, so "queueable by value" means "fits in 40
    /// bytes". Anything wider is stored by reference
    /// (`db_create_field_log`'s `dbfl_type_ref` branch, which copies nothing
    /// and points at the record's own field), and C then refuses to queue a
    /// second one — `dbel` reports such a subscription as
    /// "queueing disabled".
    ///
    /// The port cannot store a reference (every event carries an owned
    /// [`crate::server::snapshot::Snapshot`]), so this predicate is what keeps
    /// its memory profile C's: see
    /// [`crate::server::event_queue`]'s latest-only rule.
    pub fn queues_by_value(&self) -> bool {
        match self {
            // C: `dbChannelElements(chan) == 1` fails for every array field,
            // whatever its current element count.
            v if v.is_array() => false,
            // The two scalar variants with heap-allocated payloads: a
            // long-string `DBR_STRING` can exceed MAX_STRING_SIZE, and the
            // transient NTEnum carrier holds a whole label vector.
            Self::String(s) => s.len() <= NATIVE_VALUE_BYTES,
            Self::EnumWithChoices { .. } => false,
            // Every remaining variant is a fixed-width scalar of at most 8
            // bytes.
            _ => true,
        }
    }

    /// Get the element count for this value.
    pub fn count(&self) -> u32 {
        match self {
            Self::ShortArray(arr) => arr.len() as u32,
            Self::FloatArray(arr) => arr.len() as u32,
            Self::EnumArray(arr) => arr.len() as u32,
            Self::DoubleArray(arr) => arr.len() as u32,
            Self::LongArray(arr) => arr.len() as u32,
            Self::Int64Array(arr) => arr.len() as u32,
            Self::UInt64Array(arr) => arr.len() as u32,
            Self::UShortArray(arr) => arr.len() as u32,
            Self::ULongArray(arr) => arr.len() as u32,
            Self::UCharArray(arr) => arr.len() as u32,
            Self::CharArray(arr) => arr.len() as u32,
            Self::StringArray(arr) => arr.len() as u32,
            _ => 1,
        }
    }

    /// True iff this value is an array variant with zero elements.
    /// Mirrors the C-EPICS dbPut/dbCa/dbDbGetValue empty-array guard
    /// (commits 12cfd41 / 0a1fb25 / 39c8d56): empty arrays must NOT
    /// silently coerce into scalar zero — callers should treat this
    /// as a LINK_ALARM-class condition and reject the put/get.
    pub fn is_empty_array(&self) -> bool {
        matches!(
            self,
            Self::ShortArray(arr) if arr.is_empty()
        ) || matches!(
            self,
            Self::FloatArray(arr) if arr.is_empty()
        ) || matches!(
            self,
            Self::EnumArray(arr) if arr.is_empty()
        ) || matches!(
            self,
            Self::DoubleArray(arr) if arr.is_empty()
        ) || matches!(
            self,
            Self::LongArray(arr) if arr.is_empty()
        ) || matches!(
            self,
            Self::Int64Array(arr) if arr.is_empty()
        ) || matches!(
            self,
            Self::UInt64Array(arr) if arr.is_empty()
        ) || matches!(
            self,
            Self::UShortArray(arr) if arr.is_empty()
        ) || matches!(
            self,
            Self::ULongArray(arr) if arr.is_empty()
        ) || matches!(
            self,
            Self::UCharArray(arr) if arr.is_empty()
        ) || matches!(
            self,
            Self::CharArray(arr) if arr.is_empty()
        ) || matches!(
            self,
            Self::StringArray(arr) if arr.is_empty()
        )
    }

    /// Element 0 of an array variant, as the matching **scalar** variant.
    /// Returns `None` for a scalar variant (nothing to reduce) and for an
    /// empty array (no element 0).
    ///
    /// This is C's `dbPut` clamp of a request into a one-element destination:
    /// `if (no_elements < nRequest) nRequest = no_elements;` (`dbAccess.c:1359`)
    /// followed by `dbPutConvertRoutine[...](paddr, pbuffer, nRequest = 1, ...)`,
    /// which copies the first element only. The put succeeds — the surplus
    /// elements are dropped, not an error.
    pub fn first_element(&self) -> Option<EpicsValue> {
        Some(match self {
            Self::ShortArray(a) => Self::Short(*a.first()?),
            Self::FloatArray(a) => Self::Float(*a.first()?),
            Self::EnumArray(a) => Self::Enum(*a.first()?),
            Self::DoubleArray(a) => Self::Double(*a.first()?),
            Self::LongArray(a) => Self::Long(*a.first()?),
            Self::Int64Array(a) => Self::Int64(*a.first()?),
            Self::UInt64Array(a) => Self::UInt64(*a.first()?),
            Self::UShortArray(a) => Self::UShort(*a.first()?),
            Self::ULongArray(a) => Self::ULong(*a.first()?),
            Self::UCharArray(a) => Self::UChar(*a.first()?),
            Self::CharArray(a) => Self::Char(*a.first()?),
            Self::StringArray(a) => Self::String(a.first()?.clone()),
            _ => return None,
        })
    }

    /// Truncate an array value to at most `max` elements. Scalars are unchanged.
    pub fn truncate(&mut self, max: usize) {
        match self {
            Self::ShortArray(arr) => arr.truncate(max),
            Self::FloatArray(arr) => arr.truncate(max),
            Self::EnumArray(arr) => arr.truncate(max),
            Self::DoubleArray(arr) => arr.truncate(max),
            Self::LongArray(arr) => arr.truncate(max),
            Self::Int64Array(arr) => arr.truncate(max),
            Self::UInt64Array(arr) => arr.truncate(max),
            Self::UShortArray(arr) => arr.truncate(max),
            Self::ULongArray(arr) => arr.truncate(max),
            Self::UCharArray(arr) => arr.truncate(max),
            Self::CharArray(arr) => arr.truncate(max),
            Self::StringArray(arr) => arr.truncate(max),
            _ => {}
        }
    }

    /// Extract every element of an array variant as `f64`. Returns
    /// `None` for scalar variants. `CharArray` is not covered here —
    /// its signed-`epicsInt8` conversion is handled separately in
    /// `convert_to`.
    fn as_f64_array(&self) -> Option<Vec<f64>> {
        match self {
            Self::ShortArray(a) => Some(a.iter().map(|&v| v as f64).collect()),
            Self::FloatArray(a) => Some(a.iter().map(|&v| v as f64).collect()),
            Self::EnumArray(a) => Some(a.iter().map(|&v| v as f64).collect()),
            Self::LongArray(a) => Some(a.iter().map(|&v| v as f64).collect()),
            Self::DoubleArray(a) => Some(a.clone()),
            Self::Int64Array(a) => Some(a.iter().map(|&v| v as f64).collect()),
            Self::UInt64Array(a) => Some(a.iter().map(|&v| v as f64).collect()),
            Self::UShortArray(a) => Some(a.iter().map(|&v| v as f64).collect()),
            Self::ULongArray(a) => Some(a.iter().map(|&v| v as f64).collect()),
            // DBF_UCHAR[] is unsigned-numeric (0..=255), so it takes the plain
            // numeric view here — unlike the signed `CharArray`, which needs
            // the epicsInt8 sign-reinterpret arm in `convert_to`.
            Self::UCharArray(a) => Some(a.iter().map(|&v| v as f64).collect()),
            // DBR_STRING → numeric IS an array conversion in C: the
            // `putString*` family (`dbConvert.c:941-1147`) runs the SAME
            // per-element scalar parse (`epicsParseInt32`/`epicsParseFloat64`,
            // `dbConvertBase` = 0 so `"0x10"` is 16) over all `nRequest`
            // elements. Taking the numeric view here makes the array path
            // element-wise-identical to the scalar `String` tail below —
            // one definition of string→numeric, not two. Without it a
            // `caput -a WV 3 7 8 9` collapsed to a single `0.0`.
            Self::StringArray(a) => Some(
                a.iter()
                    .map(|s| parse_string_to_f64(&s.as_str_lossy()).unwrap_or(0.0))
                    .collect(),
            ),
            _ => None,
        }
    }

    /// The value as `i64` when this is an integer-family scalar
    /// (`Short`/`Enum`/`Long`/`Int64`/`UInt64`), else `None`.
    ///
    /// `UInt64` reinterprets its bit pattern (`v as i64`). Narrowing an
    /// integer target off this view then keeps the low N bits — matching
    /// C/pvxs `static_cast` truncation (`uint32 0xffffffff -> int32 -1`,
    /// `convertCast` in `pvxs/src/sharedarray.cpp:160-166`), NOT the
    /// float round-trip's saturation. `Char` is intentionally excluded:
    /// it is 8-bit (no narrowing target can overflow) and `to_f64`
    /// reinterprets it as signed `epicsInt8`, a contract this view must
    /// not silently change.
    pub(crate) fn as_int_i64(&self) -> Option<i64> {
        Some(match self {
            Self::Short(v) => *v as i64,
            Self::Enum(v) => *v as i64,
            Self::Long(v) => *v as i64,
            Self::Int64(v) => *v,
            Self::UInt64(v) => *v as i64,
            // u16/u32 widen losslessly into the i64 view; an integer target
            // then truncates off the low bits (C/pvxs `static_cast`).
            Self::UShort(v) => *v as i64,
            Self::ULong(v) => *v as i64,
            // epicsUInt8 widens unsigned (0xFF -> 255), unlike the signed
            // `Char` which `to_f64` reinterprets as epicsInt8 and is excluded.
            Self::UChar(v) => *v as i64,
            _ => return None,
        })
    }

    /// Per-element `i64` view of an integer-family array
    /// (`ShortArray`/`EnumArray`/`LongArray`/`Int64Array`/`UInt64Array`),
    /// else `None`. Same truncation contract as [`Self::as_int_i64`].
    /// `CharArray` is excluded — it carries its own signed-`epicsInt8`
    /// conversion arm in `convert_to`.
    fn as_int_i64_array(&self) -> Option<Vec<i64>> {
        Some(match self {
            Self::ShortArray(a) => a.iter().map(|&v| v as i64).collect(),
            Self::EnumArray(a) => a.iter().map(|&v| v as i64).collect(),
            Self::LongArray(a) => a.iter().map(|&v| v as i64).collect(),
            Self::Int64Array(a) => a.clone(),
            Self::UInt64Array(a) => a.iter().map(|&v| v as i64).collect(),
            Self::UShortArray(a) => a.iter().map(|&v| v as i64).collect(),
            Self::ULongArray(a) => a.iter().map(|&v| v as i64).collect(),
            // Unsigned-byte view (0..=255); `CharArray` stays excluded (signed).
            Self::UCharArray(a) => a.iter().map(|&v| v as i64).collect(),
            _ => return None,
        })
    }

    /// Convert to a different native type.
    ///
    /// Scalars convert element-wise via `to_f64`. Array variants
    /// convert **element-by-element** to the target array variant
    /// (C `dbConvert` runs the per-type GET routine over the whole
    /// array). Without this an array requested as a different DBR
    /// native type would collapse to a single zero scalar.
    pub fn convert_to(&self, target: DbFieldType) -> EpicsValue {
        // pvalink NTEnum carrier (pvxs `pvaGetValue` scalar switch,
        // `pvalink_lset.cpp:330-360`): a DBR_STRING target stores the
        // label `choices[index]` (with the "%u" index fallback out of
        // range); EVERY other target — char/short/long/int64, the
        // unsigned widths, float/double, enum — takes the numeric index.
        // Resolve the transient here, the single value-coercion owner
        // (`set_val`'s default routes a `TypeMismatch` through this, and
        // `put_field_internal` calls it directly), so `EnumWithChoices`
        // never reaches storage or the wire. Must precede the
        // `db_field_type()==target` short-circuit: the carrier's type is
        // `Enum`, so an `Enum` target would otherwise clone it verbatim
        // instead of collapsing to a bare index.
        if let Self::EnumWithChoices { index, choices } = self {
            return match target {
                DbFieldType::String => Self::String(Self::enum_index_label(choices, *index)),
                _ => Self::Enum(*index).convert_to(target),
            };
        }
        if self.db_field_type() == target {
            return self.clone();
        }

        // Array → array conversion: map each element through the
        // numeric-array view, then materialize the target variant.
        // StringArray takes this path too — C's string→numeric is a
        // per-element array conversion (`putStringLong` and siblings),
        // not a scalar one. CharArray does NOT: it needs the signed
        // `epicsInt8` reinterpret, and to String it decodes as text.
        if let Some(nums) = self.as_f64_array() {
            // Integer-family arrays also expose a lossless `i64` element
            // view; route integer targets through it so narrowing truncates
            // (C/pvxs `static_cast`: `uint32 0xffffffff -> int32 -1`)
            // instead of saturating through f64 (which would clamp to
            // `i32::MAX`). Float/Double/String targets keep the
            // value-preserving f64 view — correct for unsigned widening
            // such as `UInt64Array -> Double`.
            let ints = self.as_int_i64_array();
            return match target {
                DbFieldType::Short => EpicsValue::ShortArray(match &ints {
                    Some(v) => v.iter().map(|&x| x as i16).collect(),
                    None => nums.iter().map(|&v| c_cast::f64_to_i16(v)).collect(),
                }),
                DbFieldType::Float => {
                    EpicsValue::FloatArray(nums.iter().map(|&v| v as f32).collect())
                }
                DbFieldType::Enum => EpicsValue::EnumArray(match &ints {
                    Some(v) => v.iter().map(|&x| x as u16).collect(),
                    None => nums.iter().map(|&v| c_cast::f64_to_u16(v)).collect(),
                }),
                DbFieldType::Long => EpicsValue::LongArray(match &ints {
                    Some(v) => v.iter().map(|&x| x as i32).collect(),
                    None => nums.iter().map(|&v| c_cast::f64_to_i32(v)).collect(),
                }),
                DbFieldType::Double => EpicsValue::DoubleArray(nums),
                DbFieldType::Int64 => EpicsValue::Int64Array(match &ints {
                    Some(v) => v.clone(),
                    None => nums.iter().map(|&v| c_cast::f64_to_i64(v)).collect(),
                }),
                DbFieldType::UInt64 => EpicsValue::UInt64Array(match &ints {
                    Some(v) => v.iter().map(|&x| x as u64).collect(),
                    None => nums.iter().map(|&v| c_cast::f64_to_u64(v)).collect(),
                }),
                DbFieldType::UShort => EpicsValue::UShortArray(match &ints {
                    Some(v) => v.iter().map(|&x| x as u16).collect(),
                    None => nums.iter().map(|&v| c_cast::f64_to_u16(v)).collect(),
                }),
                DbFieldType::ULong => EpicsValue::ULongArray(match &ints {
                    Some(v) => v.iter().map(|&x| x as u32).collect(),
                    None => nums.iter().map(|&v| c_cast::f64_to_u32(v)).collect(),
                }),
                DbFieldType::Char => EpicsValue::CharArray(match &ints {
                    Some(v) => v.iter().map(|&x| x as u8).collect(),
                    None => nums.iter().map(|&v| c_cast::f64_to_u8(v)).collect(),
                }),
                // DBF_UCHAR[] narrows off the integer view (low 8 bits), the
                // unsigned twin of the Char target.
                DbFieldType::UChar => EpicsValue::UCharArray(match &ints {
                    Some(v) => v.iter().map(|&x| x as u8).collect(),
                    None => nums.iter().map(|&v| c_cast::f64_to_u8(v)).collect(),
                }),
                DbFieldType::String => {
                    EpicsValue::StringArray(nums.iter().map(|v| v.to_string().into()).collect())
                }
            };
        }
        if let EpicsValue::CharArray(bytes) = self {
            return match target {
                DbFieldType::Short => {
                    EpicsValue::ShortArray(bytes.iter().map(|&b| b as i8 as i16).collect())
                }
                DbFieldType::Float => {
                    EpicsValue::FloatArray(bytes.iter().map(|&b| b as i8 as f32).collect())
                }
                DbFieldType::Enum => {
                    EpicsValue::EnumArray(bytes.iter().map(|&b| b as u16).collect())
                }
                DbFieldType::Long => {
                    EpicsValue::LongArray(bytes.iter().map(|&b| b as i8 as i32).collect())
                }
                DbFieldType::Double => {
                    EpicsValue::DoubleArray(bytes.iter().map(|&b| b as i8 as f64).collect())
                }
                DbFieldType::Int64 => {
                    EpicsValue::Int64Array(bytes.iter().map(|&b| b as i8 as i64).collect())
                }
                DbFieldType::UInt64 => {
                    EpicsValue::UInt64Array(bytes.iter().map(|&b| b as i8 as u64).collect())
                }
                // epicsInt8 → epicsUInt16/epicsUInt32 sign-extends (C
                // `dbConvert` charToUshort/charToUlong promote the signed
                // source through int), so 0xFF → 0xFFFF / 0xFFFFFFFF.
                DbFieldType::UShort => {
                    EpicsValue::UShortArray(bytes.iter().map(|&b| b as i8 as u16).collect())
                }
                DbFieldType::ULong => {
                    EpicsValue::ULongArray(bytes.iter().map(|&b| b as i8 as u32).collect())
                }
                // CharArray as text: preserve the byte buffer verbatim
                // (no UTF-8 validation), matching pvxs raw-byte storage.
                DbFieldType::String => EpicsValue::String(PvString::from_bytes(bytes.clone())),
                DbFieldType::Char => EpicsValue::CharArray(bytes.clone()),
                // epicsInt8 -> epicsUInt8 is byte-identity (C `charToUchar`
                // casts the signed source: -1/0xFF -> 255/0xFF), so the raw
                // bytes carry over unchanged into the unsigned carrier.
                DbFieldType::UChar => EpicsValue::UCharArray(bytes.clone()),
            };
        }

        // Menu string resolution: when converting String to Short/Enum,
        // try resolve_menu_string first (e.g. "MINOR" -> 1).
        if let EpicsValue::String(s) = self {
            let s = s.as_str_lossy();
            match target {
                DbFieldType::Short => {
                    if let Some(idx) = Self::resolve_menu_string(&s) {
                        return EpicsValue::Short(idx);
                    }
                }
                DbFieldType::Enum => {
                    if let Some(idx) = Self::resolve_menu_string(&s) {
                        return EpicsValue::Enum(idx as u16);
                    }
                }
                _ => {}
            }
        }
        // Integer-source narrowing uses the `i64` view (C/pvxs `static_cast`
        // truncation: low N bits) so `uint 0xffffffff -> DBF_LONG` yields
        // `-1`, not the f64 round-trip's saturated `i32::MAX`. Float/double
        // sources have no integer view, so they keep the f64 path; that path
        // also serves the String/Float/Double targets for every source.
        match target {
            DbFieldType::String => EpicsValue::String(format!("{self}").into()),
            DbFieldType::Short => EpicsValue::Short(match self.as_int_i64() {
                Some(i) => i as i16,
                None => c_cast::f64_to_i16(self.to_f64().unwrap_or(0.0)),
            }),
            DbFieldType::Float => EpicsValue::Float(self.to_f64().unwrap_or(0.0) as f32),
            // C's numeric→`DBF_ENUM`/`DBF_MENU` put is `putDoubleEnum`
            // (`dbConvert.c`): `*pfield = (epicsEnum16)*psrc`, a WRAPPING cast of
            // the truncated value, not a saturating clamp. A float `-1.0` must
            // land as `65535` (served signed as `-1`), matching `caput REC.HSV -1`
            // on a compiled softIoc — `c_cast::f64_to_u16` saturates it to `0`,
            // which silently clamped every out-of-range menu-ordinal put. Truncate
            // toward zero via the `i64` view, then take the low 16 bits like the
            // integer-source path above.
            DbFieldType::Enum => EpicsValue::Enum(match self.as_int_i64() {
                Some(i) => i as u16,
                None => self.to_f64().unwrap_or(0.0) as i64 as u16,
            }),
            DbFieldType::Char => {
                // String → CharArray (for waveform FTVL=CHAR)
                if let EpicsValue::String(s) = self {
                    EpicsValue::CharArray(s.as_bytes().to_vec())
                } else {
                    EpicsValue::Char(match self.as_int_i64() {
                        Some(i) => i as u8,
                        None => c_cast::f64_to_u8(self.to_f64().unwrap_or(0.0)),
                    })
                }
            }
            DbFieldType::Long => EpicsValue::Long(match self.as_int_i64() {
                Some(i) => i as i32,
                None => c_cast::f64_to_i32(self.to_f64().unwrap_or(0.0)),
            }),
            DbFieldType::Double => EpicsValue::Double(self.to_f64().unwrap_or(0.0)),
            DbFieldType::Int64 => {
                // Avoid f64 round-trip when value is already Int64.
                if let EpicsValue::Int64(v) = self {
                    EpicsValue::Int64(*v)
                } else if let EpicsValue::UInt64(v) = self {
                    // UInt64 → Int64: exact bit-preserving integer cast,
                    // not an f64 round-trip (lossy above 2^53).
                    EpicsValue::Int64(*v as i64)
                } else {
                    EpicsValue::Int64(c_cast::f64_to_i64(self.to_f64().unwrap_or(0.0)))
                }
            }
            DbFieldType::UInt64 => {
                // Avoid f64 round-trip when value is already an integer.
                if let EpicsValue::UInt64(v) = self {
                    EpicsValue::UInt64(*v)
                } else if let EpicsValue::Int64(v) = self {
                    EpicsValue::UInt64(*v as u64)
                } else {
                    EpicsValue::UInt64(c_cast::f64_to_u64(self.to_f64().unwrap_or(0.0)))
                }
            }
            // Unsigned narrowing mirrors the signed targets: integer sources
            // truncate off the i64 view (C/pvxs `static_cast`), float/string
            // sources fall back to the f64 round-trip.
            DbFieldType::UShort => EpicsValue::UShort(match self.as_int_i64() {
                Some(i) => i as u16,
                None => c_cast::f64_to_u16(self.to_f64().unwrap_or(0.0)),
            }),
            DbFieldType::ULong => EpicsValue::ULong(match self.as_int_i64() {
                Some(i) => i as u32,
                None => c_cast::f64_to_u32(self.to_f64().unwrap_or(0.0)),
            }),
            DbFieldType::UChar => {
                // String → UCharArray (for waveform FTVL=UCHAR), the unsigned
                // twin of the Char target's String → CharArray path.
                if let EpicsValue::String(s) = self {
                    EpicsValue::UCharArray(s.as_bytes().to_vec())
                } else {
                    EpicsValue::UChar(match self.as_int_i64() {
                        Some(i) => i as u8,
                        None => c_cast::f64_to_u8(self.to_f64().unwrap_or(0.0)),
                    })
                }
            }
        }
    }

    /// Return the internal DbFieldType that matches this value's variant.
    /// Unlike dbr_type(), this returns Int64 for Int64 variants (not Double).
    pub fn db_field_type(&self) -> DbFieldType {
        match self {
            Self::Double(_) => DbFieldType::Double,
            Self::Float(_) => DbFieldType::Float,
            Self::Long(_) => DbFieldType::Long,
            Self::Short(_) => DbFieldType::Short,
            Self::Enum(_) | Self::EnumWithChoices { .. } => DbFieldType::Enum,
            Self::Char(_) => DbFieldType::Char,
            Self::String(_) => DbFieldType::String,
            Self::Int64(_) | Self::Int64Array(_) => DbFieldType::Int64,
            Self::UInt64(_) | Self::UInt64Array(_) => DbFieldType::UInt64,
            Self::UShort(_) | Self::UShortArray(_) => DbFieldType::UShort,
            Self::ULong(_) | Self::ULongArray(_) => DbFieldType::ULong,
            Self::UChar(_) | Self::UCharArray(_) => DbFieldType::UChar,
            Self::CharArray(_) => DbFieldType::Char,
            Self::ShortArray(_) => DbFieldType::Short,
            Self::LongArray(_) => DbFieldType::Long,
            Self::EnumArray(_) => DbFieldType::Enum,
            Self::FloatArray(_) => DbFieldType::Float,
            Self::DoubleArray(_) => DbFieldType::Double,
            Self::StringArray(_) => DbFieldType::String,
        }
    }

    /// Project this value onto a `DBF_SHORT` field, by C's rule.
    ///
    /// `None` means the value is not numeric at all — C's `S_db_badDbrtype`.
    /// [`Self::convert_to`] coerces rather than fails, so the rejection has to be
    /// made here or a non-numeric source lands in the field as a silent 0.
    ///
    /// Use this, never `c_cast::f64_to_i16(v.to_f64())`: `to_f64` erases the
    /// source's integer-ness, and C picks its conversion routine BY the source
    /// type (`dbFastGetConvertRoutine` is a 2-D table, `dbConvert.c:1571-1638`).
    /// An integer source takes C's DEFINED modular conversion; only a float
    /// source takes the UB cast that CBUG-E2 saturates. Going through `to_f64`
    /// forces the float rule onto both.
    pub(crate) fn to_dbf_i16(&self) -> Option<i16> {
        self.to_f64()?;
        match self.convert_to(DbFieldType::Short) {
            Self::Short(v) => Some(v),
            _ => None,
        }
    }

    /// [`Self::to_dbf_i16`] for a `DBF_LONG` field. Same rule, same reason.
    pub(crate) fn to_dbf_i32(&self) -> Option<i32> {
        self.to_f64()?;
        match self.convert_to(DbFieldType::Long) {
            Self::Long(v) => Some(v),
            _ => None,
        }
    }

    pub fn to_f64(&self) -> Option<f64> {
        match self {
            Self::Double(v) => Some(*v),
            Self::Float(v) => Some(*v as f64),
            Self::Long(v) => Some(*v as f64),
            Self::Short(v) => Some(*v as f64),
            Self::Enum(v) => Some(*v as f64),
            // NTEnum carrier: numeric coercion takes the index (pvxs
            // numeric branch), identical to `Enum`. Reached on the
            // multi-input/DOL/SELN paths that pre-convert link values to
            // f64 before applying them, bypassing `convert_to`.
            Self::EnumWithChoices { index, .. } => Some(*index as f64),
            Self::Int64(v) => Some(*v as f64),
            Self::UInt64(v) => Some(*v as f64),
            Self::UShort(v) => Some(*v as f64),
            Self::ULong(v) => Some(*v as f64),
            // DBF_CHAR is epicsInt8 (signed) per epics-base c5012d9f73:
            // reinterpret the storage byte as i8 before widening so 0xFF → -1.0,
            // not 255.0. The CharArray storage stays u8 because that matches the
            // CA wire byte pattern; the sign only matters when promoting to f64.
            Self::Char(v) => Some((*v as i8) as f64),
            // DBF_UCHAR is epicsUInt8 (unsigned): widen the storage byte
            // directly (0xFF → 255.0), the counterpart of `Char`'s signed
            // reinterpret above.
            Self::UChar(v) => Some(*v as f64),
            // C EPICS dbConvert (88bfd6f, 2025-11-05): string-to-integer
            // conversion auto-detects hex/octal prefixes by default
            // (`dbConvertBase = 0`). Plain `s.parse::<f64>()` handles
            // decimal floats only — `"0x1A"` and `"017"` would silently
            // parse as `0.0`. Try integer auto-detect first so CA puts of
            // `"0x1A"` to a DBF_LONG field produce `26`, then fall back to
            // decimal float parse for normal numeric strings.
            Self::String(s) => parse_string_to_f64(&s.as_str_lossy()),
            _ => None,
        }
    }

    /// Resolve EPICS menu string constants to their integer indices.
    ///
    /// C EPICS base uses a menu system to convert string constants (e.g. "NO_ALARM",
    /// "MINOR") to integer indices. This provides the same mapping for the most
    /// commonly used menus.
    fn resolve_menu_string(s: &str) -> Option<i16> {
        match s {
            // menuAlarmSevr
            "NO_ALARM" => Some(0),
            "MINOR" => Some(1),
            "MAJOR" => Some(2),
            "INVALID" => Some(3),
            // menuYesNo / menuSimm
            "NO" => Some(0),
            "YES" => Some(1),
            "RAW" => Some(2),
            // menuPriority (dbCommon PRIO)
            "LOW" => Some(0),
            "MEDIUM" => Some(1),
            "HIGH" => Some(2),
            // menuOmsl
            "supervisory" => Some(0),
            "closed_loop" => Some(1),
            // menuIvoa
            "Continue normally" => Some(0),
            "Don't drive outputs" => Some(1),
            "Set output to IVOV" => Some(2),
            // menuFtype (waveform FTVL)
            "STRING" => Some(0),
            "CHAR" => Some(1),
            "UCHAR" => Some(2),
            "SHORT" => Some(3),
            "USHORT" => Some(4),
            "LONG" => Some(5),
            "ULONG" => Some(6),
            "INT64" => Some(7),
            "UINT64" => Some(8),
            "FLOAT" => Some(9),
            "DOUBLE" => Some(10),
            "ENUM" => Some(11),
            // menuFanout / menuSelect
            "All" => Some(0),
            "Specified" => Some(1),
            "Mask" => Some(2),
            // calcoutOOPT (Output Option)
            "Every Time" => Some(0),
            "On Change" => Some(1),
            "When Zero" => Some(2),
            "When Non-zero" => Some(3),
            "Transition To Zero" => Some(4),
            "Transition To Non-zero" => Some(5),
            // calcoutDOPT (Data Option)
            "Use CALC" => Some(0),
            "Use OCAL" => Some(1),
            // menuScan
            "Passive" => Some(0),
            "Event" => Some(1),
            "I/O Intr" => Some(2),
            "10 second" => Some(3),
            "5 second" => Some(4),
            "2 second" => Some(5),
            "1 second" => Some(6),
            ".5 second" => Some(7),
            ".2 second" => Some(8),
            ".1 second" => Some(9),
            // menuPini is deliberately absent. Its real choice order is
            // NO,YES,RUN,RUNNING,PAUSE,PAUSED (`menuPini.dbd.pod:59-65`) — the
            // entries that used to live here (`RUNNING=2`, `PAUSED=4`, plus two
            // invented `*_NOT_CA` choices) named the wrong indices. `PINI`
            // resolves against its own menu via
            // `crate::server::record::PiniMode::from_str`, which is what this
            // field-blind table cannot do: the same label names different
            // indices in different menus.
            _ => None,
        }
    }

    /// Parse a `.db` field value into an EpicsValue of the given type — the
    /// BYTE-STRING form, which is what a `.db` value actually is.
    ///
    /// A `DBF_STRING` is a byte string with a 40-byte budget, not a sequence of
    /// characters: C's escape translation writes ONE byte per `\xHH`
    /// (`epicsString.c:106`, `OUT(u)` into a `char`), so `field(VAL,"h\xffz")`
    /// is the three bytes `h`, `0xFF`, `z` — measured on softIoc, where `dbgf`
    /// prints `"h\xffz"` (one escape, not the `\xc3\xbf` a UTF-8 encoding of
    /// U+00FF would print). Going through [`Self::parse`] would force the value
    /// through a Rust `String` and turn that one byte into two.
    ///
    /// Every other DBF type is a number or a menu label — ASCII by
    /// construction, and a byte outside ASCII is a parse error either way — so
    /// they are handed to [`Self::parse`] unchanged.
    pub fn parse_bytes(dbr_type: DbFieldType, bytes: &[u8]) -> CaResult<Self> {
        if dbr_type == DbFieldType::String {
            return Ok(Self::String(PvString::from_bytes(bytes.trim_ascii())));
        }
        Self::parse(dbr_type, &String::from_utf8_lossy(bytes))
    }

    /// Parse a string value into an EpicsValue of the given type
    pub fn parse(dbr_type: DbFieldType, s: &str) -> CaResult<Self> {
        // C EPICS treats empty/whitespace strings as zero for numeric fields
        let s = s.trim();
        if s.is_empty() {
            return match dbr_type {
                DbFieldType::String => Ok(Self::String(PvString::new())),
                DbFieldType::Short => Ok(Self::Short(0)),
                DbFieldType::Float => Ok(Self::Float(0.0)),
                DbFieldType::Enum => Ok(Self::Enum(0)),
                DbFieldType::Char => Ok(Self::Char(0)),
                DbFieldType::Long => Ok(Self::Long(0)),
                DbFieldType::Double => Ok(Self::Double(0.0)),
                DbFieldType::Int64 => Ok(Self::Int64(0)),
                DbFieldType::UInt64 => Ok(Self::UInt64(0)),
                DbFieldType::UShort => Ok(Self::UShort(0)),
                DbFieldType::ULong => Ok(Self::ULong(0)),
                DbFieldType::UChar => Ok(Self::UChar(0)),
            };
        }
        match dbr_type {
            DbFieldType::String => Ok(Self::String(s.into())),
            DbFieldType::Short => Self::parse_int(s)
                .map(|v| Self::Short(v as i16))
                .or_else(|_| {
                    Self::resolve_menu_string(s)
                        .map(Self::Short)
                        .ok_or_else(|| {
                            CaError::InvalidValue(format!("invalid short or menu string: {s}"))
                        })
                }),
            DbFieldType::Float => parse_string_to_f64(s)
                .map(|v| Self::Float(v as f32))
                .ok_or_else(|| CaError::InvalidValue(format!("invalid float literal: {s}"))),
            DbFieldType::Enum => Self::parse_int(s)
                .map(|v| Self::Enum(v as u16))
                .or_else(|_| {
                    Self::resolve_menu_string(s)
                        .map(|v| Self::Enum(v as u16))
                        .ok_or_else(|| {
                            CaError::InvalidValue(format!("invalid enum or menu string: {s}"))
                        })
                }),
            DbFieldType::Char => Self::parse_int(s)
                .map(|v| Self::Char(v as u8))
                .map_err(|e| CaError::InvalidValue(e.to_string())),
            DbFieldType::Long => Self::parse_int(s)
                .map(|v| Self::Long(v as i32))
                .map_err(|e| CaError::InvalidValue(e.to_string())),
            DbFieldType::Int64 => Self::parse_int(s)
                .map(Self::Int64)
                .map_err(|e| CaError::InvalidValue(e.to_string())),
            DbFieldType::UInt64 => Self::parse_uint(s)
                .map(Self::UInt64)
                .map_err(|e| CaError::InvalidValue(e.to_string())),
            DbFieldType::UShort => Self::parse_uint(s)
                .map(|v| Self::UShort(v as u16))
                .map_err(|e| CaError::InvalidValue(e.to_string())),
            DbFieldType::ULong => Self::parse_uint(s)
                .map(|v| Self::ULong(v as u32))
                .map_err(|e| CaError::InvalidValue(e.to_string())),
            // DBF_UCHAR parses unsigned like its UShort/ULong siblings
            // (epicsUInt8), narrowing off the low byte.
            DbFieldType::UChar => Self::parse_uint(s)
                .map(|v| Self::UChar(v as u8))
                .map_err(|e| CaError::InvalidValue(e.to_string())),
            DbFieldType::Double => parse_string_to_f64(s)
                .map(Self::Double)
                .ok_or_else(|| CaError::InvalidValue(format!("invalid double literal: {s}"))),
        }
    }

    /// Parse an integer string with C-style radix prefixes (0x for hex, 0 for
    /// octal). Shared C base-0 parser for the `DBF_*` string PUT path and the
    /// QSRV bridge's `ScalarValue::String` → integer conversion, matching
    /// pvxs `parseTo<int64_t>` (`std::stoll(s,&idx,0)`). Rejects empty input.
    pub fn parse_int(s: &str) -> CaResult<i64> {
        let s = s.trim();
        if s.starts_with("0x") || s.starts_with("0X") {
            i64::from_str_radix(&s[2..], 16).map_err(|e| CaError::InvalidValue(e.to_string()))
        } else if s.starts_with('0')
            && s.len() > 1
            && s.chars().nth(1).is_some_and(|c| c.is_ascii_digit())
        {
            i64::from_str_radix(&s[1..], 8).map_err(|e| CaError::InvalidValue(e.to_string()))
        } else {
            s.parse::<i64>()
                .map_err(|e| CaError::InvalidValue(e.to_string()))
        }
    }

    /// Parse an unsigned integer string with C-style radix prefixes
    /// (`0x` hex, leading-`0` octal) and `strtoul` sign handling. Mirrors C
    /// `epicsParseUInt8/16/32/64`, which dbStaticLib.c calls with `base = 0`
    /// for DBF_UCHAR/USHORT/ULONG/UINT64: an optional leading `+`/`-` precedes
    /// the radix prefix, and a `-` negates in wrapping unsigned arithmetic
    /// exactly like `strtoul` (e.g. DBF_ULONG `TEVL="-1"` → `0xFFFFFFFF`,
    /// the areaDetector "all-bits / undefined" convention). Parsing keeps the
    /// full unsigned 64-bit range — `i64`-based parsing would reject
    /// `DBF_UINT64` values above `i64::MAX`; callers cast to the field width,
    /// matching C's final `(epicsUIntN)` cast of the wrapped value. Shared
    /// with the QSRV bridge's `ScalarValue::String` → `DBF_UINT64` conversion
    /// (pvxs `parseTo<uint64_t>` = `std::stoull(s,&idx,0)`). Rejects empty
    /// input.
    pub fn parse_uint(s: &str) -> CaResult<u64> {
        let s = s.trim();
        let (negative, body) = match s.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, s.strip_prefix('+').unwrap_or(s)),
        };
        let magnitude =
            if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
                u64::from_str_radix(hex, 16)
            } else if body.starts_with('0')
                && body.len() > 1
                && body.chars().nth(1).is_some_and(|c| c.is_ascii_digit())
            {
                u64::from_str_radix(&body[1..], 8)
            } else {
                body.parse::<u64>()
            }
            .map_err(|e| CaError::InvalidValue(e.to_string()))?;
        Ok(if negative {
            0u64.wrapping_sub(magnitude)
        } else {
            magnitude
        })
    }
}

/// Convert a string value to f64 with C-style hex/octal auto-detection.
///
/// Mirrors C EPICS dbConvert (88bfd6f, 2025-11-05) where the default
/// `dbConvertBase = 0` lets `epicsParseInt*` auto-detect base 10/16/8 by
/// prefix. A signed `0x`/`0X` prefix means hex; a leading `0` followed by
/// digits means octal; everything else is parsed as a decimal float so
/// `"1.5"`, `"1e6"`, `"-3.14"` continue to work.
fn parse_string_to_f64(s: &str) -> Option<f64> {
    // C parity for `epicsParseDouble` → `epicsStrtod`
    // (libcom/src/misc/epicsStdlib.c:347-374):
    //
    //   - Strip whitespace, optional `+`/`-`.
    //   - `"0x"` prefix (case-insensitive) → `strtoll`/`strtoull`
    //     with base 16 (hex literal accepted).
    //   - Otherwise → `strtod` (decimal / scientific). Leading `0`
    //     is **NOT** treated as octal — `strtod("0377")` returns
    //     377.0, not 255.0.
    //
    // PR #678 extends the dbpf path so a string `"0xFF"` lands as
    // 255.0 on a DOUBLE/FLOAT field; the PR does NOT introduce
    // octal handling for floats (`epicsStrtod` never had any).
    // Earlier this fn accepted `"0377"` as octal 255.0 — a real
    // C-divergence that silently changed caput payloads. Removed.
    let trimmed = s.trim();
    let (sign, body) = match trimmed.strip_prefix('-') {
        Some(rest) => (-1.0f64, rest),
        None => (1.0f64, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        if let Ok(v) = u64::from_str_radix(hex, 16) {
            return Some(sign * v as f64);
        }
    }
    trimmed.parse::<f64>().ok()
}

#[cfg(test)]
mod parse_radix_tests {
    use super::*;

    #[test]
    fn parse_hex_to_long() {
        assert_eq!(
            EpicsValue::parse(DbFieldType::Long, "0xFF").unwrap(),
            EpicsValue::Long(255)
        );
        assert_eq!(
            EpicsValue::parse(DbFieldType::Long, "0X1A").unwrap(),
            EpicsValue::Long(26)
        );
    }

    #[test]
    fn parse_octal_to_long() {
        assert_eq!(
            EpicsValue::parse(DbFieldType::Long, "0377").unwrap(),
            EpicsValue::Long(255)
        );
        assert_eq!(
            EpicsValue::parse(DbFieldType::Long, "010").unwrap(),
            EpicsValue::Long(8)
        );
    }

    #[test]
    fn parse_negative_unsigned_wraps_like_strtoul() {
        // C parity: dbStaticLib.c parses DBF_USHORT/ULONG/UINT64 via
        // epicsParseUInt16/32/64, which use strtoul/strtoull and accept a
        // leading '-', negating in wrapping unsigned arithmetic then casting
        // to the field width. areaDetector NDProcess/NDROI use TEVL="-1" for
        // a DBF_ULONG field to mean 0xFFFFFFFF.
        assert_eq!(
            EpicsValue::parse(DbFieldType::ULong, "-1").unwrap(),
            EpicsValue::ULong(0xFFFF_FFFF)
        );
        assert_eq!(
            EpicsValue::parse(DbFieldType::ULong, "-5").unwrap(),
            EpicsValue::ULong(0xFFFF_FFFB)
        );
        assert_eq!(
            EpicsValue::parse(DbFieldType::UShort, "-1").unwrap(),
            EpicsValue::UShort(0xFFFF)
        );
        assert_eq!(
            EpicsValue::parse(DbFieldType::UInt64, "-1").unwrap(),
            EpicsValue::UInt64(u64::MAX)
        );
        // Sign precedes the radix prefix, and '+' is accepted (strtoul).
        assert_eq!(
            EpicsValue::parse(DbFieldType::ULong, "-0x10").unwrap(),
            EpicsValue::ULong(0xFFFF_FFF0)
        );
        assert_eq!(
            EpicsValue::parse(DbFieldType::ULong, "+255").unwrap(),
            EpicsValue::ULong(255)
        );
        // Positive hex/decimal still parse normally (asyn devInt32.db TEVL="0xa").
        assert_eq!(
            EpicsValue::parse(DbFieldType::ULong, "0xa").unwrap(),
            EpicsValue::ULong(10)
        );
    }

    #[test]
    fn parse_hex_to_double() {
        // PR #678: dbpf to a DOUBLE field with "0xFF" must accept.
        assert_eq!(
            EpicsValue::parse(DbFieldType::Double, "0xFF").unwrap(),
            EpicsValue::Double(255.0)
        );
    }

    #[test]
    fn parse_leading_zero_to_double_is_decimal_not_octal() {
        // C parity: epicsStrtod("0377") -> 377.0 via strtod (decimal
        // with a meaningless leading zero); octal recognition lives
        // only on the int-parse path (strtol base 0). Earlier this
        // test asserted octal 255.0 — a real C-divergence.
        assert_eq!(
            EpicsValue::parse(DbFieldType::Double, "0377").unwrap(),
            EpicsValue::Double(377.0)
        );
    }

    #[test]
    fn parse_hex_to_float_with_sign() {
        assert_eq!(
            EpicsValue::parse(DbFieldType::Float, "-0x10").unwrap(),
            EpicsValue::Float(-16.0)
        );
    }

    #[test]
    fn parse_double_keeps_decimal_format() {
        // The hex/octal extension must not break plain decimals.
        assert_eq!(
            EpicsValue::parse(DbFieldType::Double, "1.5").unwrap(),
            EpicsValue::Double(1.5)
        );
        assert_eq!(
            EpicsValue::parse(DbFieldType::Double, "1e6").unwrap(),
            EpicsValue::Double(1_000_000.0)
        );
    }

    #[test]
    fn parse_hex_to_short_and_char() {
        assert_eq!(
            EpicsValue::parse(DbFieldType::Short, "0x7F").unwrap(),
            EpicsValue::Short(127)
        );
        assert_eq!(
            EpicsValue::parse(DbFieldType::Char, "0xFF").unwrap(),
            EpicsValue::Char(255)
        );
    }

    #[test]
    fn char_renders_signed_to_string() {
        // C getCharString sign-extends DBF_CHAR (epicsInt8): 0xFF -> "-1",
        // 0x80 -> "-128". A DBR_STRING GET of a scalar DBF_CHAR routes the
        // value through Display, so the rendered byte must be signed.
        assert_eq!(EpicsValue::Char(0xFF).to_string(), "-1");
        assert_eq!(EpicsValue::Char(0x80).to_string(), "-128");
        assert_eq!(EpicsValue::Char(0xC8).to_string(), "-56");
        assert_eq!(EpicsValue::Char(0x7F).to_string(), "127");
        assert_eq!(EpicsValue::Char(0).to_string(), "0");
        // The same byte promotes to f64 signed, so the two views agree.
        assert_eq!(EpicsValue::Char(0xFF).to_f64(), Some(-1.0));
    }
}

#[cfg(test)]
mod array_convert_tests {
    use super::*;

    /// a numeric array converted to a different DBR native type
    /// must convert element-by-element, not collapse to one scalar.
    #[test]
    fn double_array_to_short_array() {
        let v = EpicsValue::DoubleArray(vec![1.5, 2.9, -3.1]);
        assert_eq!(
            v.convert_to(DbFieldType::Short),
            EpicsValue::ShortArray(vec![1, 2, -3])
        );
    }

    #[test]
    fn short_array_to_double_array() {
        let v = EpicsValue::ShortArray(vec![10, 20, 30]);
        assert_eq!(
            v.convert_to(DbFieldType::Double),
            EpicsValue::DoubleArray(vec![10.0, 20.0, 30.0])
        );
    }

    #[test]
    fn long_array_to_float_array() {
        let v = EpicsValue::LongArray(vec![-1, 0, 7]);
        assert_eq!(
            v.convert_to(DbFieldType::Float),
            EpicsValue::FloatArray(vec![-1.0, 0.0, 7.0])
        );
    }

    #[test]
    fn double_array_to_int64_array() {
        let v = EpicsValue::DoubleArray(vec![100.0, 200.0]);
        assert_eq!(
            v.convert_to(DbFieldType::Int64),
            EpicsValue::Int64Array(vec![100, 200])
        );
    }

    /// CharArray converts element-wise as signed `epicsInt8`.
    #[test]
    fn char_array_to_short_array_signed() {
        let v = EpicsValue::CharArray(vec![0x01, 0xFF]); // 1, -1
        assert_eq!(
            v.convert_to(DbFieldType::Short),
            EpicsValue::ShortArray(vec![1, -1])
        );
    }

    /// Empty array stays an empty array of the target type (count
    /// preserved), not a scalar zero.
    #[test]
    fn empty_array_conversion_preserves_emptiness() {
        let v = EpicsValue::DoubleArray(Vec::new());
        assert_eq!(
            v.convert_to(DbFieldType::Short),
            EpicsValue::ShortArray(Vec::new())
        );
    }

    /// Same-type conversion is an identity clone.
    #[test]
    fn same_type_array_identity() {
        let v = EpicsValue::LongArray(vec![1, 2, 3]);
        assert_eq!(v.convert_to(DbFieldType::Long), v);
    }

    /// An integer array narrowed to a signed target truncates per element
    /// (pvxs `convertCast`'s `Dest(S[i])`, `sharedarray.cpp:160-166`), it
    /// must not saturate through f64. The native PVA `uint[]` carrier is
    /// `Int64Array`, so a `uint[] {1,2,0xffffffff}` PUT into a
    /// `waveform(FTVL=LONG)` lands as `{1,2,-1}`, not `{1,2,i32::MAX}`.
    #[test]
    fn int64_array_narrows_to_signed_long_by_truncation() {
        let v = EpicsValue::Int64Array(vec![1, 2, 0xffff_ffff]);
        assert_eq!(
            v.convert_to(DbFieldType::Long),
            EpicsValue::LongArray(vec![1, 2, -1])
        );
        // i16 target truncates the same way (pvxs uint32->int16 == -1).
        assert_eq!(
            v.convert_to(DbFieldType::Short),
            EpicsValue::ShortArray(vec![1, 2, -1])
        );
        // Float/Double targets keep the value-preserving (unsigned) view.
        assert_eq!(
            EpicsValue::UInt64Array(vec![u64::MAX]).convert_to(DbFieldType::Double),
            EpicsValue::DoubleArray(vec![u64::MAX as f64])
        );
    }
}

/// Integer-source
/// scalar narrowing must use C/pvxs `static_cast` truncation, never the
/// f64 round-trip's saturation. Values pinned against pvxs
/// `test/testdata.cpp:596-599`.
#[cfg(test)]
mod int_narrow_scalar_tests {
    use super::*;

    #[test]
    fn uint_carrier_narrows_to_signed_by_truncation() {
        // uint32 0xffffffff (carried as Int64) -> int32 / int16 == -1.
        assert_eq!(
            EpicsValue::Int64(0xffff_ffff).convert_to(DbFieldType::Long),
            EpicsValue::Long(-1)
        );
        assert_eq!(
            EpicsValue::Int64(0xffff_ffff).convert_to(DbFieldType::Short),
            EpicsValue::Short(-1)
        );
        // uint32 0x80000000 -> int32 == i32::MIN (testdata.cpp:599).
        assert_eq!(
            EpicsValue::Int64(0x8000_0000).convert_to(DbFieldType::Long),
            EpicsValue::Long(i32::MIN)
        );
        // uint16 0xffff -> int32 widens losslessly (testdata.cpp:598).
        assert_eq!(
            EpicsValue::Enum(0xffff).convert_to(DbFieldType::Long),
            EpicsValue::Long(0xffff)
        );
        // DBF_INT64 target stays lossless: the full epicsUInt32 range.
        assert_eq!(
            EpicsValue::Int64(0xffff_ffff).convert_to(DbFieldType::Int64),
            EpicsValue::Int64(0xffff_ffff)
        );
        // Float source keeps the f64 path unchanged (out of this family).
        assert_eq!(
            EpicsValue::Double(99.9).convert_to(DbFieldType::Long),
            EpicsValue::Long(99)
        );
    }
}

#[cfg(test)]
mod enum_with_choices_tests {
    use super::*;

    fn carrier(index: u16) -> EpicsValue {
        EpicsValue::EnumWithChoices {
            index,
            choices: vec!["Off".into(), "On".into(), "Reset".into()],
        }
    }

    /// pvxs `pvalink_lset.cpp:330-360`, `case DBR_STRING`: a string target
    /// stores the choice LABEL, not the index. Index 2 → "Reset".
    #[test]
    fn string_target_resolves_to_label() {
        assert_eq!(
            carrier(2).convert_to(DbFieldType::String),
            EpicsValue::String("Reset".into())
        );
    }

    /// Out-of-range index falls back to the decimal index ("%u" in pvxs),
    /// not an empty string or a panic.
    #[test]
    fn string_target_out_of_range_falls_back_to_index() {
        assert_eq!(
            carrier(5).convert_to(DbFieldType::String),
            EpicsValue::String("5".into())
        );
    }

    /// Every NUMERIC target takes the bare index. One case per target type
    /// so a regression in any single width is caught in isolation.
    #[test]
    fn numeric_targets_take_index() {
        assert_eq!(
            carrier(2).convert_to(DbFieldType::Long),
            EpicsValue::Long(2)
        );
        assert_eq!(
            carrier(2).convert_to(DbFieldType::Short),
            EpicsValue::Short(2)
        );
        assert_eq!(
            carrier(2).convert_to(DbFieldType::Char),
            EpicsValue::Char(2)
        );
        assert_eq!(
            carrier(2).convert_to(DbFieldType::Int64),
            EpicsValue::Int64(2)
        );
        assert_eq!(
            carrier(2).convert_to(DbFieldType::UInt64),
            EpicsValue::UInt64(2)
        );
        assert_eq!(
            carrier(2).convert_to(DbFieldType::Float),
            EpicsValue::Float(2.0)
        );
        assert_eq!(
            carrier(2).convert_to(DbFieldType::Double),
            EpicsValue::Double(2.0)
        );
    }

    /// An ENUM target collapses the carrier to a bare `Enum(index)` — it must
    /// NOT short-circuit as an identity clone (both report `db_field_type ==
    /// Enum`), which is why `convert_to` handles the carrier before the
    /// `db_field_type()==target` check.
    #[test]
    fn enum_target_collapses_to_bare_index() {
        assert_eq!(
            carrier(2).convert_to(DbFieldType::Enum),
            EpicsValue::Enum(2)
        );
    }

    /// `enum_index_label` is the shared label/fallback helper.
    #[test]
    fn enum_index_label_in_and_out_of_range() {
        let choices: Vec<PvString> = vec!["Off".into(), "On".into()];
        assert_eq!(EpicsValue::enum_index_label(&choices, 1), "On");
        assert_eq!(EpicsValue::enum_index_label(&choices, 9), "9");
    }

    /// An in-range NTEnum choice label with non-UTF-8 bytes must reach
    /// `EpicsValue::String` verbatim — C copies the raw `std::string` choice
    /// with `strncpy(choice.c_str(), …)` (`pvalink_lset.cpp:349`). A lossy
    /// `String`/`as_str_lossy` round-trip would rewrite the `0xFF` byte as the
    /// 3-byte `U+FFFD` replacement before storage. The out-of-range fallback
    /// is ASCII and stays byte-identical.
    #[test]
    fn enum_choice_label_preserves_non_utf8_bytes() {
        let choices = vec![PvString::from_bytes(vec![0xFFu8])];
        // In-range: raw byte preserved, not the 3-byte U+FFFD replacement.
        assert_eq!(
            EpicsValue::enum_index_label(&choices, 0).as_bytes(),
            &[0xFFu8]
        );
        let carrier = EpicsValue::EnumWithChoices {
            index: 0,
            choices: choices.clone(),
        };
        match carrier.convert_to(DbFieldType::String) {
            EpicsValue::String(s) => assert_eq!(s.as_bytes(), &[0xFFu8]),
            other => panic!("expected String, got {other:?}"),
        }
        // Out-of-range: ASCII decimal index.
        assert_eq!(EpicsValue::enum_index_label(&choices, 9).as_bytes(), b"9");
    }

    /// Boundaries of C's `useValque` test, one case per boundary: a fixed-width
    /// scalar, a string exactly `sizeof(union native_value)`, one byte past it,
    /// the heap-carrying enum variant, and an array at its smallest — a
    /// one-element array is still an array field in C (`dbChannelElements`
    /// reads the declared NELM, not the current NORD).
    #[test]
    fn queues_by_value_boundaries() {
        assert!(EpicsValue::Double(1.0).queues_by_value());
        assert!(
            EpicsValue::String(PvString::from_bytes(vec![b'x'; NATIVE_VALUE_BYTES]))
                .queues_by_value()
        );
        assert!(
            !EpicsValue::String(PvString::from_bytes(vec![b'x'; NATIVE_VALUE_BYTES + 1]))
                .queues_by_value()
        );
        assert!(
            !EpicsValue::EnumWithChoices {
                index: 0,
                choices: vec![],
            }
            .queues_by_value()
        );
        assert!(!EpicsValue::DoubleArray(vec![1.0]).queues_by_value());
        assert!(!EpicsValue::DoubleArray(vec![]).queues_by_value());
        assert!(!EpicsValue::CharArray(vec![0u8; 1]).queues_by_value());
    }
}
