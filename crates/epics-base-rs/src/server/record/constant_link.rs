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

/// C `recGblInitConstantLink(plink, DBF_x, pvalue)` for ONE link → field pair,
/// or `dbLoadLinkArray` for a seed that carries an array request.
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
    #[cfg(test)]
    super::record_instance::put_gate_probe::parsed();
    let parsed = parse_link_v2(link_str.as_bytes());
    load_constant_link(record, seed, &parsed)
}

/// [`rec_gbl_init_constant_link`] for a link already parsed: the load from
/// `link` into `seed.target_field`.
fn load_constant_link<R: Record + ?Sized>(
    record: &mut R,
    seed: &ConstantInitLink,
    link: &ParsedLink,
) -> Option<EpicsValue> {
    let ParsedLink::Constant(constant) = link else {
        return None;
    };
    if let Some((dbr, nreq)) = seed.array {
        // A failed or zero-element load leaves the buffer and its count
        // alone (`if (n > 0) nea = n`).
        let value = constant.load_array(dbr, nreq).ok()?;
        if value.count() == 0 {
            return None;
        }
        record.put_field(seed.target_field, value.clone()).ok()?;
        return Some(value);
    }
    // C loads into the target field's own DBF type
    // (`recGblInitConstantLink(plink, DBF_USHORT, &prec->seln)`) through
    // `dbLoadLink`, which is the link's `loadScalar` (`recGbl.c:175`,
    // `dbLink.c:238`). A runtime-typed target (`mbbo.VAL`, whose type C's
    // `cvt_dbaddr` derives from SDEF) has no type in its declaration, so the
    // variant it currently holds is the type C would load into.
    let target_type = super::record_instance::addressed_field_type_of(record, seed.target_field)?;
    // A refused load leaves the field and UDF alone; a success that stores
    // nothing (`[]`, `cvt_st_UInt32`'s silent miss) is still a success.
    let value = match constant_load_scalar(constant, target_type)? {
        crate::types::c_parse::Converted::Stored(v) => v.into_type(target_type),
        crate::types::c_parse::Converted::Unchanged => {
            return record.get_field(seed.target_field);
        }
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
/// `link` is the parse the put installed, made before its hold, so the
/// re-seed parses nothing.
///
/// Returns the value field to post (C's `db_post_events(prec, pvalue,
/// DBE_VALUE)`), or `None` when this field is not a re-seeding link or the new
/// link is not constant.
pub fn reseed_constant_input_link<R: Record + ?Sized>(
    record: &mut R,
    link_field: &str,
    link: &ParsedLink,
) -> Option<&'static str> {
    let seed = record
        .special_reseed_input_links()
        .iter()
        .find(|(field, _)| *field == link_field)
        .map(|(field, target)| ConstantInitLink::new(field, target))?;
    load_constant_link(record, &seed, link).map(|_| seed.target_field)
}

/// C `dbConstLoadScalar(plink, dbrType, pbuffer)` (`dbConstLink.c`) — the
/// scalar load a soft dset's `recGblInitConstantLink(&prec->inp, dbrType, …)`
/// runs: the link TEXT through `convert[dbrType]`, which is the `epicsParse*`
/// row with trailing text allowed (`1e3` into `DBF_LONG` is 1), a range check
/// that refuses rather than saturates, and the text itself for `DBF_STRING`.
/// A choice type loads as `DBF_USHORT`. `None` is a non-zero status: the
/// destination is not touched.
///
/// A JSON array constant (`[…]`) is `dbPutConvertJSON` at `nReq = 1`
/// instead: element 0 in the destination's type, a string refused unless that
/// is `DBF_STRING`, and `[]` a success that writes nothing. A `{const: …}`
/// link is `lnkConst_loadScalar` instead.
pub(crate) fn constant_load_scalar(
    constant: &super::ConstantLink,
    dbr_type: DbFieldType,
) -> Option<crate::types::c_parse::Converted> {
    use crate::types::c_parse::{self, Converted, NumericField};
    if let Some(loaded) = constant.load_json_scalar(dbr_type) {
        return loaded.ok().map(Converted::Stored);
    }
    let text = constant.text();
    // `dbConstLoadScalar`'s `!pstr[0]` refusal.
    if text.is_empty() {
        return None;
    }
    let dbr_type = match dbr_type {
        DbFieldType::Enum => DbFieldType::UShort,
        t => t,
    };
    if text.starts_with('[') && text.ends_with(']') {
        let loaded = crate::server::db_convert_json::db_put_convert_json(text, dbr_type, 1).ok()?;
        return Some(match loaded.first_element() {
            Some(value) => Converted::Stored(value),
            None => Converted::Unchanged,
        });
    }
    match NumericField::of(dbr_type) {
        Some(row) => c_parse::put_string("INP", row, text).ok(),
        // `cvt_st_st`: the text itself; the field's own put applies the
        // `MAX_STRING_SIZE` cut.
        None => Some(Converted::Stored(EpicsValue::String(PvString::from(text)))),
    }
}

/// The value a reader that loads a CONSTANT link per read gets at `request`:
/// printf's `recGblInitConstantLink(plink, DBRTYPE, &val)` for each
/// conversion's type, and `dbLoadLinkLS` for `%ls` (`printfRecord.c:47-57`,
/// `:240-243`). `None` is C's failed load, or a link that is not constant.
pub(crate) fn constant_load_as(
    link: &ParsedLink,
    request: super::InputLinkRequest,
) -> Option<EpicsValue> {
    use super::{InputLinkRequest, LinkReadAs, LsLoad};
    let ParsedLink::Constant(constant) = link else {
        return None;
    };
    let InputLinkRequest::As(read_as) = request else {
        return None;
    };
    if let LinkReadAs::CharArrayAsString { .. } = read_as {
        return Some(EpicsValue::CharArray(match constant.load_ls()? {
            LsLoad::Text(s) => s.into_bytes().into(),
            LsLoad::LenOnly => Vec::new().into(),
        }));
    }
    match constant_load_scalar(constant, read_as.dbr_type()?)? {
        crate::types::c_parse::Converted::Stored(v) => Some(v),
        crate::types::c_parse::Converted::Unchanged => None,
    }
}
