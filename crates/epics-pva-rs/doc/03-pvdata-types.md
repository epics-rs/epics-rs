# 03 — pvData type system

PVA messages carry self-describing data. Every value on the wire is
preceded (directly or indirectly via a cache slot) by a type
descriptor. This doc explains the in-memory model and how it maps to
bytes.

## In-memory model

```text
FieldDesc                       PvField
──────────                      ───────
Scalar(ScalarType)              Scalar(ScalarValue)
ScalarArray(ScalarType)         ScalarArray(Vec<ScalarValue>)
Structure { id, fields }        Structure(PvStructure)
StructureArray { id, fields }   StructureArray(Vec<PvStructure>)
Union { id, variants }          Union { selector: i32, value: Box<PvField>, ... }
UnionArray { id, variants }     UnionArray(Vec<UnionItem>)
Variant                         Variant(VariantValue)
VariantArray                    VariantArray(Vec<VariantValue>)
```

| Module | Type |
|--------|------|
| `pvdata/scalar.rs` | `ScalarType`, `ScalarValue` |
| `pvdata/field.rs` | `FieldDesc`, depth-first walks, `bit_for_path` |
| `pvdata/structure.rs` | `PvField`, `PvStructure`, `Variant`, `UnionItem` |
| `pvdata/value.rs` | `Value` mutable accessor with marked-bits tracking |
| `pvdata/encode.rs` | full codec — `encode_pv_field`, `encode_type_desc`, ... |

## Type tags (on the wire)

See [`02-wire-protocol.md`](02-wire-protocol.md) for the byte values.
The mapping in `pvdata/scalar.rs::ScalarType::type_code` is the
authoritative table.

## Bit numbering for monitor deltas

PVA monitor `DATA` frames carry a `BitSet` indicating which fields
are present. Bit 0 is the root structure; nested fields are numbered
depth-first in declaration order. `FieldDesc::total_bits` returns
the total bit count for a sub-tree; `FieldDesc::bit_for_path("foo.bar")`
maps a name path to its bit.

| Selector | Bit |
|----------|-----|
| empty pvRequest (`field {}`) | every bit set |
| no `field` substructure | every bit set |
| `field { value }` | the bit for `.value` (and root) |
| `field { foo { bar }}` | root, `foo`, `foo.bar` |

Code: `pv_request.rs::request_to_mask`. The "empty pvRequest selects
all" rule is non-obvious — see the `request_to_mask` knowledge
memory in kodex for the rationale.

## Type cache (0xFD / 0xFE markers)

Each TCP connection has its own per-side type cache. Cache slots are
populated by `0xFD <slot:u16>` markers; subsequent appearances of
the same descriptor can be replaced with a 3-byte `0xFE <slot:u16>`
reference.

`pva-rs` always *accepts* both markers on decode, but **emits**
inline (no caching) by default. The reason is `pvAccessCPP`
(EPICS Base 7.x) does not parse the markers and reads the next
byte as a regular type tag, leading to "Not-a-first segmented
message" errors. See the kodex decision memory titled
"pvAccessCPP does not parse 0xFD/0xFE type-cache markers".

Override with `PvaServerConfig::emit_type_cache = true` for
pvxs-only deployments where the bandwidth win on repeated NTScalar
INIT responses matters.

Code: `pvdata/encode.rs::TypeCache` + `EncodeTypeCache` +
`encode_type_desc_cached` / `decode_type_desc_cached`.

## Full-value vs marked-fields encoding

| Helper | When to use |
|--------|-------------|
| `encode_pv_field` | full value (RPC arg, CONNECTION_VALIDATION authnz, monitor INIT seed) |
| `encode_pv_field_with_bitset` | monitor DATA — only the fields whose bit is set are emitted |
| `decode_pv_field` | full value |
| `decode_pv_field_with_bitset` | monitor DATA — fills missing fields with `default_value_for(desc)` |

Bit-set encode walks the tree and emits children only when the bit
itself or any descendant bit is set. This matches pvxs
`to_wire_valid`.

## Variant ("any")

`PvField::Variant(v)` carries an optional descriptor. On the wire:

- `0xFF` when `v.desc` is None (null variant).
- otherwise: `<encoded type desc>` followed by the inline value.

Variant arrays are length-prefixed and each element is encoded the
same way. Used inside CONNECTION_VALIDATION authnz payloads (variant
of struct{user, host, groups}) and for RPC requests where the schema
is per-call.

## NormativeTypes (NT)

Helpers under `nt/` compose the canonical NT structures:

| Module | Type |
|--------|------|
| `nt/scalar.rs` | NTScalar (alarm + timeStamp + value) |
| `nt/table.rs` | NTTable (labels + columns array) |
| `nt/uri.rs` | NTURI (scheme + path + query) |

These produce a `(FieldDesc, PvStructure)` pair you can hand to
`SharedPV::open` or compare against on the client side.

## Coercion

`Value::set_with_coercion` and `pvdata/value.rs::ScalarValue::from_str`
provide best-effort numeric ↔ string conversion (matching pvxs
`Value::from`). When the user-supplied value can't be coerced (e.g.
non-numeric string into a Double), an error string is returned and
the value is left unchanged.
