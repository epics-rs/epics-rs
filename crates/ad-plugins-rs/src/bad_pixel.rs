//! NDPluginBadPixel: replaces bad pixels using one of three correction modes.
//!
//! Bad pixel definitions are loaded from JSON in the AreaDetector C++ format:
//!
//! ```json
//! {"Bad pixels": [
//!   {"Pixel": [x, y], "Set": value},
//!   {"Pixel": [x, y], "Replace": [dx, dy]},
//!   {"Pixel": [x, y], "Median": [mx, my]}
//! ]}
//! ```
//!
//! Each bad pixel specifies its sensor-space `Pixel` `[x, y]` coordinate and
//! exactly one correction key:
//! - **Set**: replace with a fixed value.
//! - **Replace**: copy from a neighbor at relative offset `[dx, dy]`.
//! - **Median**: median of a `(2*mx+1) x (2*my+1)` kernel around the pixel
//!   (the `Median` values are half-extents, matching C++ `medianCoordinate`).

use std::collections::HashSet;
use std::sync::Arc;

use ad_core_rs::ndarray::{NDArray, NDDataBuffer};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, ProcessResult};
use serde::Deserialize;

/// The correction mode for a bad pixel.
#[derive(Debug, Clone, PartialEq)]
pub enum BadPixelMode {
    /// Replace the pixel with a fixed value.
    Set { value: f64 },
    /// Replace the pixel by copying from a neighbor at relative offset (dx, dy).
    Replace { dx: i32, dy: i32 },
    /// Replace the pixel with the median of a kernel. `half_x`/`half_y` are the
    /// kernel half-extents (C++ `medianCoordinate`); the kernel spans
    /// `(2*half_x+1) x (2*half_y+1)` pixels.
    Median { half_x: i64, half_y: i64 },
}

/// A single bad pixel definition (sensor-space coordinate + correction mode).
#[derive(Debug, Clone, PartialEq)]
pub struct BadPixel {
    pub x: i64,
    pub y: i64,
    pub mode: BadPixelMode,
}

/// Raw JSON shape of a single bad-pixel entry, matching the C++
/// `readBadPixelFile` schema. `Pixel` is required; exactly one of `Set` /
/// `Replace` / `Median` selects the correction mode.
#[derive(Debug, Deserialize)]
struct BadPixelJson {
    #[serde(rename = "Pixel")]
    pixel: [i64; 2],
    #[serde(rename = "Set", default)]
    set: Option<f64>,
    #[serde(rename = "Replace", default)]
    replace: Option<[i64; 2]>,
    #[serde(rename = "Median", default)]
    median: Option<[i64; 2]>,
}

/// Container for deserializing the C++ bad-pixel file: `{"Bad pixels": [...]}`.
#[derive(Debug, Deserialize)]
struct BadPixelFileJson {
    #[serde(rename = "Bad pixels")]
    bad_pixels: Vec<BadPixelJson>,
}

/// Processor that corrects bad pixels in incoming arrays.
pub struct BadPixelProcessor {
    pixels: Vec<BadPixel>,
    /// Set of sensor-space (x, y) for fast bad-pixel lookup, matching the C++
    /// `badPixelList` which is keyed on the sensor `coordinate`.
    bad_set: HashSet<(i64, i64)>,
    /// Cached image width from the last array.
    width: usize,
    file_name_idx: Option<usize>,
}

impl BadPixelProcessor {
    /// Create a new processor from a list of bad pixels.
    pub fn new(pixels: Vec<BadPixel>) -> Self {
        let bad_set: HashSet<(i64, i64)> = pixels.iter().map(|p| (p.x, p.y)).collect();
        Self {
            pixels,
            bad_set,
            width: 0,
            file_name_idx: None,
        }
    }

    /// Parse a bad pixel list from a JSON string in the C++ AreaDetector
    /// `{"Bad pixels": [...]}` format.
    ///
    /// As in C++ `readBadPixelFile`, when multiple correction keys are present
    /// the precedence is Median, then Set, then Replace (the later key wins).
    /// An entry with no correction key defaults to `Set { value: 0.0 }`.
    pub fn load_from_json(json_str: &str) -> Result<Vec<BadPixel>, serde_json::Error> {
        let file: BadPixelFileJson = serde_json::from_str(json_str)?;
        Ok(file
            .bad_pixels
            .into_iter()
            .map(|e| {
                // C++ checks Median, then Set, then Replace; the last present
                // key wins.
                let mut mode = BadPixelMode::Set { value: 0.0 };
                if let Some(m) = e.median {
                    mode = BadPixelMode::Median {
                        half_x: m[0],
                        half_y: m[1],
                    };
                }
                if let Some(v) = e.set {
                    mode = BadPixelMode::Set { value: v };
                }
                if let Some(r) = e.replace {
                    mode = BadPixelMode::Replace {
                        dx: r[0] as i32,
                        dy: r[1] as i32,
                    };
                }
                BadPixel {
                    x: e.pixel[0],
                    y: e.pixel[1],
                    mode,
                }
            })
            .collect())
    }

    /// Replace the bad pixel list.
    pub fn set_pixels(&mut self, pixels: Vec<BadPixel>) {
        self.bad_set = pixels.iter().map(|p| (p.x, p.y)).collect();
        self.pixels = pixels;
    }

    /// Get the current bad pixel list.
    pub fn pixels(&self) -> &[BadPixel] {
        &self.pixels
    }

    /// Check if a sensor-space coordinate is a registered bad pixel.
    fn is_bad(&self, x: i64, y: i64) -> bool {
        self.bad_set.contains(&(x, y))
    }

    /// Apply corrections to a mutable data buffer.
    ///
    /// Mirrors C++ `fixBadPixelsT`: bad-pixel coordinates and Replace/Median
    /// neighbor coordinates are all expressed in sensor space and converted to
    /// an array offset via `pixel_offset` (C++ `computePixelOffset`).
    /// Replace/Median offsets are scaled by the array binning (`scaleX`,
    /// `scaleY`), and the "is the neighbor also bad" test queries the bad set
    /// in sensor space.
    #[allow(clippy::too_many_arguments)]
    fn apply_corrections(
        &self,
        data: &mut NDDataBuffer,
        width: usize,
        height: usize,
        offset_x: i64,
        offset_y: i64,
        binning_x: i64,
        binning_y: i64,
    ) {
        let scale_x = binning_x.max(1);
        let scale_y = binning_y.max(1);

        // Convert a sensor-space coordinate to a flat array offset, or None if
        // it falls outside the readout window (C++ computePixelOffset).
        //
        // The division must FLOOR, not truncate toward zero: with binning > 1
        // a sensor coordinate just left/above the readout window has a
        // negative numerator (`sx - offset_x`). Plain `/` truncates e.g.
        // `-1 / 2` to `0`, which would alias an out-of-window pixel onto array
        // index 0. `div_euclid` with a positive divisor floors, so the result
        // stays negative and is correctly rejected by the `>= 0` bounds test.
        let pixel_offset = |sx: i64, sy: i64| -> Option<usize> {
            let x = (sx - offset_x).div_euclid(binning_x.max(1));
            let y = (sy - offset_y).div_euclid(binning_y.max(1));
            if x >= 0 && y >= 0 && x < width as i64 && y < height as i64 {
                Some(y as usize * width + x as usize)
            } else {
                None
            }
        };

        // Collect corrections, then apply (Replace reads the original buffer).
        let mut corrections: Vec<(usize, f64)> = Vec::with_capacity(self.pixels.len());

        for bp in &self.pixels {
            let Some(offset) = pixel_offset(bp.x, bp.y) else {
                continue;
            };

            let value = match &bp.mode {
                BadPixelMode::Set { value } => *value,

                BadPixelMode::Replace { dx, dy } => {
                    // Neighbor coordinate in SENSOR space, scaled by binning.
                    let nx = bp.x + (*dx as i64) * scale_x;
                    let ny = bp.y + (*dy as i64) * scale_y;
                    // Skip if the replacement pixel is also a bad pixel.
                    if self.is_bad(nx, ny) {
                        continue;
                    }
                    let Some(replace_offset) = pixel_offset(nx, ny) else {
                        continue;
                    };
                    match data.get_as_f64(replace_offset) {
                        Some(v) => v,
                        None => continue,
                    }
                }

                BadPixelMode::Median { half_x, half_y } => {
                    // Kernel half-extents: spans (2*half_x+1) x (2*half_y+1).
                    let mut neighbors = Vec::new();
                    for i in -*half_y..=*half_y {
                        let cy = bp.y + i * scale_y;
                        for j in -*half_x..=*half_x {
                            if i == 0 && j == 0 {
                                continue; // skip the bad pixel itself
                            }
                            let cx = bp.x + j * scale_x;
                            // Skip other bad pixels (sensor-space lookup).
                            if self.is_bad(cx, cy) {
                                continue;
                            }
                            let Some(idx) = pixel_offset(cx, cy) else {
                                continue;
                            };
                            if let Some(v) = data.get_as_f64(idx) {
                                neighbors.push(v);
                            }
                        }
                    }

                    if neighbors.is_empty() {
                        continue; // no valid neighbors
                    }

                    neighbors.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                    let mid = neighbors.len() / 2;
                    if neighbors.len() % 2 == 0 {
                        (neighbors[mid - 1] + neighbors[mid]) / 2.0
                    } else {
                        neighbors[mid]
                    }
                }
            };

            corrections.push((offset, value));
        }

        // Apply all corrections
        for (idx, value) in corrections {
            data.set_from_f64(idx, value);
        }
    }
}

impl NDPluginProcess for BadPixelProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        let info = array.info();
        self.width = info.x_size;
        let height = info.y_size;

        if self.pixels.is_empty() {
            // No corrections needed, pass through
            return ProcessResult::arrays(vec![Arc::new(array.clone())]);
        }

        // C `NDPluginBadPixel.cpp:99-109` reads the detector offset/binning from
        // the axis the image info names — `pArray->dims[pArrayInfo->xDim]` and
        // `[pArrayInfo->yDim]` — not from the physical dims[0]/dims[1]. They
        // differ for every non-RGB3 color layout (RGB1 keeps X in dims[1]).
        // Y falls back to offset 0 / binning 1 when `ndims <= 1` (`:105-108`).
        let [x_dim, y_dim, _] = info.user_dims();
        let offset_x = array.dims.get(x_dim).map_or(0, |d| d.offset as i64);
        let binning_x = array.dims.get(x_dim).map_or(1, |d| d.binning.max(1) as i64);
        let (offset_y, binning_y) = if array.dims.len() > 1 {
            let d = &array.dims[y_dim];
            (d.offset as i64, d.binning.max(1) as i64)
        } else {
            (0, 1)
        };

        let mut out = array.clone();
        self.apply_corrections(
            &mut out.data,
            self.width,
            height,
            offset_x,
            offset_y,
            binning_x,
            binning_y,
        );
        ProcessResult::arrays(vec![Arc::new(out)])
    }

    fn plugin_type(&self) -> &str {
        "NDPluginBadPixel"
    }

    fn register_params(
        &mut self,
        base: &mut asyn_rs::port::PortDriverBase,
    ) -> asyn_rs::error::AsynResult<()> {
        use asyn_rs::param::ParamType;
        base.create_param("BAD_PIXEL_FILE_NAME", ParamType::Octet)?;
        self.file_name_idx = base.find_param("BAD_PIXEL_FILE_NAME");
        Ok(())
    }

    fn on_param_change(
        &mut self,
        reason: usize,
        params: &ad_core_rs::plugin::runtime::PluginParamSnapshot,
    ) -> ad_core_rs::plugin::runtime::ParamChangeResult {
        use ad_core_rs::plugin::runtime::ParamChangeValue;

        if Some(reason) == self.file_name_idx {
            if let ParamChangeValue::Octet(path) = &params.value {
                if !path.is_empty() {
                    match std::fs::read_to_string(path) {
                        Ok(json_str) => match Self::load_from_json(&json_str) {
                            Ok(pixels) => {
                                self.set_pixels(pixels);
                                tracing::info!(
                                    "BadPixel: loaded {} pixels from {}",
                                    self.pixels.len(),
                                    path
                                );
                            }
                            Err(e) => {
                                tracing::warn!("BadPixel: failed to parse {}: {}", path, e);
                            }
                        },
                        Err(e) => {
                            tracing::warn!("BadPixel: failed to read {}: {}", path, e);
                        }
                    }
                }
            }
        }

        ad_core_rs::plugin::runtime::ParamChangeResult::updates(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core_rs::ndarray::{NDDataType, NDDimension};

    fn make_2d_array(x: usize, y: usize, fill: impl Fn(usize, usize) -> f64) -> NDArray {
        let mut arr = NDArray::new(
            vec![NDDimension::new(x), NDDimension::new(y)],
            NDDataType::Float64,
        );
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            for iy in 0..y {
                for ix in 0..x {
                    v[iy * x + ix] = fill(ix, iy);
                }
            }
        }
        arr
    }

    fn get_pixel(arr: &NDArray, x: usize, y: usize, width: usize) -> f64 {
        arr.data.get_as_f64(y * width + x).unwrap()
    }

    fn set(x: i64, y: i64, value: f64) -> BadPixel {
        BadPixel {
            x,
            y,
            mode: BadPixelMode::Set { value },
        }
    }

    #[test]
    fn test_set_mode() {
        let arr = make_2d_array(4, 4, |_, _| 100.0);
        let pixels = vec![set(1, 1, 0.0), set(3, 2, 42.0)];

        let mut proc = BadPixelProcessor::new(pixels);
        let pool = NDArrayPool::new(1_000_000);
        let result = proc.process_array(&arr, &pool);

        assert_eq!(result.output_arrays.len(), 1);
        let out = &result.output_arrays[0];
        assert!((get_pixel(out, 1, 1, 4) - 0.0).abs() < 1e-10);
        assert!((get_pixel(out, 3, 2, 4) - 42.0).abs() < 1e-10);
        assert!((get_pixel(out, 0, 0, 4) - 100.0).abs() < 1e-10);
    }

    #[test]
    fn test_r9_67_readout_offset_comes_from_the_user_dims_axis() {
        // R9-67 family. C takes the detector readout offset/binning from the axis
        // the image info names — `pArray->dims[pArrayInfo->xDim]` /
        // `[pArrayInfo->yDim]` (NDPluginBadPixel.cpp:99-104) — while the port read
        // the physical dims[0]/dims[1]. On RGB1 (`[color, x, y]`) dims[0] is the
        // COLOUR axis, so the port used the colour axis's offset as the X readout
        // offset and corrected the wrong pixel.
        use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
        use ad_core_rs::color::NDColorMode;

        // RGB1: dims = [color=3, x=4, y=2]. The X axis (dims[1]) carries a
        // readout offset of 2; the colour axis carries none.
        let mut arr = NDArray::new(
            vec![
                NDDimension::new(3),
                NDDimension::new(4),
                NDDimension::new(2),
            ],
            NDDataType::Float64,
        );
        arr.dims[1].offset = 2;
        arr.attributes.add(NDAttribute::new_static(
            "ColorMode",
            "",
            NDAttrSource::Driver,
            NDAttrValue::Int32(NDColorMode::RGB1 as i32),
        ));
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            v.iter_mut().for_each(|x| *x = 100.0);
        }

        // Sensor coordinate (3, 0). C: x = 3 - offsetX(2) = 1, y = 0
        //   → buffer offset y * xSize + x = 1.
        // Pre-fix the port took offsetX from dims[0] (the colour axis, offset 0)
        //   → x = 3, buffer offset 3.
        let mut proc = BadPixelProcessor::new(vec![set(3, 0, 7.0)]);
        let pool = NDArrayPool::new(1_000_000);
        let result = proc.process_array(&arr, &pool);
        let out = &result.output_arrays[0];

        assert_eq!(out.data.get_as_f64(1), Some(7.0), "corrected element 1");
        assert_eq!(
            out.data.get_as_f64(3),
            Some(100.0),
            "element 3 must be untouched — that is where the physical-index bug wrote"
        );
    }

    #[test]
    fn test_replace_mode() {
        let arr = make_2d_array(4, 4, |x, y| (x + y * 4) as f64);
        // Replace pixel (2,2) with value from (3,2)
        let pixels = vec![BadPixel {
            x: 2,
            y: 2,
            mode: BadPixelMode::Replace { dx: 1, dy: 0 },
        }];

        let mut proc = BadPixelProcessor::new(pixels);
        let pool = NDArrayPool::new(1_000_000);
        let result = proc.process_array(&arr, &pool);

        let out = &result.output_arrays[0];
        // (3,2) = 3 + 2*4 = 11
        assert!((get_pixel(out, 2, 2, 4) - 11.0).abs() < 1e-10);
    }

    #[test]
    fn test_replace_skip_bad_neighbor() {
        let arr = make_2d_array(4, 4, |_, _| 50.0);
        // Both (1,1) and (2,1) are bad. (1,1) tries to replace from (2,1), also bad.
        let pixels = vec![
            BadPixel {
                x: 1,
                y: 1,
                mode: BadPixelMode::Replace { dx: 1, dy: 0 },
            },
            set(2, 1, 0.0),
        ];

        let mut proc = BadPixelProcessor::new(pixels);
        let pool = NDArrayPool::new(1_000_000);
        let result = proc.process_array(&arr, &pool);

        let out = &result.output_arrays[0];
        // (1,1) unchanged (50.0) since replacement source is bad
        assert!((get_pixel(out, 1, 1, 4) - 50.0).abs() < 1e-10);
        // (2,1) set to 0.0
        assert!((get_pixel(out, 2, 1, 4) - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_median_mode() {
        // 7x7 image with one hot pixel at center; half-extent 1 => 3x3 kernel.
        let arr = make_2d_array(7, 7, |x, y| if x == 3 && y == 3 { 1000.0 } else { 10.0 });

        let pixels = vec![BadPixel {
            x: 3,
            y: 3,
            mode: BadPixelMode::Median {
                half_x: 1,
                half_y: 1,
            },
        }];

        let mut proc = BadPixelProcessor::new(pixels);
        let pool = NDArrayPool::new(1_000_000);
        let result = proc.process_array(&arr, &pool);

        let out = &result.output_arrays[0];
        // All 8 neighbors have value 10.0, so median = 10.0
        assert!((get_pixel(out, 3, 3, 7) - 10.0).abs() < 1e-10);
    }

    #[test]
    fn test_median_half_extent_kernel_size() {
        // Regression: Median[mx,my] is a HALF-EXTENT; half_x=3 must sample a
        // 7x7 neighborhood, not 3x3. A 9x9 image with a ring of hot pixels at
        // radius 3 (only reachable by a 7x7 kernel) shifts the median.
        let arr = make_2d_array(9, 9, |x, y| {
            let dx = x as i64 - 4;
            let dy = y as i64 - 4;
            // Hot pixels on the kernel boundary at distance 3.
            if dx.abs() == 3 || dy.abs() == 3 {
                100.0
            } else {
                10.0
            }
        });

        // half extent 3 => 7x7 kernel reaches the radius-3 ring.
        let pixels = vec![BadPixel {
            x: 4,
            y: 4,
            mode: BadPixelMode::Median {
                half_x: 3,
                half_y: 3,
            },
        }];
        let mut proc = BadPixelProcessor::new(pixels);
        let pool = NDArrayPool::new(1_000_000);
        let result = proc.process_array(&arr, &pool);
        let out = &result.output_arrays[0];
        // 7x7 kernel minus center = 48 pixels. The radius-3 ring contributes
        // 24 hot (100.0) pixels and the interior 24 are 10.0; sorted median of
        // 48 values lands at the 10.0/100.0 boundary => (10+100)/2 = 55.0.
        assert!((get_pixel(out, 4, 4, 9) - 55.0).abs() < 1e-10);

        // With a half-extent of 1 (3x3 kernel) the ring is NOT sampled and
        // the median stays at 10.0 — proving the kernel size depends on the
        // half-extent.
        let pixels = vec![BadPixel {
            x: 4,
            y: 4,
            mode: BadPixelMode::Median {
                half_x: 1,
                half_y: 1,
            },
        }];
        let mut proc = BadPixelProcessor::new(pixels);
        let result = proc.process_array(&arr, &pool);
        let out = &result.output_arrays[0];
        assert!((get_pixel(out, 4, 4, 9) - 10.0).abs() < 1e-10);
    }

    #[test]
    fn test_median_skips_bad_neighbors() {
        let arr = make_2d_array(7, 7, |_, _| 10.0);
        // Center and one neighbor are both bad
        let pixels = vec![
            BadPixel {
                x: 3,
                y: 3,
                mode: BadPixelMode::Median {
                    half_x: 1,
                    half_y: 1,
                },
            },
            set(2, 3, 999.0),
        ];

        let mut proc = BadPixelProcessor::new(pixels);
        let pool = NDArrayPool::new(1_000_000);
        let result = proc.process_array(&arr, &pool);

        let out = &result.output_arrays[0];
        // 7 valid neighbors (excluding center and (2,3)), all 10.0
        assert!((get_pixel(out, 3, 3, 7) - 10.0).abs() < 1e-10);
    }

    #[test]
    fn test_boundary_pixel() {
        let arr = make_2d_array(4, 4, |_, _| 20.0);
        let pixels = vec![BadPixel {
            x: 0,
            y: 0,
            mode: BadPixelMode::Median {
                half_x: 1,
                half_y: 1,
            },
        }];

        let mut proc = BadPixelProcessor::new(pixels);
        let pool = NDArrayPool::new(1_000_000);
        let result = proc.process_array(&arr, &pool);

        let out = &result.output_arrays[0];
        // Only 3 valid neighbors: (1,0), (0,1), (1,1)
        assert!((get_pixel(out, 0, 0, 4) - 20.0).abs() < 1e-10);
    }

    #[test]
    fn test_replace_out_of_bounds() {
        let arr = make_2d_array(4, 4, |_, _| 50.0);
        // Replace (0,0) from (-1,0) - out of bounds
        let pixels = vec![BadPixel {
            x: 0,
            y: 0,
            mode: BadPixelMode::Replace { dx: -1, dy: 0 },
        }];

        let mut proc = BadPixelProcessor::new(pixels);
        let pool = NDArrayPool::new(1_000_000);
        let result = proc.process_array(&arr, &pool);

        let out = &result.output_arrays[0];
        assert!((get_pixel(out, 0, 0, 4) - 50.0).abs() < 1e-10);
    }

    #[test]
    fn test_load_from_json_cpp_schema() {
        // C++ AreaDetector bad-pixel file format.
        let json = r#"{"Bad pixels": [
            {"Pixel": [10, 20], "Set": 0},
            {"Pixel": [5, 3], "Replace": [1, 0]},
            {"Pixel": [7, 8], "Median": [3, 3]}
        ]}"#;

        let pixels = BadPixelProcessor::load_from_json(json).unwrap();
        assert_eq!(pixels.len(), 3);
        assert_eq!(pixels[0].x, 10);
        assert_eq!(pixels[0].y, 20);
        assert_eq!(pixels[0].mode, BadPixelMode::Set { value: 0.0 });
        assert_eq!(pixels[1].mode, BadPixelMode::Replace { dx: 1, dy: 0 });
        assert_eq!(
            pixels[2].mode,
            BadPixelMode::Median {
                half_x: 3,
                half_y: 3
            }
        );
    }

    #[test]
    fn test_load_from_json_no_key_defaults_to_set_zero() {
        // An entry with only "Pixel" defaults to Set { value: 0.0 } (C++ leaves
        // the mode at its default badPixelModeSet with setValue 0).
        let json = r#"{"Bad pixels": [{"Pixel": [1, 2]}]}"#;
        let pixels = BadPixelProcessor::load_from_json(json).unwrap();
        assert_eq!(pixels.len(), 1);
        assert_eq!(pixels[0].mode, BadPixelMode::Set { value: 0.0 });
    }

    #[test]
    fn test_no_bad_pixels_passthrough() {
        let arr = make_2d_array(4, 4, |x, y| (x + y * 4) as f64);
        let mut proc = BadPixelProcessor::new(vec![]);
        let pool = NDArrayPool::new(1_000_000);
        let result = proc.process_array(&arr, &pool);

        assert_eq!(result.output_arrays.len(), 1);
        for iy in 0..4 {
            for ix in 0..4 {
                let expected = (ix + iy * 4) as f64;
                let actual = get_pixel(&result.output_arrays[0], ix, iy, 4);
                assert!((actual - expected).abs() < 1e-10);
            }
        }
    }

    #[test]
    fn test_bad_pixel_outside_image() {
        let arr = make_2d_array(4, 4, |_, _| 10.0);
        let pixels = vec![set(100, 100, 999.0)];

        let mut proc = BadPixelProcessor::new(pixels);
        let pool = NDArrayPool::new(1_000_000);
        let result = proc.process_array(&arr, &pool);

        let out = &result.output_arrays[0];
        assert!((get_pixel(out, 0, 0, 4) - 10.0).abs() < 1e-10);
    }

    #[test]
    fn test_u8_data() {
        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for val in v.iter_mut() {
                *val = 100;
            }
        }

        let pixels = vec![set(1, 1, 0.0)];

        let mut proc = BadPixelProcessor::new(pixels);
        let pool = NDArrayPool::new(1_000_000);
        let result = proc.process_array(&arr, &pool);

        let out = &result.output_arrays[0];
        assert!((get_pixel(out, 1, 1, 4) - 0.0).abs() < 1e-10);
        assert!((get_pixel(out, 0, 0, 4) - 100.0).abs() < 1e-10);
    }

    #[test]
    fn test_set_pixels() {
        let mut proc = BadPixelProcessor::new(vec![]);
        assert!(proc.pixels().is_empty());

        proc.set_pixels(vec![set(0, 0, 0.0)]);
        assert_eq!(proc.pixels().len(), 1);
    }
}
