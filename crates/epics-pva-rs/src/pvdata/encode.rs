//! pvData wire codec — encodes/decodes [`FieldDesc`] (introspection) and
//! [`PvField`] (values).
//!
//! Source: pvxs `dataencode.cpp` + `data.cpp`. Designed to be byte-exact with
//! `spvirit_codec::spvd_encode`/`spvd_decode` for the shapes our `FieldDesc`
//! covers (scalars, scalar arrays, structures, structure arrays, unions,
//! union arrays, variants).
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
//! | 0x88   | structure array (followed by 0x80 + body)|
//! | 0x89   | union array     (followed by 0x81 + body)|
//! | 0x8A   | variant array                            |
//! | scalar | tag (above) | 0x08 → scalar array        |

use std::io::Cursor;

use super::TypedScalarArray;
use super::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue, UnionItem, VariantValue};
use crate::decode_err;

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
/// Returns `Ok(Some(_))` for every one of the 12 scalar element types
/// (each maps to a typed `Arc<[T]>` variant) and `Err` on a wire decode
/// error. The `Option` wrapper is retained as an extension point: a
/// caller seeing `Ok(None)` would fall back to the generic
/// `Vec<ScalarValue>` loop in [`decode_pv_field`]. No element type
/// currently exercises that branch — pvData has exactly 12 scalar codes
/// and all are covered here, so the typed fast path is total. The
/// matching *encode*-side generic fallback ([`encode_pv_field_generic`])
/// is the one that handles obscure value/descriptor combinations.
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
    // a hostile peer can declare `n` up to u32::MAX; `get_bytes`
    // would eagerly `vec![0u8; nbytes]` (multi-GB) before `read_exact`
    // can fail. The frame can only ever carry `remaining` bytes, so a
    // declared length larger than that is unsatisfiable — reject it
    // before allocating.
    let remaining = cur.get_ref().len().saturating_sub(cur.position() as usize);
    if nbytes > remaining {
        return Err(decode_err!(
            "scalar array length {n} ({nbytes} bytes) exceeds {remaining} remaining"
        ));
    }
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

/// bound a `Vec::with_capacity` against an attacker-controlled
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
/// Non-cacheable descriptors (scalars, variants) always
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
/// Matches pvxs `dataencode.cpp:71` (`from_wire(... depth) { if(depth>20)
/// fault; }`) — without parity here, a Rust-emitted FieldDesc with depth
/// 21..=64 would round-trip through Rust but be rejected by pvxs as an
/// invalid descriptor, breaking interop with the reference client/server.
/// 20 is also the value the spec doc implies (typical NT* trees nest ≤6).
/// An adversarial peer can no longer craft a deeply-nested Structure
/// tree to blow the per-task stack (tokio default 2 MB / ~256 KB on
/// macOS — well within reach of the previous 64-level cap at ~50K
/// recursion frames).
const MAX_FIELD_DEPTH: u32 = 20;

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

/// Resolve a single type descriptor at the cursor against `cache`,
/// appending its **inline** (marker-free) wire form to `out`.
///
/// This is the rewrite primitive used by the connection reader task to
/// flatten 0xFD/0xFE type-cache markers out of inbound op-response frames
/// before they are routed to per-op tasks. The reader task processes
/// frames strictly in wire order, so a 0xFD define is always observed
/// (and folded into `cache`) before any later 0xFE reference to the same
/// slot. After rewriting, the routed frame carries a self-contained
/// descriptor and per-op tasks can decode it with an empty cache — there
/// is no cross-op decode-order dependency.
///
/// The cursor is advanced past exactly one type descriptor (including any
/// nested markers). A `0xFD` define updates `cache`; a `0xFE` reference
/// reads from it (and errors on a miss).
pub fn rewrite_type_desc_inline(
    cur: &mut Cursor<&[u8]>,
    order: ByteOrder,
    cache: &mut TypeCache,
    out: &mut Vec<u8>,
) -> Result<(), DecodeError> {
    let desc = decode_type_desc_cached_at_depth(cur, order, cache, 0)?;
    encode_type_desc(&desc, order, out);
    Ok(())
}

fn decode_type_desc_cached_at_depth(
    cur: &mut Cursor<&[u8]>,
    order: ByteOrder,
    cache: &mut TypeCache,
    depth: u32,
) -> Result<FieldDesc, DecodeError> {
    if depth > MAX_FIELD_DEPTH {
        return Err(decode_err!(
            "FieldDesc nesting exceeds MAX_FIELD_DEPTH ({MAX_FIELD_DEPTH})"
        ));
    }
    let tag = cur.get_u8()?;
    match tag {
        0xFD => {
            // define new cache slot, then full inline descriptor.
            let key = cur.get_u16(order)?;
            let desc = decode_type_desc_cached_at_depth(cur, order, cache, depth + 1)?;
            // pvxs uses `std::map::emplace` (dataencode.cpp:91-95), which is
            // a no-op when the slot is already bound: a duplicate `0xFD`
            // for an existing slot is consumed and validated (the inline
            // descriptor was decoded above) but does NOT replace the slot,
            // so later `0xFE` references resolve to the FIRST definition.
            // `HashMap::insert` would instead overwrite, diverging from
            // pvxs on the same byte stream. The current field still uses
            // the just-decoded inline `desc` (pvxs binds `descs[index]` to
            // the inline value regardless of the slot's prior contents).
            cache.entry(key).or_insert_with(|| desc.clone());
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
                return Err(decode_err!(
                    "structure-array element tag 0x{inner:02X} (expected 0x80)"
                ));
            }
            decode_structure_body_cached_at_depth(cur, order, true, cache, depth + 1)
        }
        TAG_UNION_ARRAY => {
            let inner = cur.get_u8()?;
            if inner != TAG_UNION {
                return Err(decode_err!(
                    "union-array element tag 0x{inner:02X} (expected 0x81)"
                ));
            }
            decode_union_body_cached_at_depth(cur, order, true, cache, depth + 1)
        }
        TAG_VARIANT => Ok(FieldDesc::Variant),
        TAG_VARIANT_ARRAY => Ok(FieldDesc::VariantArray),
        // pvxs faults on a bounded/fixed descriptor (`dataencode.cpp:120-123`
        // for the fixed-size bit, `:186-206` leaves bounded unhandled in the
        // default switch). The 0x83 tag is never emitted by a pvxs peer, and
        // there is no `FieldDesc` variant that produces it; reject it as an
        // invalid type code (also catches the legacy Rust-only 0x83 tag that
        // earlier builds emitted before the bounded-string descriptor was
        // dropped) rather than fall through to the scalar lookup.
        TAG_BOUNDED_STRING => Err(decode_err!(
            "bounded string descriptor (0x{TAG_BOUNDED_STRING:02X}) is not a pvAccess type code"
        )),
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
        .ok_or_else(|| decode_err!("structure field count cannot be null"))? as usize;
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
        .ok_or_else(|| decode_err!("union variant count cannot be null"))? as usize;
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
        (FieldDesc::Scalar(st), PvField::Scalar(sv)) => {
            // The descriptor's scalar type owns the wire width: when the
            // value variant matches, encode directly; otherwise coerce
            // so a stray Int-against-Double never desyncs the stream.
            if sv.scalar_type() == *st {
                encode_scalar_value(sv, order, out);
            } else {
                encode_scalar_value(&coerce_scalar(sv, *st), order, out);
            }
        }
        (FieldDesc::ScalarArray(st), PvField::ScalarArrayTyped(arr))
            if arr.scalar_type() == *st =>
        {
            // Fast path: typed array → bulk memcpy when host
            // endian == wire endian, per-element byte-swap loop
            // otherwise (still O(n) but no enum match per element).
            // pvxs `to_wire(shared_array)` parity (pvaproto.h:477).
            // Element type must equal the descriptor's; a mismatch
            // routes through the generic fallback for coercion.
            encode_size_into(arr.len() as u32, order, out);
            encode_typed_scalar_array(arr, order, out);
        }
        (FieldDesc::ScalarArray(st), PvField::ScalarArray(items))
            if items.iter().all(|v| v.scalar_type() == *st) =>
        {
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
        (FieldDesc::StructureArray { struct_id, fields }, PvField::StructureArray(items)) => {
            encode_size_into(items.len() as u32, order, out);
            // each element is preceded by a presence byte —
            // 0x00 for a null (absent) element, 0x01 followed by the
            // body for a present one (pvxs dataencode.cpp:354-365).
            // The element descriptor is the ARRAY's element descriptor:
            // its `struct_id` comes from the descriptor, not the value.
            // pvxs encodes each present element under the array element
            // descriptor it asserted the value against, and the element
            // body carries no id of its own — synthesizing the descriptor
            // from the value's `struct_id` only masked a mismatch that
            // `value_matches_descriptor` now rejects on checked paths.
            let element_desc = FieldDesc::Structure {
                struct_id: struct_id.clone(),
                fields: fields.clone(),
            };
            for s in items {
                match s {
                    None => out.put_u8(0x00),
                    Some(s) => {
                        out.put_u8(0x01);
                        encode_pv_field(&PvField::Structure(s.clone()), &element_desc, order, out);
                    }
                }
            }
        }
        (
            FieldDesc::Union { variants, .. },
            PvField::Union {
                selector, value, ..
            },
        ) => {
            // Union: selector (Size) followed by the chosen variant's value.
            // -1 → null marker (0xFF). An out-of-range selector would emit
            // a size with no value bytes and desync the frame — clamp it
            // to the null marker instead.
            match usize::try_from(*selector)
                .ok()
                .and_then(|idx| variants.get(idx).map(|v| (idx, v)))
            {
                Some((idx, (_, vdesc))) => {
                    encode_size_into(idx as u32, order, out);
                    encode_pv_field(value, vdesc, order, out);
                }
                None => out.put_u8(0xFF),
            }
        }
        (FieldDesc::UnionArray { variants, .. }, PvField::UnionArray(items)) => {
            encode_size_into(items.len() as u32, order, out);
            // pvxs `dataencode.cpp:368-379` derives the per-element
            // presence byte from `if(!elem)`: a UnionA element Value is
            // truthy only once `Value::Helper::build` runs for a *valid*
            // selector, so pvxs emits `0x01` only for an element that
            // selects a real variant and `0x00` otherwise. A union element
            // that selects no valid variant (absent, negative, or
            // out-of-range selector) is therefore the absent (`0x00`)
            // case — pvxs's own UnionA decoder collapses a present null
            // selector to absent (`dataencode.cpp:635-637`), so emitting it
            // as `0x01`-present would not survive a round trip. Collapse it
            // here so the wire form has a single meaning per element.
            for it in items {
                match it.as_ref().and_then(|it| {
                    usize::try_from(it.selector)
                        .ok()
                        .and_then(|idx| variants.get(idx).map(|(_, vdesc)| (idx, &it.value, vdesc)))
                }) {
                    Some((idx, value, vdesc)) => {
                        out.put_u8(0x01);
                        encode_size_into(idx as u32, order, out);
                        encode_pv_field(value, vdesc, order, out);
                    }
                    None => out.put_u8(0x00),
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
            // pvxs `dataencode.cpp:382-394` writes `0x01` only for a truthy
            // AnyA element (one with a stored descriptor) and `0x00`
            // otherwise. A present element with no descriptor is the absent
            // (`0x00`) case — pvxs's AnyA decoder collapses a present null
            // descriptor to absent (`dataencode.cpp:669-675`), so a
            // present-but-empty "any" would not survive a round trip.
            // Collapse it here so the wire form has a single meaning per
            // element.
            for it in items {
                match it
                    .as_ref()
                    .and_then(|it| it.desc.as_ref().map(|d| (d, &it.value)))
                {
                    Some((d, value)) => {
                        out.put_u8(0x01);
                        encode_type_desc(d, order, out);
                        encode_pv_field(value, d, order, out);
                    }
                    None => out.put_u8(0x00),
                }
            }
        }
        // ── Generic fallback ────────────────────────────────────
        //
        // The typed arms above cover the case where the value variant
        // structurally matches its descriptor. pvData itself is purely
        // descriptor-driven (pvxs `to_wire_field` keys solely on the
        // FieldDesc TypeCode), so the *descriptor* must always determine
        // the wire shape — never the runtime value variant. The cases
        // below reach the same descriptor-driven shape for value/desc
        // pairs the typed arms miss, instead of emitting zero bytes and
        // desyncing the stream.
        (desc, value) => encode_pv_field_generic(value, desc, order, out),
    }
}

/// Descriptor-driven encode for value/descriptor pairs the typed arms of
/// [`encode_pv_field`] do not match. Every shape pvxs `to_wire_field`
/// can emit is reachable here; the function never emits zero bytes for a
/// non-empty descriptor, so the wire stream cannot desync.
///
/// Handled "obscure" combinations:
///
/// - `PvField::Null` against any descriptor — encodes the descriptor's
///   canonical null/empty form (`0xFF` selector for Union/Variant, a
///   zero Size for the array kinds, a default scalar for Scalar).
/// - A `Variant` / `VariantArray` descriptor carrying a value *not*
///   wrapped in `PvField::Variant` — the value's own runtime descriptor
///   is inferred via [`PvField::descriptor`] and emitted inline, exactly
///   as pvxs does from the value's stored FieldDesc.
/// - A `Union` descriptor carrying a value that is not `PvField::Union`
///   — the value is matched against the first variant whose descriptor
///   it fits; if none fits the union is encoded as null.
/// - Mismatched scalar / scalar-array variants — re-encoded under the
///   descriptor's scalar type so the wire width is always correct.
fn encode_pv_field_generic(value: &PvField, desc: &FieldDesc, order: ByteOrder, out: &mut Vec<u8>) {
    match desc {
        FieldDesc::Scalar(st) => {
            // Coerce whatever scalar the value carries into the
            // descriptor's type so the wire width matches the tag.
            let sv = match value {
                PvField::Scalar(v) => coerce_scalar(v, *st),
                _ => zero_scalar(*st),
            };
            encode_scalar_value(&sv, order, out);
        }
        FieldDesc::ScalarArray(st) => {
            // Re-encode the elements coerced to the descriptor's type.
            match value {
                PvField::ScalarArray(items) => {
                    encode_size_into(items.len() as u32, order, out);
                    for v in items {
                        encode_scalar_value(&coerce_scalar(v, *st), order, out);
                    }
                }
                PvField::ScalarArrayTyped(arr) => {
                    // Element type differs from the descriptor: expand to
                    // ScalarValues and coerce each to the descriptor type.
                    let items = arr.to_scalar_values();
                    encode_size_into(items.len() as u32, order, out);
                    for v in &items {
                        encode_scalar_value(&coerce_scalar(v, *st), order, out);
                    }
                }
                _ => encode_size_into(0, order, out),
            }
        }
        FieldDesc::Structure { fields, .. } => {
            // Value is not a PvField::Structure — emit defaults so the
            // field count on the wire stays correct.
            for (_, child_desc) in fields {
                encode_pv_field(&default_value_for(child_desc), child_desc, order, out);
            }
        }
        FieldDesc::StructureArray { .. } => {
            // Non-StructureArray value (e.g. Null) → empty array.
            encode_size_into(0, order, out);
        }
        FieldDesc::Union { variants, .. } => {
            // pvData Union selector wire form: 0xFF == null, otherwise a
            // Size index followed by the chosen variant's value.
            match value {
                PvField::Null => out.put_u8(0xFF),
                // A raw value handed against a Union descriptor: pick the
                // first variant whose descriptor the value satisfies.
                other => {
                    let chosen = variants
                        .iter()
                        .position(|(_, vd)| value_fits_desc(other, vd));
                    match chosen {
                        Some(idx) => {
                            encode_size_into(idx as u32, order, out);
                            encode_pv_field(other, &variants[idx].1, order, out);
                        }
                        None => out.put_u8(0xFF),
                    }
                }
            }
        }
        FieldDesc::UnionArray { .. } => {
            // Non-UnionArray value → empty array.
            encode_size_into(0, order, out);
        }
        FieldDesc::Variant => {
            // "any": embed the value's own descriptor then the value.
            // pvxs `to_wire_field` emits `to_wire(desc(fld))` then
            // `to_wire_full(fld)`; a bare value carries no stored
            // descriptor so we recover one from the value.
            //
            // only emit a descriptor that is *wire-faithful*.
            // A bare descriptor-sensitive value (empty/untyped array,
            // union, union array, null) cannot be faithfully described
            // from the value alone, and emitting a degraded descriptor
            // would advertise a wrong schema the peer can't decode. For
            // those, emit a null `any` instead. To carry such a value
            // through `any`, wrap it in
            // `PvField::Variant(VariantValue { desc: Some(canonical), .. })`
            // (handled faithfully by the typed Variant arm), so the
            // canonical descriptor is used.
            match value {
                PvField::Null => out.put_u8(0xFF),
                other => match other.wire_descriptor() {
                    Some(desc) => {
                        encode_type_desc(&desc, order, out);
                        encode_pv_field(other, &desc, order, out);
                    }
                    None => out.put_u8(0xFF),
                },
            }
        }
        FieldDesc::VariantArray => {
            // Non-VariantArray value → empty array.
            encode_size_into(0, order, out);
        }
    }
}

/// Coerce a [`ScalarValue`] to the wire scalar type `st`. Numeric kinds
/// convert by `as`-cast; anything ↔ String routes through the textual
/// form. Used by the generic encode fallback so the wire width always
/// matches the descriptor's tag.
fn coerce_scalar(v: &ScalarValue, st: ScalarType) -> ScalarValue {
    if v.scalar_type() == st {
        return v.clone();
    }
    if st == ScalarType::String {
        return ScalarValue::String(scalar_to_string(v));
    }
    if let ScalarValue::String(s) = v {
        // String → numeric: parse, falling back to the zero value.
        return ScalarValue::parse(st, s).unwrap_or_else(|_| zero_scalar(st));
    }
    // Numeric → numeric: route through f64 / i64 to preserve magnitude.
    let as_f64 = scalar_as_f64(v);
    match st {
        ScalarType::Boolean => ScalarValue::Boolean(as_f64 != 0.0),
        ScalarType::Byte => ScalarValue::Byte(as_f64 as i8),
        ScalarType::UByte => ScalarValue::UByte(as_f64 as u8),
        ScalarType::Short => ScalarValue::Short(as_f64 as i16),
        ScalarType::UShort => ScalarValue::UShort(as_f64 as u16),
        ScalarType::Int => ScalarValue::Int(as_f64 as i32),
        ScalarType::UInt => ScalarValue::UInt(as_f64 as u32),
        ScalarType::Long => ScalarValue::Long(as_f64 as i64),
        ScalarType::ULong => ScalarValue::ULong(as_f64 as u64),
        ScalarType::Float => ScalarValue::Float(as_f64 as f32),
        ScalarType::Double => ScalarValue::Double(as_f64),
        ScalarType::String => ScalarValue::String(scalar_to_string(v)),
    }
}

/// Numeric magnitude of a scalar as `f64` (booleans → 0.0/1.0,
/// strings → parsed or 0.0). Used only by [`coerce_scalar`].
fn scalar_as_f64(v: &ScalarValue) -> f64 {
    match v {
        ScalarValue::Boolean(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        ScalarValue::Byte(x) => *x as f64,
        ScalarValue::UByte(x) => *x as f64,
        ScalarValue::Short(x) => *x as f64,
        ScalarValue::UShort(x) => *x as f64,
        ScalarValue::Int(x) => *x as f64,
        ScalarValue::UInt(x) => *x as f64,
        ScalarValue::Long(x) => *x as f64,
        ScalarValue::ULong(x) => *x as f64,
        ScalarValue::Float(x) => *x as f64,
        ScalarValue::Double(x) => *x,
        ScalarValue::String(s) => s.trim().parse().unwrap_or(0.0),
    }
}

/// Textual form of a scalar — its `Display` impl.
fn scalar_to_string(v: &ScalarValue) -> String {
    format!("{v}")
}

/// Whether a runtime [`PvField`] value can be encoded under descriptor
/// `desc` without coercion-induced data loss. Used by the generic Union
/// fallback to pick a variant slot for a bare value.
fn value_fits_desc(value: &PvField, desc: &FieldDesc) -> bool {
    match (desc, value) {
        (FieldDesc::Scalar(st), PvField::Scalar(v)) => v.scalar_type() == *st,
        (FieldDesc::ScalarArray(st), PvField::ScalarArray(items)) => {
            items.iter().all(|v| v.scalar_type() == *st)
        }
        (FieldDesc::ScalarArray(st), PvField::ScalarArrayTyped(arr)) => arr.scalar_type() == *st,
        (FieldDesc::Structure { .. }, PvField::Structure(_)) => true,
        (FieldDesc::StructureArray { .. }, PvField::StructureArray(_)) => true,
        (FieldDesc::Union { .. }, PvField::Union { .. }) => true,
        (FieldDesc::UnionArray { .. }, PvField::UnionArray(_)) => true,
        (FieldDesc::Variant, PvField::Variant(_)) => true,
        (FieldDesc::VariantArray, PvField::VariantArray(_)) => true,
        _ => false,
    }
}

/// Canonicalize a *selection* bitset (e.g. the output of
/// [`crate::pv_request::request_to_mask`]) into a valid wire
/// *changed*-bitset.
///
/// A selection mask freely sets a structure bit alongside a *partial*
/// set of its descendants — that is correct for request/intersection
/// logic but wrong as a wire changed-bitset, where (pvData §5.4 / pvxs
/// `BitSet`) a set structure bit means "the whole subtree changed".
/// Feeding such a mask straight to [`encode_pv_field_with_bitset`]
/// would emit the entire structure and defeat the field filter.
///
/// This walks `desc` and, for every structure node, sets the node's
/// bit **iff every descendant bit is set** in `selection`; otherwise it
/// clears the structure bit and keeps the descendant bits. Leaf bits
/// are copied verbatim. The result encodes and decodes symmetrically
/// under the §5.4 semantics enforced by `*_with_bitset`.
pub fn canonical_changed_bitset(
    desc: &FieldDesc,
    selection: &crate::proto::BitSet,
) -> crate::proto::BitSet {
    fn all_descendants_set(sel: &crate::proto::BitSet, pos: usize, desc_local: &FieldDesc) -> bool {
        let total = desc_local.total_bits();
        (0..total).all(|i| sel.get(pos + i))
    }

    fn walk(
        desc: &FieldDesc,
        bit_offset: usize,
        selection: &crate::proto::BitSet,
        out: &mut crate::proto::BitSet,
    ) {
        match desc {
            FieldDesc::Structure { fields, .. } => {
                if all_descendants_set(selection, bit_offset, desc) {
                    // Whole subtree selected → a single structure bit
                    // legitimately conveys it; descendant bits are
                    // redundant but harmless, so set them too for an
                    // unambiguous "all present" mask.
                    let total = desc.total_bits();
                    for i in 0..total {
                        out.set(bit_offset + i);
                    }
                } else {
                    // Partial → leave the structure bit clear; recurse.
                    let mut child_bit = bit_offset + 1;
                    for (_, child) in fields {
                        walk(child, child_bit, selection, out);
                        child_bit += child.total_bits();
                    }
                }
            }
            _ => {
                if selection.get(bit_offset) {
                    out.set(bit_offset);
                }
            }
        }
    }

    let mut out = crate::proto::BitSet::new();
    walk(desc, 0, selection, &mut out);
    out
}

/// structural diff of two `PvField` snapshots against their
/// shared `desc`, producing the per-event **changed bitset** — the
/// leaves whose value differs between `prev` and `curr`.
///
/// pvxs posts a monitor `Value` whose marked bitset reflects exactly
/// the leaves a source touched since the last `unmark()`
/// (`servermon.cpp:174` then intersects with the request mask). A
/// QSRV group monitor with the default self-trigger re-reads only the
/// triggered member, so consecutive group snapshots differ in only
/// that member's leaves — diffing them reproduces pvxs's marked-leaf
/// set without the source having to track marks itself.
///
/// Bit numbering matches the rest of this module (pvData spec §5.4):
/// root structure is bit 0, fields numbered depth-first in
/// declaration order. Only **leaf** bits are set; structure bits stay
/// clear so [`canonical_changed_bitset`] / the encoder treat the
/// result as a partial update.
///
/// A leaf present in `desc` but missing from either snapshot is
/// treated as changed (defensive: a shape mismatch must not silently
/// drop the field from the wire delta).
pub fn diff_changed_bitset(
    desc: &FieldDesc,
    prev: &PvField,
    curr: &PvField,
) -> crate::proto::BitSet {
    fn walk(
        desc: &FieldDesc,
        bit_offset: usize,
        prev: Option<&PvField>,
        curr: Option<&PvField>,
        out: &mut crate::proto::BitSet,
    ) {
        match desc {
            FieldDesc::Structure { fields, .. } => {
                let mut child_bit = bit_offset + 1;
                for (name, child_desc) in fields {
                    let prev_child = match prev {
                        Some(PvField::Structure(s)) => s.get_field(name),
                        _ => None,
                    };
                    let curr_child = match curr {
                        Some(PvField::Structure(s)) => s.get_field(name),
                        _ => None,
                    };
                    walk(child_desc, child_bit, prev_child, curr_child, out);
                    child_bit += child_desc.total_bits();
                }
            }
            _ => {
                // Leaf node: mark changed when the values differ, or
                // when either snapshot lacks the leaf entirely.
                let changed = match (prev, curr) {
                    (Some(p), Some(c)) => p != c,
                    _ => true,
                };
                if changed {
                    out.set(bit_offset);
                }
            }
        }
    }

    let mut out = crate::proto::BitSet::new();
    walk(desc, 0, Some(prev), Some(curr), &mut out);
    out
}

/// build a wire *selection* bitset that marks every bit
/// under each field path in `marked_paths`.
///
/// Paths are dot-separated and relative to the root structure — for a
/// QSRV group monitor they are the triggered target members'
/// `field_name` paths. Marking a node sets its entire subtree, which
/// reproduces pvxs's *assigned-not-changed* semantics
/// (`groupsource.cpp:288` marks each `+trigger` target whether or not
/// its value changed since the last post), unlike
/// [`diff_changed_bitset`] which only marks leaves that differ.
///
/// The result is a *selection* mask in the same sense as
/// [`request_to_mask`](crate::pv_request::request_to_mask): the caller
/// intersects it with the request mask and runs it through
/// [`canonical_changed_bitset`] before encoding, exactly as the diff
/// path does.
///
/// Bit numbering matches the rest of this module (pvData §5.4): root is
/// bit 0, fields depth-first in declaration order.
pub fn marked_changed_bitset(desc: &FieldDesc, marked_paths: &[String]) -> crate::proto::BitSet {
    fn set_subtree(out: &mut crate::proto::BitSet, bit_offset: usize, desc: &FieldDesc) {
        let total = desc.total_bits();
        for i in 0..total {
            out.set(bit_offset + i);
        }
    }
    fn walk(
        desc: &FieldDesc,
        bit_offset: usize,
        prefix: &str,
        marked: &[String],
        out: &mut crate::proto::BitSet,
    ) {
        // A node whose full path is explicitly marked contributes its
        // whole subtree; no need to descend further.
        if !prefix.is_empty() && marked.iter().any(|m| m == prefix) {
            set_subtree(out, bit_offset, desc);
            return;
        }
        if let FieldDesc::Structure { fields, .. } = desc {
            let mut child_bit = bit_offset + 1;
            for (name, child) in fields {
                let child_path = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}.{name}")
                };
                walk(child, child_bit, &child_path, marked, out);
                child_bit += child.total_bits();
            }
        }
    }
    let mut out = crate::proto::BitSet::new();
    walk(desc, 0, "", marked_paths, &mut out);
    out
}

/// Encode the value bytes for `value` consulting `bitset` to know which
/// fields to emit. Mirrors pvxs `to_wire_valid(buf, value)`.
///
/// pvData spec §5.4 bit numbering: the root structure is bit 0, then
/// nested fields are numbered depth-first in declaration order.
///
/// A COMPOUND (structure) node whose OWN bit is set means the *entire*
/// subtree changed — pvxs `BitSet` compresses "all descendants set" into
/// the parent bit, so when the own bit is set we emit every descendant
/// leaf regardless of whether its individual bit is set. When only some
/// descendant bits are set we recurse per-child. A subtree with neither
/// the own bit nor any descendant bit set produces *no bytes*.
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
        | FieldDesc::Union { .. }
        | FieldDesc::UnionArray { .. }
        | FieldDesc::StructureArray { .. } => {
            if bitset.get(bit_offset) {
                encode_pv_field(value, desc, order, out);
            }
            // else: emit no bytes
        }
        FieldDesc::Structure { fields, .. } => {
            // Own bit set → the whole subtree changed; emit every
            // descendant leaf unconditionally (pvxs BitSet compression).
            if bitset.get(bit_offset) {
                encode_pv_field(value, desc, order, out);
                return;
            }
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
            // Own bit set → the peer emitted the entire subtree (pvxs
            // BitSet compresses "all descendants set" into the parent
            // bit). Consume the whole subtree so the cursor stays in
            // sync, regardless of descendant bits.
            if bitset.get(bit_offset) {
                return decode_pv_field(desc, cur, order);
            }
            // The struct is "present" only if some descendant bit is
            // set. If none, return a default-filled structure.
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

/// Like [`decode_pv_field_with_bitset`] but threads `cache` through leaf
/// decodes so that 0xFD/0xFE markers inside Variant/VariantArray payloads
/// resolve against the connection-scope cache.
pub fn decode_pv_field_with_bitset_cached(
    desc: &FieldDesc,
    bitset: &crate::proto::BitSet,
    bit_offset: usize,
    cur: &mut Cursor<&[u8]>,
    order: ByteOrder,
    cache: &mut TypeCache,
) -> Result<PvField, DecodeError> {
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
        | FieldDesc::Union { .. }
        | FieldDesc::UnionArray { .. }
        | FieldDesc::StructureArray { .. } => {
            if bitset.get(bit_offset) {
                decode_pv_field_cached(desc, cur, order, cache)
            } else {
                Ok(default_value_for(desc))
            }
        }
        FieldDesc::Structure { struct_id, fields } => {
            if bitset.get(bit_offset) {
                return decode_pv_field_cached(desc, cur, order, cache);
            }
            if !any_descendant_set(bitset, bit_offset, desc) {
                return Ok(default_value_for(desc));
            }
            let mut s = PvStructure::new(struct_id);
            let mut child_bit = bit_offset + 1;
            for (name, child) in fields {
                let v = decode_pv_field_with_bitset_cached(
                    child, bitset, child_bit, cur, order, cache,
                )?;
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
        FieldDesc::Structure { .. } if bitset.get(bit_offset) => {
            // Own bit set → the decoder emitted the whole subtree fresh
            // (pvxs BitSet compression). Every descendant leaf is
            // freshly updated; keep the decoded value verbatim.
            decoded
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
    let mut empty = TypeCache::new();
    decode_pv_field_at_depth(desc, cur, order, &mut empty, 0)
}

/// Like [`decode_pv_field`] but threads `cache` through Variant and
/// VariantArray decode so that 0xFD/0xFE type-cache markers inside
/// Variant payloads resolve against the connection-scope cache.
/// Mirrors pvxs `dataencode.cpp::from_wire_full(buf, ctxt, fld)`.
pub fn decode_pv_field_cached(
    desc: &FieldDesc,
    cur: &mut Cursor<&[u8]>,
    order: ByteOrder,
    cache: &mut TypeCache,
) -> Result<PvField, DecodeError> {
    decode_pv_field_at_depth(desc, cur, order, cache, 0)
}

fn decode_pv_field_at_depth(
    desc: &FieldDesc,
    cur: &mut Cursor<&[u8]>,
    order: ByteOrder,
    cache: &mut TypeCache,
    depth: u32,
) -> Result<PvField, DecodeError> {
    if depth > MAX_FIELD_DEPTH {
        return Err(decode_err!(
            "PvField nesting exceeds MAX_FIELD_DEPTH ({MAX_FIELD_DEPTH})"
        ));
    }
    Ok(match desc {
        FieldDesc::Scalar(st) => PvField::Scalar(decode_scalar_value(*st, cur, order)?),
        FieldDesc::ScalarArray(st) => {
            let n = decode_size(cur, order)?
                .ok_or_else(|| decode_err!("scalar array length cannot be null"))?
                as usize;
            // Fast path: decode straight into a typed Arc<[T]>
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
                let v = decode_pv_field_at_depth(child_desc, cur, order, cache, depth + 1)?;
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
                    // 0x00 is a null (absent) element — push
                    // `None`, not an empty struct, so the absent-vs-
                    // present-empty distinction survives the round trip.
                    0x00 => {
                        out.push(None);
                    }
                    0x01 => {
                        let element_desc = FieldDesc::Structure {
                            struct_id: struct_id.clone(),
                            fields: fields.clone(),
                        };
                        if let PvField::Structure(s) =
                            decode_pv_field_at_depth(&element_desc, cur, order, cache, depth + 1)?
                        {
                            out.push(Some(s));
                        }
                    }
                    other => {
                        return Err(decode_err!(
                            "structure array element presence byte 0x{other:02X} (expected 0x00 or 0x01)"
                        ));
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
                    let value = decode_pv_field_at_depth(&vdesc, cur, order, cache, depth + 1)?;
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
                // pvxs `dataencode.cpp:624-650` reads a per-
                // element presence byte before the union body. 0x00
                // = null element; 0x01 = present (then selector +
                // value). Pre-fix Rust read the selector directly.
                let presence = cur.get_u8()?;
                match presence {
                    // 0x00 = null (absent) element → `None`.
                    0x00 => items.push(None),
                    // 0x01 = present. pvxs `dataencode.cpp:635-637` treats a
                    // present byte followed by a null selector "the same as
                    // 0 case" — the element stays the default empty Value,
                    // i.e. absent. Collapse it to `None` rather than
                    // retaining a distinct present null-selector union,
                    // which pvxs never preserves across a round trip.
                    0x01 => match decode_size(cur, order)? {
                        None => items.push(None),
                        Some(sel_u32) => {
                            let sel = sel_u32 as i32;
                            let (variant_name, vdesc) = variants
                                .get(sel as usize)
                                .ok_or_else(|| {
                                    decode_err!("union array selector {sel} out of range")
                                })?
                                .clone();
                            let value =
                                decode_pv_field_at_depth(&vdesc, cur, order, cache, depth + 1)?;
                            items.push(Some(UnionItem {
                                selector: sel,
                                variant_name,
                                value,
                            }));
                        }
                    },
                    other => {
                        return Err(decode_err!(
                            "union array element presence byte 0x{other:02X} (expected 0x00 or 0x01)"
                        ));
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
                // Rust stack frame. Cap the recursion via the depth
                // counter so an adversary can't blow the per-task
                // stack with chained Variant tags.
                let inner = decode_type_desc_cached(cur, order, cache)?;
                let value = decode_pv_field_at_depth(&inner, cur, order, cache, depth + 1)?;
                PvField::Variant(Box::new(VariantValue {
                    desc: Some(inner),
                    value,
                }))
            }
        }
        FieldDesc::VariantArray => {
            let n = decode_size(cur, order)?.ok_or_else(|| decode_err!("variant array length"))?
                as usize;
            let mut items = Vec::with_capacity(safe_capacity(n, cur));
            for _ in 0..n {
                // pvxs `dataencode.cpp:656-674` reads a per-
                // element presence byte before the inner descriptor.
                // 0x00 = null element; 0x01 = present (then descriptor
                // + value). Pre-fix Rust read the descriptor tag
                // directly with `0xFF` repurposed as "null", which a
                // pvxs peer reads as the presence byte (≠ 0/1 →
                // protocol fault).
                let presence = cur.get_u8()?;
                match presence {
                    // 0x00 = null (absent) element → `None`.
                    0x00 => items.push(None),
                    0x01 => {
                        // Present element. pvxs `dataencode.cpp:669-675`
                        // reads the inner descriptor after the present byte;
                        // if it is the null type tag (`0xFF`, an empty
                        // descriptor vector) the element stays the default
                        // empty Value, i.e. absent. Collapse it to `None`
                        // rather than retaining a distinct present-but-empty
                        // "any", which pvxs never preserves across a round
                        // trip.
                        let peek = cur.get_u8()?;
                        if peek == 0xFF {
                            items.push(None);
                            continue;
                        }
                        let pos = cur.position();
                        cur.set_position(pos - 1);
                        let inner = decode_type_desc_cached(cur, order, cache)?;
                        let value = decode_pv_field_at_depth(&inner, cur, order, cache, depth + 1)?;
                        items.push(Some(VariantValue {
                            desc: Some(inner),
                            value,
                        }));
                    }
                    other => {
                        return Err(decode_err!(
                            "variant array element presence byte 0x{other:02X} (expected 0x00 or 0x01)"
                        ));
                    }
                }
            }
            PvField::VariantArray(items)
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
    use crate::pvdata::{UnionItem, VariantValue};

    /// a bare descriptor-sensitive value encoded against an
    /// `any` (FieldDesc::Variant) must NOT advertise a degraded schema;
    /// the encoder emits a null `any` (0xFF) instead.
    #[test]
    fn any_bare_lossy_value_emits_null_not_degraded_schema() {
        for lossy in [
            PvField::StructureArray(Vec::new()),
            PvField::UnionArray(Vec::new()),
            PvField::UnionArray(vec![Some(UnionItem {
                selector: 0,
                variant_name: "i".into(),
                value: PvField::Scalar(ScalarValue::Int(7)),
            })]),
            PvField::ScalarArray(Vec::new()),
        ] {
            let mut out = Vec::new();
            encode_pv_field(&lossy, &FieldDesc::Variant, ByteOrder::Big, &mut out);
            assert_eq!(
                out,
                vec![0xFF],
                "bare lossy value through `any` must emit null, got {out:?}"
            );
        }
    }

    /// A bare faithful value (scalar) through `any` still encodes its
    /// inferred descriptor + value.
    #[test]
    fn any_bare_faithful_value_encodes_descriptor() {
        let mut out = Vec::new();
        encode_pv_field(
            &PvField::Scalar(ScalarValue::Int(0x01020304)),
            &FieldDesc::Variant,
            ByteOrder::Big,
            &mut out,
        );
        // type tag 0x22 (Int) + 4 BE value bytes — not a null `any`.
        assert_eq!(out, vec![0x22, 0x01, 0x02, 0x03, 0x04]);
    }

    /// The faithful way to carry a descriptor-sensitive value through
    /// `any`: wrap it in Variant with an explicit descriptor. That path
    /// uses the canonical descriptor (not lossy inference).
    #[test]
    fn any_explicit_variant_desc_is_faithful() {
        // UnionArray descriptor carried explicitly.
        let union_desc = FieldDesc::UnionArray {
            struct_id: String::new(),
            variants: vec![
                ("i".into(), FieldDesc::Scalar(ScalarType::Int)),
                ("f".into(), FieldDesc::Scalar(ScalarType::Double)),
            ],
        };
        let value = PvField::Variant(Box::new(VariantValue {
            desc: Some(union_desc.clone()),
            value: PvField::UnionArray(vec![Some(UnionItem {
                selector: 0,
                variant_name: "i".into(),
                value: PvField::Scalar(ScalarValue::Int(7)),
            })]),
        }));
        let mut out = Vec::new();
        encode_pv_field(&value, &FieldDesc::Variant, ByteOrder::Big, &mut out);
        // Must NOT be a null `any`; the canonical union-array descriptor
        // (tag bytes) leads the output.
        assert_ne!(out, vec![0xFF], "explicit-desc `any` must be faithful");
        let mut desc_bytes = Vec::new();
        encode_type_desc(&union_desc, ByteOrder::Big, &mut desc_bytes);
        assert!(
            out.starts_with(&desc_bytes),
            "explicit `any` must lead with the canonical descriptor"
        );
    }

    /// A present UnionArray/VariantArray element that selects no real
    /// variant/descriptor is not a distinct wire state: pvxs collapses it to
    /// absent on decode (`dataencode.cpp:635-637`, `:669-675`), so the
    /// encoder must emit the same bytes as an absent (`None`) element and a
    /// hand-crafted `0x01`+null wire form must round-trip to `None`.
    #[test]
    fn union_variant_array_present_null_collapses_to_absent() {
        let union_desc = FieldDesc::UnionArray {
            struct_id: String::new(),
            variants: vec![("i".into(), FieldDesc::Scalar(ScalarType::Int))],
        };
        let any_desc = FieldDesc::VariantArray;

        for order in [ByteOrder::Big, ByteOrder::Little] {
            // (a) encode: a present null-selector union element produces the
            //     same bytes as an absent (`None`) element.
            let present_null_union = PvField::UnionArray(vec![Some(UnionItem {
                selector: -1,
                variant_name: String::new(),
                value: PvField::Null,
            })]);
            let absent_union = PvField::UnionArray(vec![None]);
            let mut a = Vec::new();
            let mut b = Vec::new();
            encode_pv_field(&present_null_union, &union_desc, order, &mut a);
            encode_pv_field(&absent_union, &union_desc, order, &mut b);
            assert_eq!(
                a, b,
                "present null-selector UnionA element must encode as absent ({order:?})"
            );
            // length 1 (Size) + one presence byte 0x00
            assert_eq!(a.last(), Some(&0x00), "absent presence byte ({order:?})");

            // (b) encode: a present descriptor-less "any" element produces
            //     the same bytes as an absent element.
            let present_null_any = PvField::VariantArray(vec![Some(VariantValue {
                desc: None,
                value: PvField::Null,
            })]);
            let absent_any = PvField::VariantArray(vec![None]);
            let mut c = Vec::new();
            let mut d = Vec::new();
            encode_pv_field(&present_null_any, &any_desc, order, &mut c);
            encode_pv_field(&absent_any, &any_desc, order, &mut d);
            assert_eq!(
                c, d,
                "present null-descriptor AnyA element must encode as absent ({order:?})"
            );
            assert_eq!(c.last(), Some(&0x00), "absent presence byte ({order:?})");

            // (c) decode: a hand-crafted `0x01`+null-selector UnionA element
            //     and `0x01`+0xFF AnyA element both decode to `None`.
            let union_wire = vec![0x01, 0x01, 0xFF];
            let mut cur = Cursor::new(union_wire.as_slice());
            match decode_pv_field(&union_desc, &mut cur, order).unwrap() {
                PvField::UnionArray(items) => {
                    assert_eq!(items, vec![None], "0x01+null selector → None ({order:?})");
                }
                other => panic!("expected UnionArray, got {other:?}"),
            }

            let any_wire = vec![0x01, 0x01, 0xFF];
            let mut cur = Cursor::new(any_wire.as_slice());
            match decode_pv_field(&any_desc, &mut cur, order).unwrap() {
                PvField::VariantArray(items) => {
                    assert_eq!(items, vec![None], "0x01+0xFF desc → None ({order:?})");
                }
                other => panic!("expected VariantArray, got {other:?}"),
            }
        }
    }

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
        // The decoder lands the typed fast path.
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

    /// Build a Structure tree nested `n` levels deep:
    /// `Struct { f0: Struct { f0: ... { f0: Scalar(Int) } } }`.
    fn nested_struct(n: u32) -> FieldDesc {
        let mut d = FieldDesc::Scalar(ScalarType::Int);
        for _ in 0..n {
            d = FieldDesc::Structure {
                struct_id: String::new(),
                fields: vec![("f0".into(), d)],
            };
        }
        d
    }

    /// pvxs `dataencode.cpp:71` rejects FieldDesc with nesting depth >20.
    /// Rust must reach the same conclusion or a Rust client could ingest
    /// a 30-deep tree that the pvxs reference server would have rejected
    /// — interop divergence.
    #[test]
    fn decode_field_desc_caps_nesting_at_pvxs_parity_depth() {
        // depth = 20 (the cap) must decode fine.
        let ok = nested_struct(19); // 19 levels of Struct wrapping = depth 20 at innermost Struct's check
        let mut buf = Vec::new();
        encode_type_desc(&ok, ByteOrder::Little, &mut buf);
        let mut cur = Cursor::new(buf.as_slice());
        let decoded =
            decode_type_desc(&mut cur, ByteOrder::Little).expect("depth-20 descriptor must decode");
        assert_eq!(format!("{decoded}"), format!("{ok}"));

        // depth = 25 (>20) must be rejected: the per-level cap engages.
        let too_deep = nested_struct(25);
        let mut buf2 = Vec::new();
        encode_type_desc(&too_deep, ByteOrder::Little, &mut buf2);
        let mut cur2 = Cursor::new(buf2.as_slice());
        let err = decode_type_desc(&mut cur2, ByteOrder::Little)
            .expect_err("depth-25 descriptor must be rejected (pvxs parity)");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("MAX_FIELD_DEPTH"),
            "expected MAX_FIELD_DEPTH error, got: {msg}"
        );
    }

    /// Value-decode recursion: a chain of nested `Any`
    /// *values* must be bounded by the same depth cap as the descriptor
    /// decoder. Each `0x82` (`TAG_VARIANT`) byte costs exactly one wire
    /// byte but one `decode_pv_field_at_depth` stack frame, so an
    /// unbounded run is a remote stack overflow — a hard SIGSEGV that the
    /// `catch(std::exception)`-equivalent dispatch guard cannot catch.
    /// It is reachable pre-auth: CONNECTION_VALIDATION decodes a
    /// peer-supplied value via `decode_pv_field`, whose top type may be
    /// `Any`. The descriptor-depth test above exercises only
    /// `decode_type_desc`; this feeds the `0x82` value-recursion vector
    /// through `decode_pv_field` directly.
    #[test]
    fn decode_pv_field_caps_nested_any_value_recursion() {
        // A 20-deep Any value (20 × `0x82` tags, terminated by a `0xFF`
        // null at depth 20) sits exactly at the cap and must decode.
        let mut ok = vec![TAG_VARIANT; 20];
        ok.push(0xFF);
        let mut cur = Cursor::new(ok.as_slice());
        decode_pv_field(&FieldDesc::Variant, &mut cur, ByteOrder::Little)
            .expect("depth-20 nested Any value must decode");

        // A longer run (>20) must be rejected by the depth guard, NOT by
        // EOF: 64 tags with no terminator faults at depth 21 while ~43
        // bytes remain unread, so the cap — not a short read — is what
        // stops the recursion.
        let too_deep = vec![TAG_VARIANT; 64];
        let mut cur2 = Cursor::new(too_deep.as_slice());
        let err = decode_pv_field(&FieldDesc::Variant, &mut cur2, ByteOrder::Little)
            .expect_err("a >20-deep nested Any value must be rejected");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("MAX_FIELD_DEPTH"),
            "expected MAX_FIELD_DEPTH error, got: {msg}"
        );
    }

    // ── generic encode fallback ──────────────────────────────────

    /// `PvField::Null` against a Union descriptor must emit the `0xFF`
    /// null selector — not zero bytes — so the stream stays in sync.
    #[test]
    fn generic_null_union_round_trips() {
        let desc = FieldDesc::Union {
            struct_id: String::new(),
            variants: vec![
                ("intValue".into(), FieldDesc::Scalar(ScalarType::Int)),
                ("doubleValue".into(), FieldDesc::Scalar(ScalarType::Double)),
            ],
        };
        for order in [ByteOrder::Little, ByteOrder::Big] {
            let mut buf = Vec::new();
            encode_pv_field(&PvField::Null, &desc, order, &mut buf);
            assert_eq!(buf, vec![0xFF], "null union must encode as single 0xFF");
            let mut cur = Cursor::new(buf.as_slice());
            let dec = decode_pv_field(&desc, &mut cur, order).unwrap();
            assert!(
                matches!(dec, PvField::Union { selector: -1, .. }),
                "decoded {dec:?}"
            );
            assert_eq!(cur.remaining(), 0);
        }
    }

    /// `PvField::Null` against a Variant descriptor must emit `0xFF`.
    #[test]
    fn generic_null_variant_round_trips() {
        let desc = FieldDesc::Variant;
        for order in [ByteOrder::Little, ByteOrder::Big] {
            let mut buf = Vec::new();
            encode_pv_field(&PvField::Null, &desc, order, &mut buf);
            assert_eq!(buf, vec![0xFF]);
            let mut cur = Cursor::new(buf.as_slice());
            let dec = decode_pv_field(&desc, &mut cur, order).unwrap();
            match dec {
                PvField::Variant(vv) => assert!(vv.desc.is_none()),
                other => panic!("expected null variant, got {other:?}"),
            }
            assert_eq!(cur.remaining(), 0);
        }
    }

    /// A bare `PvField::Scalar` handed against a `Variant` descriptor
    /// must encode `<inferred desc><value>` and round-trip — mirroring
    /// pvxs `to_wire_field` reading the value's stored descriptor.
    #[test]
    fn generic_bare_scalar_against_variant_desc() {
        let desc = FieldDesc::Variant;
        let value = PvField::Scalar(ScalarValue::Int(0x1234_5678));
        for order in [ByteOrder::Little, ByteOrder::Big] {
            let mut buf = Vec::new();
            encode_pv_field(&value, &desc, order, &mut buf);
            assert!(!buf.is_empty(), "must not emit zero bytes");
            let mut cur = Cursor::new(buf.as_slice());
            let dec = decode_pv_field(&desc, &mut cur, order).unwrap();
            match dec {
                PvField::Variant(vv) => {
                    assert_eq!(vv.desc, Some(FieldDesc::Scalar(ScalarType::Int)));
                    assert_eq!(vv.value, PvField::Scalar(ScalarValue::Int(0x1234_5678)));
                }
                other => panic!("expected variant, got {other:?}"),
            }
            assert_eq!(cur.remaining(), 0);
        }
    }

    /// `PvField::Null` against the array descriptors must emit a zero
    /// Size, never zero bytes, so the length prefix stays present.
    #[test]
    fn generic_null_against_array_descs() {
        for desc in [
            FieldDesc::StructureArray {
                struct_id: "S".into(),
                fields: vec![("x".into(), FieldDesc::Scalar(ScalarType::Int))],
            },
            FieldDesc::UnionArray {
                struct_id: String::new(),
                variants: vec![("v".into(), FieldDesc::Scalar(ScalarType::Int))],
            },
            FieldDesc::VariantArray,
        ] {
            for order in [ByteOrder::Little, ByteOrder::Big] {
                let mut buf = Vec::new();
                encode_pv_field(&PvField::Null, &desc, order, &mut buf);
                assert_eq!(buf, vec![0x00], "zero-length Size for {desc}");
                let mut cur = Cursor::new(buf.as_slice());
                let dec = decode_pv_field(&desc, &mut cur, order).unwrap();
                let empty = match &dec {
                    PvField::StructureArray(v) => v.is_empty(),
                    PvField::UnionArray(v) => v.is_empty(),
                    PvField::VariantArray(v) => v.is_empty(),
                    other => panic!("unexpected decode {other:?}"),
                };
                assert!(empty, "decoded array must be empty");
                assert_eq!(cur.remaining(), 0);
            }
        }
    }

    /// A mismatched scalar variant (Int value vs Double descriptor) is
    /// re-encoded under the descriptor's width — `coerce_scalar` keeps
    /// the wire layout correct so decode does not desync.
    #[test]
    fn generic_scalar_type_coercion() {
        let desc = FieldDesc::Scalar(ScalarType::Double);
        let value = PvField::Scalar(ScalarValue::Int(42));
        let mut buf = Vec::new();
        encode_pv_field(&value, &desc, ByteOrder::Little, &mut buf);
        assert_eq!(buf.len(), 8, "must occupy a full f64 slot");
        let mut cur = Cursor::new(buf.as_slice());
        let dec = decode_pv_field(&desc, &mut cur, ByteOrder::Little).unwrap();
        assert_eq!(dec, PvField::Scalar(ScalarValue::Double(42.0)));
        assert_eq!(cur.remaining(), 0);
    }

    /// A bare structure value handed against a Union descriptor selects
    /// the matching variant slot and round-trips.
    #[test]
    fn generic_bare_value_picks_union_variant() {
        let desc = FieldDesc::Union {
            struct_id: String::new(),
            variants: vec![
                ("s".into(), FieldDesc::Scalar(ScalarType::String)),
                ("d".into(), FieldDesc::Scalar(ScalarType::Double)),
            ],
        };
        let value = PvField::Scalar(ScalarValue::Double(3.5));
        let mut buf = Vec::new();
        encode_pv_field(&value, &desc, ByteOrder::Little, &mut buf);
        let mut cur = Cursor::new(buf.as_slice());
        let dec = decode_pv_field(&desc, &mut cur, ByteOrder::Little).unwrap();
        match dec {
            PvField::Union {
                selector, value, ..
            } => {
                assert_eq!(selector, 1, "must select the Double variant");
                assert_eq!(*value, PvField::Scalar(ScalarValue::Double(3.5)));
            }
            other => panic!("expected union, got {other:?}"),
        }
        assert_eq!(cur.remaining(), 0);
    }

    // ── CRITICAL 1: compressed-bitset structure delta semantics ──────────

    /// `{ value: Double, alarm: Struct { severity: Int, status: Int } }`.
    /// Bit layout (depth-first §5.4): root=0, value=1, alarm=2,
    /// severity=3, status=4. total_bits = 5.
    fn nested_alarm_desc() -> FieldDesc {
        FieldDesc::Structure {
            struct_id: "test:S:1.0".into(),
            fields: vec![
                ("value".into(), FieldDesc::Scalar(ScalarType::Double)),
                (
                    "alarm".into(),
                    FieldDesc::Structure {
                        struct_id: "alarm_t".into(),
                        fields: vec![
                            ("severity".into(), FieldDesc::Scalar(ScalarType::Int)),
                            ("status".into(), FieldDesc::Scalar(ScalarType::Int)),
                        ],
                    },
                ),
            ],
        }
    }

    fn nested_alarm_value(value: f64, sev: i32, status: i32) -> PvField {
        let mut alarm = PvStructure::new("alarm_t");
        alarm
            .fields
            .push(("severity".into(), PvField::Scalar(ScalarValue::Int(sev))));
        alarm
            .fields
            .push(("status".into(), PvField::Scalar(ScalarValue::Int(status))));
        let mut root = PvStructure::new("test:S:1.0");
        root.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(value))));
        root.fields
            .push(("alarm".into(), PvField::Structure(alarm)));
        PvField::Structure(root)
    }

    /// pvxs `BitSet` compresses "all descendants set" into the parent
    /// structure bit. When that compressed parent bit is set but the
    /// descendant leaf bits are clear, the encoder must emit the WHOLE
    /// subtree and the decoder must consume it — otherwise the cursor
    /// desyncs and the next field decodes garbage.
    #[test]
    fn compressed_bitset_struct_bit_round_trips() {
        let desc = nested_alarm_desc();
        let value = nested_alarm_value(42.0, 2, 5);
        for order in [ByteOrder::Little, ByteOrder::Big] {
            // Compressed delta: only the `alarm` struct's own bit (2) is
            // set; its children (3, 4) are clear. value (bit 1) clear.
            let mut bs = crate::proto::BitSet::new();
            bs.set(2);

            let mut buf = Vec::new();
            encode_pv_field_with_bitset(&value, &desc, &bs, 0, order, &mut buf);

            // The whole `alarm` subtree (2 Int = 8 bytes) must be emitted;
            // `value` (bit 1 clear) must NOT be.
            assert_eq!(buf.len(), 8, "alarm subtree only, order={order:?}");

            // Append a sentinel Int so a desynced decode would be caught.
            buf.put_i32(0x7EAD, order);

            let mut cur = Cursor::new(buf.as_slice());
            let decoded = decode_pv_field_with_bitset(&desc, &bs, 0, &mut cur, order).unwrap();
            if let PvField::Structure(s) = decoded {
                // alarm subtree decoded fresh from the wire.
                if let Some(PvField::Structure(a)) = s.get_field("alarm") {
                    assert!(matches!(
                        a.get_field("severity"),
                        Some(PvField::Scalar(ScalarValue::Int(2)))
                    ));
                    assert!(matches!(
                        a.get_field("status"),
                        Some(PvField::Scalar(ScalarValue::Int(5)))
                    ));
                } else {
                    panic!("alarm must be a Structure");
                }
            } else {
                panic!("decoded must be Structure");
            }
            // Cursor must sit exactly on the sentinel — proof the
            // subtree was fully consumed.
            assert_eq!(cur.get_i32(order).unwrap(), 0x7EAD, "cursor desync");
            assert_eq!(cur.remaining(), 0);
        }
    }

    /// `fill_unmarked_from_prior` must honour the compressed parent bit
    /// too: when the `alarm` struct bit is set the whole subtree was
    /// decoded fresh, so the merged value keeps the decoded subtree
    /// rather than carrying leaves over from `prior`.
    #[test]
    fn compressed_bitset_fill_unmarked_keeps_decoded_subtree() {
        let desc = nested_alarm_desc();
        let prior = nested_alarm_value(1.0, 9, 9);
        let decoded = nested_alarm_value(0.0, 2, 5);

        let mut bs = crate::proto::BitSet::new();
        bs.set(2); // alarm struct bit only (children clear)

        let merged = fill_unmarked_from_prior(&desc, &bs, 0, decoded, &prior);
        if let PvField::Structure(s) = merged {
            // value (bit 1 clear) → carried from prior.
            assert!(matches!(
                s.get_field("value"),
                Some(PvField::Scalar(ScalarValue::Double(v))) if (*v - 1.0).abs() < 1e-9
            ));
            // alarm subtree → kept from decoded (struct bit set).
            if let Some(PvField::Structure(a)) = s.get_field("alarm") {
                assert!(matches!(
                    a.get_field("severity"),
                    Some(PvField::Scalar(ScalarValue::Int(2)))
                ));
                assert!(matches!(
                    a.get_field("status"),
                    Some(PvField::Scalar(ScalarValue::Int(5)))
                ));
            } else {
                panic!("alarm must be a Structure");
            }
        } else {
            panic!("merged must be Structure");
        }
    }

    /// Single-leaf delta: only `alarm.severity` (bit 3) changed. The
    /// encoder emits just that 4-byte Int; the decoder consumes exactly
    /// it and default-fills the rest.
    #[test]
    fn single_leaf_bitset_round_trips() {
        let desc = nested_alarm_desc();
        let value = nested_alarm_value(42.0, 7, 5);
        for order in [ByteOrder::Little, ByteOrder::Big] {
            let mut bs = crate::proto::BitSet::new();
            bs.set(3); // alarm.severity only

            let mut buf = Vec::new();
            encode_pv_field_with_bitset(&value, &desc, &bs, 0, order, &mut buf);
            assert_eq!(buf.len(), 4, "one Int leaf only, order={order:?}");

            buf.put_i32(0x7EAD, order); // sentinel

            let mut cur = Cursor::new(buf.as_slice());
            let decoded = decode_pv_field_with_bitset(&desc, &bs, 0, &mut cur, order).unwrap();
            if let PvField::Structure(s) = decoded {
                if let Some(PvField::Structure(a)) = s.get_field("alarm") {
                    assert!(matches!(
                        a.get_field("severity"),
                        Some(PvField::Scalar(ScalarValue::Int(7)))
                    ));
                    // status not marked → default 0.
                    assert!(matches!(
                        a.get_field("status"),
                        Some(PvField::Scalar(ScalarValue::Int(0)))
                    ));
                } else {
                    panic!("alarm must be a Structure");
                }
                // value not marked → default 0.0.
                assert!(matches!(
                    s.get_field("value"),
                    Some(PvField::Scalar(ScalarValue::Double(v))) if v.abs() < 1e-9
                ));
            } else {
                panic!("decoded must be Structure");
            }
            assert_eq!(cur.get_i32(order).unwrap(), 0x7EAD, "cursor desync");
            assert_eq!(cur.remaining(), 0);
        }
    }

    /// `diff_changed_bitset` marks exactly the leaf bits
    /// whose value differs between two snapshots, and leaves
    /// structure bits clear. Bit numbering for `nested_alarm_desc`:
    /// root=0, value=1, alarm(struct)=2, alarm.severity=3,
    /// alarm.status=4.
    #[test]
    fn br_r29_diff_changed_bitset_marks_only_changed_leaves() {
        let desc = nested_alarm_desc();

        // Only `value` changed (1.0 → 9.0); alarm identical.
        let prev = nested_alarm_value(1.0, 7, 3);
        let curr = nested_alarm_value(9.0, 7, 3);
        let diff = diff_changed_bitset(&desc, &prev, &curr);
        assert!(diff.get(1), "value leaf changed → bit 1 set");
        assert!(!diff.get(0), "root structure bit must stay clear");
        assert!(!diff.get(2), "alarm structure bit must stay clear");
        assert!(!diff.get(3), "alarm.severity unchanged → clear");
        assert!(!diff.get(4), "alarm.status unchanged → clear");
        assert_eq!(diff.count(), 1, "exactly one leaf changed");

        // Only a nested leaf changed (severity 7 → 2).
        let prev2 = nested_alarm_value(5.0, 7, 3);
        let curr2 = nested_alarm_value(5.0, 2, 3);
        let diff2 = diff_changed_bitset(&desc, &prev2, &curr2);
        assert!(diff2.get(3), "alarm.severity changed → bit 3 set");
        assert!(!diff2.get(1), "value unchanged → clear");
        assert!(!diff2.get(4), "alarm.status unchanged → clear");
        assert_eq!(diff2.count(), 1);

        // Identical snapshots → empty diff.
        let same = nested_alarm_value(5.0, 1, 1);
        let diff3 = diff_changed_bitset(&desc, &same, &same.clone());
        assert_eq!(diff3.count(), 0, "no change → empty changed-bitset");
    }

    /// `marked_changed_bitset` marks each named path's
    /// whole subtree (assigned-not-changed), independent of any value
    /// diff. Structure desc: root=0, value(leaf)=1, alarm(struct)=2,
    /// alarm.severity=3, alarm.status=4.
    #[test]
    fn br_fr12_marked_changed_bitset_marks_target_subtrees() {
        let desc = nested_alarm_desc();

        // Leaf target: only its own bit.
        let m_value = marked_changed_bitset(&desc, &["value".to_string()]);
        assert!(m_value.get(1), "value leaf marked → bit 1");
        assert_eq!(m_value.count(), 1, "leaf target marks only itself");

        // Structure target: the node bit plus every descendant.
        let m_alarm = marked_changed_bitset(&desc, &["alarm".to_string()]);
        assert!(m_alarm.get(2), "alarm structure bit set");
        assert!(m_alarm.get(3), "alarm.severity set");
        assert!(m_alarm.get(4), "alarm.status set");
        assert_eq!(m_alarm.count(), 3, "whole alarm subtree marked");

        // Nested leaf via dotted path.
        let m_sev = marked_changed_bitset(&desc, &["alarm.severity".to_string()]);
        assert!(m_sev.get(3), "alarm.severity marked");
        assert_eq!(m_sev.count(), 1);

        // Empty target set and an unknown path both produce nothing.
        assert_eq!(marked_changed_bitset(&desc, &[]).count(), 0);
        assert_eq!(
            marked_changed_bitset(&desc, &["nope".to_string()]).count(),
            0,
            "unknown path marks nothing"
        );
    }

    /// Root bit set → the entire value (every leaf) is emitted and
    /// consumed, regardless of descendant bits.
    #[test]
    fn root_bit_set_emits_whole_value() {
        let desc = nested_alarm_desc();
        let value = nested_alarm_value(3.5, 1, 2);
        let order = ByteOrder::Little;
        let mut bs = crate::proto::BitSet::new();
        bs.set(0); // root only

        let mut buf = Vec::new();
        encode_pv_field_with_bitset(&value, &desc, &bs, 0, order, &mut buf);
        // value(8) + severity(4) + status(4) = 16 bytes.
        assert_eq!(buf.len(), 16);

        let mut cur = Cursor::new(buf.as_slice());
        let decoded = decode_pv_field_with_bitset(&desc, &bs, 0, &mut cur, order).unwrap();
        assert_eq!(decoded, value);
        assert_eq!(cur.remaining(), 0);
    }

    // ── BUG 2: unbounded allocation from wire-supplied array length ──────

    /// A frame declaring a huge scalar-array length must fail fast
    /// instead of eagerly allocating gigabytes. The declared length far
    /// exceeds the bytes the frame can carry, so decode rejects it.
    #[test]
    fn oversized_scalar_array_length_rejected() {
        for st in [
            ScalarType::Double,
            ScalarType::Float,
            ScalarType::Long,
            ScalarType::ULong,
            ScalarType::Int,
            ScalarType::UInt,
            ScalarType::Short,
            ScalarType::UShort,
            ScalarType::Byte,
            ScalarType::UByte,
            ScalarType::Boolean,
        ] {
            for order in [ByteOrder::Little, ByteOrder::Big] {
                let desc = FieldDesc::ScalarArray(st);
                // Hostile frame: Size-encoded length 0xFFFF_FFFF then a
                // handful of payload bytes. ~9 bytes total.
                let mut buf = Vec::new();
                encode_size_into(u32::MAX, order, &mut buf);
                buf.extend_from_slice(&[0u8; 4]);

                let mut cur = Cursor::new(buf.as_slice());
                let res = decode_pv_field(&desc, &mut cur, order);
                assert!(
                    res.is_err(),
                    "huge length must be rejected for {st:?} order={order:?}"
                );
            }
        }
    }

    /// A correctly-sized scalar array still decodes fine after the
    /// length-bound check.
    #[test]
    fn well_sized_scalar_array_still_decodes() {
        let desc = FieldDesc::ScalarArray(ScalarType::Int);
        for order in [ByteOrder::Little, ByteOrder::Big] {
            let value = PvField::ScalarArrayTyped(TypedScalarArray::Int(vec![1, 2, 3, -4].into()));
            let mut buf = Vec::new();
            encode_pv_field(&value, &desc, order, &mut buf);
            let mut cur = Cursor::new(buf.as_slice());
            let decoded = decode_pv_field(&desc, &mut cur, order).unwrap();
            assert_eq!(decoded, value);
            assert_eq!(cur.remaining(), 0);
        }
    }

    /// Duplicate `0xFD` definition of an already-bound slot must NOT
    /// overwrite it: pvxs `std::map::emplace` keeps the first definition,
    /// so a later `0xFE` reference resolves to the original descriptor.
    /// The field carrying the second `0xFD` still decodes as its inline
    /// descriptor.
    #[test]
    fn duplicate_typecache_slot_keeps_first_definition() {
        for order in [ByteOrder::Little, ByteOrder::Big] {
            // Inline wire bytes for the two descriptors.
            let mut int_bytes = Vec::new();
            encode_type_desc(&FieldDesc::Scalar(ScalarType::Int), order, &mut int_bytes);
            let mut dbl_bytes = Vec::new();
            encode_type_desc(
                &FieldDesc::Scalar(ScalarType::Double),
                order,
                &mut dbl_bytes,
            );

            let mut buf = Vec::new();
            // 0xFD define slot 1 = Int
            buf.put_u8(0xFD);
            buf.put_u16(1, order);
            buf.extend_from_slice(&int_bytes);
            // 0xFD define slot 1 again = Double (duplicate)
            buf.put_u8(0xFD);
            buf.put_u16(1, order);
            buf.extend_from_slice(&dbl_bytes);
            // 0xFE reference slot 1
            buf.put_u8(0xFE);
            buf.put_u16(1, order);

            let mut cur = Cursor::new(buf.as_slice());
            let mut cache = TypeCache::new();
            // First define: current field is Int; slot 1 := Int.
            let d1 = decode_type_desc_cached(&mut cur, order, &mut cache).unwrap();
            assert_eq!(d1, FieldDesc::Scalar(ScalarType::Int));
            // Second define: current field is the inline Double, but the
            // slot is NOT overwritten (emplace no-op).
            let d2 = decode_type_desc_cached(&mut cur, order, &mut cache).unwrap();
            assert_eq!(d2, FieldDesc::Scalar(ScalarType::Double));
            // Reference resolves to the FIRST definition (Int), not Double.
            let d3 = decode_type_desc_cached(&mut cur, order, &mut cache).unwrap();
            assert_eq!(
                d3,
                FieldDesc::Scalar(ScalarType::Int),
                "0xFE must resolve to the first 0xFD definition (pvxs emplace)"
            );
            assert_eq!(cur.remaining(), 0);
        }
    }

    // ── MINOR: union out-of-range selector clamps to null ────────────────

    /// An out-of-range union selector must be emitted as the null
    /// marker (0xFF) — emitting a Size with no value bytes would
    /// desync the frame.
    #[test]
    fn union_out_of_range_selector_clamps_to_null() {
        let desc = FieldDesc::Union {
            struct_id: String::new(),
            variants: vec![("d".into(), FieldDesc::Scalar(ScalarType::Double))],
        };
        // selector 7 is out of range (only variant index 0 exists).
        let value = PvField::Union {
            selector: 7,
            variant_name: String::new(),
            value: Box::new(PvField::Scalar(ScalarValue::Double(1.0))),
        };
        for order in [ByteOrder::Little, ByteOrder::Big] {
            let mut buf = Vec::new();
            encode_pv_field(&value, &desc, order, &mut buf);
            assert_eq!(buf, vec![0xFF], "out-of-range selector → null marker");

            let mut cur = Cursor::new(buf.as_slice());
            let decoded = decode_pv_field(&desc, &mut cur, order).unwrap();
            if let PvField::Union { selector, .. } = decoded {
                assert_eq!(selector, -1, "null union decodes to selector -1");
            } else {
                panic!("expected union");
            }
            assert_eq!(cur.remaining(), 0);
        }
    }

    /// Regression: a Variant whose inner descriptor is a 0xFE
    /// back-reference must resolve against the connection-scope cache, not
    /// a fresh empty cache. Wire layout:
    ///   peek(0xFE) != 0xFF → push back → decode_type_desc_cached reads
    ///   0xFE + u16 slot key → cache lookup → Int descriptor → Int value.
    #[test]
    fn pva_r3_nested_variant_uses_typecache() {
        // Simulate a connection-scope cache already populated by an earlier
        // frame: slot 5 = Int (the 0xFD define arrived in a prior message).
        let mut cache = TypeCache::new();
        cache.insert(5, FieldDesc::Scalar(ScalarType::Int));

        // Variant wire bytes: 0xFE (backref), slot=5 (LE u16), Int value=42 (LE i32).
        let buf: &[u8] = &[0xFEu8, 0x05, 0x00, 0x2A, 0x00, 0x00, 0x00];
        let desc = FieldDesc::Variant;

        // With shared cache: 0xFE resolves to Int, value decodes to 42.
        let mut cur = Cursor::new(buf);
        let got = decode_pv_field_cached(&desc, &mut cur, ByteOrder::Little, &mut cache).unwrap();
        match got {
            PvField::Variant(vv) => {
                assert_eq!(vv.desc, Some(FieldDesc::Scalar(ScalarType::Int)));
                assert!(matches!(vv.value, PvField::Scalar(ScalarValue::Int(42))));
            }
            other => panic!("expected Variant, got {other:?}"),
        }
        assert_eq!(cur.remaining(), 0);

        // Without shared cache: decode_pv_field creates an empty cache internally,
        // so the 0xFE reference to slot 5 is a cache miss → error.
        let mut cur2 = Cursor::new(buf);
        let err = decode_pv_field(&desc, &mut cur2, ByteOrder::Little);
        assert!(
            err.is_err(),
            "must fail: 0xFE ref without prior 0xFD define in same call"
        );
    }
}
