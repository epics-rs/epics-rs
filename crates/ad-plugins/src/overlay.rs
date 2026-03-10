use std::sync::Arc;

use ad_core::ndarray::{NDArray, NDDataBuffer};
use ad_core::ndarray_pool::NDArrayPool;
use ad_core::plugin::runtime::NDPluginProcess;

/// Shape to draw.
#[derive(Debug, Clone)]
pub enum OverlayShape {
    Cross { center_x: usize, center_y: usize, size: usize },
    Rectangle { x: usize, y: usize, width: usize, height: usize },
    Ellipse { center_x: usize, center_y: usize, rx: usize, ry: usize },
}

/// Draw mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawMode {
    Set,
    XOR,
}

/// A single overlay definition.
#[derive(Debug, Clone)]
pub struct OverlayDef {
    pub shape: OverlayShape,
    pub draw_mode: DrawMode,
    pub value: u8,
}

/// Draw overlays on a 2D mono UInt8 array.
pub fn draw_overlays(src: &NDArray, overlays: &[OverlayDef]) -> NDArray {
    let mut arr = src.clone();
    if arr.dims.len() < 2 {
        return arr;
    }
    let w = arr.dims[0].size;
    let h = arr.dims[1].size;

    let set_pixel = |data: &mut [u8], x: usize, y: usize, mode: DrawMode, val: u8| {
        if x < w && y < h {
            let idx = y * w + x;
            match mode {
                DrawMode::Set => data[idx] = val,
                DrawMode::XOR => data[idx] ^= val,
            }
        }
    };

    if let NDDataBuffer::U8(ref mut data) = arr.data {
        for overlay in overlays {
            match &overlay.shape {
                OverlayShape::Cross { center_x, center_y, size } => {
                    let cx = *center_x;
                    let cy = *center_y;
                    let half = *size / 2;
                    for dx in 0..=half.min(w) {
                        if cx + dx < w { set_pixel(data, cx + dx, cy, overlay.draw_mode, overlay.value); }
                        if dx <= cx { set_pixel(data, cx - dx, cy, overlay.draw_mode, overlay.value); }
                    }
                    for dy in 0..=half.min(h) {
                        if cy + dy < h { set_pixel(data, cx, cy + dy, overlay.draw_mode, overlay.value); }
                        if dy <= cy { set_pixel(data, cx, cy - dy, overlay.draw_mode, overlay.value); }
                    }
                }
                OverlayShape::Rectangle { x, y, width, height } => {
                    for dx in 0..*width {
                        set_pixel(data, x + dx, *y, overlay.draw_mode, overlay.value);
                        if *y + height > 0 {
                            set_pixel(data, x + dx, y + height - 1, overlay.draw_mode, overlay.value);
                        }
                    }
                    for dy in 0..*height {
                        set_pixel(data, *x, y + dy, overlay.draw_mode, overlay.value);
                        if *x + width > 0 {
                            set_pixel(data, x + width - 1, y + dy, overlay.draw_mode, overlay.value);
                        }
                    }
                }
                OverlayShape::Ellipse { center_x, center_y, rx, ry } => {
                    let cx = *center_x as f64;
                    let cy = *center_y as f64;
                    let rxf = *rx as f64;
                    let ryf = *ry as f64;
                    // Simple ellipse: draw outline using angular sampling
                    let steps = ((rxf + ryf) * 4.0) as usize;
                    for i in 0..steps {
                        let angle = 2.0 * std::f64::consts::PI * i as f64 / steps as f64;
                        let px = (cx + rxf * angle.cos()).round() as usize;
                        let py = (cy + ryf * angle.sin()).round() as usize;
                        set_pixel(data, px, py, overlay.draw_mode, overlay.value);
                    }
                }
            }
        }
    }

    arr
}

/// Pure overlay processing logic.
pub struct OverlayProcessor {
    overlays: Vec<OverlayDef>,
}

impl OverlayProcessor {
    pub fn new(overlays: Vec<OverlayDef>) -> Self {
        Self { overlays }
    }
}

impl NDPluginProcess for OverlayProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> Vec<Arc<NDArray>> {
        let out = draw_overlays(array, &self.overlays);
        vec![Arc::new(out)]
    }

    fn plugin_type(&self) -> &str {
        "NDPluginOverlay"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core::ndarray::{NDDataType, NDDimension};

    fn make_8x8() -> NDArray {
        NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::UInt8,
        )
    }

    #[test]
    fn test_rectangle() {
        let arr = make_8x8();
        let overlays = vec![OverlayDef {
            shape: OverlayShape::Rectangle { x: 1, y: 1, width: 4, height: 3 },
            draw_mode: DrawMode::Set,
            value: 255,
        }];

        let out = draw_overlays(&arr, &overlays);
        if let NDDataBuffer::U8(ref v) = out.data {
            // Top edge of rectangle at y=1, x=1..4
            assert_eq!(v[1 * 8 + 1], 255);
            assert_eq!(v[1 * 8 + 2], 255);
            assert_eq!(v[1 * 8 + 3], 255);
            assert_eq!(v[1 * 8 + 4], 255);
            // Inside should still be 0
            assert_eq!(v[2 * 8 + 2], 0);
        }
    }

    #[test]
    fn test_xor_mode() {
        let mut arr = make_8x8();
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            v[0] = 0xFF;
        }

        let overlays = vec![OverlayDef {
            shape: OverlayShape::Cross { center_x: 0, center_y: 0, size: 2 },
            draw_mode: DrawMode::XOR,
            value: 0xFF,
        }];

        let out = draw_overlays(&arr, &overlays);
        if let NDDataBuffer::U8(ref v) = out.data {
            // Center pixel (0,0) is drawn twice (horiz + vert arms):
            // 0xFF ^ 0xFF ^ 0xFF = 0xFF
            assert_eq!(v[0], 0xFF);
            // Neighbor (1,0) drawn once: 0x00 ^ 0xFF = 0xFF
            assert_eq!(v[1], 0xFF);
            // Pixel (0,1) drawn once: 0x00 ^ 0xFF = 0xFF
            assert_eq!(v[1 * 8], 0xFF);
        }
    }

    #[test]
    fn test_cross() {
        let arr = make_8x8();
        let overlays = vec![OverlayDef {
            shape: OverlayShape::Cross { center_x: 4, center_y: 4, size: 4 },
            draw_mode: DrawMode::Set,
            value: 200,
        }];

        let out = draw_overlays(&arr, &overlays);
        if let NDDataBuffer::U8(ref v) = out.data {
            assert_eq!(v[4 * 8 + 4], 200); // center
            assert_eq!(v[4 * 8 + 6], 200); // right arm
            assert_eq!(v[6 * 8 + 4], 200); // bottom arm
        }
    }
}
