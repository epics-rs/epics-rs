//! `recGblInitConstantLink` — the ONE implementation of "a CONSTANT link's text
//! becomes the target field's value".
//!
//! C calls it from two places, and so does this port:
//!
//!  * `init_record` — the init-seed owner
//!    (`database::processing::seed_constant_links`), which walks
//!    [`Record::constant_init_links`].
//!  * `special()` — a RUNTIME put to the link field that leaves it constant
//!    re-runs the same load, so `caput CO.INPB 7` makes `B == 7` for the next
//!    calculation exactly as `field(INPB,"7")` at load would
//!    (`calcoutRecord.c:373-378`, `sCalcoutRecord.c:513-518`,
//!    `aCalcoutRecord.c:533-538`). The put path
//!    (`database::field_io::special_after_put`) drives it from
//!    [`Record::special_reseed_input_links`].
//!
//! Both callers go through [`rec_gbl_init_constant_link`]: the parse, the DBF
//! coercion and the bo boolean normalisation live here once, so the runtime
//! re-seed cannot drift from the init seed.

use super::{ConstantInitLink, ParsedLink, Record, parse_link_v2};
use crate::types::{DbFieldType, EpicsValue, PvString};

/// Load a CONSTANT link's value into a field of `target` type, with C's
/// constant-link conversion rules (`dbConstLink.c`'s `cvt_st_*` family). The
/// number itself was already parsed by the one owner of "link text → number",
/// [`super::parse_c_double`] (hex included).
fn constant_to_field_type(value: EpicsValue, target: DbFieldType, raw: &str) -> EpicsValue {
    use DbFieldType as T;
    // `cvt_st_st` is a plain `strncpy` of the link TEXT, so a stringout with
    // `field(DOL,"1.50")` holds "1.50" — not the "1.5" an f64 round-trip would
    // render (softIoc-verified).
    if target == T::String {
        return EpicsValue::String(PvString::from(raw));
    }
    let integral = matches!(
        target,
        T::Char
            | T::UChar
            | T::Short
            | T::UShort
            | T::Enum
            | T::Long
            | T::ULong
            | T::Int64
            | T::UInt64
    );
    // C truncates the parsed number into the (possibly unsigned) target with a
    // plain integer cast, so `-1` into a DBF_USHORT is 65535. The generic
    // float→unsigned conversion SATURATES (Rust `as`), which would make it 0 —
    // go through the integer view instead.
    if integral {
        if let Some(v) = value.to_f64() {
            if v.is_finite() {
                return EpicsValue::Int64(v.trunc() as i64).convert_to(target);
            }
        }
    }
    value.convert_to(target)
}

/// C `recGblInitConstantLink(plink, DBF_x, pvalue)` for ONE link → field pair.
///
/// Returns the value that landed in `seed.target_field`, or `None` when the
/// link is not constant (C returns FALSE and touches nothing) or the put failed.
/// The caller owns what C does with that answer — the init seeder applies its
/// `prec->udf = FALSE` rule, `special()` posts the field.
pub fn rec_gbl_init_constant_link<R: Record + ?Sized>(
    record: &mut R,
    seed: &ConstantInitLink,
) -> Option<EpicsValue> {
    let Some(EpicsValue::String(link_str)) = record.get_field(seed.link_field) else {
        return None;
    };
    let parsed = parse_link_v2(link_str.as_str_lossy().as_ref());
    let ParsedLink::Constant(raw) = &parsed else {
        return None;
    };
    let raw = raw.clone();
    let value = crate::server::recgbl::simm::constant_load_value(&parsed)?;
    // C loads into the target field's own DBF type
    // (`recGblInitConstantLink(plink, DBF_USHORT, &prec->seln)`), so coerce
    // through the field's descriptor before the put.
    // A runtime-typed target (`mbbo.VAL`, whose type C's `cvt_dbaddr` derives
    // from SDEF) has no type in its declaration, so the variant it currently
    // holds is the type C would load into.
    let target_type = super::record_instance::declared_field_type_of(record, seed.target_field)
        .or_else(|| {
            record
                .get_field(seed.target_field)
                .map(|v| v.db_field_type())
        });
    let value = match target_type {
        Some(t) => constant_to_field_type(value, t, &raw),
        None => value,
    };
    // bo loads into a temporary and stores the BOOLEAN of it
    // (`boRecord.c:146-148`: `prec->val = !!ival`), so DOL="5" leaves VAL=1.
    let value = if seed.normalize_bool {
        EpicsValue::Enum(u16::from(value.to_f64().is_some_and(|v| v != 0.0)))
    } else {
        value
    };
    record.put_field(seed.target_field, value.clone()).ok()?;
    Some(value)
}

/// The `special()` half: re-seed the input whose LINK field was just written,
/// when the record's C `special()` does that (`Record::special_reseed_input_links`).
///
/// Returns the value field to post (C's `db_post_events(prec, pvalue,
/// DBE_VALUE)`), or `None` when this field is not a re-seeding link or the new
/// link is not constant.
pub fn reseed_constant_input_link<R: Record + ?Sized>(
    record: &mut R,
    link_field: &str,
) -> Option<&'static str> {
    let seed = record
        .special_reseed_input_links()
        .iter()
        .find(|(link, _)| *link == link_field)
        .map(|(link, target)| ConstantInitLink::new(link, target))?;
    rec_gbl_init_constant_link(record, &seed).map(|_| seed.target_field)
}
