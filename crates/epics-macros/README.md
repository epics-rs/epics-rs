# epics-macros

Procedural macro crate providing `#[derive(EpicsRecord)]` for implementing EPICS record types.

Generates the `Record` trait implementation (field descriptors, get/put dispatch) from a struct definition, so each record file focuses only on its unique processing logic.

**Repository:** <https://github.com/epics-rs/epics-rs>

## Usage

```rust
use epics_macros::EpicsRecord;

#[derive(EpicsRecord)]
#[record(type = "bi")]
pub struct BiRecord {
    #[field(type = "Enum")]
    pub val: u16,

    #[field(type = "String")]
    pub znam: String,

    #[field(type = "String")]
    pub onam: String,

    #[field(type = "Short", read_only)]
    pub zsv: i16,
}
```

## Attributes

### Container: `#[record(...)]`

| Attribute | Required | Description |
|-----------|----------|-------------|
| `type = "..."` | Yes | EPICS record type name (e.g., `"ai"`, `"bo"`, `"longin"`) |
| `crate_path = "..."` | No | Override crate path for cross-crate usage (default: `crate`) |

### Field: `#[field(...)]`

| Attribute | Required | Description |
|-----------|----------|-------------|
| `type = "..."` | Yes | DBR field type (see table below) |
| `read_only` | No | Reject puts with `CaError::ReadOnlyField` |

## Supported Field Types

| DBR Type | Rust Type | EpicsValue Variant |
|----------|-----------|-------------------|
| `"Double"` | `f64` | `EpicsValue::Double` |
| `"Float"` | `f32` | `EpicsValue::Float` |
| `"Long"` | `i32` | `EpicsValue::Long` |
| `"Short"` | `i16` | `EpicsValue::Short` |
| `"Char"` | `u8` | `EpicsValue::Char` |
| `"Enum"` | `u16` | `EpicsValue::Enum` |
| `"String"` | `String` | `EpicsValue::String` |

Enum fields also accept `Long` and `Short` values on put (auto-cast to u16).

## Generated Code

The macro generates a `Record` trait implementation with:

- **`record_type()`** — returns the type string from `#[record(type = "...")]`
- **`field_list()`** — static array of `FieldDesc` (name, dbf_type, read_only)
- **`get_field(name)`** — match-based getter converting struct fields to `EpicsValue`
- **`put_field(name, value)`** — match-based setter with type checking, calling `validate_put()` and `on_put()` hooks

Field names are converted from `snake_case` to `UPPER_CASE` (e.g., `znam` -> `"ZNAM"`).

## License

MIT
