use std::sync::Arc;

use ad_core_rs::ndarray::{NDArray, NDDataBuffer};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, ProcessResult};

/// Shape to draw.
#[derive(Debug, Clone)]
pub enum OverlayShape {
    Cross {
        center_x: usize,
        center_y: usize,
        size: usize,
    },
    Rectangle {
        x: usize,
        y: usize,
        width: usize,
        height: usize,
    },
    Ellipse {
        center_x: usize,
        center_y: usize,
        rx: usize,
        ry: usize,
    },
    Text {
        x: usize,
        y: usize,
        text: String,
        font_size: usize,
    },
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
    pub color: [u8; 3], // RGB color; for Mono, only color[0] is used
}

// ---------------------------------------------------------------------------
// Minimal 5x7 bitmap font
// ---------------------------------------------------------------------------

const FONT_WIDTH: usize = 5;
const FONT_HEIGHT: usize = 7;

/// Return a 5x7 bitmap for printable ASCII characters.
/// Each row is encoded as a u8 with the 5 MSBs representing pixels.
fn get_char_bitmap(ch: char) -> [[bool; FONT_WIDTH]; FONT_HEIGHT] {
    let pattern: [u8; 7] = match ch {
        ' ' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        '!' => [0x04, 0x04, 0x04, 0x04, 0x04, 0x00, 0x04],
        '"' => [0x0A, 0x0A, 0x0A, 0x00, 0x00, 0x00, 0x00],
        '#' => [0x0A, 0x0A, 0x1F, 0x0A, 0x1F, 0x0A, 0x0A],
        '$' => [0x04, 0x1E, 0x05, 0x0E, 0x14, 0x0F, 0x04],
        '%' => [0x03, 0x13, 0x08, 0x04, 0x02, 0x19, 0x18],
        '&' => [0x06, 0x09, 0x05, 0x02, 0x15, 0x09, 0x16],
        '\'' => [0x04, 0x04, 0x02, 0x00, 0x00, 0x00, 0x00],
        '(' => [0x08, 0x04, 0x02, 0x02, 0x02, 0x04, 0x08],
        ')' => [0x02, 0x04, 0x08, 0x08, 0x08, 0x04, 0x02],
        '*' => [0x00, 0x04, 0x15, 0x0E, 0x15, 0x04, 0x00],
        '+' => [0x00, 0x04, 0x04, 0x1F, 0x04, 0x04, 0x00],
        ',' => [0x00, 0x00, 0x00, 0x00, 0x04, 0x04, 0x02],
        '-' => [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04],
        '/' => [0x10, 0x08, 0x08, 0x04, 0x02, 0x02, 0x01],
        '0' => [0x0E, 0x11, 0x19, 0x15, 0x13, 0x11, 0x0E],
        '1' => [0x04, 0x06, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x10, 0x08, 0x04, 0x02, 0x1F],
        '3' => [0x0E, 0x11, 0x10, 0x0C, 0x10, 0x11, 0x0E],
        '4' => [0x08, 0x0C, 0x0A, 0x09, 0x1F, 0x08, 0x08],
        '5' => [0x1F, 0x01, 0x0F, 0x10, 0x10, 0x11, 0x0E],
        '6' => [0x0C, 0x02, 0x01, 0x0F, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x10, 0x08, 0x04, 0x02, 0x02, 0x02],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x1E, 0x10, 0x08, 0x06],
        ':' => [0x00, 0x00, 0x04, 0x00, 0x04, 0x00, 0x00],
        ';' => [0x00, 0x00, 0x04, 0x00, 0x04, 0x04, 0x02],
        '<' => [0x08, 0x04, 0x02, 0x01, 0x02, 0x04, 0x08],
        '=' => [0x00, 0x00, 0x1F, 0x00, 0x1F, 0x00, 0x00],
        '>' => [0x02, 0x04, 0x08, 0x10, 0x08, 0x04, 0x02],
        '?' => [0x0E, 0x11, 0x10, 0x08, 0x04, 0x00, 0x04],
        '@' => [0x0E, 0x11, 0x15, 0x1D, 0x05, 0x01, 0x0E],
        'A' | 'a' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'B' | 'b' => [0x0F, 0x11, 0x11, 0x0F, 0x11, 0x11, 0x0F],
        'C' | 'c' => [0x0E, 0x11, 0x01, 0x01, 0x01, 0x11, 0x0E],
        'D' | 'd' => [0x07, 0x09, 0x11, 0x11, 0x11, 0x09, 0x07],
        'E' | 'e' => [0x1F, 0x01, 0x01, 0x0F, 0x01, 0x01, 0x1F],
        'F' | 'f' => [0x1F, 0x01, 0x01, 0x0F, 0x01, 0x01, 0x01],
        'G' | 'g' => [0x0E, 0x11, 0x01, 0x1D, 0x11, 0x11, 0x0E],
        'H' | 'h' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'I' | 'i' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        'J' | 'j' => [0x1C, 0x08, 0x08, 0x08, 0x08, 0x09, 0x06],
        'K' | 'k' => [0x11, 0x09, 0x05, 0x03, 0x05, 0x09, 0x11],
        'L' | 'l' => [0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x1F],
        'M' | 'm' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        'N' | 'n' => [0x11, 0x13, 0x15, 0x15, 0x19, 0x11, 0x11],
        'O' | 'o' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'P' | 'p' => [0x0F, 0x11, 0x11, 0x0F, 0x01, 0x01, 0x01],
        'Q' | 'q' => [0x0E, 0x11, 0x11, 0x11, 0x15, 0x09, 0x16],
        'R' | 'r' => [0x0F, 0x11, 0x11, 0x0F, 0x05, 0x09, 0x11],
        'S' | 's' => [0x0E, 0x11, 0x01, 0x0E, 0x10, 0x11, 0x0E],
        'T' | 't' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' | 'u' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'V' | 'v' => [0x11, 0x11, 0x11, 0x11, 0x0A, 0x0A, 0x04],
        'W' | 'w' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x1B, 0x11],
        'X' | 'x' => [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11],
        'Y' | 'y' => [0x11, 0x11, 0x0A, 0x04, 0x04, 0x04, 0x04],
        'Z' | 'z' => [0x1F, 0x10, 0x08, 0x04, 0x02, 0x01, 0x1F],
        '[' => [0x0E, 0x02, 0x02, 0x02, 0x02, 0x02, 0x0E],
        '\\' => [0x01, 0x02, 0x02, 0x04, 0x08, 0x08, 0x10],
        ']' => [0x0E, 0x08, 0x08, 0x08, 0x08, 0x08, 0x0E],
        '^' => [0x04, 0x0A, 0x11, 0x00, 0x00, 0x00, 0x00],
        '_' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1F],
        '`' => [0x02, 0x04, 0x08, 0x00, 0x00, 0x00, 0x00],
        '{' => [0x08, 0x04, 0x04, 0x02, 0x04, 0x04, 0x08],
        '|' => [0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        '}' => [0x02, 0x04, 0x04, 0x08, 0x04, 0x04, 0x02],
        '~' => [0x00, 0x00, 0x02, 0x15, 0x08, 0x00, 0x00],
        _ => [0x00; 7], // blank for unknown chars
    };

    let mut bitmap = [[false; FONT_WIDTH]; FONT_HEIGHT];
    for row in 0..FONT_HEIGHT {
        let byte = pattern[row];
        for col in 0..FONT_WIDTH {
            bitmap[row][col] = (byte >> col) & 1 != 0;
        }
    }
    bitmap
}

// ---------------------------------------------------------------------------
// Per-type drawing via macro
// ---------------------------------------------------------------------------

macro_rules! draw_on_typed_buffer {
    ($data:expr, $T:ty, $overlays:expr, $w:expr, $h:expr, xor) => {{
        draw_on_typed_buffer!(@inner $data, $T, $overlays, $w, $h, |data: &mut [$T], idx: usize, mode: DrawMode, value: $T| {
            match mode {
                DrawMode::Set => data[idx] = value,
                DrawMode::XOR => data[idx] ^= value,
            }
        });
    }};
    ($data:expr, $T:ty, $overlays:expr, $w:expr, $h:expr, set_only) => {{
        draw_on_typed_buffer!(@inner $data, $T, $overlays, $w, $h, |data: &mut [$T], idx: usize, _mode: DrawMode, value: $T| {
            data[idx] = value;
        });
    }};
    (@inner $data:expr, $T:ty, $overlays:expr, $w:expr, $h:expr, $set_fn:expr) => {{
        let data: &mut [$T] = $data;
        let w: usize = $w;
        let h: usize = $h;
        let set_fn = $set_fn;

        for overlay in $overlays.iter() {
            let value: $T = overlay.color[0] as $T;

            // Closure to set a single pixel
            let mut set_pixel = |x: usize, y: usize| {
                if x < w && y < h {
                    let idx = y * w + x;
                    set_fn(data, idx, overlay.draw_mode, value);
                }
            };

            match &overlay.shape {
                OverlayShape::Cross { center_x, center_y, size } => {
                    let cx = *center_x;
                    let cy = *center_y;
                    let half = *size / 2;
                    for dx in 0..=half.min(w) {
                        if cx + dx < w { set_pixel(cx + dx, cy); }
                        if dx <= cx { set_pixel(cx - dx, cy); }
                    }
                    for dy in 0..=half.min(h) {
                        if cy + dy < h { set_pixel(cx, cy + dy); }
                        if dy <= cy { set_pixel(cx, cy - dy); }
                    }
                }
                OverlayShape::Rectangle { x, y, width, height } => {
                    for dx in 0..*width {
                        set_pixel(x + dx, *y);
                        if *y + height > 0 {
                            set_pixel(x + dx, y + height - 1);
                        }
                    }
                    for dy in 0..*height {
                        set_pixel(*x, y + dy);
                        if *x + width > 0 {
                            set_pixel(x + width - 1, y + dy);
                        }
                    }
                }
                OverlayShape::Ellipse { center_x, center_y, rx, ry } => {
                    let cx = *center_x as f64;
                    let cy = *center_y as f64;
                    let rxf = *rx as f64;
                    let ryf = *ry as f64;
                    let steps = ((rxf + ryf) * 4.0) as usize;
                    for i in 0..steps {
                        let angle = 2.0 * std::f64::consts::PI * i as f64 / steps as f64;
                        let px = (cx + rxf * angle.cos()).round() as usize;
                        let py = (cy + ryf * angle.sin()).round() as usize;
                        set_pixel(px, py);
                    }
                }
                OverlayShape::Text { x, y, text, font_size } => {
                    let scale = (*font_size).max(1) / FONT_HEIGHT.max(1);
                    let scale = scale.max(1);
                    let mut cursor_x = *x;
                    for ch in text.chars() {
                        let bitmap = get_char_bitmap(ch);
                        for row in 0..FONT_HEIGHT {
                            for col in 0..FONT_WIDTH {
                                if bitmap[row][col] {
                                    for sy in 0..scale {
                                        for sx in 0..scale {
                                            set_pixel(
                                                cursor_x + col * scale + sx,
                                                *y + row * scale + sy,
                                            );
                                        }
                                    }
                                }
                            }
                        }
                        cursor_x += (FONT_WIDTH + 1) * scale;
                    }
                }
            }
        }
    }};
}

/// Draw overlays on a 2D array. Supports U8, U16, I16, I32, U32, F32, F64.
pub fn draw_overlays(src: &NDArray, overlays: &[OverlayDef]) -> NDArray {
    let mut arr = src.clone();
    if arr.dims.len() < 2 {
        return arr;
    }
    let w = arr.dims[0].size;
    let h = arr.dims[1].size;

    match &mut arr.data {
        NDDataBuffer::U8(data) => {
            draw_on_typed_buffer!(data.as_mut_slice(), u8, overlays, w, h, xor);
        }
        NDDataBuffer::U16(data) => {
            draw_on_typed_buffer!(data.as_mut_slice(), u16, overlays, w, h, xor);
        }
        NDDataBuffer::I16(data) => {
            draw_on_typed_buffer!(data.as_mut_slice(), i16, overlays, w, h, xor);
        }
        NDDataBuffer::I32(data) => {
            draw_on_typed_buffer!(data.as_mut_slice(), i32, overlays, w, h, xor);
        }
        NDDataBuffer::U32(data) => {
            draw_on_typed_buffer!(data.as_mut_slice(), u32, overlays, w, h, xor);
        }
        NDDataBuffer::F32(data) => {
            draw_on_typed_buffer!(data.as_mut_slice(), f32, overlays, w, h, set_only);
        }
        NDDataBuffer::F64(data) => {
            draw_on_typed_buffer!(data.as_mut_slice(), f64, overlays, w, h, set_only);
        }
        _ => {} // I8, I64, U64 - less common, skip
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
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        let out = draw_overlays(array, &self.overlays);
        ProcessResult::arrays(vec![Arc::new(out)])
    }

    fn plugin_type(&self) -> &str {
        "NDPluginOverlay"
    }

    fn register_params(
        &mut self,
        base: &mut asyn_rs::port::PortDriverBase,
    ) -> asyn_rs::error::AsynResult<()> {
        use asyn_rs::param::ParamType;
        // NDOverlay.template params
        base.create_param("MAX_SIZE_X", ParamType::Int32)?;
        base.create_param("MAX_SIZE_Y", ParamType::Int32)?;
        // NDOverlayN.template params
        base.create_param("NAME", ParamType::Octet)?;
        base.create_param("USE", ParamType::Int32)?;
        base.create_param("OVERLAY_POSITION_X", ParamType::Int32)?;
        base.create_param("OVERLAY_POSITION_Y", ParamType::Int32)?;
        base.create_param("OVERLAY_CENTER_X", ParamType::Int32)?;
        base.create_param("OVERLAY_CENTER_Y", ParamType::Int32)?;
        base.create_param("OVERLAY_SIZE_X", ParamType::Int32)?;
        base.create_param("OVERLAY_SIZE_Y", ParamType::Int32)?;
        base.create_param("OVERLAY_WIDTH_X", ParamType::Int32)?;
        base.create_param("OVERLAY_WIDTH_Y", ParamType::Int32)?;
        base.create_param("OVERLAY_SHAPE", ParamType::Int32)?;
        base.create_param("OVERLAY_DRAW_MODE", ParamType::Int32)?;
        base.create_param("OVERLAY_RED", ParamType::Int32)?;
        base.create_param("OVERLAY_GREEN", ParamType::Int32)?;
        base.create_param("OVERLAY_BLUE", ParamType::Int32)?;
        base.create_param("OVERLAY_DISPLAY_TEXT", ParamType::Octet)?;
        base.create_param("OVERLAY_TIMESTAMP_FORMAT", ParamType::Octet)?;
        base.create_param("OVERLAY_FONT", ParamType::Int32)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core_rs::ndarray::{NDDataType, NDDimension};

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
            shape: OverlayShape::Rectangle {
                x: 1,
                y: 1,
                width: 4,
                height: 3,
            },
            draw_mode: DrawMode::Set,
            color: [255, 0, 0],
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
            shape: OverlayShape::Cross {
                center_x: 0,
                center_y: 0,
                size: 2,
            },
            draw_mode: DrawMode::XOR,
            color: [0xFF, 0, 0],
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
            shape: OverlayShape::Cross {
                center_x: 4,
                center_y: 4,
                size: 4,
            },
            draw_mode: DrawMode::Set,
            color: [200, 0, 0],
        }];

        let out = draw_overlays(&arr, &overlays);
        if let NDDataBuffer::U8(ref v) = out.data {
            assert_eq!(v[4 * 8 + 4], 200); // center
            assert_eq!(v[4 * 8 + 6], 200); // right arm
            assert_eq!(v[6 * 8 + 4], 200); // bottom arm
        }
    }

    #[test]
    fn test_text_rendering() {
        // Render "Hi" at position (0,0), font_size=7 (1x scale)
        let arr = NDArray::new(
            vec![NDDimension::new(20), NDDimension::new(10)],
            NDDataType::UInt8,
        );
        let overlays = vec![OverlayDef {
            shape: OverlayShape::Text {
                x: 0,
                y: 0,
                text: "Hi".to_string(),
                font_size: 7,
            },
            draw_mode: DrawMode::Set,
            color: [255, 0, 0],
        }];

        let out = draw_overlays(&arr, &overlays);
        if let NDDataBuffer::U8(ref v) = out.data {
            let w = 20;
            // 'H' bitmap first row is 0x11 = bits 0 and 4 set
            // pixel (0,0) should be set (bit 0)
            assert_eq!(v[0 * w + 0], 255);
            // pixel (4,0) should be set (bit 4)
            assert_eq!(v[0 * w + 4], 255);
            // pixel (2,0) should NOT be set for 'H' row 0
            assert_eq!(v[0 * w + 2], 0);

            // 'I' starts at cursor_x = 6 (FONT_WIDTH=5 + 1 gap)
            // 'I' first row is 0x0E = bits 1,2,3 set
            assert_eq!(v[0 * w + 6 + 1], 255);
            assert_eq!(v[0 * w + 6 + 2], 255);
            assert_eq!(v[0 * w + 6 + 3], 255);
        }
    }

    #[test]
    fn test_u16_overlay() {
        let arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::UInt16,
        );
        // Fill with zeros (already done by NDArray::new)
        let overlays = vec![OverlayDef {
            shape: OverlayShape::Rectangle {
                x: 1,
                y: 1,
                width: 4,
                height: 3,
            },
            draw_mode: DrawMode::Set,
            color: [200, 0, 0],
        }];

        let out = draw_overlays(&arr, &overlays);
        if let NDDataBuffer::U16(ref v) = out.data {
            // Top edge at y=1, x=1
            assert_eq!(v[1 * 8 + 1], 200);
            assert_eq!(v[1 * 8 + 4], 200);
            // Inside should still be 0
            assert_eq!(v[2 * 8 + 2], 0);
        }
    }

    #[test]
    fn test_f32_overlay_ignores_xor() {
        let arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::Float32,
        );
        let overlays = vec![OverlayDef {
            shape: OverlayShape::Cross {
                center_x: 4,
                center_y: 4,
                size: 2,
            },
            draw_mode: DrawMode::XOR, // should be treated as Set for floats
            color: [100, 0, 0],
        }];

        let out = draw_overlays(&arr, &overlays);
        if let NDDataBuffer::F32(ref v) = out.data {
            // Center pixel should be set (XOR falls back to Set for floats)
            assert_eq!(v[4 * 8 + 4], 100.0);
        }
    }
}
