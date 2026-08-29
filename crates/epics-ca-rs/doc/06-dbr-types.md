# 06 — DBR types and encoding

DBR (DataBase Request) types are the typed payload format for CA
operations. This document is a reference for what each DBR type looks
like on the wire and how `epics-ca-rs` encodes/decodes them.

The codec lives in `crates/epics-base-rs/src/types/codec.rs` and the
type table in `crates/epics-base-rs/src/types/dbr.rs`.

## Native types

```rust
#[repr(u16)]
pub enum DbFieldType {
    String  = 0,    // 40-byte fixed-width null-padded
    Short   = 1,    // i16 BE
    Float   = 2,    // f32 BE
    Enum    = 3,    // u16 BE
    Char    = 4,    // u8
    Long    = 5,    // i32 BE
    Double  = 6,    // f64 BE
}
```

`element_size()` returns the per-element wire size; multiply by
`count` for arrays. Strings are always 40 bytes per element (the
classic EPICS `MAX_STRING_SIZE`).

## DBR families

Each native type appears in **five families** (plain/STS/TIME/GR/CTRL),
plus the alarm-acknowledge family (PUT_ACKT, PUT_ACKS,
STSACK_STRING). The DBR type code embeds both family and native
type:

```
dbr_type = family_offset + native_index
```

| Family | Range | Offset | Carries | Native types |
|--------|-------|--------|---------|--------------|
| Plain | 0..6 | 0 | value only | all 7 |
| STS | 7..13 | 7 | status + severity + value | all 7 |
| TIME | 14..20 | 14 | status + severity + 8B EPICS timestamp + value | all 7 |
| GR | 21..27 | 21 | status + display metadata + value | numeric only (else falls back to STS) |
| CTRL | 28..34 | 28 | GR + 2 control limits + value | numeric only |
| Alarm | 35..37 | – | special, see below | – |

`native_type_for_dbr(code)` maps any DBR code back to its
`DbFieldType` for codec dispatch
(`crates/epics-base-rs/src/types/dbr.rs:449`).

## Wire format per family

All multi-byte fields are big-endian. All payloads are padded to
8-byte alignment by the caller before `set_payload_size`.

### Plain (0..6)

Just the value. Examples:

| Type | Size for `count=1` |
|------|--------------------|
| `DBR_STRING` (0) | 40 bytes |
| `DBR_SHORT` (1) | 2 bytes |
| `DBR_FLOAT` (2) | 4 bytes |
| `DBR_ENUM` (3) | 2 bytes |
| `DBR_CHAR` (4) | 1 byte |
| `DBR_LONG` (5) | 4 bytes |
| `DBR_DOUBLE` (6) | 8 bytes |

For `count > 1`, elements are concatenated.

### STS (7..13)

```
status:    u16 BE   (2 B)
severity:  u16 BE   (2 B)
[type-specific padding to align value field properly — see below]
value:     N elements of native type
```

Padding rules (mirror libca):

| Native | Pad after sev | Reason |
|--------|---------------|--------|
| String | 0 B | direct |
| Short, Enum | 0 B | already 2-aligned |
| Char | 1 B (RISC pad) | 1-byte values need 4-aligned start? actually 0-aligned is fine but libca pads |
| Long, Float | 0 B | already 4-aligned |
| Double | 4 B | needs 8-byte alignment |

Total header overhead: 4 B (status+severity) plus alignment padding.

### TIME (14..20)

```
status:    u16 BE
severity:  u16 BE
stamp:     8 B (sec since EPICS epoch + nsec)
[type-specific padding]
value:     N elements
```

EPICS epoch = 1990-01-01 UTC. `EPICS_UNIX_EPOCH_OFFSET_SECS =
631152000`. The codec converts `SystemTime` → `(sec, nsec)` BE.

### GR (21..27)

GR carries display-time metadata. Layout:

```
status:    u16 BE
severity:  u16 BE
[type-specific block]
value:     N elements
```

The "type-specific block" varies:

| Native | Block layout |
|--------|--------------|
| String | (no block; falls back to STS layout) |
| Short / Long | units(8) + 6× limits in native type |
| Char | units(8) + 6× u8 limits + 1B RISC pad |
| Float | precision(2) + RISC pad(2) + units(8) + 6× f32 limits |
| Double | precision(2) + RISC pad(2) + units(8) + padding(4) + 6× f64 limits |
| Enum | num_strings(2) + 16 strings × 26 chars |

Limits order (always 6):

```
upper_disp_limit
lower_disp_limit
upper_alarm_limit
upper_warning_limit
lower_warning_limit
lower_alarm_limit
```

Source: `crates/epics-base-rs/src/types/codec.rs::write_gr_meta`
(`:633`) plus its helpers (`encode_units_limits_*`,
`encode_prec_units_limits_*`).

### CTRL (28..34)

CTRL = GR + 2 extra control limits. Same layout, but the limit array
length is 8:

```
[GR layout up to limits]
upper_disp_limit
lower_disp_limit
upper_alarm_limit
upper_warning_limit
lower_warning_limit
lower_alarm_limit
upper_ctrl_limit       // ← extra
lower_ctrl_limit       // ← extra
value: N elements
```

For string and enum: same as GR (no extra control limits).

## Alarm-acknowledge family

These three types are special — they don't fit the regular
family-stride pattern.

### `DBR_PUT_ACKT` (35) — write-only

Single u16 written to a record's `ACKT` field.

| Direction | Layout |
|-----------|--------|
| Client → server | header with `data_type=35`, payload = u16 (2 B padded to 8) |
| Server → client | none in normal operation; rejected on SimplePv targets |

Server-side handling: `tcp.rs::CA_PROTO_WRITE handler` extracts u16
from payload and calls `db.put_record_field_from_ca(name, "ACKT", v)`.

### `DBR_PUT_ACKS` (36) — write-only

Identical to PUT_ACKT but routes to `ACKS`.

### `DBR_STSACK_STRING` (37) — read-only response

48-byte fixed layout:

```
status:    u16 BE  (2 B)
severity:  u16 BE  (2 B)
ackt:      u16 BE  (2 B)   ← from record's ACKT field, 0 if SimplePv
acks:      u16 BE  (2 B)   ← from record's ACKS field, 0 if SimplePv
value:     40 B (string)
```

Total = 48 B. `AlarmInfo`
(`crates/epics-base-rs/src/server/snapshot.rs`)
gained `ackt: Option<u16>` and `acks: Option<u16>` fields to carry
this through. The TCP read handler populates them from the record
just before encoding:

```rust
if requested_type == DBR_STSACK_STRING {
    if let ChannelTarget::RecordField { record, .. } = &entry.target {
        let inst = record.read().await;
        if let Some(EpicsValue::Short(v)) = inst.resolve_field("ACKT") {
            snapshot.alarm.ackt = Some(v as u16);
        }
        ...
    }
}
```

For a `SimplePv` target, ackt/acks default to `None`; the encoder
substitutes 0.

## Codec internals

### `encode_dbr(dbr_type, snapshot)`
(`crates/epics-base-rs/src/types/codec.rs:538`)

The single entry point used by every server-side READ_NOTIFY /
EVENT_ADD path. It is a thin wrapper over `encode_dbr_into` (`:565`),
which is where the dispatch sketched below actually lives — callers that
already own the destination buffer encode into it directly:

```rust
pub fn encode_dbr(
    dbr_type: u16,
    snapshot: &Snapshot,
) -> CaResult<Vec<u8>> {
    let native = native_type_for_dbr(dbr_type)?;
    let val_bytes = convert_and_serialize(native, &snapshot.value)?;
    let status = snapshot.alarm.status;
    let severity = snapshot.alarm.severity;
    match dbr_type {
        0..=6   => Ok(val_bytes),
        7..=13  => serialize_sts(...),
        14..=20 => serialize_time(...),
        21..=27 => write_gr_meta(...),
        28..=34 => write_ctrl_meta(...),
        DBR_PUT_ACKT | DBR_PUT_ACKS => Ok(val_bytes),       // write-only, no-op encode
        DBR_STSACK_STRING => /* 48-byte layout above */,
        _       => Err(CaError::UnsupportedType(dbr_type)),
    }
}
```

### `convert_and_serialize(native, value)`

Performs **type conversion** between the requested DBR native type and
the actual EpicsValue stored in the snapshot. CA allows e.g. reading
a `DBR_DOUBLE` field as `DBR_LONG`; the codec rounds, range-checks,
and reports `BADTYPE` on overflow.

### Decoding (client side)

`decode_dbr(dbr_type, data, count)` reverses the encoding into a
`Snapshot`. Used by `client/subscription.rs::on_monitor_data` and
`client/mod.rs::get_with_timeout`. For dbr_type ≤ 6 it skips the
metadata fields and constructs a `Snapshot::new(value, 0, 0, now)`.
For STS/TIME/GR/CTRL it parses the metadata into the appropriate
`AlarmInfo` / `DisplayInfo` / `ControlInfo` / `EnumInfo` blocks.

## Element count and large arrays

The standard 16-byte header carries a `u16` element count, capping at
65 535. For larger arrays the **extended header** is used:

- `postsize = 0xFFFF`, `count = 0` → extended fields follow
- `extended_postsize: u32` (real byte size)
- `extended_count: u32` (real element count)

Total header is 24 bytes. `CaHeader::set_payload_size(size, count)`
chooses normal vs extended automatically based on whether either
exceeds the u16 range. `CaHeader::is_extended()` and `actual_*()`
return the active values.

The extended header is enabled when `EPICS_CA_MAX_ARRAY_BYTES` is
configured large enough on both ends; default is 16 MB
(`MAX_PAYLOAD_SIZE`).

## Adding a new DBR type

To add support for a new DBR type code:

1. Define the constant in `crates/epics-base-rs/src/types/dbr.rs`.
2. Add a branch in `dbr_native_index` so it maps to a sensible native
   type for codec dispatch.
3. Add an arm in `encode_dbr` (and `decode_dbr` if it's read-back).
4. If it carries new metadata, extend `Snapshot` with optional fields.
5. Update server `tcp.rs` handlers (`CA_PROTO_READ_NOTIFY`,
   `CA_PROTO_WRITE_NOTIFY`, `CA_PROTO_EVENT_ADD`) to special-case the
   new type if it doesn't follow the family pattern.
6. Add a wire-layout test in
   `crates/epics-base-rs/tests/types_tests.rs`
   asserting the byte layout matches the C reference.

## Common pitfalls

- **8-byte payload alignment** — `align8(size)` is required; the C
  client TCP parser assumes it.
- **Status/severity endianness** — always big-endian, even though the
  values are small.
- **String length** — fixed 40 bytes per element. Don't truncate to
  the actual string length on the wire.
- **DBR_STSACK_STRING** — only 48 bytes total; don't apply
  STS-style padding.
- **PUT_ACKT/ACKS payload** — single u16 padded to 8 bytes (not a
  multi-element array).
- **Native float/double endianness** — `to_be_bytes()` works
  correctly because IEEE 754 BE is what CA wants.
