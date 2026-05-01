//! pvData wire codec — encodes/decodes [`FieldDesc`] (introspection) and
//! [`PvField`] (values).
//!
//! Source: pvxs `dataencode.cpp` + `data.cpp`. Designed to be byte-exact with
//! `spvirit_codec::spvd_encode`/`spvd_decode` for the shapes our `FieldDesc`
//! covers (scalars, scalar arrays, structures, structure arrays, unions,
//! union arrays, variants, bounded strings).
//!
//! Wire-format type tags (from pvxs `dataencode.cpp`):
//!
//! | tag    | meaning                                  |
//! |--------|------------------------------------------|
//! | 0x00   | boolean                                  |
//! | 0x20-7 | signed/unsigned ints (Byte..ULong)       |
//! | 0x42-3 | float / double                           |
//! | 0x60   | string                                   |
//! | 0x80   | structure (followed by descriptor body)  |
//! | 0x81   | union     (followed by descriptor body)  |
//! | 0x82   | variant ("any")                          |
//! | 0x83   | bounded string + size word               |
//! | 0x88   | structure array (followed by 0x80 + body)|
//! | 0x89   | union array     (followed by 0x81 + body)|
//! | 0x8A   | variant array                            |
//! | scalar | tag (above) | 0x08 → scalar array        |

use std::io::Cursor;

use crate::decode_err;
use super::TypedScalarArray;
use super::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue, UnionItem, VariantValue};

/// Bulk-emit a [`TypedScalarArray`] body (no length prefix; caller
/// emits the Size). pvxs `to_wire(shared_array)` parity:
///
/// - For POD primitives (i8/u8/i16/u16/i32/u32/i64/u64/f32/f64) and a
///   matching host/wire endian, this is **a single
///   `Vec::extend_from_slice` call** which LLVM lowers to SIMD
///   memcpy. 1 M f64 elements = ~one memcpy invocation.
/// - For mismatched endian we walk the typed slice with `to_be_bytes`
///   / `to_le_bytes` (one byte-swap per element, no enum match).
/// - Boolean is encoded as 1-byte 0/1; we still use a tight loop
///   because Rust `bool` and wire `Boolean` have the same layout but
///   strict spec says only 0x00/0x01 are valid.
/// - String falls back to per-element `encode_string_into`.
#[inline]
pub(crate) fn encode_typed_scalar_array(
    arr: &TypedScalarArray,
    order: ByteOrder,
    out: &mut Vec<u8>,
) {
    let host_be = cfg!(target_endian = "big");
    let wire_be = matches!(order, ByteOrder::Big);
    let same_endian = host_be == wire_be;
    match arr {
        TypedScalarArray::Boolean(a) => {
            for &b in a.iter() {
                out.put_u8(if b { 1 } else { 0 });
            }
        }
        TypedScalarArray::Byte(a) => {
            // 1-byte primitive: endian doesn't apply. Single memcpy.
            // SAFETY: Arc<[i8]> is contiguous; cast to &[u8] is sound
            // because i8 and u8 share repr.
            let bytes: &[u8] =
                unsafe { std::slice::from_raw_parts(a.as_ptr() as *const u8, a.len()) };
            out.extend_from_slice(bytes);
        }
        TypedScalarArray::UByte(a) => {
            out.extend_from_slice(a);
        }
        TypedScalarArray::Short(a) => emit_pod(a, same_endian, out, |x: i16| {
            if wire_be {
                x.to_be_bytes()
            } else {
                x.to_le_bytes()
            }
        }),
        TypedScalarArray::UShort(a) => emit_pod(a, same_endian, out, |x: u16| {
            if wire_be {
                x.to_be_bytes()
            } else {
                x.to_le_bytes()
            }
        }),
        TypedScalarArray::Int(a) => emit_pod(a, same_endian, out, |x: i32| {
            if wire_be {
                x.to_be_bytes()
            } else {
                x.to_le_bytes()
            }
        }),
        TypedScalarArray::UInt(a) => emit_pod(a, same_endian, out, |x: u32| {
            if wire_be {
                x.to_be_bytes()
            } else {
                x.to_le_bytes()
            }
        }),
        TypedScalarArray::Long(a) => emit_pod(a, same_endian, out, |x: i64| {
            if wire_be {
                x.to_be_bytes()
            } else {
                x.to_le_bytes()
            }
        }),
        TypedScalarArray::ULong(a) => emit_pod(a, same_endian, out, |x: u64| {
            if wire_be {
                x.to_be_bytes()
            } else {
                x.to_le_bytes()
            }
        }),
        TypedScalarArray::Float(a) => emit_pod(a, same_endian, out, |x: f32| {
            if wire_be {
                x.to_be_bytes()
            } else {
                x.to_le_bytes()
            }
        }),
        TypedScalarArray::Double(a) => emit_pod(a, same_endian, out, |x: f64| {
            if wire_be {
                x.to_be_bytes()
            } else {
                x.to_le_bytes()
            }
        }),
        TypedScalarArray::String(a) => {
            for s in a.iter() {
                encode_string_into(s, order, out);
            }
        }
    }
}

/// Decode a scalar array of length `n` into a [`TypedScalarArray`].
/// Returns `Ok(Some(_))` for POD primitives + variable-length types
/// where we can safely build an `Arc<[T]>`, `Ok(None)` when the type
/// is not yet handled by the typed path (e.g. obscure encodings —
/// today none, all map to typed variants), and `Err` on a wire
/// decode error. F-G10.
#[inline]
pub(crate) fn decode_typed_scalar_array(
    st: ScalarType,
    n: usize,
    cur: &mut Cursor<&[u8]>,
    order: ByteOrder,
) -> Result<Option<TypedScalarArray>, DecodeError> {
    let host_be = cfg!(target_endian = "big");
    let wire_be = matches!(order, ByteOrder::Big);
    let same_endian = host_be == wire_be;
    Ok(Some(match st {
        ScalarType::Boolean => {
            let bytes = cur.get_bytes(n)?;
            TypedScalarArray::Boolean(bytes.iter().map(|&b| b != 0).collect::<Vec<_>>().into())
        }
        ScalarType::Byte => {
            let bytes = cur.get_bytes(n)?;
            // SAFETY: bytes is owned Vec<u8>; reinterpret as Vec<i8>
            // via pointer cast preserves layout (i8 and u8 are repr-
            // compatible) and length / capacity.
            let v: Vec<i8> = bytes.into_iter().map(|b| b as i8).collect();
            TypedScalarArray::Byte(v.into())
        }
        ScalarType::UByte => {
            let bytes = cur.get_bytes(n)?;
            TypedScalarArray::UByte(bytes.into())
        }
        ScalarType::Short => {
            TypedScalarArray::Short(read_pod_array::<i16, 2, _>(n, cur, same_endian, |b| {
                if wire_be {
                    i16::from_be_bytes(b)
                } else {
                    i16::from_le_bytes(b)
                }
            })?)
        }
        ScalarType::UShort => {
            TypedScalarArray::UShort(read_pod_array::<u16, 2, _>(n, cur, same_endian, |b| {
                if wire_be {
                    u16::from_be_bytes(b)
                } else {
                    u16::from_le_bytes(b)
                }
            })?)
        }
        ScalarType::Int => {
            TypedScalarArray::Int(read_pod_array::<i32, 4, _>(n, cur, same_endian, |b| {
                if wire_be {
                    i32::from_be_bytes(b)
                } else {
                    i32::from_le_bytes(b)
                }
            })?)
        }
        ScalarType::UInt => {
            TypedScalarArray::UInt(read_pod_array::<u32, 4, _>(n, cur, same_endian, |b| {
                if wire_be {
                    u32::from_be_bytes(b)
                } else {
                    u32::from_le_bytes(b)
                }
            })?)
        }
        ScalarType::Long => {
            TypedScalarArray::Long(read_pod_array::<i64, 8, _>(n, cur, same_endian, |b| {
                if wire_be {
                    i64::from_be_bytes(b)
                } else {
                    i64::from_le_bytes(b)
                }
            })?)
        }
        ScalarType::ULong => {
            TypedScalarArray::ULong(read_pod_array::<u64, 8, _>(n, cur, same_endian, |b| {
                if wire_be {
                    u64::from_be_bytes(b)
                } else {
                    u64::from_le_bytes(b)
                }
            })?)
        }
        ScalarType::Float => {
            TypedScalarArray::Float(read_pod_array::<f32, 4, _>(n, cur, same_endian, |b| {
                if wire_be {
                    f32::from_be_bytes(b)
                } else {
                    f32::from_le_bytes(b)
                }
            })?)
        }
        ScalarType::Double => {
            TypedScalarArray::Double(read_pod_array::<f64, 8, _>(n, cur, same_endian, |b| {
                if wire_be {
                    f64::from_be_bytes(b)
                } else {
                    f64::from_le_bytes(b)
                }
            })?)
        }
        ScalarType::String => {
            let mut v: Vec<String> = Vec::with_capacity(safe_capacity(n, cur));
            for _ in 0..n {
                v.push(decode_string(cur, order)?.unwrap_or_default());
            }
            TypedScalarArray::String(v.into())
        }
    }))
}

/// Read `n` POD-T elements from `cur`. Single memcpy when endian
/// matches; per-element swap loop otherwise. Returns an `Arc<[T]>`
/// to share data with downstream consumers without copying.
#[inline]
fn read_pod_array<T: Copy, const N: usize, F>(
    n: usize,
    cur: &mut Cursor<&[u8]>,
    same_endian: bool,
    swap: F,
) -> Result<std::sync::Arc<[T]>, DecodeError>
where
    F: Fn([u8; N]) -> T,
{
    let nbytes = n
        .checked_mul(N)
        .ok_or_else(|| decode_err!("scalar array element count × size overflows"))?;
    let bytes = cur.get_bytes(nbytes)?;
    if same_endian {
        // Allocate an aligned Vec<T>, memcpy bytes in.
        let mut v: Vec<T> = Vec::with_capacity(n);
        // SAFETY: T is a fixed-size POD primitive (i16/i32/i64/u*/f32/f64);
        // Vec<T>::with_capacity gives an alloc with align_of::<T>() and
        // capacity for n*N bytes; copy_nonoverlapping fills the prefix.
        // set_len(n) is sound because every byte was written.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), v.as_mut_ptr() as *mut u8, nbytes);
            v.set_len(n);
        }
        Ok(v.into())
    } else {
        // Mismatched endian: per-element swap. Still no enum match,
        // tight inlinable loop; LLVM auto-vectorizes the byte-reverse.
        let mut v: Vec<T> = Vec::with_capacity(n);
        let mut buf = [0u8; N];
        let mut off = 0usize;
        for _ in 0..n {
            buf.copy_from_slice(&bytes[off..off + N]);
            v.push(swap(buf));
            off += N;
        }
        Ok(v.into())
    }
}

/// Helper: emit a `Pod` slice as bulk bytes when endian matches,
/// per-element fallback otherwise. `swap` is only consulted on the
/// mismatched-endian path. Marked `#[inline]` so the closure inlines.
#[inline]
fn emit_pod<T: Copy, const N: usize, F>(a: &[T], same_endian: bool, out: &mut Vec<u8>, swap: F)
where
    F: Fn(T) -> [u8; N],
{
    if same_endian {
        // Single memcpy: pointer-cast to &[u8] then extend.
        // SAFETY: T is a fixed-size POD primitive; `a` is a
        // contiguous slice owned by the caller's Arc, alive for
        // this function's scope.
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(a.as_ptr() as *const u8, std::mem::size_of_val(a))
        };
        out.extend_from_slice(bytes);
    } else {
        out.reserve(a.len() * N);
        for &x in a {
            out.extend_from_slice(&swap(x));
        }
    }
}
use crate::proto::{
    ByteOrder, DecodeError, ReadExt, WriteExt, decode_size, decode_string, encode_size_into,
    encode_string_into,
};

const TAG_STRUCTURE: u8 = 0x80;
const TAG_UNION: u8 = 0x81;

/// P-G22: bound a `Vec::with_capacity` against an attacker-controlled
/// element count read from the wire. Every PVA decoded element must
/// consume at least 1 byte, so capping at the cursor's remaining-byte
/// count means a hostile peer announcing `n=0xFFFF_FFFF` allocates
/// `data_remaining` rather than 4 billion. Real producers stay below
/// the cap because their messages are sized accordingly.
#[inline]
fn safe_capacity(n: usize, cur: &Cursor<&[u8]>) -> usize {
    let remaining = cur.get_ref().len().saturating_sub(cur.position() as usize);
    n.min(remaining)
}
const TAG_VARIANT: u8 = 0x82;
const TAG_BOUNDED_STRING: u8 = 0x83;
const TAG_STRUCTURE_ARRAY: u8 = 0x88;
const TAG_UNION_ARRAY: u8 = 0x89;
const TAG_VARIANT_ARRAY: u8 = 0x8A;

// ── FieldDesc encode ─────────────────────────────────────────────────────

/// Encode a top-level `FieldDesc`. The output starts with a name field
/// (top-level descriptors carry an empty name) followed by the type
/// description; this matches the pvData "field" wire format used by
/// `pvRequest` and operation INIT responses.
pub fn encode_field_desc(name: &str, desc: &FieldDesc, order: ByteOrder, out: &mut Vec<u8>) {
    encode_string_into(name, order, out);
    encode_type_desc(desc, order, out);
}

/// Encode just the type-tag portion (no name) of a `FieldDesc`. Always
/// emits the inline form — never produces 0xFD/0xFE cache markers. Use
/// [`encode_type_desc_cached`] when sharing type slots across messages
/// on a single connection.
pub fn encode_type_desc(desc: &FieldDesc, order: ByteOrder, out: &mut Vec<u8>) {
    match desc {
        FieldDesc::Scalar(st) => out.put_u8(st.type_code()),
        FieldDesc::ScalarArray(st) => out.put_u8(st.array_type_code()),
        FieldDesc::Structure { struct_id, fields } => {
            out.put_u8(TAG_STRUCTURE);
            encode_structure_body(struct_id, fields, order, out);
        }
        FieldDesc::StructureArray { struct_id, fields } => {
            out.put_u8(TAG_STRUCTURE_ARRAY);
            out.put_u8(TAG_STRUCTURE);
            encode_structure_body(struct_id, fields, order, out);
        }
        FieldDesc::Union {
            struct_id,
            variants,
        } => {
            out.put_u8(TAG_UNION);
            encode_structure_body(struct_id, variants, order, out);
        }
        FieldDesc::UnionArray {
            struct_id,
            variants,
        } => {
            out.put_u8(TAG_UNION_ARRAY);
            out.put_u8(TAG_UNION);
            encode_structure_body(struct_id, variants, order, out);
        }
        FieldDesc::Variant => out.put_u8(TAG_VARIANT),
        FieldDesc::VariantArray => out.put_u8(TAG_VARIANT_ARRAY),
        FieldDesc::BoundedString(bound) => {
            out.put_u8(TAG_BOUNDED_STRING);
            encode_size_into(*bound, order, out);
        }
    }
}

fn encode_structure_body(
    struct_id: &str,
    fields: &[(String, FieldDesc)],
    order: ByteOrder,
    out: &mut Vec<u8>,
) {
    encode_string_into(struct_id, order, out);
    encode_size_into(fields.len() as u32, order, out);
    for (name, child) in fields {
        encode_field_desc(name, child, order, out);
    }
}

// ── FieldDesc encode (cached / 0xFD-0xFE emitter) ────────────────────────

/// Per-connection state for emitting `0xFD`/`0xFE` type-cache markers.
///
/// Mirror of the receiver-side [`TypeCache`]: when the same compound
/// `FieldDesc` is sent twice on a single connection the second emission
/// is replaced with a 3-byte `0xFE <slot>` reference instead of the full
/// inline body. For NTScalar/NTTable-class descriptors this saves
/// 100–500 bytes per repeat.
///
/// Only compound types (`Structure`, `StructureArray`, `Union`,
/// `UnionArray`) are cached — scalars and other 1–2 byte tags are smaller
/// inline than the 3-byte marker would be.
///
/// The receiver populates its decode `TypeCache` post-order from the
/// inline body of `0xFD <slot>` frames; the slot we allocate here is
/// arbitrary (a monotonic counter) and the decoder honours whatever key
/// we pick. Slots overflow `u16`; we panic on exhaustion (65 535 distinct
/// compound descriptors per connection — far beyond realistic use).
#[derive(Debug, Default, Clone)]
pub struct EncodeTypeCache {
    next: u16,
    map: std::collections::HashMap<FieldDesc, u16>,
}

impl EncodeTypeCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn cacheable(desc: &FieldDesc) -> bool {
        matches!(
            desc,
            FieldDesc::Structure { .. }
                | FieldDesc::StructureArray { .. }
                | FieldDesc::Union { .. }
                | FieldDesc::UnionArray { .. }
        )
    }
}

/// [`encode_field_desc`] threading an [`EncodeTypeCache`] so repeat
/// compound descriptors emit `0xFE <slot>` instead of the full body.
pub fn encode_field_desc_cached(
    name: &str,
    desc: &FieldDesc,
    order: ByteOrder,
    cache: &mut EncodeTypeCache,
    out: &mut Vec<u8>,
) {
    encode_string_into(name, order, out);
    encode_type_desc_cached(desc, order, cache, out);
}

/// [`encode_type_desc`] threading an [`EncodeTypeCache`]. On first sight
/// of a cacheable descriptor emits `0xFD <slot>` followed by the inline
/// body; on later sights of the same descriptor emits `0xFE <slot>`.
/// Non-cacheable descriptors (scalars, variants, bounded strings) always
/// fall through to the inline encoding.
pub fn encode_type_desc_cached(
    desc: &FieldDesc,
    order: ByteOrder,
    cache: &mut EncodeTypeCache,
    out: &mut Vec<u8>,
) {
    if EncodeTypeCache::cacheable(desc) {
        if let Some(&slot) = cache.map.get(desc) {
            out.put_u8(0xFE);
            out.put_u16(slot, order);
            return;
        }
        let slot = cache.next;
        cache.next = cache
            .next
            .checked_add(1)
            .expect("encode type cache slot overflow (>65535 compound descriptors)");
        cache.map.insert(desc.clone(), slot);
        out.put_u8(0xFD);
        out.put_u16(slot, order);
        // fall through and emit the inline body
    }
    encode_type_desc_inline_cached(desc, order, cache, out);
}

/// Emit the descriptor's inline body. Children may still consult the
/// cache and emit their own 0xFD/0xFE markers.
fn encode_type_desc_inline_cached(
    desc: &FieldDesc,
    order: ByteOrder,
    cache: &mut EncodeTypeCache,
    out: &mut Vec<u8>,
) {
    match desc {
        FieldDesc::Scalar(st) => out.put_u8(st.type_code()),
        FieldDesc::ScalarArray(st) => out.put_u8(st.array_type_code()),
        FieldDesc::Structure { struct_id, fields } => {
            out.put_u8(TAG_STRUCTURE);
            encode_structure_body_cached(struct_id, fields, order, cache, out);
        }
        FieldDesc::StructureArray { struct_id, fields } => {
            out.put_u8(TAG_STRUCTURE_ARRAY);
            out.put_u8(TAG_STRUCTURE);
            encode_structure_body_cached(struct_id, fields, order, cache, out);
        }
        FieldDesc::Union {
            struct_id,
            variants,
        } => {
            out.put_u8(TAG_UNION);
            encode_structure_body_cached(struct_id, variants, order, cache, out);
        }
        FieldDesc::UnionArray {
            struct_id,
            variants,
        } => {
            out.put_u8(TAG_UNION_ARRAY);
            out.put_u8(TAG_UNION);
            encode_structure_body_cached(struct_id, variants, order, cache, out);
        }
        FieldDesc::Variant => out.put_u8(TAG_VARIANT),
        FieldDesc::VariantArray => out.put_u8(TAG_VARIANT_ARRAY),
        FieldDesc::BoundedString(bound) => {
            out.put_u8(TAG_BOUNDED_STRING);
            encode_size_into(*bound, order, out);
        }
    }
}

fn encode_structure_body_cached(
    struct_id: &str,
    fields: &[(String, FieldDesc)],
    order: ByteOrder,
    cache: &mut EncodeTypeCache,
    out: &mut Vec<u8>,
) {
    encode_string_into(struct_id, order, out);
    encode_size_into(fields.len() as u32, order, out);
    for (name, child) in fields {
        encode_field_desc_cached(name, child, order, cache, out);
    }
}

// ── FieldDesc decode ─────────────────────────────────────────────────────

/// pvData type-descriptor cache. Stream-scoped: pvAccessJava (and
/// some pvxs paths) emit `0xFD <u16 key> <full desc>` to define a
/// slot, then `0xFE <u16 key>` to reference it later — saving wire
/// bytes for repeated NTScalar/etc descriptors in monitor streams.
///
/// Per-connection state. The wire emit-side encoder in this module
/// never produces 0xFD/0xFE; we accept them for interop only.
pub type TypeCache = std::collections::HashMap<u16, FieldDesc>;

/// Hard ceiling on FieldDesc / PvField nesting depth during decode.
/// pvxs uses the same convention (`Decoder::max_depth = 64`); without
/// it an adversarial peer can craft a deeply-nested Structure tree
/// that blows the per-task stack (tokio default 2 MB / ~256 KB on
/// macOS — well within reach at ~50K levels of recursion).
const MAX_FIELD_DEPTH: u32 = 64;

/// Decode a top-level `FieldDesc` (`name` + type description).
pub fn decode_field_desc(
    cur: &mut Cursor<&[u8]>,
    order: ByteOrder,
) -> Result<(String, FieldDesc), DecodeError> {
    decode_field_desc_at_depth(cur, order, 0)
}

fn decode_field_desc_at_depth(
    cur: &mut Cursor<&[u8]>,
    order: ByteOrder,
    depth: u32,
) -> Result<(String, FieldDesc), DecodeError> {
    let name = decode_string(cur, order)?.unwrap_or_default();
    let desc = decode_type_desc_at_depth(cur, order, depth)?;
    Ok((name, desc))
}

/// Variant of [`decode_field_desc`] threading a stream-scoped
/// [`TypeCache`] for 0xFD/0xFE marker support.
pub fn decode_field_desc_cached(
    cur: &mut Cursor<&[u8]>,
    order: ByteOrder,
    cache: &mut TypeCache,
) -> Result<(String, FieldDesc), DecodeError> {
    decode_field_desc_cached_at_depth(cur, order, cache, 0)
}

fn decode_field_desc_cached_at_depth(
    cur: &mut Cursor<&[u8]>,
    order: ByteOrder,
    cache: &mut TypeCache,
    depth: u32,
) -> Result<(String, FieldDesc), DecodeError> {
    let name = decode_string(cur, order)?.unwrap_or_default();
    let desc = decode_type_desc_cached_at_depth(cur, order, cache, depth)?;
    Ok((name, desc))
}

/// Decode just the type-tag portion of a descriptor (no cache).
pub fn decode_type_desc(
    cur: &mut Cursor<&[u8]>,
    order: ByteOrder,
) -> Result<FieldDesc, DecodeError> {
    decode_type_desc_at_depth(cur, order, 0)
}

fn decode_type_desc_at_depth(
    cur: &mut Cursor<&[u8]>,
    order: ByteOrder,
    depth: u32,
) -> Result<FieldDesc, DecodeError> {
    let mut empty = TypeCache::new();
    decode_type_desc_cached_at_depth(cur, order, &mut empty, depth)
}

/// Decode a type descriptor honouring 0xFD (define) / 0xFE (lookup)
/// markers against `cache`. Mirrors pvxs `dataencode.cpp::from_wire(buf,
/// descs, cache)`.
///
/// 0xFD: read a u16 key, recursively decode the full descriptor, store
/// in cache, return it. Anywhere a fresh inline descriptor would
/// appear, a peer may insert this prefix to populate the cache.
///
/// 0xFE: read a u16 key, return the cached descriptor by clone. If the
/// slot is empty an error is returned.
///
/// 0xFF: NULL — handled by callers (we reject here as caller-context
/// dependent).
pub fn decode_type_desc_cached(
    cur: &mut Cursor<&[u8]>,
    order: ByteOrder,
    cache: &mut TypeCache,
) -> Result<FieldDesc, DecodeError> {
    decode_type_desc_cached_at_depth(cur, order, cache, 0)
}

fn decode_type_desc_cached_at_depth(
    cur: &mut Cursor<&[u8]>,
    order: ByteOrder,
    cache: &mut TypeCache,
    depth: u32,
) -> Result<FieldDesc, DecodeError> {
    if depth > MAX_FIELD_DEPTH {
        return Err(decode_err!("FieldDesc nesting exceeds MAX_FIELD_DEPTH ({MAX_FIELD_DEPTH})"));
    }
    let tag = cur.get_u8()?;
    match tag {
        0xFD => {
            // define new cache slot, then full inline descriptor.
            let key = cur.get_u16(order)?;
            let desc = decode_type_desc_cached_at_depth(cur, order, cache, depth + 1)?;
            cache.insert(key, desc.clone());
            Ok(desc)
        }
        0xFE => {
            // lookup existing cache slot.
            let key = cur.get_u16(order)?;
            cache
                .get(&key)
                .cloned()
                .ok_or_else(|| decode_err!("typecache miss for slot {key}"))
        }
        TAG_STRUCTURE => decode_structure_body_cached_at_depth(cur, order, false, cache, depth + 1),
        TAG_UNION => decode_union_body_cached_at_depth(cur, order, false, cache, depth + 1),
        TAG_STRUCTURE_ARRAY => {
            let inner = cur.get_u8()?;
            if inner != TAG_STRUCTURE {
                return Err(decode_err!("structure-array element tag 0x{inner:02X} (expected 0x80)"));
            }
            decode_structure_body_cached_at_depth(cur, order, true, cache, depth + 1)
        }
        TAG_UNION_ARRAY => {
            let inner = cur.get_u8()?;
            if inner != TAG_UNION {
                return Err(decode_err!("union-array element tag 0x{inner:02X} (expected 0x81)"));
            }
            decode_union_body_cached_at_depth(cur, order, true, cache, depth + 1)
        }
        TAG_VARIANT => Ok(FieldDesc::Variant),
        TAG_VARIANT_ARRAY => Ok(FieldDesc::VariantArray),
        TAG_BOUNDED_STRING => {
            let bound = decode_size(cur, order)?
                .ok_or_else(|| decode_err!("bounded string size cannot be null"))?;
            Ok(FieldDesc::BoundedString(bound))
        }
        b if b & 0x08 != 0 => {
            let scalar = ScalarType::from_array_type_code(b)
                .ok_or_else(|| decode_err!("unknown scalar array tag 0x{b:02X}"))?;
            Ok(FieldDesc::ScalarArray(scalar))
        }
        b => {
            let scalar = ScalarType::from_type_code(b)
                .ok_or_else(|| decode_err!("unknown type tag 0x{b:02X}"))?;
            Ok(FieldDesc::Scalar(scalar))
        }
    }
}

fn decode_structure_body_cached_at_depth(
    cur: &mut Cursor<&[u8]>,
    order: ByteOrder,
    is_array: bool,
    cache: &mut TypeCache,
    depth: u32,
) -> Result<FieldDesc, DecodeError> {
    let struct_id = decode_string(cur, order)?.unwrap_or_default();
    let n = decode_size(cur, order)?
        .ok_or_else(|| decode_err!("structure field count cannot be null"))?
        as usize;
    let mut fields = Vec::with_capacity(safe_capacity(n, cur));
    for _ in 0..n {
        fields.push(decode_field_desc_cached_at_depth(cur, order, cache, depth)?);
    }
    Ok(if is_array {
        FieldDesc::StructureArray { struct_id, fields }
    } else {
        FieldDesc::Structure { struct_id, fields }
    })
}

fn decode_union_body_cached_at_depth(
    cur: &mut Cursor<&[u8]>,
    order: ByteOrder,
    is_array: bool,
    cache: &mut TypeCache,
    depth: u32,
) -> Result<FieldDesc, DecodeError> {
    let struct_id = decode_string(cur, order)?.unwrap_or_default();
    let n = decode_size(cur, order)?
        .ok_or_else(|| decode_err!("union variant count cannot be null"))?
        as usize;
    let mut variants = Vec::with_capacity(safe_capacity(n, cur));
    for _ in 0..n {
        variants.push(decode_field_desc_cached_at_depth(cur, order, cache, depth)?);
    }
    Ok(if is_array {
        FieldDesc::UnionArray {
            struct_id,
            variants,
        }
    } else {
        FieldDesc::Union {
            struct_id,
            variants,
        }
    })
}

// ── ScalarValue encode/decode ────────────────────────────────────────────

pub fn encode_scalar_value(v: &ScalarValue, order: ByteOrder, out: &mut Vec<u8>) {
    match v {
        ScalarValue::Boolean(b) => out.put_u8(if *b { 1 } else { 0 }),
        ScalarValue::Byte(x) => out.put_i8(*x),
        ScalarValue::UByte(x) => out.put_u8(*x),
        ScalarValue::Short(x) => out.put_i16(*x, order),
        ScalarValue::UShort(x) => out.put_u16(*x, order),
        ScalarValue::Int(x) => out.put_i32(*x, order),
        ScalarValue::UInt(x) => out.put_u32(*x, order),
        ScalarValue::Long(x) => out.put_i64(*x, order),
        ScalarValue::ULong(x) => out.put_u64(*x, order),
        ScalarValue::Float(x) => out.put_f32(*x, order),
        ScalarValue::Double(x) => out.put_f64(*x, order),
        ScalarValue::String(s) => encode_string_into(s, order, out),
    }
}

pub fn decode_scalar_value(
    st: ScalarType,
    cur: &mut Cursor<&[u8]>,
    order: ByteOrder,
) -> Result<ScalarValue, DecodeError> {
    Ok(match st {
        ScalarType::Boolean => ScalarValue::Boolean(cur.get_u8()? != 0),
        ScalarType::Byte => ScalarValue::Byte(cur.get_i8()?),
        ScalarType::UByte => ScalarValue::UByte(cur.get_u8()?),
        ScalarType::Short => ScalarValue::Short(cur.get_i16(order)?),
        ScalarType::UShort => ScalarValue::UShort(cur.get_u16(order)?),
        ScalarType::Int => ScalarValue::Int(cur.get_i32(order)?),
        ScalarType::UInt => ScalarValue::UInt(cur.get_u32(order)?),
        ScalarType::Long => ScalarValue::Long(cur.get_i64(order)?),
        ScalarType::ULong => ScalarValue::ULong(cur.get_u64(order)?),
        ScalarType::Float => ScalarValue::Float(cur.get_f32(order)?),
        ScalarType::Double => ScalarValue::Double(cur.get_f64(order)?),
        ScalarType::String => ScalarValue::String(decode_string(cur, order)?.unwrap_or_default()),
    })
}

// ── PvField encode/decode (full value; descriptor-driven) ────────────────

/// Encode the value bytes for a `PvField` given its descriptor.
pub fn encode_pv_field(value: &PvField, desc: &FieldDesc, order: ByteOrder, out: &mut Vec<u8>) {
    match (desc, value) {
        (FieldDesc::Scalar(_), PvField::Scalar(sv)) => encode_scalar_value(sv, order, out),
        (FieldDesc::ScalarArray(_), PvField::ScalarArrayTyped(arr)) => {
            // F-G9 fast path: typed array → bulk memcpy when host
            // endian == wire endian, per-element byte-swap loop
            // otherwise (still O(n) but no enum match per element).
            // pvxs `to_wire(shared_array)` parity (pvaproto.h:477).
            encode_size_into(arr.len() as u32, order, out);
            encode_typed_scalar_array(arr, order, out);
        }
        (FieldDesc::ScalarArray(_), PvField::ScalarArray(items)) => {
            // Slow path: legacy Vec<ScalarValue>. The per-element
            // enum match + write makes this allocator- and CPU-bound
            // for large arrays; new code should use the typed
            // constructors (PvField::scalar_array_double etc).
            encode_size_into(items.len() as u32, order, out);
            for sv in items {
                encode_scalar_value(sv, order, out);
            }
        }
        (FieldDesc::Structure { fields, .. }, PvField::Structure(s)) => {
            for (name, child_desc) in fields {
                let child_val = s
                    .get_field(name)
                    .cloned()
                    .unwrap_or_else(|| default_value_for(child_desc));
                encode_pv_field(&child_val, child_desc, order, out);
            }
        }
        (FieldDesc::StructureArray { fields, .. }, PvField::StructureArray(items)) => {
            encode_size_into(items.len() as u32, order, out);
            // Each element is preceded by a presence byte: 0x01 = element
            // follows, 0x00 = null. We always emit non-null.
            for s in items {
                out.put_u8(0x01);
                let element_desc = FieldDesc::Structure {
                    struct_id: s.struct_id.clone(),
                    fields: fields.clone(),
                };
                encode_pv_field(&PvField::Structure(s.clone()), &element_desc, order, out);
            }
        }
        (
            FieldDesc::Union { variants, .. },
            PvField::Union {
                selector, value, ..
            },
        ) => {
            // Union: selector (Size) followed by the chosen variant's value.
            // -1 → null marker (0xFF).
            if *selector < 0 {
                out.put_u8(0xFF);
            } else {
                encode_size_into(*selector as u32, order, out);
                if let Some((_, vdesc)) = variants.get(*selector as usize) {
                    encode_pv_field(value, vdesc, order, out);
                }
            }
        }
        (FieldDesc::UnionArray { variants, .. }, PvField::UnionArray(items)) => {
            encode_size_into(items.len() as u32, order, out);
            for it in items {
                if it.selector < 0 {
                    out.put_u8(0xFF);
                } else {
                    encode_size_into(it.selector as u32, order, out);
                    if let Some((_, vdesc)) = variants.get(it.selector as usize) {
                        encode_pv_field(&it.value, vdesc, order, out);
                    }
                }
            }
        }
        (FieldDesc::Variant, PvField::Variant(v)) => match &v.desc {
            None => out.put_u8(0xFF),
            Some(d) => {
                encode_type_desc(d, order, out);
                encode_pv_field(&v.value, d, order, out);
            }
        },
        (FieldDesc::VariantArray, PvField::VariantArray(items)) => {
            encode_size_into(items.len() as u32, order, out);
            for it in items {
                match &it.desc {
                    None => out.put_u8(0xFF),
                    Some(d) => {
                        encode_type_desc(d, order, out);
                        encode_pv_field(&it.value, d, order, out);
                    }
                }
            }
        }
        (FieldDesc::BoundedString(_), PvField::Scalar(ScalarValue::String(s))) => {
            encode_string_into(s, order, out);
        }
        // Fallback: write zero bytes for "missing" / mismatched values. Real
        // callers should ensure value/desc match; this just keeps encoding
        // total when they don't.
        _ => {}
    }
}

/// Encode the value bytes for `value` consulting `bitset` to know which
/// fields to emit. Mirrors pvxs `to_wire_valid(buf, value)`.
///
/// pvData spec §5.4 bit numbering: the root structure is bit 0, then
/// nested fields are numbered depth-first in declaration order. A
/// substructure is "present" when its own bit OR any descendant bit
/// is set — in that case we recurse and emit each child according to
/// its own bit. Fields whose bit is NOT set produce *no bytes*.
pub fn encode_pv_field_with_bitset(
    value: &PvField,
    desc: &FieldDesc,
    bitset: &crate::proto::BitSet,
    bit_offset: usize,
    order: ByteOrder,
    out: &mut Vec<u8>,
) {
    fn any_descendant_set(
        bitset: &crate::proto::BitSet,
        pos: usize,
        desc_local: &FieldDesc,
    ) -> bool {
        let total = desc_local.total_bits();
        for i in 0..total {
            if bitset.get(pos + i) {
                return true;
            }
        }
        false
    }

    match desc {
        FieldDesc::Scalar(_)
        | FieldDesc::ScalarArray(_)
        | FieldDesc::Variant
        | FieldDesc::VariantArray
        | FieldDesc::BoundedString(_)
        | FieldDesc::Union { .. }
        | FieldDesc::UnionArray { .. }
        | FieldDesc::StructureArray { .. } => {
            if bitset.get(bit_offset) {
                encode_pv_field(value, desc, order, out);
            }
            // else: emit no bytes
        }
        FieldDesc::Structure { fields, .. } => {
            if !any_descendant_set(bitset, bit_offset, desc) {
                return;
            }
            // Recurse into each child; emit only set ones.
            let mut child_bit = bit_offset + 1;
            for (name, child_desc) in fields {
                let child_value = match value {
                    PvField::Structure(s) => s
                        .get_field(name)
                        .cloned()
                        .unwrap_or_else(|| default_value_for(child_desc)),
                    _ => default_value_for(child_desc),
                };
                encode_pv_field_with_bitset(
                    &child_value,
                    child_desc,
                    bitset,
                    child_bit,
                    order,
                    out,
                );
                child_bit += child_desc.total_bits();
            }
        }
    }
}

/// Decode a `PvField` matching `desc`, consulting `bitset` to know which
/// fields are actually present on the wire.
///
/// pvxs / pvData encode only the fields whose bit (or whose descendants'
/// bits) is set; the rest are omitted entirely. Callers that want
/// "everything is present" semantics can pass a fully-set bitset of size
/// `desc.total_bits()`.
///
/// `bit_offset` is the bit position assigned to `desc` in the parent's
/// bitset numbering scheme — pvData spec §5.4 depth-first.
pub fn decode_pv_field_with_bitset(
    desc: &FieldDesc,
    bitset: &crate::proto::BitSet,
    bit_offset: usize,
    cur: &mut Cursor<&[u8]>,
    order: ByteOrder,
) -> Result<PvField, DecodeError> {
    // Helper: true iff the bit at `pos` is set, OR any descendant of
    // a structure starting at `pos` (with `desc_local`) is set.
    fn any_descendant_set(
        bitset: &crate::proto::BitSet,
        pos: usize,
        desc_local: &FieldDesc,
    ) -> bool {
        let total = desc_local.total_bits();
        for i in 0..total {
            if bitset.get(pos + i) {
                return true;
            }
        }
        false
    }

    // The bit at `bit_offset` represents this descriptor itself.
    // For Structure, recurse into children. For scalar/leaf, decode iff
    // the bit is set.
    match desc {
        FieldDesc::Scalar(_)
        | FieldDesc::ScalarArray(_)
        | FieldDesc::Variant
        | FieldDesc::VariantArray
        | FieldDesc::BoundedString(_)
        | FieldDesc::Union { .. }
        | FieldDesc::UnionArray { .. }
        | FieldDesc::StructureArray { .. } => {
            if bitset.get(bit_offset) {
                decode_pv_field(desc, cur, order)
            } else {
                Ok(default_value_for(desc))
            }
        }
        FieldDesc::Structure { struct_id, fields } => {
            // The root struct is "present" if its own bit OR any descendant
            // is set. If neither, return a default-filled structure.
            if !any_descendant_set(bitset, bit_offset, desc) {
                return Ok(default_value_for(desc));
            }
            let mut s = PvStructure::new(struct_id);
            // First child bit = root bit + 1.
            let mut child_bit = bit_offset + 1;
            for (name, child) in fields {
                let v = decode_pv_field_with_bitset(child, bitset, child_bit, cur, order)?;
                s.fields.push((name.clone(), v));
                child_bit += child.total_bits();
            }
            Ok(PvField::Structure(s))
        }
    }
}

/// pvxs cc5d382 (2022-11) "monitor yield 'complete' updates":
/// merge a freshly-decoded sparse `decoded` value with a previously
/// captured `prior` "complete" snapshot, so the result has every leaf
/// either freshly updated (bit set in `bitset`) OR carried over from
/// `prior` (bit not set). Used by client-side monitor delivery to
/// hand the user a complete value instead of a delta with default-
/// filled holes.
///
/// `bit_offset` matches `decode_pv_field_with_bitset`'s numbering
/// scheme — depth-first per pvData spec §5.4.
pub fn fill_unmarked_from_prior(
    desc: &FieldDesc,
    bitset: &crate::proto::BitSet,
    bit_offset: usize,
    decoded: PvField,
    prior: &PvField,
) -> PvField {
    match desc {
        FieldDesc::Scalar(_)
        | FieldDesc::ScalarArray(_)
        | FieldDesc::Variant
        | FieldDesc::VariantArray
        | FieldDesc::BoundedString(_)
        | FieldDesc::Union { .. }
        | FieldDesc::UnionArray { .. }
        | FieldDesc::StructureArray { .. } => {
            if bitset.get(bit_offset) {
                // Marked leaf: keep the freshly-decoded value.
                decoded
            } else {
                // Unmarked leaf: carry over from prior.
                prior.clone()
            }
        }
        FieldDesc::Structure { struct_id, fields } => {
            // Walk each child; recurse to honour per-field marking.
            // Pull the child PvField slots out of `decoded`'s structure
            // so we can move (not clone) them into the merged output.
            let mut decoded_children: Vec<(String, PvField)> = match decoded {
                PvField::Structure(s) => s.fields,
                _ => Vec::new(),
            };
            let prior_struct = match prior {
                PvField::Structure(s) => Some(s),
                _ => None,
            };
            let mut out = PvStructure::new(struct_id);
            let mut child_bit = bit_offset + 1;
            for (name, child_desc) in fields {
                // Take this child's decoded value out of the moved
                // vec; default-fill if it's missing (defensive — the
                // decoder always emits every field).
                let decoded_child = decoded_children
                    .iter()
                    .position(|(n, _)| n == name)
                    .map(|idx| decoded_children.swap_remove(idx).1)
                    .unwrap_or_else(|| default_value_for(child_desc));
                let prior_child = prior_struct
                    .and_then(|s| s.get_field(name))
                    .cloned()
                    .unwrap_or_else(|| default_value_for(child_desc));
                let merged = fill_unmarked_from_prior(
                    child_desc,
                    bitset,
                    child_bit,
                    decoded_child,
                    &prior_child,
                );
                out.fields.push((name.clone(), merged));
                child_bit += child_desc.total_bits();
            }
            PvField::Structure(out)
        }
    }
}

/// Decode a `PvField` matching `desc` — assumes every field is present
/// on the wire (i.e. bitset is "all bits set"). Used for cases like
/// CONNECTION_VALIDATION authnz where there's no bitset.
pub fn decode_pv_field(
    desc: &FieldDesc,
    cur: &mut Cursor<&[u8]>,
    order: ByteOrder,
) -> Result<PvField, DecodeError> {
    decode_pv_field_at_depth(desc, cur, order, 0)
}

fn decode_pv_field_at_depth(
    desc: &FieldDesc,
    cur: &mut Cursor<&[u8]>,
    order: ByteOrder,
    depth: u32,
) -> Result<PvField, DecodeError> {
    if depth > MAX_FIELD_DEPTH {
        return Err(decode_err!("PvField nesting exceeds MAX_FIELD_DEPTH ({MAX_FIELD_DEPTH})"));
    }
    Ok(match desc {
        FieldDesc::Scalar(st) => PvField::Scalar(decode_scalar_value(*st, cur, order)?),
        FieldDesc::ScalarArray(st) => {
            let n = decode_size(cur, order)?
                .ok_or_else(|| decode_err!("scalar array length cannot be null"))?
                as usize;
            // F-G10 fast path: decode straight into a typed Arc<[T]>
            // when the element is a fixed-size POD primitive. Single
            // memcpy when host endian matches wire; per-element
            // byte-swap loop otherwise. Same semantics as pvxs
            // `from_wire(shared_array)`.
            if let Some(typed) = decode_typed_scalar_array(*st, n, cur, order)? {
                return Ok(PvField::ScalarArrayTyped(typed));
            }
            let mut items = Vec::with_capacity(safe_capacity(n, cur));
            for _ in 0..n {
                items.push(decode_scalar_value(*st, cur, order)?);
            }
            PvField::ScalarArray(items)
        }
        FieldDesc::Structure { struct_id, fields } => {
            let mut s = PvStructure::new(struct_id);
            for (name, child_desc) in fields {
                let v = decode_pv_field_at_depth(child_desc, cur, order, depth + 1)?;
                s.fields.push((name.clone(), v));
            }
            PvField::Structure(s)
        }
        FieldDesc::StructureArray { struct_id, fields } => {
            let n = decode_size(cur, order)?
                .ok_or_else(|| decode_err!("structure array length cannot be null"))?
                as usize;
            let mut out = Vec::with_capacity(safe_capacity(n, cur));
            for _ in 0..n {
                // pvxs dataencode.cpp:359-361 — `to_wire(buf, uint8_t(0u))`
                // for null, `uint8_t(1u)` for present. Anything else is a
                // protocol violation.
                let presence = cur.get_u8()?;
                match presence {
                    0x00 => {
                        out.push(PvStructure::new(struct_id));
                    }
                    0x01 => {
                        let element_desc = FieldDesc::Structure {
                            struct_id: struct_id.clone(),
                            fields: fields.clone(),
                        };
                        if let PvField::Structure(s) =
                            decode_pv_field_at_depth(&element_desc, cur, order, depth + 1)?
                        {
                            out.push(s);
                        }
                    }
                    other => {
                        return Err(decode_err!("structure array element presence byte 0x{other:02X} (expected 0x00 or 0x01)"));
                    }
                }
            }
            PvField::StructureArray(out)
        }
        FieldDesc::Union { variants, .. } => {
            // Selector wire format = Size with 0xFF == null. pvxs
            // pvaproto.h:340-358 (`from_wire(buf, Selector&)`) routes
            // through the same Size codec. `decode_size` already returns
            // `None` for 0xFF, so no peek-and-pushback is needed.
            match decode_size(cur, order)? {
                None => PvField::Union {
                    selector: -1,
                    variant_name: String::new(),
                    value: Box::new(PvField::Null),
                },
                Some(sel_u32) => {
                    let sel = sel_u32 as i32;
                    let (variant_name, vdesc) = variants
                        .get(sel as usize)
                        .ok_or_else(|| decode_err!("union selector {sel} out of range"))?
                        .clone();
                    let value = decode_pv_field_at_depth(&vdesc, cur, order, depth + 1)?;
                    PvField::Union {
                        selector: sel,
                        variant_name,
                        value: Box::new(value),
                    }
                }
            }
        }
        FieldDesc::UnionArray { variants, .. } => {
            let n = decode_size(cur, order)?
                .ok_or_else(|| decode_err!("union array length cannot be null"))?
                as usize;
            let mut items = Vec::with_capacity(safe_capacity(n, cur));
            for _ in 0..n {
                // Per-element selector encoding matches the scalar Union
                // case: Size with 0xFF as the null sentinel.
                match decode_size(cur, order)? {
                    None => items.push(UnionItem {
                        selector: -1,
                        variant_name: String::new(),
                        value: PvField::Null,
                    }),
                    Some(sel_u32) => {
                        let sel = sel_u32 as i32;
                        let (variant_name, vdesc) = variants
                            .get(sel as usize)
                            .ok_or_else(|| {
                                decode_err!("union array selector {sel} out of range")
                            })?
                            .clone();
                        let value = decode_pv_field_at_depth(&vdesc, cur, order, depth + 1)?;
                        items.push(UnionItem {
                            selector: sel,
                            variant_name,
                            value,
                        });
                    }
                }
            }
            PvField::UnionArray(items)
        }
        FieldDesc::Variant => {
            // First byte is the type tag of the carried value, OR 0xFF for null.
            let peek = cur.get_u8()?;
            if peek == 0xFF {
                PvField::Variant(Box::new(VariantValue {
                    desc: None,
                    value: PvField::Null,
                }))
            } else {
                let pos = cur.position();
                cur.set_position(pos - 1);
                // Variant peeling: each FieldDesc::Variant value
                // carries an inline FieldDesc + recursive PvField,
                // costing only 1 byte on the wire per peel but one
                // Rust stack frame. R-2: cap the recursion via the
                // depth counter so an adversary can't blow the
                // per-task stack with chained Variant tags.
                let inner = decode_type_desc(cur, order)?;
                let value = decode_pv_field_at_depth(&inner, cur, order, depth + 1)?;
                PvField::Variant(Box::new(VariantValue {
                    desc: Some(inner),
                    value,
                }))
            }
        }
        FieldDesc::VariantArray => {
            let n = decode_size(cur, order)?
                .ok_or_else(|| decode_err!("variant array length"))?
                as usize;
            let mut items = Vec::with_capacity(safe_capacity(n, cur));
            for _ in 0..n {
                let peek = cur.get_u8()?;
                if peek == 0xFF {
                    items.push(VariantValue {
                        desc: None,
                        value: PvField::Null,
                    });
                    continue;
                }
                let pos = cur.position();
                cur.set_position(pos - 1);
                let inner = decode_type_desc(cur, order)?;
                let value = decode_pv_field_at_depth(&inner, cur, order, depth + 1)?;
                items.push(VariantValue {
                    desc: Some(inner),
                    value,
                });
            }
            PvField::VariantArray(items)
        }
        FieldDesc::BoundedString(_) => {
            let s = decode_string(cur, order)?.unwrap_or_default();
            PvField::Scalar(ScalarValue::String(s))
        }
    })
}

/// Default-zero value for a descriptor — used to fill missing fields when a
/// caller-supplied `PvStructure` is sparser than its descriptor.
pub fn default_value_for(desc: &FieldDesc) -> PvField {
    match desc {
        FieldDesc::Scalar(st) => PvField::Scalar(zero_scalar(*st)),
        FieldDesc::ScalarArray(_) => PvField::ScalarArray(Vec::new()),
        FieldDesc::Structure { struct_id, fields } => {
            let mut s = PvStructure::new(struct_id);
            for (name, child) in fields {
                s.fields.push((name.clone(), default_value_for(child)));
            }
            PvField::Structure(s)
        }
        FieldDesc::StructureArray { .. } => PvField::StructureArray(Vec::new()),
        FieldDesc::Union { .. } => PvField::Union {
            selector: -1,
            variant_name: String::new(),
            value: Box::new(PvField::Null),
        },
        FieldDesc::UnionArray { .. } => PvField::UnionArray(Vec::new()),
        FieldDesc::Variant => PvField::Variant(Box::new(VariantValue {
            desc: None,
            value: PvField::Null,
        })),
        FieldDesc::VariantArray => PvField::VariantArray(Vec::new()),
        FieldDesc::BoundedString(_) => PvField::Scalar(ScalarValue::String(String::new())),
    }
}

fn zero_scalar(st: ScalarType) -> ScalarValue {
    match st {
        ScalarType::Boolean => ScalarValue::Boolean(false),
        ScalarType::Byte => ScalarValue::Byte(0),
        ScalarType::Short => ScalarValue::Short(0),
        ScalarType::Int => ScalarValue::Int(0),
        ScalarType::Long => ScalarValue::Long(0),
        ScalarType::UByte => ScalarValue::UByte(0),
        ScalarType::UShort => ScalarValue::UShort(0),
        ScalarType::UInt => ScalarValue::UInt(0),
        ScalarType::ULong => ScalarValue::ULong(0),
        ScalarType::Float => ScalarValue::Float(0.0),
        ScalarType::Double => ScalarValue::Double(0.0),
        ScalarType::String => ScalarValue::String(String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nt_scalar_double_desc() -> FieldDesc {
        FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![
                ("value".into(), FieldDesc::Scalar(ScalarType::Double)),
                (
                    "alarm".into(),
                    FieldDesc::Structure {
                        struct_id: "alarm_t".into(),
                        fields: vec![
                            ("severity".into(), FieldDesc::Scalar(ScalarType::Int)),
                            ("status".into(), FieldDesc::Scalar(ScalarType::Int)),
                            ("message".into(), FieldDesc::Scalar(ScalarType::String)),
                        ],
                    },
                ),
            ],
        }
    }

    #[test]
    #[allow(clippy::approx_constant)]
    fn scalar_value_round_trip() {
        for v in [
            ScalarValue::Boolean(true),
            ScalarValue::Int(-12345),
            ScalarValue::ULong(u64::MAX - 1),
            ScalarValue::Double(2.71828),
            ScalarValue::String("hello".into()),
        ] {
            for order in [ByteOrder::Little, ByteOrder::Big] {
                let mut buf = Vec::new();
                encode_scalar_value(&v, order, &mut buf);
                let mut cur = Cursor::new(buf.as_slice());
                let decoded = decode_scalar_value(v.scalar_type(), &mut cur, order).unwrap();
                assert_eq!(decoded, v, "{:?} order={:?}", v, order);
                assert_eq!(cur.remaining(), 0);
            }
        }
    }

    #[test]
    fn field_desc_round_trip_structure() {
        let desc = nt_scalar_double_desc();
        for order in [ByteOrder::Little, ByteOrder::Big] {
            let mut buf = Vec::new();
            encode_field_desc("", &desc, order, &mut buf);
            let mut cur = Cursor::new(buf.as_slice());
            let (name, dec) = decode_field_desc(&mut cur, order).unwrap();
            assert_eq!(name, "");
            assert_eq!(format!("{dec}"), format!("{desc}"));
        }
    }

    #[test]
    fn field_desc_round_trip_union() {
        let desc = FieldDesc::Union {
            struct_id: String::new(),
            variants: vec![
                (
                    "doubleValue".into(),
                    FieldDesc::ScalarArray(ScalarType::Double),
                ),
                ("intValue".into(), FieldDesc::ScalarArray(ScalarType::Int)),
            ],
        };
        for order in [ByteOrder::Little, ByteOrder::Big] {
            let mut buf = Vec::new();
            encode_type_desc(&desc, order, &mut buf);
            let mut cur = Cursor::new(buf.as_slice());
            let dec = decode_type_desc(&mut cur, order).unwrap();
            assert_eq!(format!("{dec}"), format!("{desc}"));
        }
    }

    #[test]
    fn field_desc_round_trip_structure_array() {
        let desc = FieldDesc::StructureArray {
            struct_id: "epics:nt/NTAttribute:1.0".into(),
            fields: vec![
                ("name".into(), FieldDesc::Scalar(ScalarType::String)),
                ("value".into(), FieldDesc::Variant),
            ],
        };
        for order in [ByteOrder::Little, ByteOrder::Big] {
            let mut buf = Vec::new();
            encode_type_desc(&desc, order, &mut buf);
            let mut cur = Cursor::new(buf.as_slice());
            let dec = decode_type_desc(&mut cur, order).unwrap();
            assert_eq!(format!("{dec}"), format!("{desc}"));
        }
    }

    #[test]
    fn pv_field_round_trip_through_desc() {
        let desc = nt_scalar_double_desc();
        let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
        s.set("value", PvField::Scalar(ScalarValue::Double(42.5)));
        let mut alarm = PvStructure::new("alarm_t");
        alarm.set("severity", PvField::Scalar(ScalarValue::Int(0)));
        alarm.set("status", PvField::Scalar(ScalarValue::Int(0)));
        alarm.set("message", PvField::Scalar(ScalarValue::String("OK".into())));
        s.set("alarm", PvField::Structure(alarm));

        let value = PvField::Structure(s);
        let mut buf = Vec::new();
        encode_pv_field(&value, &desc, ByteOrder::Little, &mut buf);
        let mut cur = Cursor::new(buf.as_slice());
        let dec = decode_pv_field(&desc, &mut cur, ByteOrder::Little).unwrap();
        assert_eq!(format!("{dec}"), format!("{value}"));
        assert_eq!(cur.remaining(), 0);
    }

    #[test]
    fn scalar_array_round_trip() {
        let desc = FieldDesc::ScalarArray(ScalarType::Int);
        let v = PvField::ScalarArray(vec![
            ScalarValue::Int(1),
            ScalarValue::Int(2),
            ScalarValue::Int(3),
        ]);
        let mut buf = Vec::new();
        encode_pv_field(&v, &desc, ByteOrder::Little, &mut buf);
        let mut cur = Cursor::new(buf.as_slice());
        let dec = decode_pv_field(&desc, &mut cur, ByteOrder::Little).unwrap();
        // After F-G10 the decoder lands the typed fast path.
        match dec {
            PvField::ScalarArrayTyped(arr) => {
                assert_eq!(arr.as_ints().unwrap(), &[1, 2, 3]);
            }
            PvField::ScalarArray(items) => {
                assert_eq!(items.len(), 3);
                assert_eq!(items[0], ScalarValue::Int(1));
                assert_eq!(items[2], ScalarValue::Int(3));
            }
            other => panic!("expected ScalarArray or ScalarArrayTyped, got {other:?}"),
        }
    }

    #[test]
    fn union_round_trip() {
        let desc = FieldDesc::Union {
            struct_id: String::new(),
            variants: vec![
                ("intValue".into(), FieldDesc::Scalar(ScalarType::Int)),
                ("doubleValue".into(), FieldDesc::Scalar(ScalarType::Double)),
            ],
        };
        let v = PvField::Union {
            selector: 1,
            variant_name: "doubleValue".into(),
            value: Box::new(PvField::Scalar(ScalarValue::Double(1.5))),
        };
        let mut buf = Vec::new();
        encode_pv_field(&v, &desc, ByteOrder::Little, &mut buf);
        let mut cur = Cursor::new(buf.as_slice());
        let dec = decode_pv_field(&desc, &mut cur, ByteOrder::Little).unwrap();
        match dec {
            PvField::Union {
                selector,
                variant_name,
                value,
            } => {
                assert_eq!(selector, 1);
                assert_eq!(variant_name, "doubleValue");
                assert_eq!(*value, PvField::Scalar(ScalarValue::Double(1.5)));
            }
            other => panic!("expected union, got {other:?}"),
        }
    }

    #[test]
    fn variant_round_trip() {
        let desc = FieldDesc::Variant;
        let v = PvField::Variant(Box::new(VariantValue {
            desc: Some(FieldDesc::Scalar(ScalarType::Int)),
            value: PvField::Scalar(ScalarValue::Int(42)),
        }));
        let mut buf = Vec::new();
        encode_pv_field(&v, &desc, ByteOrder::Little, &mut buf);
        let mut cur = Cursor::new(buf.as_slice());
        let dec = decode_pv_field(&desc, &mut cur, ByteOrder::Little).unwrap();
        match dec {
            PvField::Variant(vv) => {
                assert!(matches!(vv.value, PvField::Scalar(ScalarValue::Int(42))));
            }
            other => panic!("expected variant, got {other:?}"),
        }
    }

    // ── TypeStore (0xFD/0xFE) encode tests ──────────────────────────────

    #[test]
    fn cached_first_emission_starts_with_fd() {
        let desc = nt_scalar_double_desc();
        for order in [ByteOrder::Little, ByteOrder::Big] {
            let mut cache = EncodeTypeCache::new();
            let mut buf = Vec::new();
            encode_type_desc_cached(&desc, order, &mut cache, &mut buf);
            assert_eq!(buf[0], 0xFD, "first emission must define a slot");
            // cache holds outer NTScalar + nested alarm
            assert_eq!(cache.len(), 2);
        }
    }

    #[test]
    fn cached_second_emission_is_three_byte_reference() {
        let desc = nt_scalar_double_desc();
        let order = ByteOrder::Little;
        let mut cache = EncodeTypeCache::new();
        let mut first = Vec::new();
        encode_type_desc_cached(&desc, order, &mut cache, &mut first);

        let mut second = Vec::new();
        encode_type_desc_cached(&desc, order, &mut cache, &mut second);

        // Repeat must collapse to exactly 3 bytes: 0xFE + u16 slot.
        assert_eq!(second.len(), 3);
        assert_eq!(second[0], 0xFE);
        assert!(
            second.len() < first.len() / 4,
            "repeat must shrink dramatically (first={}, second={})",
            first.len(),
            second.len()
        );
    }

    #[test]
    fn cached_round_trip_through_decoder() {
        let desc = nt_scalar_double_desc();
        for order in [ByteOrder::Little, ByteOrder::Big] {
            let mut enc_cache = EncodeTypeCache::new();
            let mut buf = Vec::new();
            // Two consecutive INIT-like emissions on the same connection.
            encode_type_desc_cached(&desc, order, &mut enc_cache, &mut buf);
            encode_type_desc_cached(&desc, order, &mut enc_cache, &mut buf);

            let mut dec_cache = TypeCache::new();
            let mut cur = Cursor::new(buf.as_slice());
            let first = decode_type_desc_cached(&mut cur, order, &mut dec_cache).unwrap();
            let second = decode_type_desc_cached(&mut cur, order, &mut dec_cache).unwrap();
            assert_eq!(format!("{first}"), format!("{desc}"));
            assert_eq!(format!("{second}"), format!("{desc}"));
            assert_eq!(cur.remaining(), 0);
        }
    }

    #[test]
    fn cached_shares_nested_struct_across_outer_types() {
        // Two distinct outer descriptors that share an inner `alarm_t`.
        let alarm = FieldDesc::Structure {
            struct_id: "alarm_t".into(),
            fields: vec![
                ("severity".into(), FieldDesc::Scalar(ScalarType::Int)),
                ("status".into(), FieldDesc::Scalar(ScalarType::Int)),
                ("message".into(), FieldDesc::Scalar(ScalarType::String)),
            ],
        };
        let nt_double = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![
                ("value".into(), FieldDesc::Scalar(ScalarType::Double)),
                ("alarm".into(), alarm.clone()),
            ],
        };
        let nt_int = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![
                ("value".into(), FieldDesc::Scalar(ScalarType::Int)),
                ("alarm".into(), alarm.clone()),
            ],
        };

        let order = ByteOrder::Little;
        let mut enc_cache = EncodeTypeCache::new();
        let mut buf = Vec::new();
        encode_type_desc_cached(&nt_double, order, &mut enc_cache, &mut buf);
        let len_after_first = buf.len();
        encode_type_desc_cached(&nt_int, order, &mut enc_cache, &mut buf);
        let len_after_second = buf.len();
        let second_size = len_after_second - len_after_first;

        // Second NTScalar should be smaller than the first because the
        // shared `alarm_t` body collapses to a 3-byte 0xFE reference.
        assert!(
            second_size < len_after_first,
            "shared inner struct should compress: first={}, second={}",
            len_after_first,
            second_size
        );

        let mut dec_cache = TypeCache::new();
        let mut cur = Cursor::new(buf.as_slice());
        let dec_double = decode_type_desc_cached(&mut cur, order, &mut dec_cache).unwrap();
        let dec_int = decode_type_desc_cached(&mut cur, order, &mut dec_cache).unwrap();
        assert_eq!(format!("{dec_double}"), format!("{nt_double}"));
        assert_eq!(format!("{dec_int}"), format!("{nt_int}"));
    }

    #[test]
    fn cached_does_not_wrap_scalars() {
        // Scalar tags are 1 byte inline; 0xFD/0xFE markers would inflate.
        let order = ByteOrder::Little;
        let mut cache = EncodeTypeCache::new();
        let mut buf = Vec::new();
        encode_type_desc_cached(
            &FieldDesc::Scalar(ScalarType::Double),
            order,
            &mut cache,
            &mut buf,
        );
        assert_eq!(buf, vec![ScalarType::Double.type_code()]);
        assert!(cache.is_empty());
    }

    #[test]
    fn fill_unmarked_carries_prior_for_unmarked_leaves() {
        // Two-leaf NTScalar-like struct: { value: Double, alarm: Int }.
        // Bit layout (depth-first): root=0, value=1, alarm=2.
        let desc = FieldDesc::Structure {
            struct_id: "test:S:1.0".into(),
            fields: vec![
                ("value".into(), FieldDesc::Scalar(ScalarType::Double)),
                ("alarm".into(), FieldDesc::Scalar(ScalarType::Int)),
            ],
        };

        let mut prior_struct = PvStructure::new("test:S:1.0");
        prior_struct
            .fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(1.0))));
        prior_struct
            .fields
            .push(("alarm".into(), PvField::Scalar(ScalarValue::Int(7))));
        let prior = PvField::Structure(prior_struct);

        // "Decoded" delta: value=42.0 marked, alarm absent (default 0).
        let mut decoded_struct = PvStructure::new("test:S:1.0");
        decoded_struct
            .fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(42.0))));
        decoded_struct
            .fields
            .push(("alarm".into(), PvField::Scalar(ScalarValue::Int(0))));
        let decoded = PvField::Structure(decoded_struct);

        let mut bs = crate::proto::BitSet::new();
        bs.set(1); // value bit only

        let merged = fill_unmarked_from_prior(&desc, &bs, 0, decoded, &prior);

        if let PvField::Structure(s) = merged {
            // value: from decoded (marked → fresh)
            assert!(matches!(
                s.get_field("value"),
                Some(PvField::Scalar(ScalarValue::Double(v))) if (*v - 42.0).abs() < 1e-9
            ));
            // alarm: carried over from prior (unmarked → prior wins)
            assert!(matches!(
                s.get_field("alarm"),
                Some(PvField::Scalar(ScalarValue::Int(7)))
            ));
        } else {
            panic!("merged must be Structure");
        }
    }

    #[test]
    fn fill_unmarked_keeps_decoded_when_marked() {
        let desc = FieldDesc::Scalar(ScalarType::Int);
        let prior = PvField::Scalar(ScalarValue::Int(99));
        let decoded = PvField::Scalar(ScalarValue::Int(42));
        let mut bs = crate::proto::BitSet::new();
        bs.set(0); // leaf bit set

        let merged = fill_unmarked_from_prior(&desc, &bs, 0, decoded, &prior);
        assert!(matches!(merged, PvField::Scalar(ScalarValue::Int(42))));
    }

    #[test]
    fn fill_unmarked_carries_prior_when_leaf_unmarked() {
        let desc = FieldDesc::Scalar(ScalarType::Int);
        let prior = PvField::Scalar(ScalarValue::Int(99));
        let decoded = PvField::Scalar(ScalarValue::Int(0));
        let bs = crate::proto::BitSet::new(); // no bits set

        let merged = fill_unmarked_from_prior(&desc, &bs, 0, decoded, &prior);
        assert!(matches!(merged, PvField::Scalar(ScalarValue::Int(99))));
    }
}
