//! NDPluginBadPixel: replaces bad pixels using one of three correction modes.
//!
//! Bad pixel definitions are loaded from JSON. Each bad pixel specifies its (x, y)
//! coordinate and a correction mode:
//! - **Set**: replace with a fixed value.
//! - **Replace**: copy from a neighbor at offset (dx, dy).
//! - **Median**: compute the median of a rectangular kernel around the pixel.

use std::collections::HashSet;
use std::sync::Arc;

use ad_core_rs::ndarray::{NDArray, NDDataBuffer};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, ProcessResult};
use serde::Deserialize;

/// The correction mode for a bad pixel.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "mode")]
pub enum BadPixelMode {
    /// Replace the pixel with a fixed value.
    #[serde(rename = "set")]
    Set { value: f64 },
    /// Replace the pixel by copying from a neighbor at relative offset (dx, dy).
    #[serde(rename = "replace")]
    Replace { dx: i32, dy: i32 },
    /// Replace the pixel with the median of a rectangular kernel.
    #[serde(rename = "median")]
    Median { kernel_x: usize, kernel_y: usize },
}

/// A single bad pixel definition.
#[derive(Debug, Clone, Deserialize)]
pub struct BadPixel {
    pub x: usize,
    pub y: usize,
    #[serde(flatten)]
    pub mode: BadPixelMode,
}

/// Container for deserializing a list of bad pixels from JSON.
#[derive(Debug, Clone, Deserialize)]
pub struct BadPixelList {
    pub bad_pixels: Vec<BadPixel>,
}

/// Processor that corrects bad pixels in incoming arrays.
pub struct BadPixelProcessor {
    pixels: Vec<BadPixel>,
    /// Set of (x, y) for fast bad-pixel lookup.
    bad_set: HashSet<(usize, usize)>,
    /// Cached image width from the last array.
    width: usize,
}

impl BadPixelProcessor {
    /// Create a new processor from a list of bad pixels.
    pub fn new(pixels: Vec<BadPixel>) -> Self {
        let bad_set: HashSet<(usize, usize)> = pixels.iter().map(|p| (p.x, p.y)).collect();
        Self {
            pixels,
            bad_set,
            width: 0,
        }
    }

    /// Parse a bad pixel list from a JSON string.
    pub fn load_from_json(json_str: &str) -> Result<Vec<BadPixel>, serde_json::Error> {
        let list: BadPixelList = serde_json::from_str(json_str)?;
        Ok(list.bad_pixels)
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

    /// Check if a coordinate is a bad pixel.
    fn is_bad(&self, x: usize, y: usize) -> bool {
        self.bad_set.contains(&(x, y))
    }

    /// Apply corrections to a mutable data buffer.
    fn apply_corrections(&self, data: &mut NDDataBuffer, width: usize, height: usize) {
        // We need to read original values for Replace/Median, so take a snapshot first.
        // For Set mode, we could do it in-place, but for consistency we read from the
        // original and write to a separate buffer when needed.

        // Collect corrections to apply
        let mut corrections: Vec<(usize, f64)> = Vec::with_capacity(self.pixels.len());

        for bp in &self.pixels {
            if bp.x >= width || bp.y >= height {
                continue;
            }

            let value = match &bp.mode {
                BadPixelMode::Set { value } => *value,

                BadPixelMode::Replace { dx, dy } => {
                    let nx = bp.x as i64 + *dx as i64;
                    let ny = bp.y as i64 + *dy as i64;

                    if nx < 0 || nx >= width as i64 || ny < 0 || ny >= height as i64 {
                        continue; // replacement out of bounds, skip
                    }

                    let nx = nx as usize;
                    let ny = ny as usize;

                    // Skip if replacement pixel is also bad
                    if self.is_bad(nx, ny) {
                        continue;
                    }

                    let idx = ny * width + nx;
                    match data.get_as_f64(idx) {
                        Some(v) => v,
                        None => continue,
                    }
                }

                BadPixelMode::Median { kernel_x, kernel_y } => {
                    let half_x = (*kernel_x / 2) as i64;
                    let half_y = (*kernel_y / 2) as i64;
                    let cx = bp.x as i64;
                    let cy = bp.y as i64;

                    let mut neighbors = Vec::new();
                    for ky in (cy - half_y)..=(cy + half_y) {
                        for kx in (cx - half_x)..=(cx + half_x) {
                            if kx < 0 || kx >= width as i64 || ky < 0 || ky >= height as i64 {
                                continue;
                            }
                            let kxu = kx as usize;
                            let kyu = ky as usize;
                            // Skip the bad pixel itself and other bad pixels
                            if kxu == bp.x && kyu == bp.y {
                                continue;
                            }
                            if self.is_bad(kxu, kyu) {
                                continue;
                            }
                            let idx = kyu * width + kxu;
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

            let idx = bp.y * width + bp.x;
            corrections.push((idx, value));
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

        let mut out = array.clone();
        self.apply_corrections(&mut out.data, self.width, height);
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
        Ok(())
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

    #[test]
    fn test_set_mode() {
        let arr = make_2d_array(4, 4, |_, _| 100.0);
        let pixels = vec![
            BadPixel {
                x: 1,
                y: 1,
                mode: BadPixelMode::Set { value: 0.0 },
            },
            BadPixel {
                x: 3,
                y: 2,
                mode: BadPixelMode::Set { value: 42.0 },
            },
        ];

        let mut proc = BadPixelProcessor::new(pixels);
        let pool = NDArrayPool::new(1_000_000);
        let result = proc.process_array(&arr, &pool);

        assert_eq!(result.output_arrays.len(), 1);
        let out = &result.output_arrays[0];
        assert!((get_pixel(out, 1, 1, 4) - 0.0).abs() < 1e-10);
        assert!((get_pixel(out, 3, 2, 4) - 42.0).abs() < 1e-10);
        // Unaffected pixels stay at 100
        assert!((get_pixel(out, 0, 0, 4) - 100.0).abs() < 1e-10);
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
        // Both (1,1) and (2,1) are bad. (1,1) tries to replace from (2,1), which is also bad.
        let pixels = vec![
            BadPixel {
                x: 1,
                y: 1,
                mode: BadPixelMode::Replace { dx: 1, dy: 0 },
            },
            BadPixel {
                x: 2,
                y: 1,
                mode: BadPixelMode::Set { value: 0.0 },
            },
        ];

        let mut proc = BadPixelProcessor::new(pixels);
        let pool = NDArrayPool::new(1_000_000);
        let result = proc.process_array(&arr, &pool);

        let out = &result.output_arrays[0];
        // (1,1) should remain unchanged (50.0) since replacement source is bad
        assert!((get_pixel(out, 1, 1, 4) - 50.0).abs() < 1e-10);
        // (2,1) should be set to 0.0
        assert!((get_pixel(out, 2, 1, 4) - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_median_mode() {
        // 5x5 image with one hot pixel at center
        let arr = make_2d_array(5, 5, |x, y| if x == 2 && y == 2 { 1000.0 } else { 10.0 });

        let pixels = vec![BadPixel {
            x: 2,
            y: 2,
            mode: BadPixelMode::Median {
                kernel_x: 3,
                kernel_y: 3,
            },
        }];

        let mut proc = BadPixelProcessor::new(pixels);
        let pool = NDArrayPool::new(1_000_000);
        let result = proc.process_array(&arr, &pool);

        let out = &result.output_arrays[0];
        // All 8 neighbors have value 10.0, so median = 10.0
        assert!((get_pixel(out, 2, 2, 5) - 10.0).abs() < 1e-10);
    }

    #[test]
    fn test_median_skips_bad_neighbors() {
        let arr = make_2d_array(5, 5, |_, _| 10.0);
        // Center and one neighbor are both bad
        let pixels = vec![
            BadPixel {
                x: 2,
                y: 2,
                mode: BadPixelMode::Median {
                    kernel_x: 3,
                    kernel_y: 3,
                },
            },
            BadPixel {
                x: 1,
                y: 2,
                mode: BadPixelMode::Set { value: 999.0 },
            },
        ];

        let mut proc = BadPixelProcessor::new(pixels);
        let pool = NDArrayPool::new(1_000_000);
        let result = proc.process_array(&arr, &pool);

        let out = &result.output_arrays[0];
        // 7 valid neighbors (excluding center and (1,2)), all have value 10.0
        assert!((get_pixel(out, 2, 2, 5) - 10.0).abs() < 1e-10);
    }

    #[test]
    fn test_boundary_pixel() {
        let arr = make_2d_array(4, 4, |_, _| 20.0);
        // Corner pixel with median filter
        let pixels = vec![BadPixel {
            x: 0,
            y: 0,
            mode: BadPixelMode::Median {
                kernel_x: 3,
                kernel_y: 3,
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
        // Try to replace (0,0) from (-1, 0) - out of bounds
        let pixels = vec![BadPixel {
            x: 0,
            y: 0,
            mode: BadPixelMode::Replace { dx: -1, dy: 0 },
        }];

        let mut proc = BadPixelProcessor::new(pixels);
        let pool = NDArrayPool::new(1_000_000);
        let result = proc.process_array(&arr, &pool);

        let out = &result.output_arrays[0];
        // Should be unchanged since replacement is out of bounds
        assert!((get_pixel(out, 0, 0, 4) - 50.0).abs() < 1e-10);
    }

    #[test]
    fn test_load_from_json() {
        let json = r#"{"bad_pixels": [
            {"x": 10, "y": 20, "mode": "set", "value": 0},
            {"x": 5, "y": 3, "mode": "replace", "dx": 1, "dy": 0},
            {"x": 7, "y": 8, "mode": "median", "kernel_x": 3, "kernel_y": 3}
        ]}"#;

        let pixels = BadPixelProcessor::load_from_json(json).unwrap();
        assert_eq!(pixels.len(), 3);
        assert_eq!(pixels[0].x, 10);
        assert_eq!(pixels[0].y, 20);
        match &pixels[0].mode {
            BadPixelMode::Set { value } => assert!((value - 0.0).abs() < 1e-10),
            _ => panic!("expected Set mode"),
        }
        match &pixels[1].mode {
            BadPixelMode::Replace { dx, dy } => {
                assert_eq!(*dx, 1);
                assert_eq!(*dy, 0);
            }
            _ => panic!("expected Replace mode"),
        }
        match &pixels[2].mode {
            BadPixelMode::Median { kernel_x, kernel_y } => {
                assert_eq!(*kernel_x, 3);
                assert_eq!(*kernel_y, 3);
            }
            _ => panic!("expected Median mode"),
        }
    }

    #[test]
    fn test_no_bad_pixels_passthrough() {
        let arr = make_2d_array(4, 4, |x, y| (x + y * 4) as f64);
        let mut proc = BadPixelProcessor::new(vec![]);
        let pool = NDArrayPool::new(1_000_000);
        let result = proc.process_array(&arr, &pool);

        assert_eq!(result.output_arrays.len(), 1);
        // Data should be unchanged
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
        let pixels = vec![BadPixel {
            x: 100,
            y: 100,
            mode: BadPixelMode::Set { value: 999.0 },
        }];

        let mut proc = BadPixelProcessor::new(pixels);
        let pool = NDArrayPool::new(1_000_000);
        let result = proc.process_array(&arr, &pool);

        // Should not crash; all pixels remain at 10.0
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

        let pixels = vec![BadPixel {
            x: 1,
            y: 1,
            mode: BadPixelMode::Set { value: 0.0 },
        }];

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

        let new_pixels = vec![BadPixel {
            x: 0,
            y: 0,
            mode: BadPixelMode::Set { value: 0.0 },
        }];
        proc.set_pixels(new_pixels);
        assert_eq!(proc.pixels().len(), 1);
    }
}
