use std::path::Path;
use std::sync::Arc;

use asyn_rs::error::AsynResult;
use asyn_rs::param::ParamValue;
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriverBase, PortFlags};
use asyn_rs::request::ParamSetValue;
use asyn_rs::trace::TraceMask;
use asyn_rs::user::AsynUser;

use crate::attributes::{
    EpicsPvAttributeSource, FunctionAttributeSource, NDAttrSource, NDAttrValue, NDAttribute,
    NDAttributeFunctionRegistry, ParamAttributeSource, PvAttrDbrType,
};
use crate::color::NDColorMode;
use crate::ndarray::{NDArray, NDArrayMut};
use crate::ndarray_pool::{DriverArray, DriverPool, NDArrayPool, PersistentArray, PublishedArray};
use crate::params::ndarray_driver::NDArrayDriverParams;
use crate::plugin::channel::{NDArrayOutput, NDArraySender, QueuedArrayCounter};

/// An octet parameter read back as text.
///
/// `getStringParam` hands out the bytes the driver stored, which is what C's
/// `char *` is; the file-name and attribute paths below are text, so they
/// decode here rather than making the parameter store guess.
fn octet_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `ND_ATTRIBUTES_STATUS` code: attributes loaded successfully
/// (C++ `NDAttributesOK`).
pub const ATTR_STATUS_OK: i32 = 0;
/// `ND_ATTRIBUTES_STATUS` code: the attributes file could not be opened
/// (C++ `NDAttributesFileNotFound`).
pub const ATTR_STATUS_FILE_NOT_FOUND: i32 = 1;
/// `ND_ATTRIBUTES_STATUS` code: the attributes XML failed to parse
/// (C++ `NDAttributesXMLSyntaxError`).
pub const ATTR_STATUS_XML_SYNTAX_ERROR: i32 = 2;
/// `ND_ATTRIBUTES_STATUS` code: macro expansion failed
/// (C++ `NDAttributesMacroError`). Reserved — macro substitution is not
/// implemented in the Rust port.
pub const ATTR_STATUS_MACRO_ERROR: i32 = 3;

/// Normalize a FilePath in place and report whether the directory exists.
///
/// The single implementation of C++ `asynNDArrayDriver::checkPath(std::string&)`
/// (asynNDArrayDriver.cpp:58-88), which every driver and file plugin inherits:
/// strip one trailing separator (Windows `stat` will not find a directory that
/// has one), test the directory, then append the separator back **even if the
/// caller never had one**. An empty path is left untouched and reports `false`.
pub fn check_path_str(file_path: &mut String) -> bool {
    if file_path.is_empty() {
        return false;
    }
    // C tests '/' on POSIX, and '/' or '\\' on Windows.
    if file_path.ends_with('/') || file_path.ends_with(std::path::MAIN_SEPARATOR) {
        file_path.pop();
    }
    let exists = Path::new(file_path.as_str()).is_dir();
    file_path.push(std::path::MAIN_SEPARATOR);
    exists
}

/// Extract the value of an XML attribute `key="..."` from a single tag body.
fn xml_attr<'a>(tag: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("{key}=\"");
    let start = tag.find(&pat)? + pat.len();
    let end = tag[start..].find('"')? + start;
    Some(&tag[start..end])
}

/// Parse the areaDetector `NDAttributesFile` XML schema.
///
/// Recognizes a root `<Attributes>` element containing
/// `<Attribute name="..." source="..." type="..." description="..." .../>`
/// children. The `type` attribute selects the [`NDAttrSource`]; C++
/// `NDAttribute::attrSourceString` defines the canonical uppercase names
/// `PARAM`, `EPICS_PV`, `FUNCTION`, `CONST` — the match is case-insensitive so
/// historic lowercase forms still parse.
///
/// G10: live attributes are built with concrete [`crate::attributes::NDAttributeSource`]
/// backends:
/// - `PARAM` → [`ParamAttributeSource`], fed by the driver from the asyn
///   parameter library (optional `addr` attribute, default 0).
/// - `FUNCTION` → [`FunctionAttributeSource`], calling a function registered
///   in `registry` (optional `param` attribute passed to the function).
/// - `EPICS_PV` → [`EpicsPvAttributeSource`], carrying the `dbrtype` the
///   monitor must publish as. Fed by a CA monitor once the port has a client —
///   see [`NDArrayDriverBase::read_nd_attributes_file`].
/// - `CONST` → static value carrying the literal `source` string.
fn parse_attributes_xml(
    xml: &str,
    registry: &std::sync::Arc<NDAttributeFunctionRegistry>,
) -> Result<Vec<NDAttribute>, String> {
    if !xml.contains("<Attributes>") {
        return Err("missing <Attributes> root element".into());
    }
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(open) = rest.find("<Attribute ") {
        let after = &rest[open + 1..];
        let close = after
            .find("/>")
            .or_else(|| after.find('>'))
            .ok_or_else(|| "unterminated <Attribute> tag".to_string())?;
        let tag = &after[..close];

        let name = xml_attr(tag, "name")
            .ok_or_else(|| "Attribute missing name".to_string())?
            .to_string();
        let description = xml_attr(tag, "description").unwrap_or("").to_string();
        let source_str =
            xml_attr(tag, "source").ok_or_else(|| format!("Attribute {name} missing source"))?;
        // C++ default attribute type is EPICS_PV.
        let attr_type = xml_attr(tag, "type").unwrap_or("EPICS_PV");

        let attr = match attr_type.to_ascii_uppercase().as_str() {
            "EPICS_PV" => {
                // C parses `dbrtype` here and returns asynError on a name that
                // is not one of its nine (asynNDArrayDriver.cpp:421-436), so a
                // typo fails the whole file rather than silently producing an
                // attribute of the wrong type.
                let dbrtype = xml_attr(tag, "dbrtype").unwrap_or("DBR_NATIVE");
                let dbr_type = PvAttrDbrType::from_xml(dbrtype)
                    .ok_or_else(|| format!("Attribute {name} unknown dbrType {dbrtype}"))?;
                let src = EpicsPvAttributeSource::new(source_str, dbr_type);
                NDAttribute::new_with_source(
                    name,
                    description,
                    NDAttrSource::EpicsPV(source_str.to_string()),
                    src,
                )
            }
            "PARAM" => {
                // C++ paramAttribute: optional `addr` (default 0).
                let addr = xml_attr(tag, "addr")
                    .and_then(|s| s.parse::<i32>().ok())
                    .unwrap_or(0);
                // The `datatype` attr selects the read/publish type, NOT the
                // param's runtime type (asynNDArrayDriver.cpp:445-446 →
                // paramAttribute.cpp:80-95). Omitted → C substitutes the
                // lower-case `"int"`, which matches no upper-case branch and
                // leaves the attribute un-typed / never-updated.
                let datatype = xml_attr(tag, "datatype").unwrap_or("int");
                let src = ParamAttributeSource::new(
                    source_str,
                    addr,
                    crate::attributes::ParamAttrType::from_datatype(datatype),
                );
                NDAttribute::new_with_source(
                    name,
                    description,
                    NDAttrSource::Param {
                        port_name: String::new(),
                        param_name: source_str.to_string(),
                    },
                    src,
                )
            }
            "FUNCTION" => {
                // C++ functAttribute: `source` is the function name, optional
                // `param` is the string passed to the function.
                let func_param = xml_attr(tag, "param").unwrap_or("");
                let src = FunctionAttributeSource::new(registry.clone(), source_str, func_param);
                NDAttribute::new_with_source(
                    name,
                    description,
                    NDAttrSource::Function(source_str.to_string()),
                    src,
                )
            }
            "CONST" => NDAttribute::new_static(
                name,
                description,
                NDAttrSource::Constant(source_str.to_string()),
                NDAttrValue::String(source_str.to_string()),
            ),
            other => {
                return Err(format!(
                    "unknown attribute type '{other}' for attribute {name}"
                ));
            }
        };

        out.push(attr);
        rest = &after[close..];
    }
    Ok(out)
}

/// Parse a C printf-style template with two `%s` and one `%d`-like specifier.
///
/// Handles format specifiers like `%s`, `%d`, `%3.3d`, `%04d`, `%06d`, etc.
/// The C++ original does: `epicsSnprintf(buf, max, template, path, name, number)`.
fn sprintf_template(template: &str, path: &str, name: &str, number: i32) -> String {
    let mut result = String::with_capacity(template.len() + path.len() + name.len() + 16);
    let mut chars = template.chars().peekable();
    let mut string_arg_idx = 0; // 0 = path, 1 = name

    while let Some(ch) = chars.next() {
        if ch == '%' {
            // Collect the format specifier
            let mut spec = String::new();
            // Collect flags, width, precision
            while let Some(&c) = chars.peek() {
                if c == 's' || c == 'd' || c == 'i' || c == 'o' || c == 'x' || c == 'X' {
                    break;
                }
                if c == '%' {
                    break;
                }
                spec.push(c);
                chars.next();
            }
            match chars.next() {
                Some('s') => {
                    let s = if string_arg_idx == 0 { path } else { name };
                    string_arg_idx += 1;
                    result.push_str(s);
                }
                Some('d') | Some('i') => {
                    // Parse width and precision from spec like "3.3", "04", "06"
                    let formatted = format_int_spec(&spec, number);
                    result.push_str(&formatted);
                }
                Some('%') => {
                    result.push('%');
                }
                Some(c) => {
                    result.push('%');
                    result.push_str(&spec);
                    result.push(c);
                }
                None => {
                    result.push('%');
                    result.push_str(&spec);
                }
            }
        } else {
            result.push(ch);
        }
    }
    result
}

/// Format an integer with a printf-style width/precision spec.
///
/// Emulates C `printf` integer conversion:
/// - **precision** (`.N`) is the minimum number of digits — the value is
///   zero-padded on the left to at least that many digits.
/// - **width** (`N`) is the minimum field width — the (already
///   precision-padded) string is then padded with spaces on the left
///   (right-justified) to at least that width.
/// - the `0` flag, when present and there is no precision, makes the width
///   pad with zeros instead of spaces (C ignores `0` when a precision is
///   given for integer conversions).
///
/// Examples: `%3.3d` of 7 → `"007"`; `%5.3d` of 42 → `"  042"`;
/// `%04d` of 7 → `"0007"`; `%5d` of 7 → `"    7"`.
fn format_int_spec(spec: &str, value: i32) -> String {
    if spec.is_empty() {
        return value.to_string();
    }

    let zero_flag = spec.starts_with('0');
    // Strip only the leading flag '0' before parsing width digits.
    let spec_clean = if zero_flag { &spec[1..] } else { spec };

    // Split on '.' into width.precision.
    let (width_str, prec_str) = if let Some(dot_pos) = spec_clean.find('.') {
        (&spec_clean[..dot_pos], Some(&spec_clean[dot_pos + 1..]))
    } else {
        (spec_clean, None)
    };

    let width: usize = width_str.parse().unwrap_or(0);
    let has_precision = prec_str.is_some();
    let precision: usize = prec_str.and_then(|s| s.parse().ok()).unwrap_or(0);

    // Step 1: render the integer, zero-padded to `precision` digits.
    let negative = value < 0;
    let digits = value.unsigned_abs().to_string();
    let digits = if digits.len() < precision {
        format!("{}{}", "0".repeat(precision - digits.len()), digits)
    } else {
        digits
    };
    let body = if negative {
        format!("-{digits}")
    } else {
        digits
    };

    // Step 2: pad to the field width. C uses zero-padding for the width only
    // when the `0` flag is set AND no precision was specified.
    if body.len() >= width {
        body
    } else if zero_flag && !has_precision {
        let pad = width - body.len();
        if negative {
            // Keep the sign at the front of zero-padding (C behavior).
            format!("-{}{}", "0".repeat(pad), &body[1..])
        } else {
            format!("{}{}", "0".repeat(pad), body)
        }
    } else {
        format!("{}{}", " ".repeat(width - body.len()), body)
    }
}

/// Write all per-array parameters from an `NDArray` into the parameter library.
///
/// This is the shared body used by both `NDArrayDriverBase::prepare_array` and
/// `ADDriverBase::prepare_array`. It populates the array-info parameters that
/// C++ drivers set for every frame:
/// `ARRAY_SIZE_X/Y/Z`, `ARRAY_SIZE`, `UNIQUE_ID`, `ARRAY_NDIMENSIONS`,
/// `DATA_TYPE`, `COLOR_MODE`, `BAYER_PATTERN`,
/// `TIME_STAMP`, `EPICS_TS_SEC`, `EPICS_TS_NSEC`, `CODEC`, `COMPRESSED_SIZE`.
pub(crate) fn write_array_params(
    port_base: &mut PortDriverBase,
    params: &NDArrayDriverParams,
    array: &NDArray,
) -> AsynResult<()> {
    let info = array.info();
    port_base.set_int32_param(params.array_size_x, 0, info.x_size as i32)?;
    port_base.set_int32_param(params.array_size_y, 0, info.y_size as i32)?;
    port_base.set_int32_param(params.array_size_z, 0, info.color_size as i32)?;
    port_base.set_int32_param(params.array_size, 0, info.total_bytes as i32)?;
    port_base.set_int32_param(params.unique_id, 0, array.unique_id)?;

    // G7: dimension count. No C driver writes NDDimensions; only
    // NDPluginDriver posts it (NDPluginDriver.cpp:246).
    port_base.set_int32_param(params.n_dimensions, 0, array.dims.len() as i32)?;

    // G7: data type and color mode.
    port_base.set_int32_param(params.data_type, 0, array.data().data_type() as i32)?;
    port_base.set_int32_param(params.color_mode, 0, info.color_mode as i32)?;

    // G5: Bayer pattern, derived from the `bayerPattern` array attribute.
    if let Some(bp) = array
        .attributes
        .get("bayerPattern")
        .and_then(|a| a.value.as_i64())
    {
        let pattern = crate::color::NDBayerPattern::from_i32(bp as i32);
        port_base.set_int32_param(params.bayer_pattern, 0, pattern.as_i32())?;
    }

    // G7: timestamps. `time_stamp` is the double timestamp; `timestamp` is the
    // epicsTS (sec/nsec) split across the two Int32 params.
    port_base.set_float64_param(params.timestamp_rbv, 0, array.time_stamp)?;
    port_base.set_int32_param(params.epics_ts_sec, 0, array.timestamp.sec as i32)?;
    port_base.set_int32_param(params.epics_ts_nsec, 0, array.timestamp.nsec as i32)?;

    // G6: codec name and compressed size, published from NDArray.codec.
    match &array.codec {
        Some(codec) => {
            port_base.set_string_param(params.codec, 0, codec.name.as_str())?;
            port_base.set_int32_param(params.compressed_size, 0, codec.compressed_size as i32)?;
        }
        None => {
            port_base.set_string_param(params.codec, 0, String::new())?;
            port_base.set_int32_param(params.compressed_size, 0, info.total_bytes as i32)?;
        }
    }
    Ok(())
}

/// `ADCORE_VERSION.ADCORE_REVISION.ADCORE_MODIFICATION` (ADCoreVersion.h) of
/// the ADCore this crate ports, `6c53844e` (`doc/reference-pin.md`).
const AD_CORE_VERSION: &str = "3.14.0";

/// Write the C++ `asynNDArrayDriver` constructor's "read-only parameters"
/// block (`asynNDArrayDriver.cpp:954-1005`) onto a freshly created
/// [`NDArrayDriverParams`].
///
/// C states the mechanism in the block's own comment: "If a value is not set
/// here then the read request will return an error (uninitialized)". A param
/// this port never writes reaches `get_int32_strict`
/// (asyn-rs/src/port.rs:1663-1665) as `AsynError::ParamUndefined`, and the
/// record it feeds sits UDF/INVALID for the life of the IOC unless something
/// else happens to write it — which for the pure read-backs in this block is
/// never. Every port that creates `NDArrayDriverParams` is an
/// `asynNDArrayDriver` in C and therefore runs this block, so it lives here
/// once rather than being re-typed in each constructor.
///
/// The block ends with the pool statistics (C `:1000-1003`): buffer counts
/// from the pool but both memory figures 0, which only a `PoolPollStats`
/// write (`refresh_pool_stats`) replaces with the pool's real sizes.
pub fn init_read_only_params(
    port_base: &mut PortDriverBase,
    params: &NDArrayDriverParams,
    pool: &NDArrayPool,
    port_name: &str,
) -> AsynResult<()> {
    port_base.set_string_param(params.port_name_self, 0, port_name)?;
    // C `:961-966` publishes the ADCore version into both strings; a
    // concrete driver overwrites DriverVersion after construction.
    port_base.set_string_param(params.ad_core_version, 0, AD_CORE_VERSION)?;
    port_base.set_string_param(params.driver_version, 0, AD_CORE_VERSION)?;

    port_base.set_int32_param(params.array_size_x, 0, 0)?;
    port_base.set_int32_param(params.array_size_y, 0, 0)?;
    port_base.set_int32_param(params.array_size_z, 0, 0)?;
    port_base.set_int32_param(params.array_size, 0, 0)?;
    port_base.set_int32_param(params.n_dimensions, 0, 0)?;
    port_base.set_int32_param(params.color_mode, 0, NDColorMode::Mono as i32)?;
    port_base.set_int32_param(params.unique_id, 0, 0)?;
    port_base.set_float64_param(params.timestamp_rbv, 0, 0.0)?;
    port_base.set_int32_param(params.epics_ts_sec, 0, 0)?;
    port_base.set_int32_param(params.epics_ts_nsec, 0, 0)?;
    port_base.set_int32_param(params.bayer_pattern, 0, 0)?;
    port_base.set_int32_param(params.array_counter, 0, 0)?;

    port_base.set_int32_param(params.write_file, 0, 0)?;
    port_base.set_int32_param(params.read_file, 0, 0)?;
    port_base.set_int32_param(params.capture, 0, 0)?;
    port_base.set_int32_param(params.file_write_status, 0, 0)?;
    port_base.set_string_param(params.file_write_message, 0, String::new())?;
    port_base.set_string_param(params.file_path, 0, String::new())?;
    port_base.set_string_param(params.file_name, 0, String::new())?;
    port_base.set_int32_param(params.file_number, 0, 0)?;
    port_base.set_int32_param(params.auto_increment, 0, 0)?;
    // C `:991`, whose comment at `:985-986` explains why this one is not left
    // to the database: it is a waveform record, and the waveform does not read
    // the driver value at initialization.
    port_base.set_string_param(params.file_template, 0, "%s%s_%3.3d.dat")?;
    port_base.set_int32_param(params.num_captured, 0, 0)?;
    port_base.set_int32_param(params.free_capture, 0, 0)?;
    port_base.set_int32_param(params.create_dir, 0, 0)?;
    port_base.set_string_param(params.temp_suffix, 0, String::new())?;

    port_base.set_string_param(params.attributes_file, 0, String::new())?;
    // C `:997`. A port that has not loaded an attribute file reports "File
    // not found", not OK: `readNDAttributesFile` returns early on an empty
    // name without touching the status (asynNDArrayDriver.cpp:327), so this
    // is the value an IOC with no attributes file actually reads.
    port_base.set_int32_param(params.attributes_status, 0, ATTR_STATUS_FILE_NOT_FOUND)?;
    port_base.set_string_param(params.attributes_macros, 0, String::new())?;

    port_base.set_int32_param(
        params.pool_alloc_buffers,
        0,
        pool.num_alloc_buffers() as i32,
    )?;
    port_base.set_int32_param(params.pool_free_buffers, 0, pool.num_free_buffers() as i32)?;
    port_base.set_float64_param(params.pool_max_memory, 0, 0.0)?;
    port_base.set_float64_param(params.pool_used_memory, 0, 0.0)?;

    port_base.set_int32_param(params.num_queued_arrays, 0, 0)?;
    // C's setIntegerParam override (`:655-660`) clears AcquireBusy when
    // NumQueuedArrays goes to 0 with Acquire not set, so the line above
    // defines it on every port.
    port_base.set_int32_param(params.acquire_busy, 0, 0)?;
    Ok(())
}

/// C++ `MEGABYTE_DBL`, the unit of the pool's memory readbacks.
const MEGABYTE: f64 = 1_048_576.0;

/// What the four pool-statistics readbacks (`POOL_MAX_MEMORY`,
/// `POOL_USED_MEMORY`, `POOL_ALLOC_BUFFERS`, `POOL_FREE_BUFFERS`) say about a
/// pool right now, as C reads them in one block
/// (asynNDArrayDriver.cpp:690-693).
///
/// The single owner of that reading, so a port that publishes it through a
/// param batch rather than a `PortDriverBase` cannot drift from one that does.
pub(crate) fn pool_stats_values(
    params: &NDArrayDriverParams,
    pool: &NDArrayPool,
) -> Vec<ParamSetValue> {
    vec![
        ParamSetValue::new(
            params.pool_max_memory,
            0,
            ParamValue::Float64(pool.max_memory() as f64 / MEGABYTE),
        ),
        ParamSetValue::new(
            params.pool_used_memory,
            0,
            ParamValue::Float64(pool.allocated_bytes() as f64 / MEGABYTE),
        ),
        ParamSetValue::new(
            params.pool_alloc_buffers,
            0,
            ParamValue::Int32(pool.num_alloc_buffers() as i32),
        ),
        ParamSetValue::new(
            params.pool_free_buffers,
            0,
            ParamValue::Int32(pool.num_free_buffers() as i32),
        ),
    ]
}

/// Refresh the pool-statistics parameters from a pool.
///
/// The `NDPoolPollStats` dispatch.
pub fn refresh_pool_stats(
    port_base: &mut PortDriverBase,
    params: &NDArrayDriverParams,
    pool: &NDArrayPool,
) -> AsynResult<()> {
    for set in pool_stats_values(params, pool) {
        match set {
            ParamSetValue::Value {
                reason,
                addr,
                value: ParamValue::Float64(v),
            } => port_base.set_float64_param(reason, addr, v)?,
            ParamSetValue::Value {
                reason,
                addr,
                value: ParamValue::Int32(v),
            } => port_base.set_int32_param(reason, addr, v)?,
            _ => unreachable!("pool_stats_values yields only Float64/Int32 values"),
        }
    }
    Ok(())
}

/// C++ `asynNDArrayDriver::preAllocateBuffers` (asynNDArrayDriver.cpp:710-740):
/// `NumPreAllocBuffers` copies of `template`, the pool posted after each,
/// then all of them released to the free list.
fn pre_allocate_buffers(
    port_base: &mut PortDriverBase,
    params: &NDArrayDriverParams,
    pool: &NDArrayPool,
    template: Option<&NDArray>,
) -> AsynResult<()> {
    let Some(template) = template else {
        port_base.trace_print(
            TraceMask::ERROR,
            format_args!(
                "asynNDArrayDriver::preAllocateBuffers: ERROR, must collect an array to get dimensions first\n"
            ),
        );
        return Ok(());
    };
    let count = port_base
        .get_int32_param(params.pool_num_pre_alloc_buffers, 0)
        .unwrap_or(0);
    // Released when dropped, on every return. C's failure return
    // (:726-730) skips its release loop and leaks the copies it holds.
    let mut copies = Vec::new();
    for i in 0..count {
        let Ok(copy) = pool.copy(template, None, true, true, true) else {
            port_base.trace_print(
                TraceMask::ERROR,
                format_args!(
                    "asynNDArrayDriver::preAllocateBuffers: ERROR, could not allocate array {i}\n"
                ),
            );
            return Ok(());
        };
        copies.push(copy);
        port_base.set_int32_param(
            params.pool_alloc_buffers,
            0,
            pool.num_alloc_buffers() as i32,
        )?;
        port_base.set_int32_param(params.pool_free_buffers, 0, pool.num_free_buffers() as i32)?;
        port_base.set_float64_param(
            params.pool_used_memory,
            0,
            pool.allocated_bytes() as f64 / MEGABYTE,
        )?;
        port_base.call_param_callbacks(0)?;
    }
    Ok(())
}

/// The pool branch of C++ `asynNDArrayDriver::writeInt32`
/// (asynNDArrayDriver.cpp:684-694), run once `param_index` is stored.
///
/// Returns `true` when `param_index` was one of the three pool controls.
/// `template_array` is C's `pArrays[0]`, the array `POOL_PRE_ALLOC_BUFFERS`
/// copies; a failed pre-allocation is printed and, as in C, not returned.
pub fn handle_pool_write_int32(
    port_base: &mut PortDriverBase,
    params: &NDArrayDriverParams,
    pool: &NDArrayPool,
    param_index: usize,
    template_array: Option<&NDArray>,
) -> AsynResult<bool> {
    if param_index == params.pool_empty_free_list {
        pool.empty_free_list();
    } else if param_index == params.pool_pre_alloc {
        pre_allocate_buffers(port_base, params, pool, template_array)?;
        port_base.set_int32_param(params.pool_pre_alloc, 0, 0)?;
    } else if param_index == params.pool_poll_stats {
        refresh_pool_stats(port_base, params, pool)?;
    } else {
        return Ok(false);
    }
    Ok(true)
}

/// C++ `asynNDArrayDriver::writeInt32` (asynNDArrayDriver.cpp:671-708):
/// store, the pool branch, callbacks, and the closing trace line. For a
/// driver built directly on `asynNDArrayDriver`; `template_array` is its
/// `pArrays[0]`.
pub fn handle_write_int32(
    port_base: &mut PortDriverBase,
    params: &NDArrayDriverParams,
    pool: &NDArrayPool,
    template_array: Option<&NDArray>,
    user: &AsynUser,
    value: i32,
) -> AsynResult<()> {
    let addr = port_base.parse_user(user)?;
    let status = port_base.set_int32_param(user.reason, addr, value);
    // C keeps only setIntegerParam's status (:682).
    let _ = handle_pool_write_int32(port_base, params, pool, user.reason, template_array);
    // C's callParamCallbacks() is list 0 (asynNDArrayDriver.cpp:696).
    let _ = port_base.call_param_callbacks(0);
    trace_write_int32(port_base, user, "asynNDArrayDriver", &status, value);
    status
}

/// The line C's `writeInt32` closes with (asynNDArrayDriver.cpp:699-706,
/// ADDriver.cpp:107-114), under `driver_name`.
pub(crate) fn trace_write_int32(
    port_base: &PortDriverBase,
    user: &AsynUser,
    driver_name: &str,
    status: &AsynResult<()>,
    value: i32,
) {
    let name = port_base.params.param_name(user.reason).unwrap_or("");
    match status {
        Err(e) => user.print(
            TraceMask::ERROR,
            file!(),
            line!(),
            format_args!(
                "{driver_name}:writeInt32: error, status={} function={}, paramName={name}, value={value}\n",
                e.c_status_code(),
                user.reason,
            ),
        ),
        Ok(()) => user.print(
            TraceMask::IO_DRIVER,
            file!(),
            line!(),
            format_args!(
                "{driver_name}:writeInt32: function={}, paramName={name}, value={value}\n",
                user.reason,
            ),
        ),
    }
}

/// Base state for asynNDArrayDriver (file handling, attribute mgmt, pool).
/// The CA monitor tasks feeding the current `EPICS_PV` attribute set.
///
/// Dropping this aborts every task it holds, and the only way to install a new
/// set is to overwrite the field, so a monitor cannot outlive the attribute
/// whose [`crate::attributes::LiveValueCell`] it writes into. Every path that
/// replaces the attribute list -- a fresh `NDAttributesFile`, a file that
/// cannot be opened, XML that does not parse -- goes through
/// `NDArrayDriverBase::clear_attributes`, which replaces both together.
#[cfg(feature = "ioc")]
#[derive(Default)]
pub struct CaMonitorSet(Vec<tokio::task::JoinHandle<()>>);

#[cfg(feature = "ioc")]
impl Drop for CaMonitorSet {
    fn drop(&mut self) {
        for handle in &self.0 {
            handle.abort();
        }
    }
}

/// The `NDAttributesFile` machinery of C++ `asynNDArrayDriver`: the attribute
/// definitions, the function registry they resolve against and the CA monitors
/// feeding them. One struct because [`NDArrayDriverBase`] and
/// [`crate::driver::ADDriverBase`] each stand in for that C++ base class, and a
/// detector driver needs `NDAttributesFile` as much as a bare NDArray port.
pub struct DriverAttributes {
    /// Attribute definitions loaded from `ND_ATTRIBUTES_FILE`
    /// (C++ `asynNDArrayDriver::pAttributeList`).
    pub list: crate::attributes::NDAttributeList,
    /// Registry of named attribute functions for `FUNCTION`-type attributes
    /// (C++ `registryFunctionFind` / `registerNDAttributeFunction`).
    pub functions: std::sync::Arc<NDAttributeFunctionRegistry>,
    /// CA client and the runtime its monitors live on, installed by the IOC
    /// that owns this port ([`Self::set_ca_client`]). C++ has no
    /// equivalent because `PVAttribute` subscribes through the global CA
    /// context every IOC already has; here the client is a value someone must
    /// hand over, and without it an `EPICS_PV` attribute has no feeder.
    #[cfg(feature = "ioc")]
    ca_feeder: Option<(
        std::sync::Arc<epics_ca_rs::client::CaClient>,
        tokio::runtime::Handle,
    )>,
    /// Monitors feeding the attributes currently loaded.
    #[cfg(feature = "ioc")]
    ca_monitors: CaMonitorSet,
}

impl Default for DriverAttributes {
    fn default() -> Self {
        Self::new()
    }
}

impl DriverAttributes {
    pub fn new() -> Self {
        Self {
            list: crate::attributes::NDAttributeList::new(),
            functions: NDAttributeFunctionRegistry::new(),
            #[cfg(feature = "ioc")]
            ca_feeder: None,
            #[cfg(feature = "ioc")]
            ca_monitors: CaMonitorSet::default(),
        }
    }

    /// C++ `asynNDArrayDriver::getAttributes(pArray->pAttributeList)`:
    /// re-evaluate every attribute and merge the values onto an array's
    /// `attributes`. An empty set leaves them untouched, so a plain array is
    /// never deep-copied through `Arc::make_mut`.
    pub fn attach(
        &mut self,
        port_base: &PortDriverBase,
        attributes: &mut crate::attributes::NDAttributeList,
    ) {
        if !self.list.is_empty() {
            let fresh = self.update(port_base);
            attributes.copy_from(&fresh);
        }
    }

    /// Load NDArray attribute definitions from the `ND_ATTRIBUTES_FILE`
    /// parameter, mirroring C++ `asynNDArrayDriver::readNDAttributesFile`.
    ///
    /// The parameter value is either a path to an XML file or inline XML
    /// (recognized by containing `<Attributes>`). The XML schema is the
    /// areaDetector `NDAttributesFile` schema: a root `<Attributes>` element
    /// with `<Attribute name="..." source="..." type="..." .../>` children.
    /// The text is macro-expanded against `ND_ATTRIBUTES_MACROS` first; see
    /// `expand_attribute_macros`. Attributes are stored on `self.list`; when a file is
    /// named, `ND_ATTRIBUTES_STATUS` is set to the resulting status code and
    /// every non-OK code is also returned as an `Err`. An empty name leaves
    /// the status alone, as C `:327` does.
    ///
    /// An `EPICS_PV` attribute is fed from here when the IOC has handed this
    /// port a CA client through [`Self::set_ca_client`]; each one gets a
    /// monitor and the status is OK. Without a client there is no feeder, so
    /// such an attribute evaluates `Undefined` for the life of the IOC and is
    /// dropped by every file writer — that case keeps a non-OK code rather
    /// than letting the operator see "Attributes file OK". The rest of the
    /// file is still loaded, because `PARAM`, `FUNCTION` and `CONST`
    /// attributes are fed and work; only the status tells the truth about the
    /// ones that are not. `NDAttributesStatus` is an mbbi with exactly four
    /// states (NDArrayBase.template:805-822), so `XML_SYNTAX_ERROR` is the
    /// nearest of C's codes rather than an accurate one.
    ///
    /// It is the nearest even though C's own vocabulary has a code for "no
    /// attribute set is in effect": `NDAttributesFileNotFound` is what the
    /// constructor seeds (C `asynNDArrayDriver.cpp:997`) and what an IOC that
    /// never loads a file keeps. That is exactly why this case cannot borrow it
    /// — code 1 is now the value every port carries before anything is loaded,
    /// so reusing it here would make a file whose `PARAM`/`FUNCTION`/`CONST`
    /// attributes are live and working indistinguishable from a port that
    /// loaded nothing at
    /// all. `write_octet` discards this `Err`, so the mbbi is the operator's
    /// only signal that the `EPICS_PV` half is dead.
    /// Install the CA client that feeds `EPICS_PV` attributes, and start
    /// feeding any that are already loaded.
    ///
    /// Feeding the current set here rather than only at load time is what
    /// makes the order the IOC does things in stop mattering: install first
    /// then load, or load first then install, and either way every `EPICS_PV`
    /// attribute ends up with a monitor.
    ///
    /// `runtime` is the handle the monitor tasks are spawned on; it is named
    /// explicitly because `NDAttributesFile` writes arrive on an asyn port
    /// thread with no ambient runtime.
    #[cfg(feature = "ioc")]
    pub fn set_ca_client(
        &mut self,
        client: std::sync::Arc<epics_ca_rs::client::CaClient>,
        runtime: tokio::runtime::Handle,
    ) {
        self.ca_feeder = Some((client, runtime));
        self.feed_epics_pv_attributes();
    }

    /// Replace the monitor set with one subscription per loaded `EPICS_PV`
    /// attribute, and return the names left unfed.
    ///
    /// The assignment to `ca_monitors` is the finalizer: it drops the previous
    /// set, and dropping aborts it, so re-reading the attributes file cannot
    /// leave a monitor writing into a cell nothing reads.
    #[cfg(feature = "ioc")]
    fn feed_epics_pv_attributes(&mut self) -> Vec<String> {
        let Some((client, runtime)) = self.ca_feeder.clone() else {
            return self
                .list
                .iter()
                .filter(|a| matches!(a.source, NDAttrSource::EpicsPV(_)))
                .map(|a| a.name.clone())
                .collect();
        };
        let handles: Vec<_> = self
            .list
            .iter()
            .filter_map(|a| a.epics_pv_source())
            .map(|src| crate::attributes::spawn_ca_monitor(&runtime, client.clone(), src))
            .collect();
        self.ca_monitors = CaMonitorSet(handles);
        Vec::new()
    }

    /// Without the `ioc` feature there is no CA client to build a feeder from,
    /// so every `EPICS_PV` attribute is unfed.
    #[cfg(not(feature = "ioc"))]
    fn feed_epics_pv_attributes(&mut self) -> Vec<String> {
        self.list
            .iter()
            .filter(|a| matches!(a.source, NDAttrSource::EpicsPV(_)))
            .map(|a| a.name.clone())
            .collect()
    }

    /// Drop the loaded attributes together with the monitors feeding them.
    ///
    /// The two are only ever replaced as a pair; separating them is what would
    /// let a monitor survive the attribute it writes into.
    fn clear(&mut self) {
        self.list.clear();
        #[cfg(feature = "ioc")]
        {
            self.ca_monitors = CaMonitorSet::default();
        }
    }

    pub fn read_file(
        &mut self,
        port_base: &mut PortDriverBase,
        params: &NDArrayDriverParams,
    ) -> AsynResult<()> {
        let file_param = port_base
            .get_string_param(params.attributes_file, 0)
            .map(octet_text)?;

        // Clear any existing attributes (C++ clears unconditionally first).
        self.clear();
        if file_param.is_empty() {
            // C `:327` — `if (fileName.length() == 0) return asynSuccess;`
            // with no `setIntegerParam`. The status keeps whatever it had,
            // which for an IOC that never loaded a file is the constructor's
            // `NDAttributesFileNotFound`. Writing OK here erased that value
            // on every `NDAttributesFile` PINI write (NDArrayBase.template:793).
            return Ok(());
        }

        // The parameter is inline XML if it contains the root element.
        let xml = if file_param.contains("<Attributes>") {
            file_param
        } else {
            match std::fs::read_to_string(&file_param) {
                Ok(s) => s,
                Err(_) => {
                    port_base.set_int32_param(
                        params.attributes_status,
                        0,
                        ATTR_STATUS_FILE_NOT_FOUND,
                    )?;
                    return Err(asyn_rs::error::AsynError::Status {
                        status: asyn_rs::error::AsynStatus::Error,
                        message: format!("readNDAttributesFile: cannot open {file_param}"),
                    });
                }
            }
        };

        let macros = port_base
            .get_string_param(params.attributes_macros, 0)
            .map(octet_text)
            .unwrap_or_default();
        let xml = expand_attribute_macros(port_base, xml, &macros);

        match parse_attributes_xml(&xml, &self.functions) {
            Ok(attrs) => {
                for attr in attrs {
                    self.list.add(attr);
                }
                let unfed = self.feed_epics_pv_attributes();
                if unfed.is_empty() {
                    port_base.set_int32_param(params.attributes_status, 0, ATTR_STATUS_OK)?;
                    return Ok(());
                }
                port_base.set_int32_param(
                    params.attributes_status,
                    0,
                    ATTR_STATUS_XML_SYNTAX_ERROR,
                )?;
                Err(asyn_rs::error::AsynError::Status {
                    status: asyn_rs::error::AsynStatus::Error,
                    message: format!(
                        "readNDAttributesFile: no CA client to monitor EPICS_PV \
                         attribute(s) {}; they will evaluate Undefined",
                        unfed.join(", ")
                    ),
                })
            }
            Err(msg) => {
                port_base.set_int32_param(
                    params.attributes_status,
                    0,
                    ATTR_STATUS_XML_SYNTAX_ERROR,
                )?;
                Err(asyn_rs::error::AsynError::Status {
                    status: asyn_rs::error::AsynStatus::Error,
                    message: format!("readNDAttributesFile: {msg}"),
                })
            }
        }
    }

    /// Re-evaluate every live attribute, then return a snapshot of the list to
    /// attach to an outgoing NDArray.
    ///
    /// Port of C++ `asynNDArrayDriver::getAttributes` →
    /// `NDAttributeList::updateValues()`. `PARAM` attributes are refreshed from
    /// this driver's asyn parameter library (mirroring
    /// `paramAttribute::updateValue`, which reads `pDriver->getXxxParam`);
    /// `FUNCTION` attributes call their registered function; `EPICS_PV`
    /// attributes read whatever value a CA-monitor task last fed into their
    /// cell. `CONST` / `DRIVER` attributes are static.
    pub fn update(&mut self, port_base: &PortDriverBase) -> crate::attributes::NDAttributeList {
        let reads = self.param_reads(port_base, &[]);
        self.update_with(&reads, &[])
    }

    /// The store half of [`Self::update`]: every `PARAM` attribute's read,
    /// taken now. A read `pending` covers is not taken, since C would read
    /// the value it has just set (see [`Self::update_with`]).
    pub fn param_reads(&self, port_base: &PortDriverBase, pending: &[PendingParam]) -> ParamReads {
        let mut reads = Vec::new();
        for attr in self.list.iter() {
            let Some(src) = attr.param_source() else {
                continue;
            };
            let Some(index) = port_base.params.find_param(&src.param_name) else {
                continue;
            };
            let stored = port_base
                .params
                .get_value(index, src.addr)
                .ok()
                .map(std::mem::discriminant);
            let read = ParamRead {
                name: src.param_name.clone(),
                addr: src.addr,
                param_type: src.param_type,
                index,
                stored,
                value: None,
            };
            let value = match read.pending(pending) {
                Some(_) => None,
                None => Self::read_param_value(port_base, src),
            };
            reads.push(ParamRead { value, ..read });
        }
        ParamReads(reads)
    }

    /// Re-evaluate the set from `reads`, with `pending` — values set ahead
    /// of the call and not yet flushed — read in place of the store, as C's
    /// `getXxxParam` reads its own earlier `setXxxParam`. A pending value of
    /// another type than the stored one is one C's set refused, so the store
    /// is read. Nothing is written to the store.
    pub fn update_with(
        &mut self,
        reads: &ParamReads,
        pending: &[PendingParam],
    ) -> crate::attributes::NDAttributeList {
        for attr in self.list.iter() {
            let Some(src) = attr.param_source() else {
                continue;
            };
            let Some(read) = reads.0.iter().find(|r| {
                r.name == src.param_name && r.addr == src.addr && r.param_type == src.param_type
            }) else {
                continue;
            };
            let value = match read.pending(pending) {
                Some(value) => read_as(value, read.param_type),
                None => read.value.clone(),
            };
            if let Some(value) = value {
                src.cell().set(value);
            }
        }
        self.list.update_values();
        self.list.clone()
    }

    /// Read a `Param` attribute's current value from the asyn parameter
    /// library, using the read type the XML `datatype` selected — NOT the
    /// param's runtime type. Mirrors C++ `paramAttribute::updateValue`
    /// (paramAttribute.cpp:131-151), which dispatches on `paramType` to the
    /// matching `getXxxParam` and publishes a fixed `NDAttrDataType`.
    ///
    /// C sets the value whatever the getter's status, so an undefined value
    /// or a mismatched read type publishes the getter's zero-initialized
    /// local (`0` / `""`). Returns `None` (leaving the attribute's cell at its
    /// prior value) only when the param is unknown or the `datatype` was
    /// unrecognized (`ParamAttrType::Unknown` — C's `paramAttrTypeUnknown`,
    /// which never refreshes the attribute).
    fn read_param_value(
        port_base: &PortDriverBase,
        src: &crate::attributes::ParamAttributeSource,
    ) -> Option<NDAttrValue> {
        use crate::attributes::ParamAttrType;
        let index = port_base.params.find_param(&src.param_name)?;
        let addr = src.addr;
        match src.param_type {
            ParamAttrType::Int32 => Some(NDAttrValue::Int32(
                port_base.get_int32_param(index, addr).unwrap_or(0),
            )),
            ParamAttrType::Int64 => Some(NDAttrValue::Int64(
                port_base.get_int64_param(index, addr).unwrap_or(0),
            )),
            ParamAttrType::Float64 => Some(NDAttrValue::Float64(
                port_base.get_float64_param(index, addr).unwrap_or(0.0),
            )),
            ParamAttrType::String => Some(NDAttrValue::String(
                port_base
                    .get_string_param(index, addr)
                    .map(octet_text)
                    .unwrap_or_default(),
            )),
            ParamAttrType::Unknown => None,
        }
    }
}

/// A value set ahead of a `getAttributes` call: reason, address, value.
pub type PendingParam = (usize, i32, ParamValue);

/// The store's answers to a set's `PARAM` reads, taken in one pass on the
/// port (see [`DriverAttributes::param_reads`]).
#[derive(Debug, Clone, Default)]
pub struct ParamReads(Vec<ParamRead>);

#[derive(Debug, Clone)]
struct ParamRead {
    name: String,
    addr: i32,
    param_type: crate::attributes::ParamAttrType,
    index: usize,
    /// The stored value's type, which a pending set must match to land.
    stored: Option<std::mem::Discriminant<ParamValue>>,
    value: Option<NDAttrValue>,
}

impl ParamRead {
    /// The latest pending set of this param that C's set would have stored.
    fn pending<'a>(&self, pending: &'a [PendingParam]) -> Option<&'a ParamValue> {
        pending
            .iter()
            .rev()
            .find(|(reason, addr, _)| *reason == self.index && *addr == self.addr)
            .map(|(_, _, value)| value)
            .filter(|value| Some(std::mem::discriminant(*value)) == self.stored)
    }
}

/// `value` through the getter `param_type` names; a type the getter cannot
/// read gives its zero-initialized local, as `paramAttribute` publishes it.
fn read_as(
    value: &ParamValue,
    param_type: crate::attributes::ParamAttrType,
) -> Option<NDAttrValue> {
    use crate::attributes::ParamAttrType;
    match param_type {
        ParamAttrType::Int32 => Some(NDAttrValue::Int32(value.as_int32().unwrap_or(0))),
        ParamAttrType::Int64 => Some(NDAttrValue::Int64(value.as_int64().unwrap_or(0))),
        ParamAttrType::Float64 => Some(NDAttrValue::Float64(value.as_float64().unwrap_or(0.0))),
        ParamAttrType::String => Some(NDAttrValue::String(
            value.as_octet().map(octet_text).unwrap_or_default(),
        )),
        ParamAttrType::Unknown => None,
    }
}

/// The macro pass of C++ `readNDAttributesFile` (asynNDArrayDriver.cpp:345-381):
/// `macParseDefns` on `NDAttributesMacros`, then `macExpandString` over the
/// whole file against a handle with no environment behind it. Any failure —
/// an undefined macro included — is traced and the text goes on to the XML
/// parser unexpanded, as C's `goto done_macros` leaves `buffer` alone.
fn expand_attribute_macros(port_base: &PortDriverBase, xml: String, macros: &str) -> String {
    use epics_libcom_rs::runtime::mac_lib::{MacroExpandOptions, expand_macros, macro_defn_pairs};

    if macros.is_empty() {
        return xml;
    }
    // A name with no `=` is a deletion, which has nothing to delete in a
    // handle created for this one call.
    let defs: Vec<(String, String)> = macro_defn_pairs(macros)
        .into_iter()
        .filter_map(|(name, value)| value.map(|v| (name, v)))
        .collect();
    let expansion = expand_macros(&xml, defs, MacroExpandOptions::default());
    if expansion.errored() {
        port_base.trace_print(
            asyn_rs::trace::TraceMask::ERROR,
            format_args!("asynNDArrayDriver::readNDAttributesFile, error expanding macros\n"),
        );
        return xml;
    }
    expansion.text
}

/// C++ `asynNDArrayDriver::checkPath()` on a port's `FilePath`; see
/// [`NDArrayDriverBase::check_path`].
pub(crate) fn check_path(
    port_base: &mut PortDriverBase,
    params: &NDArrayDriverParams,
) -> AsynResult<bool> {
    let mut path = port_base
        .get_string_param(params.file_path, 0)
        .map(octet_text)?;
    if path.is_empty() {
        return Ok(true);
    }

    let exists = check_path_str(&mut path);
    port_base.set_string_param(params.file_path, 0, path)?;
    port_base.set_int32_param(params.file_path_exists, 0, exists as i32)?;
    Ok(exists)
}

/// How `asynNDArrayDriver::writeOctet` names itself in its closing line and
/// error message (asynNDArrayDriver.cpp:540-546).
pub const ASYN_ND_ARRAY_DRIVER: asyn_rs::port::OctetWriter = asyn_rs::port::OctetWriter {
    driver: "asynNDArrayDriver",
    name_key: "paramName",
};

/// The branches of C++ `asynNDArrayDriver::writeOctet` (asynNDArrayDriver.cpp
/// :526-538), as rules the `writeOctet` default runs after the store:
/// `NDAttributesFile` / `NDAttributesMacros` reload the attribute set, and
/// `FilePath` runs `checkPath`, then `createFilePath` bounded by `CREATE_DIR`
/// and `checkPath` again when the directory is missing.
///
/// A FilePath still missing fails the write: C overwrites checkPath's status
/// with callParamCallbacks' (:540), so the write succeeds; not copied. C never
/// takes the reload's status (:528).
pub(crate) fn write_octet_rules(
    port_base: &mut PortDriverBase,
    params: &NDArrayDriverParams,
    attributes: &Arc<parking_lot::Mutex<DriverAttributes>>,
) {
    port_base.set_octet_writer(ASYN_ND_ARRAY_DRIVER);
    let p = *params;
    let attrs = Arc::clone(attributes);
    let reload = move |base: &mut PortDriverBase, _addr, _text: &[u8]| {
        let _ = attrs.lock().read_file(base, &p);
        Ok(())
    };
    port_base.set_write_octet_rule(params.attributes_file, Arc::new(reload.clone()));
    port_base.set_write_octet_rule(params.attributes_macros, Arc::new(reload));
    port_base.set_write_octet_rule(
        params.file_path,
        Arc::new(move |base: &mut PortDriverBase, _addr, text: &[u8]| {
            let missing = |base: &mut PortDriverBase| matches!(check_path(base, &p), Ok(false));
            if missing(base) {
                let depth = base.get_int32_param(p.create_dir, 0).unwrap_or(0);
                let _ = NDArrayDriverBase::create_file_path(&octet_text(text), depth);
                if missing(base) {
                    return Err(asyn_rs::error::AsynError::Status {
                        status: asyn_rs::error::AsynStatus::Error,
                        message: format!("checkPath: {} does not exist", octet_text(text)),
                    });
                }
            }
            Ok(())
        }),
    );
}

/// C++ `asynNDArrayDriver::setIntegerParam` (asynNDArrayDriver.cpp:636-663),
/// which every store of `ADAcquire` or `NDNumQueuedArrays` reaches, whoever
/// makes it. A stop clears `ADAcquireBusy` at once unless `ADWaitForPlugins`
/// is set and arrays are still queued; it reads the live count, because the
/// `NDNumQueuedArrays` parameter lags it (:633-635). The parameter is
/// stored by the port's updater thread, which the drain branch waits on.
pub(crate) fn acquire_busy_rules(
    port_base: &mut PortDriverBase,
    params: &NDArrayDriverParams,
    queued: &Arc<QueuedArrayCounter>,
) {
    queued_count_updater(port_base, params.num_queued_arrays, queued);
    let (acquire, busy, wait) = (params.acquire, params.acquire_busy, params.wait_for_plugins);
    let queued = Arc::clone(queued);
    port_base.set_int32_rule(
        acquire,
        Arc::new(move |base: &mut PortDriverBase, list, value| {
            if value != 0 {
                base.set_int32_param(busy, list, 1)
            } else if base.get_int32_param(wait, list).unwrap_or(0) == 0 || queued.get() == 0 {
                base.set_int32_param(busy, list, 0)
            } else {
                Ok(())
            }
        }),
    );
    port_base.set_int32_rule(
        params.num_queued_arrays,
        Arc::new(move |base: &mut PortDriverBase, list, value| {
            if value == 0 && base.get_int32_param(acquire, list).unwrap_or(0) == 0 {
                base.set_int32_param(busy, list, 0)
            } else {
                Ok(())
            }
        }),
    );
}

/// C's `<port>_updateQueuedArrayCount` thread (asynNDArrayDriver.cpp:782-797,
/// created :1011-1021): on each change of the live count, store it as
/// `NDNumQueuedArrays` and run the callbacks. Started once the port has a
/// handle; it ends when the counter is dropped or the port stops.
fn queued_count_updater(
    port_base: &mut PortDriverBase,
    num_queued_arrays: usize,
    queued: &Arc<QueuedArrayCounter>,
) {
    let counter = Arc::downgrade(queued);
    port_base.on_runtime(move |handle| {
        let Some(queued) = counter.upgrade() else {
            return;
        };
        let port = handle.port_name().to_string();
        let trace = handle.trace().clone();
        // Weak, so the updater never keeps its own port running.
        let port_handle = handle.downgrade();
        let spawned = std::thread::Builder::new()
            .name(format!("{port}_updateQueuedArrayCount"))
            .spawn(move || {
                loop {
                    std::thread::park();
                    let Some(queued) = counter.upgrade() else {
                        break;
                    };
                    if !queued.take_signal() {
                        continue;
                    }
                    let count = queued.get() as i32;
                    drop(queued);
                    let Some(handle) = port_handle.upgrade() else {
                        break;
                    };
                    let update = ParamSetValue::new(num_queued_arrays, 0, ParamValue::Int32(count));
                    if handle.set_params_and_notify_blocking(0, vec![update]).is_err() {
                        break;
                    }
                }
            });
        match spawned {
            Ok(thread) => queued.set_updater(thread.thread().clone()),
            Err(_) => trace.print(
                Some(&port),
                None,
                0,
                asyn_rs::trace::TraceMask::ERROR,
                format_args!(
                    "asynNDArrayDriver::asynNDArrayDriver error creating updateQueuedArrayCount thread\n"
                ),
            ),
        }
    });
}

/// What C reaches through `pArray->pDriver`: the driver an array was
/// allocated for. Its pool holds it weakly, so every array from that pool
/// leads back to the driver's queued count, port and attribute set.
pub struct ArrayOwner {
    queued: Arc<QueuedArrayCounter>,
    port: std::sync::OnceLock<asyn_rs::port_handle::WeakPortHandle>,
    attributes: Arc<parking_lot::Mutex<DriverAttributes>>,
    arrays: parking_lot::Mutex<Vec<Option<Held>>>,
}

/// [`ArrayOwner::persistent`]'s array, refilled and published in place. Only
/// [`ArrayOwner::replace_persistent`] replaces it, releasing the old buffer
/// before the new one is allocated, as C does.
///
/// ```compile_fail,E0308
/// # use ad_core_rs::ndarray::{NDDataType, NDDimension};
/// # use ad_core_rs::ndarray_pool::{DriverPool, PersistentArray};
/// # let pool = DriverPool::standalone(16);
/// # let alloc = || pool.alloc(vec![NDDimension::new(2)], NDDataType::Int32).unwrap();
/// # let owner = pool.owner().clone();
/// owner.replace_persistent(0, || Some(alloc()));
/// let next = PersistentArray::new(alloc());
/// owner.persistent(0, |p| *p = next);
/// ```
pub struct PersistentArrayMut<'a>(&'a mut PersistentArray);

impl PersistentArrayMut<'_> {
    /// [`PersistentArray::refill`].
    pub fn refill(&mut self) -> NDArrayMut<'_> {
        self.0.refill()
    }

    /// [`PersistentArray::published`].
    pub fn published(&mut self) -> PublishedArray {
        self.0.published()
    }
}

impl std::ops::Deref for PersistentArrayMut<'_> {
    type Target = NDArray;

    fn deref(&self) -> &NDArray {
        self.0
    }
}

/// What C's `pArrays[addr]` points at.
enum Held {
    /// An array handed over whole, which nothing changes after.
    Shared(Arc<NDArray>),
    /// An array the driver goes on writing into, as NDDriverStdArrays does
    /// its `pArrays[0]` (NDDriverStdArrays.cpp:220-250).
    Persistent(Box<PersistentArray>),
}

impl Held {
    /// The array a reader gets: a persistent one's current publish.
    fn array(&mut self) -> Arc<NDArray> {
        match self {
            Self::Shared(array) => array.clone(),
            Self::Persistent(array) => array.published().array().clone(),
        }
    }
}

impl ArrayOwner {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            queued: Arc::new(QueuedArrayCounter::new()),
            port: std::sync::OnceLock::new(),
            attributes: Arc::new(parking_lot::Mutex::new(DriverAttributes::new())),
            arrays: parking_lot::Mutex::new(Vec::new()),
        })
    }

    /// Fill in the port handle once the port being built runs.
    pub(crate) fn bind_port(self: &Arc<Self>, port_base: &mut PortDriverBase) {
        let weak = Arc::downgrade(self);
        port_base.on_runtime(move |handle| {
            if let Some(owner) = weak.upgrade() {
                let _ = owner.port.set(handle.downgrade());
            }
        });
    }

    /// C++ `queuedArrayCount_`.
    pub fn queued(&self) -> &Arc<QueuedArrayCounter> {
        &self.queued
    }

    /// The driver's port, `None` before it runs or once it has stopped.
    pub fn port(&self) -> Option<asyn_rs::port_handle::PortHandle> {
        self.port.get()?.upgrade()
    }

    /// The `NDAttributesFile` attribute set (C++ `pAttributeList`).
    pub fn attributes(&self) -> &Arc<parking_lot::Mutex<DriverAttributes>> {
        &self.attributes
    }

    /// C++ `pArrays[addr]` (asynNDArrayDriver.h:238): the driver's last
    /// array at `addr`, which keeps its buffer charged; `pArrays[0]` is the
    /// `preAllocateBuffers` template (asynNDArrayDriver.cpp:717-726).
    /// Returns what the slot held.
    pub fn store_array(&self, addr: usize, array: Arc<NDArray>) -> Option<Arc<NDArray>> {
        Self::slot(&mut self.arrays.lock(), addr)
            .replace(Held::Shared(array))
            .map(|mut held| held.array())
    }

    /// The array [`ArrayOwner::store_array`] last left at `addr`.
    pub fn array(&self, addr: usize) -> Option<Arc<NDArray>> {
        self.arrays.lock().get_mut(addr)?.as_mut().map(Held::array)
    }

    /// C's driver releasing `pArrays[addr]` and leaving it NULL, as Timepix3
    /// does to a band it no longer fills (serval_stream.cpp:476-479).
    /// Returns what the slot held.
    pub fn release_array(&self, addr: usize) -> Option<Arc<NDArray>> {
        self.arrays
            .lock()
            .get_mut(addr)?
            .take()
            .map(|mut held| held.array())
    }

    /// C's driver releasing `pArrays[addr]` and allocating the next array
    /// into it under the driver lock, as computeImage does
    /// (simDetector.cpp:694-699): a reader waits through `make` and finds
    /// the new array. A refused `make` leaves the slot empty, as C's
    /// refused alloc leaves it NULL.
    pub fn replace_array(
        &self,
        addr: usize,
        make: impl FnOnce() -> Option<DriverArray>,
    ) -> Option<PublishedArray> {
        let mut arrays = self.arrays.lock();
        let slot = Self::slot(&mut arrays, addr);
        // Released first, so `make` can reuse its buffer.
        drop(slot.take());
        let array = make()?.share();
        *slot = Some(Held::Shared(array.array().clone()));
        Some(array)
    }

    /// [`ArrayOwner::replace_array`] for an array the driver then writes into
    /// in place, as NDDriverStdArrays allocates its `pArrays[0]`
    /// (NDDriverStdArrays.cpp:153-158). `false` is a refused `make`.
    pub fn replace_persistent(
        &self,
        addr: usize,
        make: impl FnOnce() -> Option<DriverArray>,
    ) -> bool {
        let mut arrays = self.arrays.lock();
        let slot = Self::slot(&mut arrays, addr);
        drop(slot.take());
        *slot = make().map(|array| Held::Persistent(Box::new(PersistentArray::new(array))));
        slot.is_some()
    }

    /// The array [`ArrayOwner::replace_persistent`] left at `addr`, under the
    /// lock, so a reader waits through `f`; `None` when the slot holds none.
    pub fn persistent<R>(
        &self,
        addr: usize,
        f: impl FnOnce(&mut PersistentArrayMut<'_>) -> R,
    ) -> Option<R> {
        match self.arrays.lock().get_mut(addr)? {
            Some(Held::Persistent(array)) => Some(f(&mut PersistentArrayMut(array))),
            _ => None,
        }
    }

    /// C's driver changing its `pArrays[addr]` in place under the driver
    /// lock, as `simTask` stamps `pArrays[0]` after the exposure wait
    /// (simDetector.cpp:824-828): a reader waits through `change` and finds
    /// the changed array. An array someone else still holds is changed in
    /// a copy.
    pub fn refill(
        &self,
        addr: usize,
        array: PublishedArray,
        change: impl FnOnce(&mut NDArrayMut<'_>),
    ) -> crate::error::ADResult<PublishedArray> {
        let mut arrays = self.arrays.lock();
        let slot = Self::slot(&mut arrays, addr);
        // The slot's reference goes first, so `array` can be changed in place.
        drop(slot.take());
        let mut array = match array.try_into_driver() {
            Ok(array) => array,
            Err(shared) => match shared.pool().alloc_copy(&shared) {
                Ok(copy) => copy,
                Err(e) => {
                    *slot = Some(Held::Shared(shared.array().clone()));
                    return Err(e);
                }
            },
        };
        change(&mut array.fill());
        let array = array.share();
        *slot = Some(Held::Shared(array.array().clone()));
        Ok(array)
    }

    fn slot(arrays: &mut Vec<Option<Held>>, addr: usize) -> &mut Option<Held> {
        if arrays.len() <= addr {
            arrays.resize_with(addr + 1, || None);
        }
        &mut arrays[addr]
    }

    /// C++ `getAttributes(pArray->pAttributeList)` from a driver's own task
    /// (simDetector.cpp:828), which holds the driver lock: the set is
    /// re-evaluated on the port's actor, where its `PARAM` values live, and
    /// merged onto an array's `attributes`. An empty set or a stopped port
    /// leaves them alone.
    pub async fn get_attributes(
        &self,
        attributes: &mut crate::attributes::NDAttributeList,
    ) -> AsynResult<()> {
        if let Some(fresh) = self.evaluate_attributes().await? {
            attributes.copy_from(&fresh);
        }
        Ok(())
    }

    /// [`ArrayOwner::get_attributes`]'s evaluated set, for an array the task
    /// does not hold across the await; `None` for an empty set or a stopped
    /// port.
    pub async fn evaluate_attributes(
        &self,
    ) -> AsynResult<Option<crate::attributes::NDAttributeList>> {
        if self.attributes.lock().list.is_empty() {
            return Ok(None);
        }
        let Some(port) = self.port() else {
            return Ok(None);
        };
        let attributes = self.attributes.clone();
        let fresh = Arc::new(parking_lot::Mutex::new(None));
        let slot = fresh.clone();
        let call = asyn_rs::request::DriverCall::new(move |driver| {
            *slot.lock() = Some(attributes.lock().update(driver.base()));
        });
        port.submit_async(
            asyn_rs::request::RequestOp::WithDriver(call),
            asyn_rs::user::AsynUser::default(),
        )
        .await?;
        Ok(fresh.lock().take())
    }

    /// C++ `getAttributes(pList)` from a plugin's `processCallbacks`, which
    /// holds the port lock: the set re-evaluated on the port's actor, `None`
    /// when it is empty or the port has stopped. `pending` are values the
    /// plugin has set ahead of the call but not yet flushed; a `PARAM`
    /// attribute reads them as C reads its own sets
    /// ([`DriverAttributes::update_with`]). Blocks the caller, which must not
    /// be the port's actor.
    pub fn evaluate_attributes_blocking(
        &self,
        pending: Vec<PendingParam>,
    ) -> Option<crate::attributes::NDAttributeList> {
        if self.attributes.lock().list.is_empty() {
            return None;
        }
        let port = self.port()?;
        let attributes = self.attributes.clone();
        let fresh = Arc::new(parking_lot::Mutex::new(None));
        let slot = fresh.clone();
        let call = asyn_rs::request::DriverCall::new(move |driver| {
            let base = driver.base();
            let mut attributes = attributes.lock();
            let reads = attributes.param_reads(base, &pending);
            *slot.lock() = Some(attributes.update_with(&reads, &pending));
        });
        let _ = port.submit_blocking(
            asyn_rs::request::RequestOp::WithDriver(call),
            asyn_rs::user::AsynUser::default(),
        );
        fresh.lock().take()
    }

    /// The store half of [`Self::evaluate_attributes_blocking`], for a
    /// caller that evaluates later against what has been set by then. `None`
    /// when the set is empty or the port has stopped.
    pub fn param_reads_blocking(&self, pending: Vec<PendingParam>) -> Option<ParamReads> {
        if self.attributes.lock().list.is_empty() {
            return None;
        }
        let port = self.port()?;
        let attributes = self.attributes.clone();
        let taken = Arc::new(parking_lot::Mutex::new(None));
        let slot = taken.clone();
        let call = asyn_rs::request::DriverCall::new(move |driver| {
            *slot.lock() = Some(attributes.lock().param_reads(driver.base(), &pending));
        });
        let _ = port.submit_blocking(
            asyn_rs::request::RequestOp::WithDriver(call),
            asyn_rs::user::AsynUser::default(),
        );
        taken.lock().take()
    }
}

pub struct NDArrayDriverBase {
    pub port_base: PortDriverBase,
    pub params: NDArrayDriverParams,
    pub pool: DriverPool,
    pub array_output: NDArrayOutput,
    pub owner: Arc<ArrayOwner>,
}

/// The interfaces `asynNDArrayDriver` ORs into its subclass's
/// (asynNDArrayDriver.cpp:866).
pub const ND_ARRAY_DRIVER_INTERFACES: InterfaceMask = InterfaceMask::INT32
    .or(InterfaceMask::FLOAT64)
    .or(InterfaceMask::OCTET)
    .or(InterfaceMask::INT32_ARRAY)
    .or(InterfaceMask::GENERIC_POINTER)
    .or(InterfaceMask::DRV_USER);

/// The interrupts `asynNDArrayDriver` ORs into its subclass's
/// (asynNDArrayDriver.cpp:867).
pub const ND_ARRAY_DRIVER_INTERRUPTS: InterfaceMask = InterfaceMask::INT32
    .or(InterfaceMask::FLOAT64)
    .or(InterfaceMask::OCTET)
    .or(InterfaceMask::INT32_ARRAY)
    .or(InterfaceMask::GENERIC_POINTER);

impl NDArrayDriverBase {
    /// `interface_mask` / `interrupt_mask` are the subclass's own, as C's
    /// constructor arguments.
    pub fn new(
        port_name: &str,
        max_addr: usize,
        max_memory: usize,
        asyn_flags: PortFlags,
        interface_mask: InterfaceMask,
        interrupt_mask: InterfaceMask,
    ) -> AsynResult<Self> {
        // C takes `maxAddr` and `asynFlags` from the driver (asynNDArrayDriver.cpp:862-868).
        let mut port_base = PortDriverBase::new(
            port_name,
            max_addr,
            asyn_flags,
            interface_mask | ND_ARRAY_DRIVER_INTERFACES,
            interrupt_mask | ND_ARRAY_DRIVER_INTERRUPTS,
        );

        let (params, owner) = NDArrayDriverParams::create(&mut port_base)?;
        let pool = DriverPool::for_driver(max_memory, &owner);

        init_read_only_params(&mut port_base, &params, &pool, port_name)?;

        Ok(Self {
            port_base,
            params,
            pool,
            array_output: NDArrayOutput::new(),
            owner,
        })
    }

    /// Connect a downstream channel-based receiver.
    pub fn connect_downstream(&mut self, sender: NDArraySender) {
        self.array_output.add(sender);
    }

    /// Handle a write to a pool-control Int32 parameter (`POOL_EMPTY_FREELIST`,
    /// `POOL_POLL_STATS`, `POOL_PRE_ALLOC_BUFFERS`), mirroring the pool branch
    /// of C++ `asynNDArrayDriver::writeInt32`.
    ///
    /// Returns `true` when `param_index` was a recognized pool-control
    /// parameter. Driver layers route their `writeInt32` through this so the
    /// `POOL_*` parameters act on the pool instead of being dead.
    pub fn write_int32_pool(&mut self, param_index: usize, _value: i32) -> AsynResult<bool> {
        let template = self.owner.array(0);
        handle_pool_write_int32(
            &mut self.port_base,
            &self.params,
            &self.pool,
            param_index,
            template.as_deref(),
        )
    }

    /// Number of connected downstream channels.
    pub fn num_plugins(&self) -> usize {
        self.array_output.num_senders()
    }

    /// Updates driver param cache and fires param callbacks for a new array.
    /// If array callbacks are enabled, returns the array that the caller must
    /// publish asynchronously to downstream consumers via
    /// `array_output.publish(arr).await`.
    ///
    /// This function does NOT publish the array — the caller is responsible
    /// for that in an async context. Returns `None` when callbacks are disabled.
    pub fn prepare_array(&mut self, mut array: DriverArray) -> AsynResult<Option<PublishedArray>> {
        let counter = self
            .port_base
            .get_int32_param(self.params.array_counter, 0)?
            + 1;
        self.port_base
            .set_int32_param(self.params.array_counter, 0, counter)?;

        // G10: re-evaluate every live attribute (PARAM from the parameter
        // library, FUNCTION from the registry, EPICS_PV from its CA cell) and
        // merge the fresh values onto the outgoing array. Port of C++
        // `asynNDArrayDriver::doCallbacksGenericPointer` calling
        // `getAttributes(pArray->pAttributeList)` before the callback.
        self.owner
            .attributes
            .lock()
            .attach(&self.port_base, array.fill().attributes_mut());

        // G5/G6/G7: write all per-array parameters (size, dims, type, color,
        // Bayer, timestamps, codec).
        write_array_params(&mut self.port_base, &self.params, &array)?;
        let array = array.share();

        self.owner.store_array(0, array.array().clone());

        // Update pool stats
        self.port_base.set_float64_param(
            self.params.pool_used_memory,
            0,
            self.pool.allocated_bytes() as f64 / 1_048_576.0,
        )?;
        self.port_base.set_int32_param(
            self.params.pool_free_buffers,
            0,
            self.pool.num_free_buffers() as i32,
        )?;
        self.port_base.set_int32_param(
            self.params.pool_alloc_buffers,
            0,
            self.pool.num_alloc_buffers() as i32,
        )?;

        let callbacks_enabled = self
            .port_base
            .get_int32_param(self.params.array_callbacks, 0)
            .unwrap_or(0)
            != 0;

        let to_publish = if callbacks_enabled {
            // C doCallbacksGenericPointer(pImage, NDArrayData, 0): posted
            // here, ahead of the flush below, and stored nowhere.
            self.port_base.do_callbacks_array(
                self.params.ndarray_data,
                0,
                ParamValue::GenericPointer(array.array().clone()),
            )?;
            Some(array)
        } else {
            None
        };

        self.port_base.call_param_callbacks(0)?;

        Ok(to_publish)
    }

    /// Construct a file path from template, path, name, and number.
    ///
    /// Matches C++ `asynNDArrayDriver::createFileName` which uses
    /// `epicsSnprintf(fullFileName, maxChars, fileTemplate, filePath, fileName, fileNumber)`.
    /// The template is a C printf format string, e.g., `"%s%s_%3.3d.dat"`.
    pub fn create_file_name(&mut self) -> AsynResult<String> {
        // asynNDArrayDriver.cpp:203 — createFileName calls checkPath() before
        // reading any parameter, so FilePath is normalized (trailing separator
        // written back to the parameter) and FilePathExists refreshed on every
        // call. Without it the default template "%s%s_%3.3d.dat" run-togethers a
        // separator-less FilePath into "/dataimg_000.dat". C discards checkPath's
        // status here, so a missing directory does not abort the name.
        self.check_path()?;

        // An octet parameter is bytes; a file name is text. Decode here, at
        // the one place these three become a path.
        let path = octet_text(self.port_base.get_string_param(self.params.file_path, 0)?);
        let name = octet_text(self.port_base.get_string_param(self.params.file_name, 0)?);
        let number = self.port_base.get_int32_param(self.params.file_number, 0)?;
        let template = octet_text(
            self.port_base
                .get_string_param(self.params.file_template, 0)?,
        );
        let auto_increment = self
            .port_base
            .get_int32_param(self.params.auto_increment, 0)
            .unwrap_or(0);

        // C parity: an empty FILE_TEMPLATE is passed straight to epicsSnprintf,
        // which yields an empty string. Do NOT fabricate a default template.
        // sprintf_template handles the empty case correctly (no specifiers).
        let full = sprintf_template(&template, &path, &name, number);

        self.port_base
            .set_string_param(self.params.full_file_name, 0, &full)?;

        // C++: auto-increment file number after creating filename
        if auto_increment != 0 {
            self.port_base
                .set_int32_param(self.params.file_number, 0, number + 1)?;
        }

        Ok(full)
    }

    /// Stamp `epicsTS` and the derived double `timeStamp` on `array`.
    ///
    /// C++ `asynNDArrayDriver::updateTimeStamps` (asynNDArrayDriver.cpp:832-836).
    /// The time is the port's `updateTimeStamp`, so the frame and the
    /// callbacks that follow carry the same stamp, from a registered source
    /// when there is one; `timeStamp` is always derived from `epicsTS` — the
    /// two cannot disagree. `NDArrayPool::alloc` sets neither; this is the
    /// only place that sets them for a newly produced frame.
    pub fn update_time_stamps(&mut self, array: &mut NDArrayMut<'_>) {
        array.update_time_stamps(self.port_base.update_time_stamp().into());
    }

    /// Check whether the `FILE_PATH` directory exists, normalizing the path.
    ///
    /// C++ `asynNDArrayDriver::checkPath()` (asynNDArrayDriver.cpp:98-109): an
    /// empty FilePath returns early — neither the path nor `FilePathExists` is
    /// written. Otherwise the path is normalized in place by
    /// [`check_path_str`], written back to the parameter, and `FilePathExists`
    /// is refreshed. Returns C's status: `false` only for a missing directory,
    /// so an empty FilePath never reaches createFilePath (:530-531).
    pub fn check_path(&mut self) -> AsynResult<bool> {
        check_path(&mut self.port_base, &self.params)
    }

    /// Recursively create the directory components of `path`.
    ///
    /// Mirrors C++ `asynNDArrayDriver::createFilePath`: directory parts at
    /// index `>= path_depth` are created (parts before that depth are assumed
    /// to already exist). A `path_depth` of 0 is a no-op; a negative
    /// `path_depth` counts from the end (`num_parts + path_depth`, clamped to a
    /// minimum of 1). `EEXIST` is not an error.
    pub fn create_file_path(path: &str, path_depth: i32) -> AsynResult<()> {
        if path_depth == 0 {
            return Ok(());
        }

        // Leading prefix to preserve verbatim: an optional Windows drive
        // designator ("C:") plus any leading path separators.
        let bytes: Vec<char> = path.chars().collect();
        let mut i = 0usize;
        let mut prefix = String::new();
        if bytes.len() >= 2 && bytes[1] == ':' {
            prefix.push(bytes[0]);
            prefix.push(':');
            i = 2;
        }
        while i < bytes.len() && (bytes[i] == '/' || bytes[i] == '\\') {
            prefix.push(bytes[i]);
            i += 1;
        }

        let rest: String = bytes[i..].iter().collect();
        let parts: Vec<&str> = rest.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
        let num_parts = parts.len() as i32;

        let mut depth = path_depth;
        if depth < 0 {
            depth += num_parts;
            if depth < 1 {
                depth = 1;
            }
        }

        let mut next_dir = prefix;
        for (idx, part) in parts.iter().enumerate() {
            next_dir.push_str(part);
            if idx as i32 >= depth {
                match std::fs::create_dir(&next_dir) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e.into()),
                }
            }
            next_dir.push('/');
        }
        Ok(())
    }

    /// C++ `asynNDArrayDriver::writeOctet`, for an override to chain to.
    pub fn write_octet(&mut self, user: &AsynUser, data: &[u8]) -> AsynResult<usize> {
        self.port_base.write_octet(user, data)
    }

    /// See [`DriverAttributes::set_ca_client`].
    #[cfg(feature = "ioc")]
    pub fn set_ca_client(
        &mut self,
        client: std::sync::Arc<epics_ca_rs::client::CaClient>,
        runtime: tokio::runtime::Handle,
    ) {
        self.owner.attributes.lock().set_ca_client(client, runtime);
    }

    /// See [`DriverAttributes::read_file`].
    pub fn read_nd_attributes_file(&mut self) -> AsynResult<()> {
        self.owner
            .attributes
            .lock()
            .read_file(&mut self.port_base, &self.params)
    }

    /// Access the driver's NDArray attribute list (populated by
    /// `read_nd_attributes_file`).
    pub fn attributes(&self) -> crate::attributes::NDAttributeList {
        self.owner.attributes.lock().list.clone()
    }

    /// See [`DriverAttributes::update`].
    pub fn update_attributes(&mut self) -> crate::attributes::NDAttributeList {
        self.owner.attributes.lock().update(&self.port_base)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::plugin::channel::ndarray_channel;
    use crate::plugin::runtime::ASYN_CANBLOCK;

    /// Assert the whole of C++ `asynNDArrayDriver.cpp:954-1005` through the
    /// *strict* getters — the ones a record read goes through
    /// (`get_int32_strict`, asyn-rs/src/port.rs:1663). A param the
    /// constructor never wrote fails here exactly as its record would sit
    /// UDF/INVALID on a live IOC.
    pub(crate) fn assert_c_read_only_block(
        base: &PortDriverBase,
        params: &NDArrayDriverParams,
        port_name: &str,
    ) {
        for (name, index, want) in [
            ("PORT_NAME_SELF", params.port_name_self, port_name),
            ("ADCORE_VERSION", params.ad_core_version, "3.14.0"),
            ("DRIVER_VERSION", params.driver_version, "3.14.0"),
            ("FILE_WRITE_MESSAGE", params.file_write_message, ""),
            ("FILE_PATH", params.file_path, ""),
            ("FILE_NAME", params.file_name, ""),
            ("FILE_TEMPLATE", params.file_template, "%s%s_%3.3d.dat"),
            ("TEMP_SUFFIX", params.temp_suffix, ""),
            ("ND_ATTRIBUTES_FILE", params.attributes_file, ""),
            ("ND_ATTRIBUTES_MACROS", params.attributes_macros, ""),
        ] {
            assert_eq!(
                base.get_string_param(index, 0)
                    .unwrap_or_else(|e| { panic!("{name} unset after construction: {e:?}") }),
                want.as_bytes(),
                "{name}"
            );
        }

        for (name, index, want) in [
            ("ARRAY_SIZE_X", params.array_size_x, 0),
            ("ARRAY_SIZE_Y", params.array_size_y, 0),
            ("ARRAY_SIZE_Z", params.array_size_z, 0),
            ("ARRAY_SIZE", params.array_size, 0),
            ("ND_DIMENSIONS", params.n_dimensions, 0),
            ("COLOR_MODE", params.color_mode, NDColorMode::Mono as i32),
            ("UNIQUE_ID", params.unique_id, 0),
            ("EPICS_TS_SEC", params.epics_ts_sec, 0),
            ("EPICS_TS_NSEC", params.epics_ts_nsec, 0),
            ("BAYER_PATTERN", params.bayer_pattern, 0),
            ("ARRAY_COUNTER", params.array_counter, 0),
            ("WRITE_FILE", params.write_file, 0),
            ("READ_FILE", params.read_file, 0),
            ("CAPTURE", params.capture, 0),
            ("FILE_WRITE_STATUS", params.file_write_status, 0),
            ("FILE_NUMBER", params.file_number, 0),
            ("AUTO_INCREMENT", params.auto_increment, 0),
            ("NUM_CAPTURED", params.num_captured, 0),
            ("FREE_CAPTURE", params.free_capture, 0),
            ("CREATE_DIR", params.create_dir, 0),
            (
                "ND_ATTRIBUTES_STATUS",
                params.attributes_status,
                ATTR_STATUS_FILE_NOT_FOUND,
            ),
            ("NUM_QUEUED_ARRAYS", params.num_queued_arrays, 0),
        ] {
            assert_eq!(
                base.get_int32_param(index, 0)
                    .unwrap_or_else(|e| panic!("{name} unset after construction: {e:?}")),
                want,
                "{name}"
            );
        }

        assert_eq!(
            base.get_float64_param(params.timestamp_rbv, 0)
                .expect("TIME_STAMP unset after construction"),
            0.0
        );
    }

    /// The four pool statistics, C `:1000-1003`, for a pool nothing has
    /// allocated from yet.
    pub(crate) fn assert_fresh_pool_stats(base: &PortDriverBase, params: &NDArrayDriverParams) {
        assert_eq!(
            base.get_int32_param(params.pool_alloc_buffers, 0)
                .expect("POOL_ALLOC_BUFFERS unset after construction"),
            0
        );
        assert_eq!(
            base.get_int32_param(params.pool_free_buffers, 0)
                .expect("POOL_FREE_BUFFERS unset after construction"),
            0
        );
        assert_eq!(
            base.get_float64_param(params.pool_used_memory, 0)
                .expect("POOL_USED_MEMORY unset after construction"),
            0.0
        );
        assert_eq!(
            base.get_float64_param(params.pool_max_memory, 0)
                .expect("POOL_MAX_MEMORY unset after construction"),
            0.0
        );
    }

    pub(crate) type Posts = Arc<
        parking_lot::Mutex<
            Vec<(
                usize,
                Option<asyn_rs::interfaces::InterfaceType>,
                ParamValue,
            )>,
        >,
    >;

    /// Every post `base` makes from here on: reason, interface, value.
    pub(crate) fn record_posts(
        base: &mut PortDriverBase,
    ) -> (Posts, asyn_rs::interrupt::SyncCallbackSubscription) {
        use asyn_rs::interrupt::{InterruptFilter, InterruptValue};
        use epics_libcom_rs::runtime::interrupt_accept::InterruptGate;
        base.own_interrupts(InterruptGate::opened());
        let posts = Posts::default();
        let recorder = posts.clone();
        let sub = base
            .interrupts
            .register_sync_callback(InterruptFilter::default(), move |iv: &InterruptValue| {
                recorder
                    .lock()
                    .push((iv.reason, iv.iface, iv.value.clone()))
            })
            .unwrap();
        (posts, sub)
    }

    /// A driver's `doCallbacksGenericPointer(pImage, NDArrayData, 0)`
    /// (simDetector.cpp:834): the frame `id`, posted typed ahead of the
    /// frame's callParamCallbacks.
    pub(crate) fn assert_ndarray_data_ahead_of_params(
        posts: &Posts,
        params: &NDArrayDriverParams,
        id: i32,
    ) {
        use asyn_rs::interfaces::InterfaceType;
        let posts = posts.lock();
        let at = |reason| posts.iter().position(|(r, ..)| *r == reason);
        let data = at(params.ndarray_data).map(|i| match &posts[i] {
            (_, iface, ParamValue::GenericPointer(p)) => {
                (*iface, p.downcast_ref::<NDArray>().map(|a| a.unique_id))
            }
            (_, iface, _) => (*iface, None),
        });
        assert_eq!(
            data,
            Some((Some(InterfaceType::GenericPointer), Some(id))),
            "{posts:?}"
        );
        assert!(
            at(params.ndarray_data) < at(params.array_counter),
            "{posts:?}"
        );
    }

    #[test]
    fn a_frame_posts_ndarray_data_ahead_of_its_params() {
        let mut drv = NDArrayDriverBase::new(
            "TEST",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        drv.port_base
            .set_int32_param(drv.params.array_callbacks, 0, 1)
            .unwrap();
        let (posts, _sub) = record_posts(&mut drv.port_base);
        let arr = drv
            .pool
            .alloc(
                vec![crate::ndarray::NDDimension::new(8)],
                crate::ndarray::NDDataType::UInt8,
            )
            .unwrap();
        let id = arr.unique_id;
        drv.prepare_array(arr).unwrap();
        assert_ndarray_data_ahead_of_params(&posts, &drv.params, id);
    }

    /// C's `pArrays[addr]->release(); pArrays[addr] = NULL`
    /// (serval_stream.cpp:476-479) empties the slot and lets the buffer go.
    #[test]
    fn release_array_empties_the_slot_and_frees_the_buffer() {
        let pool = crate::ndarray_pool::DriverPool::standalone(1_000_000);
        let owner = pool.owner().clone();
        let image = pool
            .alloc(
                vec![crate::ndarray::NDDimension::new(4)],
                crate::ndarray::NDDataType::UInt8,
            )
            .unwrap()
            .share();
        owner.store_array(2, image.array().clone());
        drop(image);
        assert!(owner.release_array(2).is_some());
        assert!(owner.array(2).is_none());
        assert_eq!(pool.num_free_buffers(), 1);
        assert!(owner.release_array(5).is_none());
    }

    /// NDDriverStdArrays's `pArrays[0]`: written in place whoever holds its
    /// publish, read as its current publish, on its one pool charge.
    #[test]
    fn a_persistent_slot_is_written_in_place_and_read_as_its_publish() {
        use crate::ndarray::{NDDataType, NDDataView, NDDimension};
        let pool = crate::ndarray_pool::DriverPool::standalone(1_000_000);
        let owner = pool.owner().clone();
        let f64s = |a: &NDArray| match a.data().view() {
            NDDataView::F64(v) => v.to_vec(),
            other => panic!("{:?}", other.data_type()),
        };
        assert!(owner.replace_persistent(0, || {
            pool.alloc(vec![NDDimension::new(2)], NDDataType::Float64)
                .ok()
        }));
        let held = owner.persistent(0, |a| a.published()).unwrap();
        owner.persistent(0, |a| a.refill().data_mut().fill_from_f64(&[1.0, 2.0]));
        assert_eq!(f64s(&owner.array(0).unwrap()), [1.0, 2.0]);
        assert_eq!(f64s(held.array()), [0.0, 0.0]);
        assert_eq!(pool.num_alloc_buffers(), 1);
        drop(held);

        assert!(!owner.replace_persistent(0, || None));
        assert!(owner.array(0).is_none());
        assert!(owner.persistent(0, |_| ()).is_none());
        assert_eq!(pool.num_free_buffers(), 1);
    }

    /// C releases the old array before allocating its replacement
    /// (NDDriverStdArrays.cpp:153-158), so a pool with room for one buffer
    /// still replaces it.
    #[test]
    fn a_persistent_replacement_releases_the_old_buffer_first() {
        use crate::ndarray::{NDDataType, NDDimension};
        let pool = crate::ndarray_pool::DriverPool::standalone(8);
        let owner = pool.owner().clone();
        let alloc = || {
            pool.alloc(vec![NDDimension::new(2)], NDDataType::Int32)
                .ok()
        };
        assert!(owner.replace_persistent(0, alloc));
        assert!(owner.replace_persistent(0, alloc));
        assert_eq!(pool.num_alloc_buffers(), 1);
    }

    /// C changes `pArrays[0]` under the driver lock (simDetector.cpp:824-828),
    /// so a reader arriving mid-change waits and finds the changed array.
    #[test]
    fn a_reader_during_refill_waits_for_the_refilled_array() {
        let pool = crate::ndarray_pool::DriverPool::standalone(1_000_000);
        let owner = pool.owner().clone();
        let image = pool
            .alloc(
                vec![crate::ndarray::NDDimension::new(4)],
                crate::ndarray::NDDataType::UInt8,
            )
            .unwrap()
            .share();
        owner.store_array(0, image.array().clone());
        let mut reader = None;
        let image = owner
            .refill(0, image, |array| {
                array.set_unique_id(7);
                let owner = owner.clone();
                reader = Some(std::thread::spawn(move || {
                    owner.array(0).map(|a| a.unique_id)
                }));
                std::thread::sleep(std::time::Duration::from_millis(50));
            })
            .unwrap();
        assert_eq!(reader.unwrap().join().unwrap(), Some(7));
        assert_eq!(image.unique_id, 7);
    }

    /// C `preAllocateBuffers` (asynNDArrayDriver.cpp:722-733) posts the pool
    /// after each copy and names the copy that failed.
    #[test]
    fn pre_alloc_posts_each_copy_and_names_the_one_that_failed() {
        use asyn_rs::services::PortServices;
        use asyn_rs::trace::TraceFile;
        const PORT: &str = "PREALLOC_EACH";
        let mut drv = NDArrayDriverBase::new(
            PORT,
            1,
            3 * 64,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        let template = drv
            .pool
            .alloc(
                vec![crate::ndarray::NDDimension::new(64)],
                crate::ndarray::NDDataType::UInt8,
            )
            .unwrap();
        drv.owner.store_array(0, template.share().array().clone());
        drv.port_base
            .set_int32_param(drv.params.pool_num_pre_alloc_buffers, 0, 5)
            .unwrap();
        let log = std::env::temp_dir().join(format!("{PORT}-{}", std::process::id()));
        PortServices::global().trace().set_trace_file(
            Some(PORT),
            TraceFile::File(Arc::new(std::sync::Mutex::new(
                std::fs::File::create(&log).unwrap(),
            ))),
        );
        let (posts, _sub) = record_posts(&mut drv.port_base);

        let user = AsynUser::new(drv.params.pool_pre_alloc);
        let template = drv.owner.array(0);
        handle_write_int32(
            &mut drv.port_base,
            &drv.params,
            &drv.pool,
            template.as_deref(),
            &user,
            1,
        )
        .unwrap();

        let of = |reason| {
            posts
                .lock()
                .iter()
                .filter_map(|(r, _, v)| match v {
                    ParamValue::Int32(v) if *r == reason => Some(*v),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(of(drv.params.pool_alloc_buffers), [2, 3]);
        assert_eq!(of(drv.params.pool_pre_alloc), [1, 0]);
        let trace = std::fs::read_to_string(&log).unwrap();
        let _ = std::fs::remove_file(&log);
        assert!(
            trace.ends_with(
                " asynNDArrayDriver::preAllocateBuffers: ERROR, could not allocate array 2\n"
            ),
            "{trace}"
        );
        // C returns here still holding copies 0 and 1 (:726-730); they go
        // back to the free list instead, after the last post.
        assert_eq!(drv.pool.num_free_buffers(), 2);
        assert_eq!(
            drv.port_base
                .get_int32_param(drv.params.pool_free_buffers, 0)
                .unwrap(),
            0
        );
    }

    #[test]
    fn max_addr_comes_from_the_caller() {
        // asynNDArrayDriver.cpp:862-868 hands maxAddr to asynPortDriver.
        let drv = NDArrayDriverBase::new(
            "TEST",
            3,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        assert_eq!(drv.port_base.max_addr, 3);
    }

    #[test]
    fn test_constructor_seeds_the_c_read_only_block() {
        let drv = NDArrayDriverBase::new(
            "TEST",
            1,
            4_194_304,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        assert_c_read_only_block(&drv.port_base, &drv.params, "TEST");
        assert_fresh_pool_stats(&drv.port_base, &drv.params);
        // C `:655-660` via the NumQueuedArrays seed.
        assert_eq!(
            drv.port_base
                .get_int32_param(drv.params.acquire_busy, 0)
                .unwrap(),
            0
        );
    }

    #[test]
    fn an_ndarray_driver_port_drives_acquire_busy() {
        // C ADCSimDetector is an asynNDArrayDriver, not an ADDriver.
        let mut drv = NDArrayDriverBase::new(
            "TEST",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        let (acquire, busy, wait, queued) = (
            drv.params.acquire,
            drv.params.acquire_busy,
            drv.params.wait_for_plugins,
            drv.params.num_queued_arrays,
        );
        let read = |d: &NDArrayDriverBase| d.port_base.get_int32_param(busy, 0).unwrap();
        drv.port_base.set_int32_param(acquire, 0, 1).unwrap();
        assert_eq!(read(&drv), 1);
        drv.port_base.set_int32_param(acquire, 0, 0).unwrap();
        assert_eq!(read(&drv), 0);

        drv.port_base.set_int32_param(wait, 0, 1).unwrap();
        drv.port_base.set_int32_param(acquire, 0, 1).unwrap();
        drv.owner.queued().increment();
        drv.port_base.set_int32_param(acquire, 0, 0).unwrap();
        assert_eq!(read(&drv), 1);
        drv.owner.queued().decrement();
        drv.port_base.set_int32_param(queued, 0, 0).unwrap();
        assert_eq!(read(&drv), 0);
    }

    /// C's updater waits on `queuedArrayEvent_`, which only a change
    /// signals (asynNDArrayDriver.cpp:785-792): a port that queues nothing
    /// never runs its callbacks from the updater.
    #[test]
    fn the_updater_posts_only_after_a_change() {
        use asyn_rs::interrupt::{InterruptFilter, InterruptValue};
        use asyn_rs::registry::PortRegistry;
        use asyn_rs::runtime::config::RuntimeConfig;
        use asyn_rs::runtime::port::create_port_runtime;
        use epics_libcom_rs::runtime::interrupt_accept::InterruptGate;

        struct Port(NDArrayDriverBase);
        impl asyn_rs::port::PortDriver for Port {
            fn base(&self) -> &PortDriverBase {
                &self.0.port_base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0.port_base
            }
        }
        let mut drv = NDArrayDriverBase::new(
            "NDQUEUEDIDLE",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        drv.port_base.own_interrupts(InterruptGate::opened());
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let seen_cb = seen.clone();
        let _sub = drv
            .port_base
            .interrupts
            .register_sync_callback(
                InterruptFilter {
                    addr: Some(0),
                    ..Default::default()
                },
                move |iv: &InterruptValue| seen_cb.lock().push(iv.reason),
            )
            .unwrap();
        let num_queued_arrays = drv.params.num_queued_arrays;
        let queued = drv.owner.queued().clone();
        let claim = PortRegistry::new().claim("NDQUEUEDIDLE").unwrap();
        let (_runtime, _actor) =
            create_port_runtime(&claim, Port(drv), RuntimeConfig::default()).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(seen.lock().is_empty(), "{:?}", seen.lock());

        queued.increment();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !seen.lock().contains(&num_queued_arrays) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(seen.lock().contains(&num_queued_arrays));
    }

    /// The updater stores the live count, so a stop under WaitForPlugins
    /// clears AcquireBusy when the queue drains, with no driver writing
    /// NDNumQueuedArrays.
    #[test]
    fn a_stop_with_arrays_queued_clears_acquire_busy_when_the_queue_drains() {
        use asyn_rs::registry::PortRegistry;
        use asyn_rs::runtime::config::RuntimeConfig;
        use asyn_rs::runtime::port::create_port_runtime;

        struct Port(NDArrayDriverBase);
        impl asyn_rs::port::PortDriver for Port {
            fn base(&self) -> &PortDriverBase {
                &self.0.port_base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0.port_base
            }
        }
        let drv = NDArrayDriverBase::new(
            "NDQUEUEDUPD",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        let p = drv.params;
        let queued = drv.owner.queued().clone();
        let claim = PortRegistry::new().claim("NDQUEUEDUPD").unwrap();
        let (runtime, _actor) =
            create_port_runtime(&claim, Port(drv), RuntimeConfig::default()).unwrap();
        let handle = runtime.port_handle().clone();
        let store = |reason, value| {
            handle
                .set_params_and_notify_blocking(
                    0,
                    vec![ParamSetValue::new(reason, 0, ParamValue::Int32(value))],
                )
                .unwrap();
        };
        let read = |reason: usize| {
            handle
                .with_driver_blocking(move |d: &mut Port| {
                    d.0.port_base.get_int32_param(reason, 0).unwrap()
                })
                .unwrap()
        };
        let settle = |reason: usize, want: i32| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while read(reason) != want && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            read(reason)
        };

        store(p.wait_for_plugins, 1);
        store(p.acquire, 1);
        queued.increment();
        assert_eq!(settle(p.num_queued_arrays, 1), 1);
        store(p.acquire, 0);
        assert_eq!(read(p.acquire_busy), 1);

        queued.decrement();
        assert_eq!(settle(p.num_queued_arrays, 0), 0);
        assert_eq!(read(p.acquire_busy), 0);
    }

    /// The updater must not hold the port up: dropping every handle stops the
    /// actor, which drops the driver and so ends the updater.
    #[test]
    fn a_refused_alloc_reports_on_the_owning_port() {
        use asyn_rs::registry::PortRegistry;
        use asyn_rs::runtime::config::RuntimeConfig;
        use asyn_rs::runtime::port::create_port_runtime;

        struct Port(NDArrayDriverBase);
        impl asyn_rs::port::PortDriver for Port {
            fn base(&self) -> &PortDriverBase {
                &self.0.port_base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0.port_base
            }
        }
        let drv = NDArrayDriverBase::new(
            "NDPOOLLIMIT",
            1,
            64,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        let pool = drv.pool.clone();
        let owner = drv.owner.clone();
        let claim = PortRegistry::new().claim("NDPOOLLIMIT").unwrap();
        let (runtime, _actor) =
            create_port_runtime(&claim, Port(drv), RuntimeConfig::default()).unwrap();
        let log = std::env::temp_dir().join(format!("pool-limit-{}.log", std::process::id()));
        let file = std::fs::File::create(&log).unwrap();
        runtime.port_handle().trace().set_trace_file(
            Some("NDPOOLLIMIT"),
            asyn_rs::trace::TraceFile::File(Arc::new(std::sync::Mutex::new(file))),
        );
        assert!(owner.port().is_some());

        let refused = pool.alloc(
            vec![crate::ndarray::NDDimension::new(100)],
            crate::ndarray::NDDataType::UInt8,
        );
        assert!(refused.is_err());
        let text = std::fs::read_to_string(&log).unwrap();
        let _ = std::fs::remove_file(&log);
        assert!(
            text.contains("NDArrayPool::alloc:: error: reached limit of 64 memory (1 buffers)"),
            "{text}"
        );
        drop(runtime);
    }

    #[test]
    fn dropping_every_handle_stops_an_ndarray_port() {
        use asyn_rs::registry::PortRegistry;
        use asyn_rs::runtime::config::RuntimeConfig;
        use asyn_rs::runtime::port::create_port_runtime;

        struct Port(NDArrayDriverBase);
        impl asyn_rs::port::PortDriver for Port {
            fn base(&self) -> &PortDriverBase {
                &self.0.port_base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0.port_base
            }
        }
        let drv = NDArrayDriverBase::new(
            "NDQUEUEDDROP",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        let counter = Arc::downgrade(drv.owner.queued());
        let owner = Arc::downgrade(&drv.owner);
        let claim = PortRegistry::new().claim("NDQUEUEDDROP").unwrap();
        let (runtime, actor) =
            create_port_runtime(&claim, Port(drv), RuntimeConfig::default()).unwrap();
        // Wake the updater once so it has taken and released a handle.
        counter.upgrade().unwrap().increment();
        assert!(
            owner.upgrade().unwrap().port().is_some(),
            "the owner learns its port"
        );
        drop(runtime);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !actor.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(
            actor.is_finished(),
            "the port actor outlived its last handle"
        );
        assert!(counter.upgrade().is_none(), "the driver outlived its port");
        assert!(owner.upgrade().is_none());
    }

    #[test]
    fn test_empty_attributes_file_keeps_the_constructor_status() {
        // C `:327` returns asynSuccess on an empty NDAttributesFile without
        // touching NDAttributesStatus, so an IOC that never loads one reads
        // "File not found" — including after the PINI="YES" write that
        // NDArrayBase.template:793 performs at iocInit.
        let mut drv = NDArrayDriverBase::new(
            "TEST",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        drv.read_nd_attributes_file().unwrap();
        assert_eq!(drv.attributes().len(), 0);
        assert_eq!(
            drv.port_base
                .get_int32_param(drv.params.attributes_status, 0)
                .unwrap(),
            ATTR_STATUS_FILE_NOT_FOUND
        );
    }

    #[test]
    fn test_new_leaves_array_callbacks_to_the_database() {
        // asynNDArrayDriver never sets NDArrayCallbacks, so the init readback
        // fails and the record's VAL stands.
        let drv = NDArrayDriverBase::new(
            "TEST",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        assert!(
            drv.port_base
                .get_int32_param(drv.params.array_callbacks, 0)
                .is_err()
        );
    }

    #[test]
    fn test_prepare_array() {
        let mut drv = NDArrayDriverBase::new(
            "TEST",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        let arr = drv
            .pool
            .alloc(
                vec![
                    crate::ndarray::NDDimension::new(64),
                    crate::ndarray::NDDimension::new(64),
                ],
                crate::ndarray::NDDataType::UInt8,
            )
            .unwrap();
        drv.prepare_array(arr).unwrap();
        assert_eq!(
            drv.port_base
                .get_int32_param(drv.params.array_counter, 0)
                .unwrap(),
            1,
        );
    }

    #[test]
    fn test_prepare_updates_size_info() {
        let mut drv = NDArrayDriverBase::new(
            "TEST",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        let arr = drv
            .pool
            .alloc(
                vec![
                    crate::ndarray::NDDimension::new(320),
                    crate::ndarray::NDDimension::new(240),
                ],
                crate::ndarray::NDDataType::UInt16,
            )
            .unwrap();
        drv.prepare_array(arr).unwrap();
        assert_eq!(
            drv.port_base
                .get_int32_param(drv.params.array_size_x, 0)
                .unwrap(),
            320,
        );
        assert_eq!(
            drv.port_base
                .get_int32_param(drv.params.array_size_y, 0)
                .unwrap(),
            240,
        );
    }

    #[test]
    fn test_create_file_name_empty_template_yields_empty() {
        // C parity (B9): an empty FILE_TEMPLATE is passed through epicsSnprintf
        // verbatim, producing an empty string — no fabricated default.
        let mut drv = NDArrayDriverBase::new(
            "TEST",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        drv.port_base
            .set_string_param(drv.params.file_path, 0, "/tmp/")
            .unwrap();
        drv.port_base
            .set_string_param(drv.params.file_name, 0, "test_")
            .unwrap();
        drv.port_base
            .set_int32_param(drv.params.file_number, 0, 42)
            .unwrap();
        drv.port_base
            .set_string_param(drv.params.file_template, 0, "")
            .unwrap();

        let name = drv.create_file_name().unwrap();
        assert_eq!(name, "");
    }

    #[test]
    fn test_create_file_name_standard_template() {
        let mut drv = NDArrayDriverBase::new(
            "TEST",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        drv.port_base
            .set_string_param(drv.params.file_path, 0, "/tmp/")
            .unwrap();
        drv.port_base
            .set_string_param(drv.params.file_name, 0, "test")
            .unwrap();
        drv.port_base
            .set_int32_param(drv.params.file_number, 0, 42)
            .unwrap();
        drv.port_base
            .set_string_param(drv.params.file_template, 0, "%s%s_%3.3d.dat")
            .unwrap();

        let name = drv.create_file_name().unwrap();
        // checkPath re-terminates FilePath with the OS separator, exactly as C
        // does under `#ifdef _WIN32` (asynNDArrayDriver.cpp:72-86, `\` on
        // Windows), so the joined name carries `\` there — build the expected
        // with MAIN_SEPARATOR like the sibling checkPath tests.
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(name, format!("/tmp{sep}test_042.dat"));
    }

    #[test]
    fn test_r6_64_create_file_name_normalizes_file_path() {
        // R6-64 / asynNDArrayDriver.cpp:203 — createFileName calls checkPath()
        // first, so a FilePath without a trailing separator (e.g. one a driver
        // seeded with set_string_param, bypassing writeOctet) is normalized
        // before the template runs. Without it the "%s%s_%3.3d.dat" default
        // produces "/tmpimg_042.dat".
        let mut drv = NDArrayDriverBase::new(
            "TEST",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        let tmp = std::env::temp_dir();
        let no_sep = tmp
            .to_string_lossy()
            .trim_end_matches(std::path::MAIN_SEPARATOR)
            .to_string();
        drv.port_base
            .set_string_param(drv.params.file_path, 0, no_sep.clone())
            .unwrap();
        drv.port_base
            .set_string_param(drv.params.file_name, 0, "img")
            .unwrap();
        drv.port_base
            .set_int32_param(drv.params.file_number, 0, 42)
            .unwrap();
        drv.port_base
            .set_string_param(drv.params.file_template, 0, "%s%s_%3.3d.dat")
            .unwrap();

        let name = drv.create_file_name().unwrap();
        let expected = format!("{}{}img_042.dat", no_sep, std::path::MAIN_SEPARATOR);
        assert_eq!(name, expected, "createFileName must run checkPath first");

        // checkPath also writes the normalized path back to the parameter and
        // refreshes FilePathExists (asynNDArrayDriver.cpp:107-108).
        let stored = drv
            .port_base
            .get_string_param(drv.params.file_path, 0)
            .map(octet_text)
            .unwrap();
        assert_eq!(
            stored,
            format!("{}{}", no_sep, std::path::MAIN_SEPARATOR),
            "FilePath must be written back with the trailing separator"
        );
        assert_eq!(
            drv.port_base
                .get_int32_param(drv.params.file_path_exists, 0)
                .unwrap(),
            1
        );
    }

    #[test]
    fn test_r6_64_check_path_empty_path_touches_nothing() {
        // C checkPath() returns early for an empty FilePath (:104), leaving both
        // FilePath and FilePathExists alone.
        let mut drv = NDArrayDriverBase::new(
            "TEST",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        drv.port_base
            .set_int32_param(drv.params.file_path_exists, 0, 1)
            .unwrap();
        assert!(drv.check_path().unwrap(), "C returns asynSuccess");
        assert_eq!(
            drv.port_base
                .get_int32_param(drv.params.file_path_exists, 0)
                .unwrap(),
            1,
            "empty FilePath must not overwrite FilePathExists"
        );
    }

    #[test]
    fn test_r6_64_check_path_str_appends_separator_when_missing() {
        // C checkPath(std::string&) appends the delimiter even when the input
        // had none (asynNDArrayDriver.cpp:85-86), and strips exactly one
        // trailing separator before the stat.
        let tmp = std::env::temp_dir().to_string_lossy().into_owned();
        let base = tmp.trim_end_matches(std::path::MAIN_SEPARATOR).to_string();
        let sep = std::path::MAIN_SEPARATOR;

        let mut p = base.clone();
        assert!(check_path_str(&mut p));
        assert_eq!(p, format!("{base}{sep}"));

        // Already normalized: unchanged.
        let mut p = format!("{base}{sep}");
        assert!(check_path_str(&mut p));
        assert_eq!(p, format!("{base}{sep}"));

        // Missing directory: still normalized, reports false.
        let mut p = format!("{base}{sep}definitely-not-a-real-dir-r6-64");
        assert!(!check_path_str(&mut p));
        assert_eq!(
            p,
            format!("{base}{sep}definitely-not-a-real-dir-r6-64{sep}")
        );

        // Empty: untouched.
        let mut p = String::new();
        assert!(!check_path_str(&mut p));
        assert_eq!(p, "");
    }

    #[test]
    fn test_format_int_spec_width_vs_precision() {
        // B10: precision = min digits (zero-pad); width = field (space-pad).
        assert_eq!(format_int_spec("3.3", 7), "007");
        assert_eq!(format_int_spec("5.3", 42), "  042");
        assert_eq!(format_int_spec("04", 7), "0007");
        assert_eq!(format_int_spec("5", 7), "    7");
        assert_eq!(format_int_spec("", 7), "7");
        assert_eq!(format_int_spec("2.5", 12345), "12345");
        // Negative values keep the sign in front.
        assert_eq!(format_int_spec("6.3", -4), "  -004");
        assert_eq!(format_int_spec("05", -4), "-0004");
    }

    #[test]
    fn test_check_path_exists() {
        let mut drv = NDArrayDriverBase::new(
            "TEST",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        // The platform temp dir exists on every OS; a hard-coded `/tmp`
        // does not exist on Windows, so `check_path`'s `is_dir()` reported
        // it missing and the assertion failed there. Mirrors the
        // `std::env::temp_dir()` already used by `test_create_file_path_recursive`.
        let tmp = std::env::temp_dir();
        drv.port_base
            .set_string_param(drv.params.file_path, 0, tmp.to_string_lossy().into_owned())
            .unwrap();
        assert!(drv.check_path().unwrap());
    }

    #[test]
    fn test_check_path_not_exists() {
        let mut drv = NDArrayDriverBase::new(
            "TEST",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        drv.port_base
            .set_string_param(drv.params.file_path, 0, "/nonexistent_path_xyz")
            .unwrap();
        assert!(!drv.check_path().unwrap());
    }

    /// No C driver writes NDDimensions; only NDPluginDriver (:246) and
    /// NDDriverStdArrays (:266) post it.
    #[test]
    fn a_driver_frame_writes_no_array_dimensions() {
        let mut drv = NDArrayDriverBase::new(
            "TEST",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        let dims = drv.params.array_dimensions;
        let before = drv.port_base.params.get_int32_array(dims, 0).ok();
        let (posts, _sub) = record_posts(&mut drv.port_base);
        let arr = drv
            .pool
            .alloc(
                vec![crate::ndarray::NDDimension::new(8)],
                crate::ndarray::NDDataType::UInt8,
            )
            .unwrap();
        drv.prepare_array(arr).unwrap();
        assert!(posts.lock().iter().all(|(r, ..)| *r != dims));
        assert_eq!(drv.port_base.params.get_int32_array(dims, 0).ok(), before);
    }

    #[test]
    fn test_prepare_array_publishes_dims_type_timestamps() {
        // G7: prepare_array must publish N_DIMENSIONS,
        // DATA_TYPE, COLOR_MODE, TIME_STAMP, EPICS_TS_SEC/NSEC.
        let mut drv = NDArrayDriverBase::new(
            "TEST",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        let mut arr = drv
            .pool
            .alloc(
                vec![
                    crate::ndarray::NDDimension::new(64),
                    crate::ndarray::NDDimension::new(48),
                ],
                crate::ndarray::NDDataType::UInt16,
            )
            .unwrap();
        arr.fill().set_time_stamp(100.5);
        arr.fill().set_timestamp(crate::timestamp::EpicsTimestamp {
            sec: 1234,
            nsec: 5678,
        });
        drv.prepare_array(arr).unwrap();

        assert_eq!(
            drv.port_base
                .get_int32_param(drv.params.n_dimensions, 0)
                .unwrap(),
            2
        );
        assert_eq!(
            drv.port_base
                .get_int32_param(drv.params.data_type, 0)
                .unwrap(),
            crate::ndarray::NDDataType::UInt16 as i32
        );
        assert_eq!(
            drv.port_base
                .get_float64_param(drv.params.timestamp_rbv, 0)
                .unwrap(),
            100.5
        );
        assert_eq!(
            drv.port_base
                .get_int32_param(drv.params.epics_ts_sec, 0)
                .unwrap(),
            1234
        );
        assert_eq!(
            drv.port_base
                .get_int32_param(drv.params.epics_ts_nsec, 0)
                .unwrap(),
            5678
        );
    }

    #[test]
    fn test_prepare_array_publishes_codec_and_bayer() {
        // G5/G6: prepare_array publishes CODEC, COMPRESSED_SIZE, BAYER_PATTERN.
        use crate::attributes::{NDAttrSource, NDAttrValue, NDAttribute};

        let mut drv = NDArrayDriverBase::new(
            "TEST",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        let mut arr = drv
            .pool
            .alloc(
                vec![crate::ndarray::NDDimension::new(16)],
                crate::ndarray::NDDataType::UInt8,
            )
            .unwrap();
        arr.fill().set_codec(Some(crate::codec::Codec {
            name: crate::codec::CodecName::BSLZ4,
            compressed_size: 9,
            level: 0,
            shuffle: 0,
            compressor: 0,
            original_data_type: crate::ndarray::NDDataType::UInt8,
        }));
        arr.fill().attributes_mut().add(NDAttribute {
            name: "bayerPattern".into(),
            description: String::new(),
            source: NDAttrSource::Driver,
            value: NDAttrValue::Int32(crate::color::NDBayerPattern::GRBG as i32),
            source_impl: None,
        });
        drv.prepare_array(arr).unwrap();

        assert_eq!(
            drv.port_base.get_string_param(drv.params.codec, 0).unwrap(),
            b"bslz4"
        );
        assert_eq!(
            drv.port_base
                .get_int32_param(drv.params.compressed_size, 0)
                .unwrap(),
            9
        );
        assert_eq!(
            drv.port_base
                .get_int32_param(drv.params.bayer_pattern, 0)
                .unwrap(),
            crate::color::NDBayerPattern::GRBG as i32
        );
    }

    #[test]
    fn test_connect_downstream() {
        let mut drv = NDArrayDriverBase::new(
            "TEST",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        drv.port_base
            .set_int32_param(drv.params.array_callbacks, 0, 1)
            .unwrap();
        let (sender, mut receiver) = ndarray_channel("DOWNSTREAM", 10);
        drv.connect_downstream(sender);
        assert_eq!(drv.num_plugins(), 1);

        let arr = drv
            .pool
            .alloc(
                vec![crate::ndarray::NDDimension::new(8)],
                crate::ndarray::NDDataType::UInt8,
            )
            .unwrap();
        let id = arr.unique_id;
        let to_publish = drv.prepare_array(arr).unwrap().unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _ = rt.block_on(drv.array_output.publish(to_publish));

        let received = receiver.blocking_recv().unwrap();
        assert_eq!(received.unique_id, id);
    }

    #[test]
    fn test_create_file_path_recursive() {
        // G9: createFilePath creates directory components at depth >= path_depth.
        let base = std::env::temp_dir().join(format!(
            "ad_core_rs_cfp_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let nested = base.join("a").join("b").join("c");
        let path = format!("{}/", nested.to_string_lossy());
        // path_depth 0 = no-op.
        NDArrayDriverBase::create_file_path(&path, 0).unwrap();
        assert!(!nested.exists());
        // path_depth 1 creates everything from index 1 onward.
        NDArrayDriverBase::create_file_path(&path, 1).unwrap();
        assert!(nested.is_dir());
        // Idempotent — EEXIST is not an error.
        NDArrayDriverBase::create_file_path(&path, 1).unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_read_nd_attributes_file_inline_xml() {
        // G9: readNDAttributesFile parses inline XML (NDAttributesFile schema).
        let mut drv = NDArrayDriverBase::new(
            "TEST",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        let xml = r#"<Attributes>
            <Attribute name="Gain" type="param" source="GAIN" description="detector gain"/>
            <Attribute name="Comment" type="const" source="hello"/>
            <Attribute name="Temp" type="EPICS_PV" source="$(P)Temp"/>
        </Attributes>"#;
        drv.port_base
            .set_string_param(drv.params.attributes_file, 0, xml)
            .unwrap();
        // The EPICS_PV attribute parses and is kept, but nothing can feed it,
        // so the file does not load clean — see the status assertion below.
        assert!(drv.read_nd_attributes_file().is_err());

        assert_eq!(drv.attributes().len(), 3);
        let attributes = drv.attributes();
        let gain = attributes.get("Gain").unwrap();
        assert!(matches!(gain.source, NDAttrSource::Param { .. }));
        let comment = attributes.get("Comment").unwrap();
        assert_eq!(comment.value, NDAttrValue::String("hello".into()));
        // C publishes the original XML `source` string verbatim, not a label.
        assert_eq!(comment.source.source_string(), "hello");
        assert_eq!(gain.source.source_string(), "GAIN");
        let temp = attributes.get("Temp").unwrap();
        assert!(matches!(temp.source, NDAttrSource::EpicsPV(_)));
        assert_eq!(temp.source.source_string(), "$(P)Temp");
        assert_eq!(
            drv.port_base
                .get_int32_param(drv.params.attributes_status, 0)
                .unwrap(),
            ATTR_STATUS_XML_SYNTAX_ERROR
        );
    }

    fn load_with_macros(xml: &str, macros: &str) -> NDArrayDriverBase {
        let mut drv = NDArrayDriverBase::new(
            "TEST",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        drv.port_base
            .set_string_param(drv.params.attributes_macros, 0, macros)
            .unwrap();
        drv.port_base
            .set_string_param(drv.params.attributes_file, 0, xml)
            .unwrap();
        let _ = drv.read_nd_attributes_file();
        drv
    }

    /// C `readNDAttributesFile` expands the whole file against
    /// `NDAttributesMacros` before parsing it; a quoted value keeps its comma.
    #[test]
    fn test_attributes_macros_expand_the_file() {
        let drv = load_with_macros(
            r#"<Attributes>
                <Attribute name="$(N)Gain" type="PARAM" source="$(PARAM)" datatype="DOUBLE"/>
                <Attribute name="Note" type="CONST" source="$(TEXT)" datatype="STRING"/>
            </Attributes>"#,
            r#"N=Det,PARAM=GAIN,TEXT="a,b""#,
        );
        assert_eq!(
            drv.attributes()
                .get("DetGain")
                .unwrap()
                .source
                .source_string(),
            "GAIN"
        );
        assert_eq!(
            drv.attributes().get("Note").unwrap().value,
            NDAttrValue::String("a,b".into())
        );
    }

    /// Boundary: no macro string — the text is parsed as written.
    #[test]
    fn test_attributes_without_macros_are_not_expanded() {
        let drv = load_with_macros(
            r#"<Attributes><Attribute name="A" type="CONST" source="$(X)"/></Attributes>"#,
            "",
        );
        assert_eq!(
            drv.attributes().get("A").unwrap().source.source_string(),
            "$(X)"
        );
    }

    /// Boundary: a reference the macro string does not define. C's
    /// `goto done_macros` parses the unexpanded buffer, so the defined macro
    /// in the same file is not applied either.
    #[test]
    fn test_an_undefined_attributes_macro_leaves_the_file_unexpanded() {
        let drv = load_with_macros(
            r#"<Attributes>
                <Attribute name="A" type="CONST" source="$(X)"/>
                <Attribute name="B" type="CONST" source="$(MISSING)"/>
            </Attributes>"#,
            "X=1",
        );
        assert_eq!(
            drv.attributes().get("A").unwrap().source.source_string(),
            "$(X)"
        );
        assert_eq!(
            drv.attributes().get("B").unwrap().source.source_string(),
            "$(MISSING)"
        );
    }

    /// The environment is not behind the handle (C `macCreateHandle(&h, 0)`).
    #[test]
    fn test_attributes_macros_do_not_read_the_environment() {
        let drv = load_with_macros(
            r#"<Attributes><Attribute name="A" type="CONST" source="$(PATH)"/></Attributes>"#,
            "X=1",
        );
        assert_eq!(
            drv.attributes().get("A").unwrap().source.source_string(),
            "$(PATH)"
        );
    }

    #[test]
    fn test_epics_pv_attribute_is_not_reported_ok() {
        // An EPICS_PV attribute evaluates Undefined for the life of the IOC
        // (no CA feeder exists), so NDAttributesStatus must not read
        // "Attributes file OK" — that is what made the gap invisible.
        let mut drv = NDArrayDriverBase::new(
            "TEST",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        drv.port_base
            .set_string_param(
                drv.params.attributes_file,
                0,
                r#"<Attributes>
                    <Attribute name="Temp" type="EPICS_PV" source="TST:Temp"/>
                </Attributes>"#,
            )
            .unwrap();
        assert!(drv.read_nd_attributes_file().is_err());
        assert_ne!(
            drv.port_base
                .get_int32_param(drv.params.attributes_status, 0)
                .unwrap(),
            ATTR_STATUS_OK
        );
        assert_eq!(
            drv.update_attributes().get("Temp").unwrap().value,
            NDAttrValue::Undefined
        );

        // A file with no EPICS_PV attribute still loads clean.
        drv.port_base
            .set_string_param(
                drv.params.attributes_file,
                0,
                r#"<Attributes>
                    <Attribute name="Comment" type="const" source="hello"/>
                </Attributes>"#,
            )
            .unwrap();
        drv.read_nd_attributes_file().unwrap();
        assert_eq!(
            drv.port_base
                .get_int32_param(drv.params.attributes_status, 0)
                .unwrap(),
            ATTR_STATUS_OK
        );
    }

    #[test]
    fn test_epics_pv_unknown_dbrtype_is_refused() {
        // C returns asynError on any dbrtype outside its nine names
        // (asynNDArrayDriver.cpp:421-436); the port must not accept a typo and
        // publish the attribute as the native type instead.
        let mut drv = NDArrayDriverBase::new(
            "TEST",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        drv.port_base
            .set_string_param(
                drv.params.attributes_file,
                0,
                r#"<Attributes>
                    <Attribute name="Temp" type="EPICS_PV" source="TST:Temp"
                               dbrtype="DBR_FLOAT32"/>
                </Attributes>"#,
            )
            .unwrap();
        let err = drv.read_nd_attributes_file().unwrap_err().to_string();
        assert!(err.contains("DBR_FLOAT32"), "{err}");
        assert_eq!(drv.attributes().len(), 0);
        assert_eq!(
            drv.port_base
                .get_int32_param(drv.params.attributes_status, 0)
                .unwrap(),
            ATTR_STATUS_XML_SYNTAX_ERROR
        );

        // Every one of C's nine spellings still parses.
        for name in [
            "DBR_CHAR",
            "DBR_SHORT",
            "DBR_INT",
            "DBR_ENUM",
            "DBR_LONG",
            "DBR_FLOAT",
            "DBR_DOUBLE",
            "DBR_STRING",
            "DBR_NATIVE",
        ] {
            drv.port_base
                .set_string_param(
                    drv.params.attributes_file,
                    0,
                    format!(
                        "<Attributes><Attribute name=\"T\" type=\"EPICS_PV\" \
                         source=\"TST:Temp\" dbrtype=\"{name}\"/></Attributes>"
                    ),
                )
                .unwrap();
            // Accepted by the dbrtype table; still refused for having no
            // feeder, which is the other half of this finding.
            assert!(
                !drv.read_nd_attributes_file()
                    .unwrap_err()
                    .to_string()
                    .contains("dbrType"),
                "{name}"
            );
            assert_eq!(drv.attributes().len(), 1, "{name}");
        }
    }

    #[test]
    fn test_param_attribute_reevaluates_from_param_library() {
        // G10: a PARAM attribute loaded from NDAttributesFile XML must
        // re-evaluate from the driver's asyn parameter library on
        // update_attributes(), not stay frozen at its Undefined load value.
        let mut drv = NDArrayDriverBase::new(
            "TEST",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        let xml = r#"<Attributes>
            <Attribute name="Counter" type="PARAM" source="ARRAY_COUNTER" datatype="INT"/>
            <Attribute name="Maker" type="PARAM" source="MANUFACTURER" datatype="STRING"/>
        </Attributes>"#;
        drv.port_base
            .set_string_param(drv.params.attributes_file, 0, xml)
            .unwrap();
        drv.read_nd_attributes_file().unwrap();

        // Drive the parameter library, then re-evaluate the attributes.
        drv.port_base
            .set_int32_param(drv.params.array_counter, 0, 17)
            .unwrap();
        drv.port_base
            .set_string_param(drv.params.manufacturer, 0, "ACME")
            .unwrap();
        let snap = drv.update_attributes();
        assert_eq!(snap.get("Counter").unwrap().value, NDAttrValue::Int32(17));
        assert_eq!(
            snap.get("Maker").unwrap().value,
            NDAttrValue::String("ACME".into())
        );

        // A later parameter change is picked up on the next update.
        drv.port_base
            .set_int32_param(drv.params.array_counter, 0, 99)
            .unwrap();
        let snap2 = drv.update_attributes();
        assert_eq!(snap2.get("Counter").unwrap().value, NDAttrValue::Int32(99));
    }

    #[test]
    fn test_param_attribute_omitted_datatype_stays_undefined() {
        // ADC-1: C++ selects the attribute's published type from the XML
        // `datatype`. An omitted `datatype` defaults to the lower-case "int"
        // (asynNDArrayDriver.cpp:446), which matches none of the upper-case
        // `strcmp` branches (paramAttribute.cpp:80-95) → paramType Unknown →
        // updateValue()'s switch falls through `default` and never refreshes
        // the attribute, so it stays NDAttrUndefined. (The port previously
        // derived the type from the param's runtime type and published an
        // Int32 value.)
        let mut drv = NDArrayDriverBase::new(
            "TEST",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        let xml = r#"<Attributes>
            <Attribute name="Counter" type="PARAM" source="ARRAY_COUNTER"/>
        </Attributes>"#;
        drv.port_base
            .set_string_param(drv.params.attributes_file, 0, xml)
            .unwrap();
        drv.read_nd_attributes_file().unwrap();
        drv.port_base
            .set_int32_param(drv.params.array_counter, 0, 17)
            .unwrap();
        let snap = drv.update_attributes();
        assert_eq!(
            snap.get("Counter").unwrap().value,
            NDAttrValue::Undefined,
            "an omitted datatype must leave the PARAM attribute Undefined"
        );
    }

    #[test]
    fn test_param_attribute_datatype_selects_read_type() {
        // ADC-1: the read getter is chosen by `datatype`, NOT the param's
        // runtime type. ARRAY_COUNTER is an Int32 param; reading it with
        // datatype="DOUBLE" dispatches to getDoubleParam, which wrong-types on
        // an Int32 param — so the attribute is not refreshed (stays Undefined)
        // rather than coercing the Int32 into a Float64. With the matching
        // datatype="INT" the same param publishes Int32(value).
        let mut drv = NDArrayDriverBase::new(
            "TEST",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        let xml = r#"<Attributes>
            <Attribute name="AsDouble" type="PARAM" source="ARRAY_COUNTER" datatype="DOUBLE"/>
            <Attribute name="AsInt" type="PARAM" source="ARRAY_COUNTER" datatype="INT"/>
        </Attributes>"#;
        drv.port_base
            .set_string_param(drv.params.attributes_file, 0, xml)
            .unwrap();
        drv.read_nd_attributes_file().unwrap();
        drv.port_base
            .set_int32_param(drv.params.array_counter, 0, 42)
            .unwrap();
        let snap = drv.update_attributes();
        assert_eq!(
            snap.get("AsDouble").unwrap().value,
            NDAttrValue::Float64(0.0),
            "datatype=DOUBLE on an Int32 param publishes the wrong-type read's 0 \
             (paramAttribute.cpp:141-145)"
        );
        assert_eq!(snap.get("AsInt").unwrap().value, NDAttrValue::Int32(42));
    }

    #[test]
    fn test_function_attribute_reevaluates_from_registry() {
        // G10: a FUNCTION attribute loaded from XML must call its registered
        // function on update_attributes().
        let mut drv = NDArrayDriverBase::new(
            "TEST",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        // Register a function whose return value changes on each call.
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0));
        let c = counter.clone();
        drv.owner
            .attributes()
            .lock()
            .functions
            .register("tick", move |param: &str| {
                let n = c.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                // The XML `param` string is passed through to the function.
                NDAttrValue::String(format!("{param}={n}"))
            });

        let xml = r#"<Attributes>
            <Attribute name="Live" type="FUNCTION" source="tick" param="seq"/>
        </Attributes>"#;
        drv.port_base
            .set_string_param(drv.params.attributes_file, 0, xml)
            .unwrap();
        drv.read_nd_attributes_file().unwrap();
        // Construction evaluated the function once (value = "seq=1").
        assert_eq!(
            drv.attributes().get("Live").unwrap().value,
            NDAttrValue::String("seq=1".into())
        );

        let snap = drv.update_attributes();
        assert_eq!(
            snap.get("Live").unwrap().value,
            NDAttrValue::String("seq=2".into())
        );
        let snap2 = drv.update_attributes();
        assert_eq!(
            snap2.get("Live").unwrap().value,
            NDAttrValue::String("seq=3".into())
        );
    }

    #[test]
    fn test_function_attribute_missing_function_is_undefined() {
        // G10: a FUNCTION attribute naming an unregistered function evaluates
        // to Undefined (C++ functAttribute::updateValue returns asynError).
        let mut drv = NDArrayDriverBase::new(
            "TEST",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        let xml = r#"<Attributes>
            <Attribute name="Missing" type="FUNCTION" source="no_such_fn"/>
        </Attributes>"#;
        drv.port_base
            .set_string_param(drv.params.attributes_file, 0, xml)
            .unwrap();
        drv.read_nd_attributes_file().unwrap();
        let snap = drv.update_attributes();
        assert_eq!(snap.get("Missing").unwrap().value, NDAttrValue::Undefined);
    }

    #[test]
    fn test_read_nd_attributes_file_empty_clears_attributes() {
        // G9: an empty ND_ATTRIBUTES_FILE clears the attribute list. The
        // status it leaves behind is asserted by
        // `test_empty_attributes_file_keeps_the_constructor_status`.
        let mut drv = NDArrayDriverBase::new(
            "TEST",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        drv.owner
            .attributes()
            .lock()
            .list
            .add(NDAttribute::new_static(
                "Gone",
                "",
                NDAttrSource::Constant("1".into()),
                NDAttrValue::Int32(1),
            ));
        drv.read_nd_attributes_file().unwrap();
        assert_eq!(drv.attributes().len(), 0);
    }

    #[test]
    fn test_read_nd_attributes_file_missing_file() {
        // G9: a non-existent file path yields FILE_NOT_FOUND status.
        let mut drv = NDArrayDriverBase::new(
            "TEST",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        drv.port_base
            .set_string_param(drv.params.attributes_file, 0, "/nonexistent_attrs_xyz.xml")
            .unwrap();
        assert!(drv.read_nd_attributes_file().is_err());
        assert_eq!(
            drv.port_base
                .get_int32_param(drv.params.attributes_status, 0)
                .unwrap(),
            ATTR_STATUS_FILE_NOT_FOUND
        );
    }

    /// asynNDArrayDriver.cpp:682, :699-702: the line carries setIntegerParam's
    /// own status, asynParamWrongType (8) for a Float64 parameter.
    #[test]
    fn a_refused_write_prints_the_param_librarys_status() {
        use asyn_rs::services::PortServices;
        use asyn_rs::user::UserTrace;
        let port = "NDWRONGTYPE";
        let mut drv = NDArrayDriverBase::new(
            port,
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        let manager = PortServices::global().trace().clone();
        let log = std::env::temp_dir().join(format!("nd-wrong-type-{}.log", std::process::id()));
        let file = std::fs::File::create(&log).unwrap();
        manager.set_trace_file(
            Some(port),
            asyn_rs::trace::TraceFile::File(Arc::new(std::sync::Mutex::new(file))),
        );
        let reason = drv.params.timestamp_rbv;
        let mut user = AsynUser::new(reason);
        user.trace = Some(UserTrace {
            manager,
            port: port.into(),
        });
        let pool = NDArrayPool::new(0);
        assert!(
            handle_write_int32(&mut drv.port_base, &drv.params, &pool, None, &user, 1).is_err()
        );
        let text = std::fs::read_to_string(&log).unwrap();
        let _ = std::fs::remove_file(&log);
        assert!(
            text.contains(&format!(
                "asynNDArrayDriver:writeInt32: error, status=8 function={reason}, paramName=TIME_STAMP, value=1"
            )),
            "{text}"
        );
    }

    /// asynNDArrayDriver.cpp:544-546: the write closes with its own line.
    #[test]
    fn a_write_octet_closes_with_the_asyn_nd_array_driver_line() {
        use asyn_rs::registry::PortRegistry;
        use asyn_rs::runtime::config::RuntimeConfig;
        use asyn_rs::runtime::port::create_port_runtime;

        struct Port(NDArrayDriverBase);
        impl asyn_rs::port::PortDriver for Port {
            fn base(&self) -> &PortDriverBase {
                &self.0.port_base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0.port_base
            }
        }
        let drv = NDArrayDriverBase::new(
            "NDOCTETLINE",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        let file_name = drv.params.file_name;
        let claim = PortRegistry::new().claim("NDOCTETLINE").unwrap();
        let (runtime, _actor) =
            create_port_runtime(&claim, Port(drv), RuntimeConfig::default()).unwrap();
        let port = runtime.port_handle();
        let log = std::env::temp_dir().join(format!("octet-line-{}.log", std::process::id()));
        let file = std::fs::File::create(&log).unwrap();
        port.trace().set_trace_file(
            Some("NDOCTETLINE"),
            asyn_rs::trace::TraceFile::File(Arc::new(std::sync::Mutex::new(file))),
        );
        port.trace()
            .set_trace_mask(Some("NDOCTETLINE"), TraceMask::ERROR | TraceMask::IO_DRIVER);
        port.submit_blocking(
            asyn_rs::request::RequestOp::OctetWrite {
                data: b"img".to_vec(),
            },
            AsynUser::new(file_name),
        )
        .unwrap();
        let text = std::fs::read_to_string(&log).unwrap();
        let _ = std::fs::remove_file(&log);
        assert!(
            text.contains(&format!(
                "asynNDArrayDriver:writeOctet: function={file_name}, paramName=FILE_NAME, value=img"
            )),
            "{text}"
        );
        drop(runtime);
    }

    #[test]
    fn test_write_octet_file_path_creates_dir() {
        // G9: writeOctet on FILE_PATH runs checkPath then createFilePath.
        let mut drv = NDArrayDriverBase::new(
            "TEST",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        let base = std::env::temp_dir().join(format!(
            "ad_core_rs_wo_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let target = base.join("sub");
        let path = format!("{}/", target.to_string_lossy());
        drv.port_base
            .set_int32_param(drv.params.create_dir, 0, 1)
            .unwrap();
        let user = AsynUser::new(drv.params.file_path);
        drv.write_octet(&user, format!("{path}\0").as_bytes())
            .unwrap();
        assert!(target.is_dir());
        // checkPath re-terminates with the OS separator, '\\' on Windows.
        let checked = format!("{}{}", target.to_string_lossy(), std::path::MAIN_SEPARATOR);
        assert_eq!(
            drv.port_base
                .get_string_param(drv.params.file_path, 0)
                .unwrap(),
            checked.as_bytes()
        );
        assert_eq!(
            drv.port_base
                .get_int32_param(drv.params.file_path_exists, 0)
                .unwrap(),
            1
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_write_octet_file_path_left_missing_fails() {
        let mut drv = NDArrayDriverBase::new(
            "TEST",
            1,
            1_000_000,
            ASYN_CANBLOCK,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )
        .unwrap();
        let missing = std::env::temp_dir()
            .join(format!("ad_core_rs_miss_{}", std::process::id()))
            .join("sub");
        let path = format!("{}/", missing.to_string_lossy());
        let reason = drv.params.file_path;
        let Err(asyn_rs::error::AsynError::Status { message, .. }) =
            drv.write_octet(&AsynUser::new(reason), path.as_bytes())
        else {
            panic!("a missing FilePath must fail the write");
        };
        assert_eq!(
            message,
            format!(
                "asynNDArrayDriver:writeOctet: status=3, function={reason}, paramName=FILE_PATH, value={path}"
            )
        );
        assert_eq!(
            drv.port_base
                .get_int32_param(drv.params.file_path_exists, 0)
                .unwrap(),
            0
        );
    }
}
