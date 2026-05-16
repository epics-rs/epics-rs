use std::path::Path;
use std::sync::Arc;

use asyn_rs::error::AsynResult;
use asyn_rs::port::{PortDriverBase, PortFlags};

use crate::ndarray::NDArray;
use crate::ndarray_pool::NDArrayPool;
use crate::params::ndarray_driver::NDArrayDriverParams;
use crate::plugin::channel::{NDArrayOutput, NDArraySender, QueuedArrayCounter};

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

    // G7: dimensions.
    port_base.set_int32_param(params.n_dimensions, 0, array.dims.len() as i32)?;
    let dim_sizes: Vec<i32> = array.dims.iter().map(|d| d.size as i32).collect();
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
            port_base.set_string_param(params.codec, 0, codec.name.as_str().into())?;
            port_base.set_int32_param(params.compressed_size, 0, codec.compressed_size as i32)?;
        }
        None => {
            port_base.set_string_param(params.codec, 0, String::new())?;
            port_base.set_int32_param(params.compressed_size, 0, info.total_bytes as i32)?;
        }
    }
    Ok(())
}

/// Refresh the pool-statistics parameters (`POOL_MAX_MEMORY`,
/// `POOL_USED_MEMORY`, `POOL_ALLOC_BUFFERS`, `POOL_FREE_BUFFERS`) from a pool.
///
/// Shared by the `NDPoolPollStats` dispatch and `preAllocateBuffers`.
pub(crate) fn refresh_pool_stats(
    port_base: &mut PortDriverBase,
    params: &NDArrayDriverParams,
    pool: &NDArrayPool,
) -> AsynResult<()> {
    const MEGABYTE: f64 = 1_048_576.0;
    port_base.set_float64_param(
        params.pool_max_memory,
        0,
        pool.max_memory() as f64 / MEGABYTE,
    )?;
    port_base.set_float64_param(
        params.pool_used_memory,
        0,
        pool.allocated_bytes() as f64 / MEGABYTE,
    )?;
    port_base.set_int32_param(
        params.pool_alloc_buffers,
        0,
        pool.num_alloc_buffers() as i32,
    )?;
    port_base.set_int32_param(params.pool_free_buffers, 0, pool.num_free_buffers() as i32)?;
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
pub struct NDArrayDriverBase {
    pub port_base: PortDriverBase,
    pub params: NDArrayDriverParams,
    pub pool: Arc<NDArrayPool>,
    pub array_output: NDArrayOutput,
    pub queued_counter: Arc<QueuedArrayCounter>,
    /// Most recently prepared array (C++ `pArrays[0]`), used as the template
    /// for `preAllocateBuffers`.
    pub last_array: Option<Arc<NDArray>>,
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

        port_base.set_int32_param(params.array_callbacks, 0, 1)?;
        port_base.set_float64_param(params.pool_max_memory, 0, max_memory as f64 / 1_048_576.0)?;

        let pool = Arc::new(NDArrayPool::new(max_memory));

        Ok(Self {
            port_base,
            params,
            pool,
            array_output: NDArrayOutput::new(),
            queued_counter: Arc::new(QueuedArrayCounter::new()),
            last_array: None,
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
    pub fn prepare_array(&mut self, array: Arc<NDArray>) -> AsynResult<Option<Arc<NDArray>>> {
        let counter = self
            .port_base
            .get_int32_param(self.params.array_counter, 0)?
            + 1;
        self.port_base
            .set_int32_param(self.params.array_counter, 0, counter)?;

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
        let path = self.port_base.get_string_param(self.params.file_path, 0)?;
        let name = self.port_base.get_string_param(self.params.file_name, 0)?;
        let number = self.port_base.get_int32_param(self.params.file_number, 0)?;
        let template = self
            .port_base
            .get_string_param(self.params.file_template, 0)?;
        let auto_increment = self
            .port_base
            .get_int32_param(self.params.auto_increment, 0)
            .unwrap_or(0);

        // C parity: an empty FILE_TEMPLATE is passed straight to epicsSnprintf,
        // which yields an empty string. Do NOT fabricate a default template.
        // sprintf_template handles the empty case correctly (no specifiers).
        let full = sprintf_template(template, path, name, number);

        self.port_base
            .set_string_param(self.params.full_file_name, 0, full.clone())?;

        // C++: auto-increment file number after creating filename
        if auto_increment != 0 {
            self.port_base
                .set_int32_param(self.params.file_number, 0, number + 1)?;
        }

        Ok(full)
    }

    /// Check if the file path directory exists.
    /// Normalizes the path to ensure it has a trailing '/'.
    pub fn check_path(&mut self) -> AsynResult<bool> {
        let path_ref = self.port_base.get_string_param(self.params.file_path, 0)?;
        let mut path = path_ref.to_string();
        // Ensure trailing separator (C++ checkPath does this)
        if !path.is_empty() && !path.ends_with('/') && !path.ends_with(std::path::MAIN_SEPARATOR) {
            path.push('/');
            self.port_base
                .set_string_param(self.params.file_path, 0, path.clone())?;
        }
        let exists = Path::new(&path).is_dir();
        self.port_base
            .set_int32_param(self.params.file_path_exists, 0, exists as i32)?;
        Ok(exists)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::channel::ndarray_channel;

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
            .set_string_param(drv.params.file_path, 0, "/tmp/".into())
            .unwrap();
        drv.port_base
            .set_string_param(drv.params.file_name, 0, "test_".into())
            .unwrap();
        drv.port_base
            .set_int32_param(drv.params.file_number, 0, 42)
            .unwrap();
        drv.port_base
            .set_string_param(drv.params.file_template, 0, "".into())
            .unwrap();

        let name = drv.create_file_name().unwrap();
        assert_eq!(name, "");
    }

    #[test]
    fn test_create_file_name_standard_template() {
        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
        drv.port_base
            .set_string_param(drv.params.file_path, 0, "/tmp/".into())
            .unwrap();
        drv.port_base
            .set_string_param(drv.params.file_name, 0, "test".into())
            .unwrap();
        drv.port_base
            .set_int32_param(drv.params.file_number, 0, 42)
            .unwrap();
        drv.port_base
            .set_string_param(drv.params.file_template, 0, "%s%s_%3.3d.dat".into())
            .unwrap();

        let name = drv.create_file_name().unwrap();
        assert_eq!(name, "/tmp/test_042.dat");
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
        drv.port_base
            .set_string_param(drv.params.file_path, 0, "/tmp".into())
            .unwrap();
        assert!(drv.check_path().unwrap());
    }

    #[test]
    fn test_check_path_not_exists() {
        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
        drv.port_base
            .set_string_param(drv.params.file_path, 0, "/nonexistent_path_xyz".into())
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
        assert_eq!(&dims[..], &[64, 48]);
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
            "bslz4"
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
        rt.block_on(drv.array_output.publish(to_publish));

        let received = receiver.blocking_recv().unwrap();
        assert_eq!(received.unique_id, id);
    }
}
