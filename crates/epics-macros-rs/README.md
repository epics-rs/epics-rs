# epics-macros-rs

The workspace's procedural macros.

| macro | what it is for |
|---|---|
| `#[derive(EpicsRecord)]` | the `Record` trait's get/put dispatch, from a struct definition, so each record file carries only its own processing logic |
| `#[epics_main]` | an IOC entry point — what `#[tokio::main]` would be, routed through the runtime seam so the RTEMS build works too |
| `#[epics_test]` | the same for an async test (`#[tokio::test]`) |
| `#[derive(NTScalar)]`, `#[derive(NTTable)]` | a PVA NormativeType's `FieldDesc` and value, from a struct |
| `#[pva_service]` | a PVA RPC service `impl` block — request decode and response encode around plain typed `async fn`s |

The rest of this file is about `#[derive(EpicsRecord)]`.

**Repository:** <https://github.com/epics-rs/epics-rs>

## Usage

```rust
use epics_macros_rs::EpicsRecord;

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
| `constant_init = "LINK:TARGET,..."` | No | the record's C `recGblInitConstantLink` table, emitted as `constant_init_links` |
| `init = some_fn` | No | a `fn(&mut Self, u8) -> CaResult<()>` to emit as `init_record`; without it the trait's no-op applies |
| `metadata_override = some_fn` | No | a `fn(&Self, &str) -> Option<FieldMetadataOverride>` for the fields the C rset answers itself |
| `link_metadata_field = some_fn` | No | a `fn(&Self, &str) -> Option<String>` naming the LINK field a metadata answer comes from |
| `no_value_monitor` | No | the process cycle posts no VAL monitor (`fanout`, `seq`) |
| `dset_owns_udf_on_computed` | No | the dset, not the framework, rederives UDF on a computed read |

Each of the four function-valued attributes takes a free function rather than
an inherent method, so the derive never has to assume a method exists.

### Field: `#[field(...)]`

| Attribute | Required | Description |
|-----------|----------|-------------|
| `type = "..."` | Yes | DBR field type (see table below) |
| `read_only` | No | Reject puts with `CaError::ReadOnlyField` |
| `menu_choices = SOME_CONST` | No | the choice strings of a `DBR_ENUM` menu field, emitted as `menu_field_choices` |

## Supported Field Types

| DBR Type | Rust Type | EpicsValue Variant |
|----------|-----------|-------------------|
| `"Double"` | `f64` | `EpicsValue::Double` |
| `"Float"` | `f32` | `EpicsValue::Float` |
| `"Long"` | `i32` | `EpicsValue::Long` |
| `"Short"` | `i16` | `EpicsValue::Short` |
| `"Char"` | `u8` | `EpicsValue::Char` |
| `"Int64"` | `i64` | `EpicsValue::Int64` |
| `"UShort"` | `u16` | `EpicsValue::UShort` |
| `"Enum"` | `u16` | `EpicsValue::Enum` |
| `"String"` | `String` | `EpicsValue::String` |
| `"PvStr"` | `PvString` | `EpicsValue::String`, without the `String` round trip |

Enum fields also accept `Long` and `Short` values on put (auto-cast to u16).

## Generated Code

The macro generates a `Record` trait implementation with:

- **`record_type()`** — returns the type string from `#[record(type = "...")]`
- **`get_field(name)`** — match-based getter converting struct fields to `EpicsValue`
- **`put_field(name, value)`** — match-based setter with type checking, calling `validate_put()` and `on_put()` hooks
- one method per optional `#[record(...)]` attribute above, and
  `menu_field_choices` for the fields that named choices

The field *declaration* is not generated. A record type's fields, their DBF
types, menus and `special(SPC_NOMOD)` come from its `.dbd`: for a base record
type from the table `tools/dbd-codegen` generates into
`server::record::dbd_generated`, and for a downstream record type from its own
crate's generated table, returned through `Record::declared_fields`. The two
are mutually exclusive, which is what keeps one declaration per record type.

Field names are converted from `snake_case` to `UPPER_CASE` (e.g., `znam` -> `"ZNAM"`).

## License

[EPICS Open License](../../LICENSE)
