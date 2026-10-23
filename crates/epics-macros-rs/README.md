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
#[record(type = "bi", crate_path = "epics_base_rs")]
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
| `link_read_as(LINK = Variant, ...)` | No | each named link's `LinkReadAs` (C `dbGetLink`'s `dbrType`), emitted as `input_link_request`; `Native` for the rest |
| `soft_input_read_as = Variant` | No | one `LinkReadAs` for INP under every soft DTYP, emitted as `soft_input_request` |
| `take_cycle_posted_fields = some_fn` | No | a `fn(&mut Self) -> CycleMarks` for what the record's C `monitor()` posts, taken every cycle |
| `fields_posted_only_when_marked = SOME_CONST` | No | the `&'static [&'static str]` posted only when that function marks them |
| `process_posted_fields = SOME_STATIC` | No | the closed `[FieldIx; N]` set C `process()` + `monitor()` post (e.g. a `declared_set`) |
| `process_status = SOME_CONST` | No | the record's `ProcessStatusRule`, emitted as `process_status_rule` |
| `spill(FIELD, ...)` | No | the declared fields the struct keeps no member for, stored by the framework; with it, a declared field neither a `#[field]` nor named here does not compile (`spill()` when there are none) |
| `no_value_monitor` | No | the process cycle posts no VAL monitor (`fanout`, `seq`) |
| `dset_owns_udf_on_computed` | No | the dset, not the framework, rederives UDF on a computed read |
| `seeds_trackers` | No | seed MLST/ALST/LALM from VAL after a dset init that did not fail (`longin`, `int64in`) |

Each of the four function-valued attributes takes a free function rather than
an inherent method, so the derive never has to assume a method exists.

### Field: `#[field(...)]`

| Attribute | Required | Description |
|-----------|----------|-------------|
| `type = "..."` | Yes | DBR field type (see table below) |
| `read_only` | No | a `SPC_NOMOD` member: `put_field` stores it and only `slot_ref` maps it; the database's NOMOD gate refuses a client put |
| `menu_choices = SOME_CONST` | No | the choice strings of a `DBR_ENUM` menu field, emitted as `menu_field_choices` |
| `link` | No | a `DBF_INLINK`/`OUTLINK`/`FWDLINK` field; requires `type = "PvStr"`; lent through `link_text_ref` |

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

`Enum` and `UShort` fields also accept `Long` and `Short` values on put (cast to u16).

## Generated Code

The macro generates a `Record` trait implementation with:

- **`record_type()`** — returns the type string from `#[record(type = "...")]`
- **`get_field(name)`** — match-based getter converting struct fields to `EpicsValue`
- **`put_field(name, value)`** — match-based setter with type checking, calling `validate_put()` and `on_put()` hooks
- one method per optional `#[record(...)]` attribute above,
  `menu_field_choices` for the fields that named choices, and `link_text_ref`
  for the `link` fields
- `slot_ref`/`slot_mut` for every `Double`, `Float`, `Long`, `Int64`, `Enum`,
  `UShort` or `PvStr` member that is not a menu or link field, at
  `dbd_generated::slots::<type>::<FIELD>`; `simm_cells`, `seq_group_cells` and
  `cycle_cell`/`put_cycle_cell` when the struct carries those members

A struct with such a member therefore needs a record type whose `.dbd`
`epics-base-rs` vendors, and a member that `.dbd` does not declare does not
compile; a record type defined elsewhere implements `Record` by hand. A member
named after a scan-cycle cell (`MDEL`, `ADEL`, `MLST`, `ALST`, `HYST`, `LALM`,
`AFTC`, `AFVL`, a numeric `VAL`/`OVAL`) must be numeric.

The field *declaration* is not generated. A record type's fields, their DBF
types, menus and `special(SPC_NOMOD)` come from its `.dbd`: for a base record
type from the table `tools/dbd-codegen` generates into
`server::record::dbd_generated`, and for a downstream record type from its own
crate's generated table, returned through `Record::declared_fields`. The two
are mutually exclusive, which is what keeps one declaration per record type.

Field names are converted from `snake_case` to `UPPER_CASE` (e.g., `znam` -> `"ZNAM"`).

## License

[EPICS Open License](../../LICENSE)
