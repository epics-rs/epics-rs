use std::path::Path;
use std::sync::Arc;

use asyn_rs::error::AsynResult;
use asyn_rs::param::ParamValue;
use asyn_rs::port::{PortDriverBase, PortFlags};
use asyn_rs::request::ParamSetValue;

use crate::attributes::{
    EpicsPvAttributeSource, FunctionAttributeSource, NDAttrSource, NDAttrValue, NDAttribute,
    NDAttributeFunctionRegistry, ParamAttributeSource, PvAttrDbrType,
};
use crate::color::NDColorMode;
use crate::ndarray::NDArray;
use crate::ndarray_pool::NDArrayPool;
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
/// `ARRAY_DIMENSIONS`, `DATA_TYPE`, `COLOR_MODE`, `BAYER_PATTERN`,
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

    // G7: dimensions. C++ posts the fixed-length `dimsPrev_[ND_ARRAY_MAX_DIMS]`
    // (NDPluginDriver.cpp:220-231) — always 10 elements, zero-filled beyond the
    // array's `ndims` — so `readInt32Array` returns NORD=10, not `ndims`.
    port_base.set_int32_param(params.n_dimensions, 0, array.dims.len() as i32)?;
    let mut dim_sizes = vec![0i32; crate::ndarray::ND_ARRAY_MAX_DIMS];
    for (slot, d) in dim_sizes
        .iter_mut()
        .zip(array.dims.iter().take(crate::ndarray::ND_ARRAY_MAX_DIMS))
    {
        *slot = d.size as i32;
    }
    port_base
        .params
        .set_int32_array(params.array_dimensions, 0, dim_sizes)?;

    // G7: data type and color mode.
    port_base.set_int32_param(params.data_type, 0, array.data.data_type() as i32)?;
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
/// The four pool statistics (C `:1000-1003`) are deliberately absent: they
/// belong to `refresh_pool_stats`, which is this port's single owner of
/// them. A constructor that owns an [`NDArrayPool`] calls that immediately
/// after this.
pub fn init_read_only_params(
    port_base: &mut PortDriverBase,
    params: &NDArrayDriverParams,
    port_name: &str,
) -> AsynResult<()> {
    port_base.set_string_param(params.port_name_self, 0, port_name)?;
    // C `:961-966` publishes "ADCORE_VERSION.ADCORE_REVISION.
    // ADCORE_MODIFICATION" into both strings. There is no ADCore in this
    // workspace to take a version from, so both carry the `ad-core-rs` crate
    // version, which is the same artefact's version for this port. C's
    // comment at `:964-965` applies unchanged: a concrete driver overwrites
    // DriverVersion after construction.
    let version = env!("CARGO_PKG_VERSION");
    port_base.set_string_param(params.ad_core_version, 0, version)?;
    port_base.set_string_param(params.driver_version, 0, version)?;

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

    port_base.set_int32_param(params.num_queued_arrays, 0, 0)?;
    Ok(())
}

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
    const MEGABYTE: f64 = 1_048_576.0;
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
/// Shared by the `NDPoolPollStats` dispatch and `preAllocateBuffers`.
pub(crate) fn refresh_pool_stats(
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

/// Handle a write to a pool-control Int32 parameter, mirroring the pool branch
/// of C++ `asynNDArrayDriver::writeInt32` (asynNDArrayDriver.cpp:684-694).
///
/// `param_index` is the parameter that was just written; `value` is the value
/// written. Returns `true` when the parameter was a recognized pool-control
/// parameter and was handled. `template_array` is used by the
/// `POOL_PRE_ALLOC_BUFFERS` path (C++ uses `pArrays[0]` — the most recent
/// array); pass the driver's last array, or `None` if none exists yet.
pub(crate) fn handle_pool_write_int32(
    port_base: &mut PortDriverBase,
    params: &NDArrayDriverParams,
    pool: &NDArrayPool,
    param_index: usize,
    template_array: Option<&NDArray>,
) -> AsynResult<bool> {
    if param_index == params.pool_empty_free_list {
        pool.empty_free_list();
        refresh_pool_stats(port_base, params, pool)?;
        Ok(true)
    } else if param_index == params.pool_poll_stats {
        refresh_pool_stats(port_base, params, pool)?;
        Ok(true)
    } else if param_index == params.pool_pre_alloc {
        if let Some(template) = template_array {
            let count = port_base
                .get_int32_param(params.pool_num_pre_alloc_buffers, 0)
                .unwrap_or(0)
                .max(0) as usize;
            // C++ preAllocateBuffers ignores allocation errors per-array; here
            // we surface them so the caller knows the pool limit was hit.
            pool.pre_allocate_buffers(template, count).map_err(|e| {
                asyn_rs::error::AsynError::Status {
                    status: asyn_rs::error::AsynStatus::Error,
                    message: e.to_string(),
                }
            })?;
            refresh_pool_stats(port_base, params, pool)?;
        }
        // C++ resets NDPoolPreAllocBuffers back to 0 after running.
        port_base.set_int32_param(params.pool_pre_alloc, 0, 0)?;
        Ok(true)
    } else {
        Ok(false)
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

pub struct NDArrayDriverBase {
    pub port_base: PortDriverBase,
    pub params: NDArrayDriverParams,
    pub pool: Arc<NDArrayPool>,
    pub array_output: NDArrayOutput,
    pub queued_counter: Arc<QueuedArrayCounter>,
    /// Most recently prepared array (C++ `pArrays[0]`), used as the template
    /// for `preAllocateBuffers`.
    pub last_array: Option<Arc<NDArray>>,
    /// NDArray attribute definitions loaded from `ND_ATTRIBUTES_FILE`
    /// (C++ `asynNDArrayDriver::pAttributeList`).
    pub attributes: crate::attributes::NDAttributeList,
    /// Registry of named attribute functions for `FUNCTION`-type attributes
    /// (C++ `registryFunctionFind` / `registerNDAttributeFunction`).
    pub attr_functions: std::sync::Arc<NDAttributeFunctionRegistry>,
    /// CA client and the runtime its monitors live on, installed by the IOC
    /// that owns this port ([`NDArrayDriverBase::set_ca_client`]). C++ has no
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

impl NDArrayDriverBase {
    pub fn new(port_name: &str, max_memory: usize) -> AsynResult<Self> {
        let mut port_base = PortDriverBase::new(
            port_name,
            1,
            PortFlags {
                can_block: true,
                ..Default::default()
            },
        );

        let params = NDArrayDriverParams::create(&mut port_base)?;
        let pool = Arc::new(NDArrayPool::new(max_memory));

        init_read_only_params(&mut port_base, &params, port_name)?;
        refresh_pool_stats(&mut port_base, &params, &pool)?;
        // Not part of C's block: `asynNDArrayDriver` leaves NDArrayCallbacks
        // to the database. This port has no database behind a bare
        // `NDArrayDriverBase`, so callbacks default on.
        port_base.set_int32_param(params.array_callbacks, 0, 1)?;

        Ok(Self {
            port_base,
            params,
            pool,
            array_output: NDArrayOutput::new(),
            queued_counter: Arc::new(QueuedArrayCounter::new()),
            last_array: None,
            attributes: crate::attributes::NDAttributeList::new(),
            attr_functions: NDAttributeFunctionRegistry::new(),
            #[cfg(feature = "ioc")]
            ca_feeder: None,
            #[cfg(feature = "ioc")]
            ca_monitors: CaMonitorSet::default(),
        })
    }

    /// Connect a downstream channel-based receiver.
    pub fn connect_downstream(&mut self, mut sender: NDArraySender) {
        sender.set_queued_counter(self.queued_counter.clone());
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
        let template = self.last_array.clone();
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
    pub fn prepare_array(&mut self, mut array: Arc<NDArray>) -> AsynResult<Option<Arc<NDArray>>> {
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
        // `getAttributes(pArray->pAttributeList)` before the callback. Skipped
        // when the driver has no attribute definitions, so a plain array is
        // never needlessly deep-copied via `Arc::make_mut`.
        if !self.attributes.is_empty() {
            let fresh = self.update_attributes();
            Arc::make_mut(&mut array).attributes.copy_from(&fresh);
        }

        // G5/G6/G7: write all per-array parameters (size, dims, type, color,
        // Bayer, timestamps, codec).
        write_array_params(&mut self.port_base, &self.params, &array)?;

        // Record this as the template array for preAllocateBuffers.
        self.last_array = Some(array.clone());

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
            .get_int32_param(self.params.array_callbacks, 0)?
            != 0;

        let to_publish = if callbacks_enabled {
            self.port_base.set_generic_pointer_param(
                self.params.ndarray_data,
                0,
                array.clone() as Arc<dyn std::any::Any + Send + Sync>,
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
            .set_string_param(self.params.full_file_name, 0, full.clone())?;

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
    /// The time comes from the port's registered timestamp source, so a driver
    /// with a hardware clock stamps from it (C `updateTimeStamp`), and
    /// `timeStamp` is always derived from `epicsTS` — the two cannot disagree.
    /// `NDArrayPool::alloc` sets neither; this is the only place that sets them
    /// for a newly produced frame.
    pub fn update_time_stamps(&self, array: &mut NDArray) {
        array.update_time_stamps(self.port_base.current_timestamp().into());
    }

    /// Check whether the `FILE_PATH` directory exists, normalizing the path.
    ///
    /// C++ `asynNDArrayDriver::checkPath()` (asynNDArrayDriver.cpp:98-109): an
    /// empty FilePath returns early — neither the path nor `FilePathExists` is
    /// written. Otherwise the path is normalized in place by
    /// [`check_path_str`], written back to the parameter, and `FilePathExists`
    /// is refreshed.
    pub fn check_path(&mut self) -> AsynResult<bool> {
        let mut path = self
            .port_base
            .get_string_param(self.params.file_path, 0)
            .map(octet_text)?;
        if path.is_empty() {
            return Ok(false);
        }

        let exists = check_path_str(&mut path);
        self.port_base
            .set_string_param(self.params.file_path, 0, path)?;
        self.port_base
            .set_int32_param(self.params.file_path_exists, 0, exists as i32)?;
        Ok(exists)
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

    /// Handle a write to an Octet parameter, mirroring the relevant branches of
    /// C++ `asynNDArrayDriver::writeOctet`.
    ///
    /// - `ND_ATTRIBUTES_FILE` / `ND_ATTRIBUTES_MACROS`: reload the attribute
    ///   definitions via [`Self::read_nd_attributes_file`].
    /// - `FILE_PATH`: run `checkPath`; if the directory does not exist, attempt
    ///   `createFilePath` bounded by `CREATE_DIR`, then re-check.
    ///
    /// The caller is expected to have already stored `value` into the parameter
    /// library. Returns `true` when `param_index` was a recognized parameter.
    pub fn write_octet(&mut self, param_index: usize, value: &str) -> AsynResult<bool> {
        if param_index == self.params.attributes_file
            || param_index == self.params.attributes_macros
        {
            let _ = self.read_nd_attributes_file();
            Ok(true)
        } else if param_index == self.params.file_path {
            if !self.check_path()? {
                let depth = self
                    .port_base
                    .get_int32_param(self.params.create_dir, 0)
                    .unwrap_or(0);
                let _ = Self::create_file_path(value, depth);
                self.check_path()?;
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Load NDArray attribute definitions from the `ND_ATTRIBUTES_FILE`
    /// parameter, mirroring C++ `asynNDArrayDriver::readNDAttributesFile`.
    ///
    /// The parameter value is either a path to an XML file or inline XML
    /// (recognized by containing `<Attributes>`). The XML schema is the
    /// areaDetector `NDAttributesFile` schema: a root `<Attributes>` element
    /// with `<Attribute name="..." source="..." type="..." .../>` children.
    /// Macro substitution (C++ `ND_ATTRIBUTES_MACROS`) is not supported and is
    /// ignored. Attributes are stored on `self.attributes`; when a file is
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
                .attributes
                .iter()
                .filter(|a| matches!(a.source, NDAttrSource::EpicsPV(_)))
                .map(|a| a.name.clone())
                .collect();
        };
        let handles: Vec<_> = self
            .attributes
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
        self.attributes
            .iter()
            .filter(|a| matches!(a.source, NDAttrSource::EpicsPV(_)))
            .map(|a| a.name.clone())
            .collect()
    }

    /// Drop the loaded attributes together with the monitors feeding them.
    ///
    /// The two are only ever replaced as a pair; separating them is what would
    /// let a monitor survive the attribute it writes into.
    fn clear_attributes(&mut self) {
        self.attributes.clear();
        #[cfg(feature = "ioc")]
        {
            self.ca_monitors = CaMonitorSet::default();
        }
    }

    pub fn read_nd_attributes_file(&mut self) -> AsynResult<()> {
        let file_param = self
            .port_base
            .get_string_param(self.params.attributes_file, 0)
            .map(octet_text)?;

        // Clear any existing attributes (C++ clears unconditionally first).
        self.clear_attributes();
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
                    self.port_base.set_int32_param(
                        self.params.attributes_status,
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

        match parse_attributes_xml(&xml, &self.attr_functions) {
            Ok(attrs) => {
                for attr in attrs {
                    self.attributes.add(attr);
                }
                let unfed = self.feed_epics_pv_attributes();
                if unfed.is_empty() {
                    self.port_base.set_int32_param(
                        self.params.attributes_status,
                        0,
                        ATTR_STATUS_OK,
                    )?;
                    return Ok(());
                }
                self.port_base.set_int32_param(
                    self.params.attributes_status,
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
                self.port_base.set_int32_param(
                    self.params.attributes_status,
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

    /// Access the driver's NDArray attribute list (populated by
    /// `read_nd_attributes_file`).
    pub fn attributes(&self) -> &crate::attributes::NDAttributeList {
        &self.attributes
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
    pub fn update_attributes(&mut self) -> crate::attributes::NDAttributeList {
        // 1. Feed each PARAM attribute's cell from the parameter library.
        //    Done first (immutable borrow of attributes + port_base), so the
        //    subsequent update_values() re-read picks up the fresh value.
        for attr in self.attributes.iter() {
            if let Some(param_src) = attr.param_source() {
                if let Some(value) = self.read_param_value(param_src) {
                    param_src.cell().set(value);
                }
            }
        }
        // 2. Re-evaluate every attribute from its (now-fresh) source.
        self.attributes.update_values();
        // 3. Return a snapshot for the outgoing array.
        self.attributes.clone()
    }

    /// Read a `Param` attribute's current value from the asyn parameter
    /// library, using the read type the XML `datatype` selected — NOT the
    /// param's runtime type. Mirrors C++ `paramAttribute::updateValue`
    /// (paramAttribute.cpp:131-151), which dispatches on `paramType` to the
    /// matching `getXxxParam` and publishes a fixed `NDAttrDataType`.
    ///
    /// Returns `None` (leaving the attribute's cell at its prior / `Undefined`
    /// value) when the param is unknown, the value is undefined, the read type
    /// mismatches the param's actual type, or the `datatype` was unrecognized
    /// (`ParamAttrType::Unknown` — C's `paramAttrTypeUnknown`, which never
    /// refreshes the attribute).
    fn read_param_value(
        &self,
        src: &crate::attributes::ParamAttributeSource,
    ) -> Option<NDAttrValue> {
        use crate::attributes::ParamAttrType;
        let index = self.port_base.params.find_param(&src.param_name)?;
        let addr = src.addr;
        match src.param_type {
            ParamAttrType::Int32 => self
                .port_base
                .params
                .get_int32(index, addr)
                .ok()
                .map(NDAttrValue::Int32),
            ParamAttrType::Int64 => self
                .port_base
                .params
                .get_int64(index, addr)
                .ok()
                .map(NDAttrValue::Int64),
            ParamAttrType::Float64 => self
                .port_base
                .params
                .get_float64(index, addr)
                .ok()
                .map(NDAttrValue::Float64),
            ParamAttrType::String => self
                .port_base
                .params
                .get_string(index, addr)
                .ok()
                .map(|s| NDAttrValue::String(octet_text(s))),
            ParamAttrType::Unknown => None,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::plugin::channel::ndarray_channel;

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
        let version = env!("CARGO_PKG_VERSION");
        for (name, index, want) in [
            ("PORT_NAME_SELF", params.port_name_self, port_name),
            ("ADCORE_VERSION", params.ad_core_version, version),
            ("DRIVER_VERSION", params.driver_version, version),
            ("FILE_WRITE_MESSAGE", params.file_write_message, ""),
            ("FILE_PATH", params.file_path, ""),
            ("FILE_NAME", params.file_name, ""),
            ("FILE_TEMPLATE", params.file_template, "%s%s_%3.3d.dat"),
            ("TEMP_SUFFIX", params.temp_suffix, ""),
            ("ND_ATTRIBUTES_FILE", params.attributes_file, ""),
            ("ND_ATTRIBUTES_MACROS", params.attributes_macros, ""),
        ] {
            assert_eq!(
                base.get_string_param_strict(index, 0)
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
                base.get_int32_param_strict(index, 0)
                    .unwrap_or_else(|e| panic!("{name} unset after construction: {e:?}")),
                want,
                "{name}"
            );
        }

        assert_eq!(
            base.get_float64_param_strict(params.timestamp_rbv, 0)
                .expect("TIME_STAMP unset after construction"),
            0.0
        );
    }

    /// The four pool statistics, C `:1000-1003`, for a pool nothing has
    /// allocated from yet.
    pub(crate) fn assert_fresh_pool_stats(
        base: &PortDriverBase,
        params: &NDArrayDriverParams,
        max_memory_mb: f64,
    ) {
        assert_eq!(
            base.get_int32_param_strict(params.pool_alloc_buffers, 0)
                .expect("POOL_ALLOC_BUFFERS unset after construction"),
            0
        );
        assert_eq!(
            base.get_int32_param_strict(params.pool_free_buffers, 0)
                .expect("POOL_FREE_BUFFERS unset after construction"),
            0
        );
        assert_eq!(
            base.get_float64_param_strict(params.pool_used_memory, 0)
                .expect("POOL_USED_MEMORY unset after construction"),
            0.0
        );
        assert_eq!(
            base.get_float64_param_strict(params.pool_max_memory, 0)
                .expect("POOL_MAX_MEMORY unset after construction"),
            max_memory_mb
        );
    }

    #[test]
    fn test_constructor_seeds_the_c_read_only_block() {
        let drv = NDArrayDriverBase::new("TEST", 4_194_304).unwrap();
        assert_c_read_only_block(&drv.port_base, &drv.params, "TEST");
        // C `:1002-1003` leaves both memory figures at 0 and fills MaxMemory
        // from the pool later (asynNDArrayDriver.cpp:690). The port has the
        // pool in hand at construction, so it publishes C's eventual value
        // from the start rather than a transient 0.
        assert_fresh_pool_stats(&drv.port_base, &drv.params, 4.0);
    }

    #[test]
    fn test_empty_attributes_file_keeps_the_constructor_status() {
        // C `:327` returns asynSuccess on an empty NDAttributesFile without
        // touching NDAttributesStatus, so an IOC that never loads one reads
        // "File not found" — including after the PINI="YES" write that
        // NDArrayBase.template:793 performs at iocInit.
        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
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
    fn test_new_sets_callbacks_enabled() {
        let drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
        assert_eq!(
            drv.port_base
                .get_int32_param(drv.params.array_callbacks, 0)
                .unwrap(),
            1,
        );
    }

    #[test]
    fn test_prepare_array() {
        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
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
        drv.prepare_array(Arc::new(arr)).unwrap();
        assert_eq!(
            drv.port_base
                .get_int32_param(drv.params.array_counter, 0)
                .unwrap(),
            1,
        );
    }

    #[test]
    fn test_prepare_updates_size_info() {
        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
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
        drv.prepare_array(Arc::new(arr)).unwrap();
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
        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
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
        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
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
        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
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
        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
        drv.port_base
            .set_int32_param(drv.params.file_path_exists, 0, 1)
            .unwrap();
        assert!(!drv.check_path().unwrap());
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
        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
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
        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
        drv.port_base
            .set_string_param(drv.params.file_path, 0, "/nonexistent_path_xyz")
            .unwrap();
        assert!(!drv.check_path().unwrap());
    }

    #[test]
    fn test_prepare_array_publishes_dims_type_timestamps() {
        // G7: prepare_array must publish N_DIMENSIONS, ARRAY_DIMENSIONS,
        // DATA_TYPE, COLOR_MODE, TIME_STAMP, EPICS_TS_SEC/NSEC.
        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
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
        arr.time_stamp = 100.5;
        arr.timestamp = crate::timestamp::EpicsTimestamp {
            sec: 1234,
            nsec: 5678,
        };
        drv.prepare_array(Arc::new(arr)).unwrap();

        assert_eq!(
            drv.port_base
                .get_int32_param(drv.params.n_dimensions, 0)
                .unwrap(),
            2
        );
        let dims = drv
            .port_base
            .params
            .get_int32_array(drv.params.array_dimensions, 0)
            .unwrap();
        // C++ posts the fixed ND_ARRAY_MAX_DIMS (10) array, zero-filled beyond
        // the 2 real dims (NDPluginDriver.cpp:220-231).
        assert_eq!(&dims[..], &[64, 48, 0, 0, 0, 0, 0, 0, 0, 0]);
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

        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
        let mut arr = drv
            .pool
            .alloc(
                vec![crate::ndarray::NDDimension::new(16)],
                crate::ndarray::NDDataType::UInt8,
            )
            .unwrap();
        arr.codec = Some(crate::codec::Codec {
            name: crate::codec::CodecName::BSLZ4,
            compressed_size: 9,
            level: 0,
            shuffle: 0,
            compressor: 0,
            original_data_type: crate::ndarray::NDDataType::UInt8,
        });
        arr.attributes.add(NDAttribute {
            name: "bayerPattern".into(),
            description: String::new(),
            source: NDAttrSource::Driver,
            value: NDAttrValue::Int32(crate::color::NDBayerPattern::GRBG as i32),
            source_impl: None,
        });
        drv.prepare_array(Arc::new(arr)).unwrap();

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
        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
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
        let to_publish = drv.prepare_array(Arc::new(arr)).unwrap().unwrap();

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
        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
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
        let gain = drv.attributes().get("Gain").unwrap();
        assert!(matches!(gain.source, NDAttrSource::Param { .. }));
        let comment = drv.attributes().get("Comment").unwrap();
        assert_eq!(comment.value, NDAttrValue::String("hello".into()));
        // C publishes the original XML `source` string verbatim, not a label.
        assert_eq!(comment.source.source_string(), "hello");
        assert_eq!(gain.source.source_string(), "GAIN");
        let temp = drv.attributes().get("Temp").unwrap();
        assert!(matches!(temp.source, NDAttrSource::EpicsPV(_)));
        assert_eq!(temp.source.source_string(), "$(P)Temp");
        assert_eq!(
            drv.port_base
                .get_int32_param(drv.params.attributes_status, 0)
                .unwrap(),
            ATTR_STATUS_XML_SYNTAX_ERROR
        );
    }

    #[test]
    fn test_epics_pv_attribute_is_not_reported_ok() {
        // An EPICS_PV attribute evaluates Undefined for the life of the IOC
        // (no CA feeder exists), so NDAttributesStatus must not read
        // "Attributes file OK" — that is what made the gap invisible.
        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
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
        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
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
        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
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
        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
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
        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
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
            NDAttrValue::Undefined,
            "datatype=DOUBLE on an Int32 param must not coerce — stays Undefined"
        );
        assert_eq!(snap.get("AsInt").unwrap().value, NDAttrValue::Int32(42));
    }

    #[test]
    fn test_function_attribute_reevaluates_from_registry() {
        // G10: a FUNCTION attribute loaded from XML must call its registered
        // function on update_attributes().
        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
        // Register a function whose return value changes on each call.
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0));
        let c = counter.clone();
        drv.attr_functions.register("tick", move |param: &str| {
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
        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
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
        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
        drv.attributes.add(NDAttribute::new_static(
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
        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
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

    #[test]
    fn test_write_octet_file_path_creates_dir() {
        // G9: writeOctet on FILE_PATH runs checkPath then createFilePath.
        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
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
        drv.port_base
            .set_string_param(drv.params.file_path, 0, path.clone())
            .unwrap();
        let handled = drv.write_octet(drv.params.file_path, &path).unwrap();
        assert!(handled);
        assert!(target.is_dir());
        assert_eq!(
            drv.port_base
                .get_int32_param(drv.params.file_path_exists, 0)
                .unwrap(),
            1
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
