use std::sync::Arc;

use ad_core_rs::ndarray::{NDArray, NDDataBuffer};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, ProcessResult};
use parking_lot::Mutex;

/// Shape to draw.
#[derive(Debug, Clone)]
pub enum OverlayShape {
    Cross {
        center_x: usize,
        center_y: usize,
        /// X arm half-extent source — C draws the horizontal arm over
        /// `SizeX/2` each side of the center, independent of `SizeY`.
        size_x: usize,
        /// Y arm half-extent source — C draws the vertical arm over `SizeY/2`.
        size_y: usize,
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
        /// X extent (SizeX) — characters past `x + size_x` are not drawn,
        /// matching C++ `xmax = PositionX + SizeX`.
        size_x: usize,
        /// Y extent (SizeY) — drawing stops at `min(y + size_y, y + font.height)`.
        size_y: usize,
        text: String,
        /// Bitmap font index (0..=3): C++ `NDPluginOverlayTextFontBitmaps`.
        font: usize,
        /// Optional strftime format. When non-empty, the formatted NDArray
        /// timestamp is appended to `text` (C++ `TimeStampFormat`).
        timestamp_format: String,
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
    /// RGB color; for Mono, `color[1]` (green) is used.
    ///
    /// `i32`, not `u8`: C's `NDOverlay_t` holds `int red/green/blue`
    /// (NDPluginOverlay.h:38-40) fed from unclamped `asynParamInt32` params
    /// (`getIntegerParam`, NDPluginOverlay.cpp:339-341), and `setPixel`
    /// (:41-52) casts that `int` straight to the pixel type. A u8 channel
    /// cannot express the overlay values 16- and 32-bit images need — a
    /// full-scale marker on NDUInt16 is 65535.
    pub color: [i32; 3],
    pub width_x: usize, // line thickness in X direction (0 or 1 = 1px)
    pub width_y: usize, // line thickness in Y direction (0 or 1 = 1px)
}

// ---------------------------------------------------------------------------
// Bitmap fonts — ported from ADCore NDPluginOverlayTextFont.cpp
// ---------------------------------------------------------------------------

use crate::overlay_font::{BitmapFont, FONTS};

/// Number of selectable bitmap fonts (C++ `NDPluginOverlayTextFontBitmapTypeN`).
pub const NUM_FONTS: usize = 4;

/// Resolve a font index to its bitmap descriptor, clamping out-of-range
/// indices to font 0 (C++ guards `Font >= 0 && Font < N`).
fn font_for(index: usize) -> &'static BitmapFont {
    &FONTS[index.min(NUM_FONTS - 1)]
}

/// Test whether bit `col` of character `ch` row `row` is set in `font`.
///
/// Mirrors C++ `NDPluginOverlay.cpp` text rendering: each character occupies
/// `height` rows of `bytes_per_char` bytes; bit order is MSB-first within
/// each byte (`mask = 0x80`). Characters below `first_char` or above the
/// font range render blank.
fn font_pixel(font: &BitmapFont, ch: char, row: usize, col: usize) -> bool {
    let code = ch as u32;
    if code < font.first_char as u32 {
        return false;
    }
    let ci = (code - font.first_char as u32) as usize;
    if ci >= font.num_chars || row >= font.height || col >= font.width {
        return false;
    }
    let byte_in_row = col / 8;
    let bit = 7 - (col % 8);
    let offset = (font.height * ci + row) * font.bytes_per_char + byte_in_row;
    (font.bitmap[offset] >> bit) & 1 != 0
}

/// Format an EPICS timestamp with a strftime-style format string.
///
/// Mirrors C++ `epicsTimeToStrftime` for the conversion specifiers commonly
/// used in AreaDetector overlay configs: `%Y %m %d %H %M %S %f %%`. `%f` is
/// the fractional seconds in microseconds (6 digits). Unknown specifiers are
/// passed through verbatim.
fn format_epics_time(ts: ad_core_rs::timestamp::EpicsTimestamp, fmt: &str) -> String {
    // Decompose the UTC time-of-day from the EPICS timestamp.
    let secs = ts.sec as u64 + 631_152_000; // EPICS epoch -> Unix epoch
    let days = secs / 86_400;
    let tod = secs % 86_400;
    let (hour, minute, second) = (tod / 3600, (tod % 3600) / 60, tod % 60);

    // Civil date from days since Unix epoch (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };

    let mut out = String::with_capacity(fmt.len() + 16);
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('Y') => out.push_str(&format!("{year:04}")),
            Some('m') => out.push_str(&format!("{month:02}")),
            Some('d') => out.push_str(&format!("{day:02}")),
            Some('H') => out.push_str(&format!("{hour:02}")),
            Some('M') => out.push_str(&format!("{minute:02}")),
            Some('S') => out.push_str(&format!("{second:02}")),
            Some('f') => out.push_str(&format!("{:06}", ts.nsec / 1000)),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Per-type drawing via macro
// ---------------------------------------------------------------------------

macro_rules! draw_on_typed_buffer {
    ($data:expr, $T:ty, $overlays:expr, $info:expr, $ts:expr) => {{
        let data: &mut [$T] = $data;
        let info: &ad_core_rs::ndarray::NDArrayInfo = $info;
        let array_ts: ad_core_rs::timestamp::EpicsTimestamp = $ts;

        // NDPluginOverlay.cpp:29-33 `addPixel` bounds-checks against
        // pArrayInfo->xSize/ySize and addresses the sample as
        // `iy*yStride + ix*xStride`, and setPixel (:38-53) walks the three
        // color planes by `colorStride` when the array is RGB1/RGB2/RGB3.
        // Both the geometry and the written value are color-mode aware, so
        // everything below goes through getInfo() rather than assuming a
        // packed mono `y*width + x` layout.
        let is_rgb = matches!(
            info.color_mode,
            ad_core_rs::color::NDColorMode::RGB1
                | ad_core_rs::color::NDColorMode::RGB2
                | ad_core_rs::color::NDColorMode::RGB3
        );

        for overlay in $overlays.iter() {
            // Mono uses pOverlay->green; RGB writes red/green/blue in turn.
            let rgb: [i32; 3] = overlay.color;
            let wx = overlay.width_x.max(1);
            let wy = overlay.width_y.max(1);

            // Write one sample.
            //
            // NDPluginOverlay.cpp:41-52 `setPixel`, templated over every
            // `epicsType` including epicsFloat32/epicsFloat64:
            //   Set: *pValue = (epicsType)color
            //   XOR: *pValue = (epicsType)((int)*pValue ^ (int)color)
            // The XOR therefore narrows through a 32-bit `int` for *all* widths
            // — float pixels are truncated to int, xor'd, and cast back; 64-bit
            // pixels lose their high word. Rust's `as` casts reproduce both.
            // (C's float->int conversion is UB when out of range; `as i32`
            // saturates instead of trapping.)
            let put_sample = |data: &mut [$T], idx: usize, color: i32| {
                // C indexes the raw buffer unchecked; keep the write in bounds
                // when a ColorMode attribute disagrees with the real dims.
                if let Some(slot) = data.get_mut(idx) {
                    *slot = match overlay.draw_mode {
                        DrawMode::Set => color as $T,
                        DrawMode::XOR => ((*slot as i32) ^ color) as $T,
                    };
                }
            };

            let mut set_pixel = |x: usize, y: usize| {
                if x < info.x_size && y < info.y_size {
                    let idx = y * info.y_stride + x * info.x_stride;
                    if is_rgb {
                        for (plane, color) in rgb.iter().enumerate() {
                            put_sample(data, idx + plane * info.color_stride, *color);
                        }
                    } else {
                        put_sample(data, idx, rgb[1]);
                    }
                }
            };

            match &overlay.shape {
                OverlayShape::Cross {
                    center_x,
                    center_y,
                    size_x,
                    size_y,
                } => {
                    // C++ doOverlayT Cross (NDPluginOverlay.cpp:94-117): the
                    // horizontal arm spans SizeX/2 each side of the center, the
                    // vertical arm SizeY/2 — independent extents. xwide/ywide
                    // are WidthX/2, WidthY/2 (half-thicknesses). Rows inside the
                    // band [ycent-ywide, ycent+ywide] draw the full horizontal
                    // arm; other rows draw only the vertical strip
                    // [xcent-xwide, xcent+xwide]. Each pixel is visited exactly
                    // once, so the center is not double-XOR'd.
                    let cx = *center_x as i64;
                    let cy = *center_y as i64;
                    let half_x = (*size_x / 2) as i64;
                    let half_y = (*size_y / 2) as i64;
                    let xwide = (wx / 2) as i64;
                    let ywide = (wy / 2) as i64;
                    let mut put = |x: i64, y: i64| {
                        if x >= 0 && y >= 0 {
                            set_pixel(x as usize, y as usize);
                        }
                    };
                    for iy in (cy - half_y)..=(cy + half_y) {
                        if iy >= cy - ywide && iy <= cy + ywide {
                            // Inside the horizontal band: full horizontal arm.
                            for ix in (cx - half_x)..=(cx + half_x) {
                                put(ix, iy);
                            }
                        } else {
                            // Outside the band: vertical strip only.
                            for ix in (cx - xwide)..=(cx + xwide) {
                                put(ix, iy);
                            }
                        }
                    }
                }
                OverlayShape::Rectangle {
                    x,
                    y,
                    width,
                    height,
                } => {
                    // C doOverlayT Rectangle (NDPluginOverlay.cpp:119-145):
                    // xmax = PositionX + SizeX is INCLUSIVE, so the rectangle is
                    // SizeX+1 px wide / SizeY+1 tall. Border thickness grows
                    // inward; xwide/ywide = MIN(Width, Size-1) (raw width, signed
                    // so a zero size yields an empty border as in C).
                    let xmin = *x as i64;
                    let xmax = (*x + *width) as i64;
                    let ymin = *y as i64;
                    let ymax = (*y + *height) as i64;
                    let xwide = (overlay.width_x as i64).min(*width as i64 - 1);
                    let ywide = (overlay.width_y as i64).min(*height as i64 - 1);
                    let mut put = |x: i64, y: i64| {
                        if x >= 0 && y >= 0 {
                            set_pixel(x as usize, y as usize);
                        }
                    };
                    for iy in ymin..=ymax {
                        if iy < ymin + ywide || iy > ymax - ywide {
                            // Top/bottom border rows: full horizontal span.
                            for ix in xmin..=xmax {
                                put(ix, iy);
                            }
                        } else {
                            // Interior rows: left and right vertical borders.
                            for ix in xmin..(xmin + xwide) {
                                put(ix, iy);
                            }
                            for ix in (xmax - xwide + 1)..=xmax {
                                put(ix, iy);
                            }
                        }
                    }
                }
                OverlayShape::Ellipse {
                    center_x,
                    center_y,
                    rx,
                    ry,
                } => {
                    // C++ doOverlayT Ellipse: parametric over the first
                    // quadrant, mirrored to the other three; for each of
                    // `xwide` thickness layers shrink the radii by jj. C++
                    // sorts+uniques the resulting pixel list before drawing
                    // "or the XOR draw mode won't work because the pixel will
                    // be set and then unset". We dedup pixels here for the
                    // same reason.
                    let cx = *center_x as i64;
                    let cy = *center_y as i64;
                    let xsize = *rx as i64;
                    let ysize = *ry as i64;
                    // C++: xwide = MIN(WidthX, SizeX-1); SizeX = 2*rx.
                    let xwide = (wx as i64).min((2 * xsize - 1).max(0));
                    let n_steps = (2 * (xsize + ysize)).max(1);
                    let theta_step = std::f64::consts::FRAC_PI_2 / n_steps as f64;
                    let mut pixels: Vec<(i64, i64)> = Vec::new();
                    for ii in 0..=n_steps {
                        let theta = ii as f64 * theta_step;
                        for jj in 0..xwide.max(1) {
                            let ix = (((xsize - jj) as f64) * theta.cos() + 0.5) as i64;
                            let iy = (((ysize - jj) as f64) * theta.sin() + 0.5) as i64;
                            pixels.push((cx + ix, cy + iy));
                            pixels.push((cx + ix, cy - iy));
                            pixels.push((cx - ix, cy + iy));
                            pixels.push((cx - ix, cy - iy));
                        }
                    }
                    // Remove duplicates so XOR mode does not self-cancel.
                    pixels.sort_unstable();
                    pixels.dedup();
                    for (px, py) in pixels {
                        if px >= 0 && py >= 0 {
                            set_pixel(px as usize, py as usize);
                        }
                    }
                }
                OverlayShape::Text {
                    x,
                    y,
                    size_x,
                    size_y,
                    text,
                    font,
                    timestamp_format,
                } => {
                    // C++ NDPluginOverlay.cpp text path: a fixed-cell bitmap
                    // font (no scaling); characters advance by the full font
                    // width; xmax = PositionX + SizeX clips trailing chars;
                    // ymax = min(PositionY + SizeY, PositionY + font.height).
                    let bmp = font_for(*font);
                    // Append the formatted timestamp when a format is set
                    // (C++ epicsTimeToStrftime + DisplayText concatenation).
                    let rendered = if timestamp_format.is_empty() {
                        text.clone()
                    } else {
                        format!("{}{}", text, format_epics_time(array_ts, timestamp_format))
                    };
                    let xmin = *x;
                    let xmax = x.saturating_add(*size_x);
                    let ymax = y.saturating_add(*size_y).min(y.saturating_add(bmp.height));
                    for (ci, ch) in rendered.chars().enumerate() {
                        // C tests `if (cp[ii] < 32) continue;` on a signed
                        // `char`, so bytes >= 128 are negative and skipped along
                        // with control codes: only printable ASCII 32..=127 is
                        // drawn. The cell still advances (C `continue`, ci++).
                        let code = ch as u32;
                        if !(32..128).contains(&code) {
                            continue;
                        }
                        let char_x0 = xmin + ci * bmp.width;
                        if char_x0 >= xmax {
                            break; // none of this character fits
                        }
                        for row in 0..bmp.height {
                            let iy = *y + row;
                            if iy >= ymax {
                                break;
                            }
                            for col in 0..bmp.width {
                                let ix = char_x0 + col;
                                if ix >= xmax {
                                    break;
                                }
                                if font_pixel(bmp, ch, row, col) {
                                    set_pixel(ix, iy);
                                }
                            }
                        }
                    }
                }
            }
        }
    }};
}

/// Draw overlays on an array. Supports I8, U8, I16, U16, I32, U32, I64, U64, F32, F64.
///
/// The pixel geometry comes from [`NDArray::info`] (C++ `NDArray::getInfo`), so
/// RGB1/RGB2/RGB3 arrays are addressed by their real x/y/color strides and each
/// overlay paints red, green and blue into the three color planes — exactly what
/// `NDPluginOverlay::addPixel`/`setPixel` do. Arrays with fewer than two usable
/// dimensions get `y_size == 0` from `info()` and are left untouched, as in C.
pub fn draw_overlays(src: &NDArray, overlays: &[OverlayDef]) -> NDArray {
    let mut arr = src.clone();
    let info = arr.info();
    let ts = arr.timestamp;

    match &mut arr.data {
        NDDataBuffer::U8(data) => {
            draw_on_typed_buffer!(data.as_mut_slice(), u8, overlays, &info, ts);
        }
        NDDataBuffer::U16(data) => {
            draw_on_typed_buffer!(data.as_mut_slice(), u16, overlays, &info, ts);
        }
        NDDataBuffer::I16(data) => {
            draw_on_typed_buffer!(data.as_mut_slice(), i16, overlays, &info, ts);
        }
        NDDataBuffer::I32(data) => {
            draw_on_typed_buffer!(data.as_mut_slice(), i32, overlays, &info, ts);
        }
        NDDataBuffer::U32(data) => {
            draw_on_typed_buffer!(data.as_mut_slice(), u32, overlays, &info, ts);
        }
        NDDataBuffer::F32(data) => {
            draw_on_typed_buffer!(data.as_mut_slice(), f32, overlays, &info, ts);
        }
        NDDataBuffer::F64(data) => {
            draw_on_typed_buffer!(data.as_mut_slice(), f64, overlays, &info, ts);
        }
        NDDataBuffer::I8(data) => {
            draw_on_typed_buffer!(data.as_mut_slice(), i8, overlays, &info, ts);
        }
        NDDataBuffer::I64(data) => {
            draw_on_typed_buffer!(data.as_mut_slice(), i64, overlays, &info, ts);
        }
        NDDataBuffer::U64(data) => {
            draw_on_typed_buffer!(data.as_mut_slice(), u64, overlays, &info, ts);
        }
    }

    arr
}

/// Maximum number of overlays.
const MAX_OVERLAYS: usize = 8;

/// Runtime overlay state — one per addr (0..7).
#[derive(Debug, Clone)]
struct OverlaySlot {
    use_overlay: bool,
    shape: i32,     // C NDOverlayShape_t: 0=Cross, 1=Rectangle, 2=Text, 3=Ellipse
    draw_mode: i32, // 0=Set, 1=XOR
    position_x: usize,
    position_y: usize,
    // Stored CenterX/CenterY (signed, like the C++ param) so a SizeX change
    // with freeze OFF can recover the frozen center exactly.
    center_x: i32,
    center_y: i32,
    size_x: usize,
    size_y: usize,
    width_x: usize,
    width_y: usize,
    // C `NDOverlay_t` (NDPluginOverlay.h:38-40): `int red/green/blue`, read
    // from asynInt32 params with no range clamp.
    red: i32,
    green: i32,
    blue: i32,
    display_text: String,
    timestamp_format: String,
    font: usize,
    /// C++ `freezePositionX`: true once PositionX was written more recently
    /// than CenterX. A SizeX change then keeps PositionX fixed (moving the
    /// center); false keeps CenterX fixed (moving the position).
    freeze_position_x: bool,
    freeze_position_y: bool,
}

impl Default for OverlaySlot {
    fn default() -> Self {
        Self {
            use_overlay: false,
            shape: 1, // Rectangle
            draw_mode: 0,
            position_x: 0,
            position_y: 0,
            center_x: 0,
            center_y: 0,
            size_x: 0,
            size_y: 0,
            width_x: 1,
            width_y: 1,
            red: 255,
            green: 0,
            blue: 0,
            display_text: String::new(),
            timestamp_format: String::new(),
            font: 0,
            freeze_position_x: true,
            freeze_position_y: true,
        }
    }
}

impl OverlaySlot {
    fn to_overlay_def(&self) -> Option<OverlayDef> {
        if !self.use_overlay {
            return None;
        }
        let draw_mode = if self.draw_mode == 1 {
            DrawMode::XOR
        } else {
            DrawMode::Set
        };
        let color = [self.red, self.green, self.blue];
        let shape = match self.shape {
            0 => OverlayShape::Cross {
                center_x: self.position_x + self.size_x / 2,
                center_y: self.position_y + self.size_y / 2,
                size_x: self.size_x,
                size_y: self.size_y,
            },
            1 => OverlayShape::Rectangle {
                x: self.position_x,
                y: self.position_y,
                width: self.size_x,
                height: self.size_y,
            },
            // C `NDOverlayShape_t` enum (NDPluginOverlay.h:8-13):
            // Cross=0, Rectangle=1, Text=2, Ellipse=3.
            2 => OverlayShape::Text {
                x: self.position_x,
                y: self.position_y,
                size_x: self.size_x,
                size_y: self.size_y,
                text: self.display_text.clone(),
                font: self.font,
                timestamp_format: self.timestamp_format.clone(),
            },
            3 => OverlayShape::Ellipse {
                center_x: self.position_x + self.size_x / 2,
                center_y: self.position_y + self.size_y / 2,
                rx: self.size_x / 2,
                ry: self.size_y / 2,
            },
            _ => OverlayShape::Rectangle {
                x: self.position_x,
                y: self.position_y,
                width: self.size_x,
                height: self.size_y,
            },
        };
        Some(OverlayDef {
            shape,
            draw_mode,
            color,
            width_x: self.width_x,
            width_y: self.width_y,
        })
    }
}

/// Param indices for per-overlay params.
#[derive(Default)]
struct OverlayParamIndices {
    use_overlay: Option<usize>,
    position_x: Option<usize>,
    position_y: Option<usize>,
    center_x: Option<usize>,
    center_y: Option<usize>,
    size_x: Option<usize>,
    size_y: Option<usize>,
    width_x: Option<usize>,
    width_y: Option<usize>,
    shape: Option<usize>,
    draw_mode: Option<usize>,
    red: Option<usize>,
    green: Option<usize>,
    blue: Option<usize>,
    display_text: Option<usize>,
    timestamp_format: Option<usize>,
    font: Option<usize>,
}

/// Pure overlay processing logic with runtime-configurable overlays.
pub struct OverlayProcessor {
    /// The overlay slots, rewritten by an addressed param write while the
    /// frame path may be drawing from them.
    slots: Mutex<[OverlaySlot; MAX_OVERLAYS]>,
    params: OverlayParamIndices,
}

impl OverlayProcessor {
    pub fn new(overlays: Vec<OverlayDef>) -> Self {
        let mut slots: [OverlaySlot; MAX_OVERLAYS] = Default::default();
        for (i, o) in overlays.into_iter().enumerate().take(MAX_OVERLAYS) {
            let slot = &mut slots[i];
            slot.use_overlay = true;
            slot.draw_mode = if o.draw_mode == DrawMode::XOR { 1 } else { 0 };
            slot.red = o.color[0];
            slot.green = o.color[1];
            slot.blue = o.color[2];
            slot.width_x = o.width_x;
            slot.width_y = o.width_y;
            match o.shape {
                OverlayShape::Cross {
                    center_x,
                    center_y,
                    size_x,
                    size_y,
                } => {
                    slot.shape = 0;
                    slot.position_x = center_x.saturating_sub(size_x / 2);
                    slot.position_y = center_y.saturating_sub(size_y / 2);
                    slot.size_x = size_x;
                    slot.size_y = size_y;
                }
                OverlayShape::Rectangle {
                    x,
                    y,
                    width,
                    height,
                } => {
                    slot.shape = 1;
                    slot.position_x = x;
                    slot.position_y = y;
                    slot.size_x = width;
                    slot.size_y = height;
                }
                OverlayShape::Ellipse {
                    center_x,
                    center_y,
                    rx,
                    ry,
                } => {
                    slot.shape = 3;
                    slot.position_x = center_x.saturating_sub(rx);
                    slot.position_y = center_y.saturating_sub(ry);
                    slot.size_x = rx * 2;
                    slot.size_y = ry * 2;
                }
                OverlayShape::Text {
                    x,
                    y,
                    size_x,
                    size_y,
                    text,
                    font,
                    timestamp_format,
                } => {
                    slot.shape = 2;
                    slot.position_x = x;
                    slot.position_y = y;
                    slot.size_x = size_x;
                    slot.size_y = size_y;
                    slot.display_text = text;
                    slot.timestamp_format = timestamp_format;
                    slot.font = font.min(NUM_FONTS - 1);
                }
            }
        }
        Self {
            slots: Mutex::new(slots),
            params: OverlayParamIndices::default(),
        }
    }

    fn build_active_overlays(&self) -> Vec<OverlayDef> {
        self.slots
            .lock()
            .iter()
            .filter_map(|s| s.to_overlay_def())
            .collect()
    }
}

impl NDPluginProcess for OverlayProcessor {
    fn process_array(&self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        let active = self.build_active_overlays();
        let out = draw_overlays(array, &active);
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
        base.create_param("MAX_SIZE_X", ParamType::Int32)?;
        base.create_param("MAX_SIZE_Y", ParamType::Int32)?;
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

        self.params.use_overlay = base.find_param("USE");
        self.params.position_x = base.find_param("OVERLAY_POSITION_X");
        self.params.position_y = base.find_param("OVERLAY_POSITION_Y");
        self.params.center_x = base.find_param("OVERLAY_CENTER_X");
        self.params.center_y = base.find_param("OVERLAY_CENTER_Y");
        self.params.size_x = base.find_param("OVERLAY_SIZE_X");
        self.params.size_y = base.find_param("OVERLAY_SIZE_Y");
        self.params.width_x = base.find_param("OVERLAY_WIDTH_X");
        self.params.width_y = base.find_param("OVERLAY_WIDTH_Y");
        self.params.shape = base.find_param("OVERLAY_SHAPE");
        self.params.draw_mode = base.find_param("OVERLAY_DRAW_MODE");
        self.params.red = base.find_param("OVERLAY_RED");
        self.params.green = base.find_param("OVERLAY_GREEN");
        self.params.blue = base.find_param("OVERLAY_BLUE");
        self.params.display_text = base.find_param("OVERLAY_DISPLAY_TEXT");
        self.params.timestamp_format = base.find_param("OVERLAY_TIMESTAMP_FORMAT");
        self.params.font = base.find_param("OVERLAY_FONT");
        Ok(())
    }

    fn on_param_change(
        &self,
        reason: usize,
        params: &ad_core_rs::plugin::runtime::PluginParamSnapshot,
    ) -> ad_core_rs::plugin::runtime::ParamChangeResult {
        use ad_core_rs::plugin::runtime::{ParamChangeResult, ParamChangeValue, ParamUpdate};

        let idx = params.addr as usize;
        if idx >= MAX_OVERLAYS {
            return ParamChangeResult::updates(vec![]);
        }
        let mut slots = self.slots.lock();
        let slot = &mut slots[idx];
        let mut updates = Vec::new();

        // C++ NDPluginOverlay::writeInt32 freeze semantics. Position/Center/
        // Size are stored as signed i32 so the center<->position recompute
        // can pass through negative intermediates exactly like C++.
        if Some(reason) == self.params.use_overlay {
            slot.use_overlay = params.value.as_i32() != 0;
        } else if Some(reason) == self.params.shape {
            slot.shape = params.value.as_i32();
        } else if Some(reason) == self.params.draw_mode {
            slot.draw_mode = params.value.as_i32();
        } else if Some(reason) == self.params.position_x {
            // PositionX written -> CenterX = PositionX + SizeX/2; freeze ON.
            let pos = params.value.as_i32().max(0);
            slot.position_x = pos as usize;
            slot.freeze_position_x = true;
            slot.center_x = pos + (slot.size_x / 2) as i32;
            if let Some(ci) = self.params.center_x {
                updates.push(ParamUpdate::int32_addr(ci, idx as i32, slot.center_x));
            }
        } else if Some(reason) == self.params.position_y {
            let pos = params.value.as_i32().max(0);
            slot.position_y = pos as usize;
            slot.freeze_position_y = true;
            slot.center_y = pos + (slot.size_y / 2) as i32;
            if let Some(ci) = self.params.center_y {
                updates.push(ParamUpdate::int32_addr(ci, idx as i32, slot.center_y));
            }
        } else if Some(reason) == self.params.center_x {
            // CenterX written -> PositionX = CenterX - SizeX/2; freeze OFF.
            slot.center_x = params.value.as_i32();
            let pos = slot.center_x - (slot.size_x / 2) as i32;
            slot.position_x = pos.max(0) as usize;
            slot.freeze_position_x = false;
            if let Some(pi) = self.params.position_x {
                updates.push(ParamUpdate::int32_addr(pi, idx as i32, pos));
            }
        } else if Some(reason) == self.params.center_y {
            slot.center_y = params.value.as_i32();
            let pos = slot.center_y - (slot.size_y / 2) as i32;
            slot.position_y = pos.max(0) as usize;
            slot.freeze_position_y = false;
            if let Some(pi) = self.params.position_y {
                updates.push(ParamUpdate::int32_addr(pi, idx as i32, pos));
            }
        } else if Some(reason) == self.params.size_x {
            // SizeX written: if PositionX is frozen keep it and move the
            // center; otherwise keep the center and move the position.
            slot.size_x = params.value.as_i32().max(0) as usize;
            if slot.freeze_position_x {
                slot.center_x = slot.position_x as i32 + (slot.size_x / 2) as i32;
                if let Some(ci) = self.params.center_x {
                    updates.push(ParamUpdate::int32_addr(ci, idx as i32, slot.center_x));
                }
            } else {
                let pos = slot.center_x - (slot.size_x / 2) as i32;
                slot.position_x = pos.max(0) as usize;
                if let Some(pi) = self.params.position_x {
                    updates.push(ParamUpdate::int32_addr(pi, idx as i32, pos));
                }
            }
        } else if Some(reason) == self.params.size_y {
            slot.size_y = params.value.as_i32().max(0) as usize;
            if slot.freeze_position_y {
                slot.center_y = slot.position_y as i32 + (slot.size_y / 2) as i32;
                if let Some(ci) = self.params.center_y {
                    updates.push(ParamUpdate::int32_addr(ci, idx as i32, slot.center_y));
                }
            } else {
                let pos = slot.center_y - (slot.size_y / 2) as i32;
                slot.position_y = pos.max(0) as usize;
                if let Some(pi) = self.params.position_y {
                    updates.push(ParamUpdate::int32_addr(pi, idx as i32, pos));
                }
            }
        } else if Some(reason) == self.params.width_x {
            slot.width_x = params.value.as_i32().max(0) as usize;
        } else if Some(reason) == self.params.width_y {
            slot.width_y = params.value.as_i32().max(0) as usize;
        } else if Some(reason) == self.params.red {
            // No clamp: C stores the raw epicsInt32 (setIntegerParam ->
            // getIntegerParam into `int red`, NDPluginOverlay.cpp:339-341) and
            // narrows only at the pixel write (`(epicsType)pOverlay->red`,
            // :44). Clamping to 0..=255 here made every value above 255
            // unreachable on 16-/32-bit images.
            slot.red = params.value.as_i32();
        } else if Some(reason) == self.params.green {
            slot.green = params.value.as_i32();
        } else if Some(reason) == self.params.blue {
            slot.blue = params.value.as_i32();
        } else if Some(reason) == self.params.display_text {
            if let ParamChangeValue::Octet(s) = &params.value {
                slot.display_text = s.clone();
            }
        } else if Some(reason) == self.params.timestamp_format {
            if let ParamChangeValue::Octet(s) = &params.value {
                slot.timestamp_format = s.clone();
            }
        } else if Some(reason) == self.params.font {
            slot.font = (params.value.as_i32().max(0) as usize).min(NUM_FONTS - 1);
        }

        ParamChangeResult::updates(updates)
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
    fn test_adp10_shape_ordinals_match_c_enum() {
        // C NDOverlayShape_t (NDPluginOverlay.h:8-13): Cross=0, Rectangle=1,
        // Text=2, Ellipse=3. The 2/3 ordinals must not be swapped.
        let mut slot = OverlaySlot {
            use_overlay: true,
            ..Default::default()
        };
        slot.shape = 0;
        assert!(matches!(
            slot.to_overlay_def().map(|d| d.shape),
            Some(OverlayShape::Cross { .. })
        ));
        slot.shape = 1;
        assert!(matches!(
            slot.to_overlay_def().map(|d| d.shape),
            Some(OverlayShape::Rectangle { .. })
        ));
        slot.shape = 2;
        assert!(
            matches!(
                slot.to_overlay_def().map(|d| d.shape),
                Some(OverlayShape::Text { .. })
            ),
            "OVERLAY_SHAPE=2 must draw Text (C NDOverlayText)"
        );
        slot.shape = 3;
        assert!(
            matches!(
                slot.to_overlay_def().map(|d| d.shape),
                Some(OverlayShape::Ellipse { .. })
            ),
            "OVERLAY_SHAPE=3 must draw Ellipse (C NDOverlayEllipse)"
        );
    }

    // R6-72: C's overlay color channels are `int` (NDPluginOverlay.h:38-40)
    // written straight into the pixel by setPixel (:44, `(epicsType)red`), so a
    // 16-bit image can carry a full-scale 65535 marker. The u8 channel could
    // not express anything above 255.
    #[test]
    fn test_r6_72_color_above_255_reaches_a_16bit_pixel() {
        let arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::UInt16,
        );
        let overlays = vec![OverlayDef {
            shape: OverlayShape::Rectangle {
                x: 1,
                y: 1,
                width: 4,
                height: 3,
            },
            draw_mode: DrawMode::Set,
            // Mono takes the green channel (C `NDPluginOverlay.cpp:58`).
            color: [0, 65535, 0],
            width_x: 1,
            width_y: 1,
        }];

        let out = draw_overlays(&arr, &overlays);
        let NDDataBuffer::U16(ref v) = out.data else {
            panic!("expected U16 buffer");
        };
        assert_eq!(v[8 + 1], 65535, "full-scale 16-bit marker must survive");
        assert_eq!(v[2 * 8 + 2], 0, "interior untouched");
    }

    // R6-72 boundary: 256 is the first value the old 0..=255 clamp destroyed.
    #[test]
    fn test_r6_72_param_write_above_255_is_not_clamped() {
        use ad_core_rs::plugin::runtime::{ParamChangeValue, PluginParamSnapshot};
        use asyn_rs::port::{PortDriverBase, PortFlags};

        let mut proc = OverlayProcessor::new(vec![]);
        let mut base = PortDriverBase::new("R6_72", MAX_OVERLAYS + 1, PortFlags::default());
        proc.register_params(&mut base).unwrap();

        let red = proc.params.red.expect("OVERLAY_RED registered");
        let green = proc.params.green.expect("OVERLAY_GREEN registered");
        let blue = proc.params.blue.expect("OVERLAY_BLUE registered");
        for (reason, value) in [(red, 256), (green, 65535), (blue, 4095)] {
            proc.on_param_change(
                reason,
                &PluginParamSnapshot {
                    enable_callbacks: true,
                    reason,
                    addr: 0,
                    value: ParamChangeValue::Int32(value),
                },
            );
        }
        proc.on_param_change(
            proc.params.use_overlay.unwrap(),
            &PluginParamSnapshot {
                enable_callbacks: true,
                reason: proc.params.use_overlay.unwrap(),
                addr: 0,
                value: ParamChangeValue::Int32(1),
            },
        );

        let def = proc.slots.lock()[0]
            .to_overlay_def()
            .expect("overlay in use");
        assert_eq!(
            def.color,
            [256, 65535, 4095],
            "C stores the raw epicsInt32 and narrows only at the pixel write"
        );
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
            color: [0, 255, 0],
            width_x: 1,
            width_y: 1,
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
    fn test_adp21_rectangle_inclusive_bounds() {
        // C Rectangle (NDPluginOverlay.cpp:119-145): xmax = PositionX + SizeX
        // is inclusive, so a width=4/height=3 rectangle at (1,1) spans
        // x[1..=5], y[1..=4] — SizeX+1 by SizeY+1 pixels.
        let arr = NDArray::new(
            vec![NDDimension::new(10), NDDimension::new(10)],
            NDDataType::UInt8,
        );
        let overlays = vec![OverlayDef {
            shape: OverlayShape::Rectangle {
                x: 1,
                y: 1,
                width: 4,
                height: 3,
            },
            draw_mode: DrawMode::Set,
            color: [0, 255, 0],
            width_x: 1,
            width_y: 1,
        }];
        let out = draw_overlays(&arr, &overlays);
        let px = |x: usize, y: usize| {
            if let NDDataBuffer::U8(ref v) = out.data {
                v[y * 10 + x]
            } else {
                0
            }
        };
        // Right edge x=5 (= PositionX+SizeX, inclusive) is now drawn.
        assert_eq!(
            px(5, 1),
            255,
            "top-right corner x=PositionX+SizeX inclusive"
        );
        assert_eq!(px(5, 4), 255, "bottom-right corner");
        // Bottom edge y=4 (= PositionY+SizeY, inclusive) drawn full width.
        assert_eq!(px(1, 4), 255);
        assert_eq!(px(3, 4), 255);
        // Nothing beyond xmax / ymax.
        assert_eq!(px(6, 1), 0, "no pixel past PositionX+SizeX");
        assert_eq!(px(1, 5), 0, "no pixel past PositionY+SizeY");
        // Interior is hollow.
        assert_eq!(px(3, 2), 0);
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
                size_x: 2,
                size_y: 2,
            },
            draw_mode: DrawMode::XOR,
            color: [0, 0xFF, 0],
            width_x: 1,
            width_y: 1,
        }];

        let out = draw_overlays(&arr, &overlays);
        if let NDDataBuffer::U8(ref v) = out.data {
            // C++ Cross visits each pixel exactly once, so the center is
            // XOR'd a single time: 0xFF ^ 0xFF = 0x00 (not double-toggled).
            assert_eq!(v[0], 0x00);
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
                size_x: 4,
                size_y: 4,
            },
            draw_mode: DrawMode::Set,
            color: [0, 200, 0],
            width_x: 1,
            width_y: 1,
        }];

        let out = draw_overlays(&arr, &overlays);
        if let NDDataBuffer::U8(ref v) = out.data {
            assert_eq!(v[4 * 8 + 4], 200); // center
            assert_eq!(v[4 * 8 + 6], 200); // right arm
            assert_eq!(v[6 * 8 + 4], 200); // bottom arm
        }
    }

    #[test]
    fn test_adp20_cross_independent_size_x_size_y() {
        // C Cross (NDPluginOverlay.cpp:94-117): the horizontal arm spans
        // SizeX/2 and the vertical arm SizeY/2 — independent. A 6x2 cross must
        // NOT collapse to a 6x6 square (the prior max(SizeX,SizeY) bug).
        let arr = NDArray::new(
            vec![NDDimension::new(20), NDDimension::new(20)],
            NDDataType::UInt8,
        );
        let overlays = vec![OverlayDef {
            shape: OverlayShape::Cross {
                center_x: 10,
                center_y: 10,
                size_x: 6,
                size_y: 2,
            },
            draw_mode: DrawMode::Set,
            color: [0, 200, 0],
            width_x: 1,
            width_y: 1,
        }];
        let out = draw_overlays(&arr, &overlays);
        let px = |x: usize, y: usize| {
            if let NDDataBuffer::U8(ref v) = out.data {
                v[y * 20 + x]
            } else {
                0
            }
        };
        // Horizontal arm reaches half_x = SizeX/2 = 3 → x in [7..=13] on row 10.
        assert_eq!(px(13, 10), 200, "horizontal arm spans SizeX/2 = 3");
        assert_eq!(px(7, 10), 200);
        // Vertical arm reaches only half_y = SizeY/2 = 1 → rows 9 and 11.
        assert_eq!(px(10, 9), 200);
        assert_eq!(px(10, 11), 200);
        // Rows beyond half_y must be untouched (set only if collapsed to square).
        assert_eq!(
            px(10, 7),
            0,
            "vertical arm must span SizeY/2, not SizeX/2 (no square collapse)"
        );
        assert_eq!(px(10, 13), 0);
    }

    #[test]
    fn test_adp22_extended_chars_not_drawn() {
        // C `if (cp[ii] < 32) continue;` on a signed char skips codes >= 128.
        // The Rust font covers codes 32..=222, so extended Latin letters used
        // to render; they must now draw nothing while ASCII still does.
        let count_set = |s: &str| {
            let arr = NDArray::new(
                vec![NDDimension::new(40), NDDimension::new(20)],
                NDDataType::UInt8,
            );
            let overlays = vec![OverlayDef {
                shape: OverlayShape::Text {
                    x: 0,
                    y: 0,
                    size_x: 40,
                    size_y: 20,
                    text: s.to_string(),
                    font: 0,
                    timestamp_format: String::new(),
                },
                draw_mode: DrawMode::Set,
                color: [0, 255, 0],
                width_x: 1,
                width_y: 1,
            }];
            let out = draw_overlays(&arr, &overlays);
            if let NDDataBuffer::U8(ref v) = out.data {
                v.iter().filter(|&&p| p != 0).count()
            } else {
                0
            }
        };
        assert!(count_set("A") > 0, "printable ASCII 'A' must render");
        assert_eq!(
            count_set("\u{00C0}\u{00C9}\u{00D1}"),
            0,
            "codes >= 128 (À É Ñ) must not draw (C signed-char skip)"
        );
    }

    #[test]
    fn test_text_rendering() {
        // Render "Hi" at (0,0) with bitmap font 0 (6x13). Each glyph is a
        // 6-px-wide cell; the rendered pixels must match font_pixel().
        let arr = NDArray::new(
            vec![NDDimension::new(40), NDDimension::new(20)],
            NDDataType::UInt8,
        );
        let overlays = vec![OverlayDef {
            shape: OverlayShape::Text {
                x: 0,
                y: 0,
                size_x: 40,
                size_y: 20,
                text: "Hi".to_string(),
                font: 0,
                timestamp_format: String::new(),
            },
            draw_mode: DrawMode::Set,
            color: [0, 255, 0],
            width_x: 1,
            width_y: 1,
        }];

        let out = draw_overlays(&arr, &overlays);
        if let NDDataBuffer::U8(ref v) = out.data {
            let w = 40;
            let bmp = font_for(0);
            // Every drawn pixel of each glyph must agree with font_pixel().
            for (ci, ch) in "Hi".chars().enumerate() {
                for row in 0..bmp.height {
                    for col in 0..bmp.width {
                        let expect = font_pixel(bmp, ch, row, col);
                        let px = v[row * w + ci * bmp.width + col];
                        assert_eq!(px != 0, expect, "glyph {ch} pixel ({col},{row}) mismatch");
                    }
                }
            }
            // At least some pixels must be drawn (font is not all-blank).
            assert!(v.iter().any(|&p| p != 0), "text rendered nothing");
        }
    }

    #[test]
    fn test_text_font_selection_differs() {
        // Fonts 0 (6x13) and 2 (9x15) have different cell sizes; the 9x15
        // font extends past column 6, so the rendered pixel sets differ.
        let render = |font: usize| -> usize {
            let arr = NDArray::new(
                vec![NDDimension::new(80), NDDimension::new(20)],
                NDDataType::UInt8,
            );
            let ov = vec![OverlayDef {
                shape: OverlayShape::Text {
                    x: 0,
                    y: 0,
                    size_x: 80,
                    size_y: 20,
                    text: "W".to_string(),
                    font,
                    timestamp_format: String::new(),
                },
                draw_mode: DrawMode::Set,
                color: [0, 255, 0],
                width_x: 1,
                width_y: 1,
            }];
            let out = draw_overlays(&arr, &ov);
            if let NDDataBuffer::U8(v) = &out.data {
                v.iter().filter(|&&p| p != 0).count()
            } else {
                0
            }
        };
        assert_ne!(render(0), render(2), "font selection had no effect");
    }

    #[test]
    fn test_text_size_x_clips_characters() {
        // SizeX limits how many characters fit: with size_x = 6 only the
        // first 6-px-wide glyph is drawn (font 0).
        let arr = NDArray::new(
            vec![NDDimension::new(40), NDDimension::new(20)],
            NDDataType::UInt8,
        );
        let ov = vec![OverlayDef {
            shape: OverlayShape::Text {
                x: 0,
                y: 0,
                size_x: 6,
                size_y: 20,
                text: "WW".to_string(),
                font: 0,
                timestamp_format: String::new(),
            },
            draw_mode: DrawMode::Set,
            color: [0, 255, 0],
            width_x: 1,
            width_y: 1,
        }];
        let out = draw_overlays(&arr, &ov);
        if let NDDataBuffer::U8(v) = &out.data {
            let w = 40;
            // The second glyph would start at column 6 == xmax, so nothing
            // past column 5 may be set.
            for row in 0..font_for(0).height {
                for col in 6..40 {
                    assert_eq!(v[row * w + col], 0, "pixel ({col},{row}) past xmax");
                }
            }
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
            color: [0, 200, 0],
            width_x: 1,
            width_y: 1,
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

    /// Build an `[color, x, y]` RGB1 array carrying the `ColorMode` attribute.
    fn make_rgb1(x: usize, y: usize) -> NDArray {
        use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
        use ad_core_rs::color::NDColorMode;
        let mut arr = NDArray::new(
            vec![
                NDDimension::new(3),
                NDDimension::new(x),
                NDDimension::new(y),
            ],
            NDDataType::UInt8,
        );
        arr.attributes.add(NDAttribute {
            name: "ColorMode".into(),
            description: "Color Mode".into(),
            source: NDAttrSource::Driver,
            value: NDAttrValue::Int32(NDColorMode::RGB1 as i32),
            source_impl: None,
        });
        arr
    }

    #[test]
    fn test_r6_61_rgb1_uses_color_strides_and_writes_three_planes() {
        // R6-61 / NDPluginOverlay.cpp:39-55 — for an RGB1 array getInfo gives
        // xStride=3, yStride=3*xSize, colorStride=1, so pixel (x,y) lives at
        // 3*(y*xSize + x) and setPixel writes red/green/blue into the three
        // consecutive samples. The old code treated dims as [w=3, h=x] mono.
        let arr = make_rgb1(8, 6);
        let overlays = vec![OverlayDef {
            shape: OverlayShape::Cross {
                center_x: 4,
                center_y: 3,
                size_x: 0,
                size_y: 0,
            },
            draw_mode: DrawMode::Set,
            color: [10, 20, 30],
            width_x: 1,
            width_y: 1,
        }];

        let out = draw_overlays(&arr, &overlays);
        let NDDataBuffer::U8(ref v) = out.data else {
            panic!("expected U8 buffer");
        };
        let base = 3 * (3 * 8 + 4); // colorStride=1, xStride=3, yStride=24
        assert_eq!(
            (v[base], v[base + 1], v[base + 2]),
            (10, 20, 30),
            "RGB1 pixel must receive red/green/blue on the three color planes"
        );
        // Nothing else painted: exactly three samples differ from zero.
        assert_eq!(v.iter().filter(|&&s| s != 0).count(), 3);
    }

    #[test]
    fn test_r6_61_rgb1_out_of_range_pixel_is_clipped_by_x_size() {
        // The C bound check is against xSize/ySize from getInfo (8x6 here), not
        // against dims[0]/dims[1] (3x8). x=7 is inside; x=8 must be dropped.
        let arr = make_rgb1(8, 6);
        let overlays = vec![OverlayDef {
            shape: OverlayShape::Cross {
                center_x: 8,
                center_y: 5,
                size_x: 0,
                size_y: 0,
            },
            draw_mode: DrawMode::Set,
            color: [10, 20, 30],
            width_x: 1,
            width_y: 1,
        }];
        let out = draw_overlays(&arr, &overlays);
        let NDDataBuffer::U8(ref v) = out.data else {
            panic!("expected U8 buffer");
        };
        assert!(
            v.iter().all(|&s| s == 0),
            "x == xSize is out of bounds in C addPixel"
        );
    }

    #[test]
    fn test_r6_68_f32_xor_narrows_through_int() {
        // R6-68 / NDPluginOverlay.cpp:60 — the XOR arm of setPixel is templated
        // over every epicsType, floats included:
        //   *pValue = (epicsType)((int)*pValue ^ (int)pOverlay->green)
        // So a float pixel is truncated to int, xor'd, and cast back. It must
        // NOT degrade to Set.
        let mut arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::Float32,
        );
        // Seed the two pixels we check: 12.75 truncates to 12, 0.0 to 0.
        if let NDDataBuffer::F32(ref mut v) = arr.data {
            v[4 * 8 + 4] = 12.75;
        }
        let overlays = vec![OverlayDef {
            shape: OverlayShape::Cross {
                center_x: 4,
                center_y: 4,
                size_x: 2,
                size_y: 2,
            },
            draw_mode: DrawMode::XOR,
            color: [0, 100, 0],
            width_x: 1,
            width_y: 1,
        }];

        let out = draw_overlays(&arr, &overlays);
        let NDDataBuffer::F32(ref v) = out.data else {
            panic!("expected F32 buffer");
        };
        // (int)12.75 == 12; 12 ^ 100 == 104 (the fraction is dropped, as in C).
        assert_eq!(v[4 * 8 + 4], 104.0, "float XOR must narrow through int");
        // A zero pixel on the arm xors to the plain color.
        assert_eq!(v[4 * 8 + 5], 100.0);
    }

    #[test]
    fn test_r6_68_i64_xor_narrows_through_int() {
        // The same `(int)` narrowing applies to 64-bit pixels: C xors only the
        // low 32 bits and sign-extends the result back to epicsInt64.
        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::Int64,
        );
        if let NDDataBuffer::I64(ref mut v) = arr.data {
            v[1 * 4 + 1] = 0x0000_0007_0000_0005; // high word must be discarded
        }
        let overlays = vec![OverlayDef {
            shape: OverlayShape::Cross {
                center_x: 1,
                center_y: 1,
                size_x: 0,
                size_y: 0,
            },
            draw_mode: DrawMode::XOR,
            color: [0, 3, 0],
            width_x: 1,
            width_y: 1,
        }];

        let out = draw_overlays(&arr, &overlays);
        let NDDataBuffer::I64(ref v) = out.data else {
            panic!("expected I64 buffer");
        };
        // C: (epicsInt64)((int)0x0000000700000005 ^ 3) == (epicsInt64)(5 ^ 3) == 6
        assert_eq!(v[1 * 4 + 1], 6);
    }

    #[test]
    fn test_cross_thickness_half_width() {
        // C++ Cross uses xwide = WidthX/2: WidthY=4 => horizontal band of
        // 2*2+1 = 5 rows centered on the cross.
        let arr = NDArray::new(
            vec![NDDimension::new(20), NDDimension::new(20)],
            NDDataType::UInt8,
        );
        let overlays = vec![OverlayDef {
            shape: OverlayShape::Cross {
                center_x: 10,
                center_y: 10,
                size_x: 8,
                size_y: 8,
            },
            draw_mode: DrawMode::Set,
            color: [0, 255, 0],
            width_x: 1,
            width_y: 4,
        }];
        let out = draw_overlays(&arr, &overlays);
        if let NDDataBuffer::U8(ref v) = out.data {
            let w = 20;
            // The horizontal band spans rows [cy-2, cy+2] = [8, 12]. A column
            // away from the vertical strip (e.g. x=7) is set inside the band
            // and clear outside.
            for y in 8..=12 {
                assert_eq!(v[y * w + 7], 255, "row {y} should be in the band");
            }
            assert_eq!(v[7 * w + 7], 0, "row 7 is outside the band");
            assert_eq!(v[13 * w + 7], 0, "row 13 is outside the band");
        }
    }

    #[test]
    fn test_xor_ellipse_no_double_toggle() {
        // Regression: an XOR ellipse must not leave holes from double-toggled
        // pixels. Every drawn pixel ends up XOR'd exactly once: 0 -> 0xFF.
        let arr = NDArray::new(
            vec![NDDimension::new(40), NDDimension::new(40)],
            NDDataType::UInt8,
        );
        let overlays = vec![OverlayDef {
            shape: OverlayShape::Ellipse {
                center_x: 20,
                center_y: 20,
                rx: 12,
                ry: 8,
            },
            draw_mode: DrawMode::XOR,
            color: [0, 0xFF, 0],
            width_x: 3,
            width_y: 3,
        }];
        let out = draw_overlays(&arr, &overlays);
        if let NDDataBuffer::U8(ref v) = out.data {
            // Any non-zero pixel must be exactly 0xFF — a double-toggled pixel
            // would have wrapped back to 0x00, so the ellipse would have a
            // hole. Count drawn pixels to confirm the ellipse is non-empty.
            let mut drawn = 0;
            for &px in v.iter() {
                assert!(px == 0 || px == 0xFF, "double-toggled pixel: {px}");
                if px == 0xFF {
                    drawn += 1;
                }
            }
            assert!(drawn > 0, "ellipse drew no pixels");
        }
    }

    #[test]
    fn test_text_timestamp_format_appends() {
        // A non-empty timestamp_format appends a formatted timestamp; an empty
        // one renders the bare text. Compare rendered pixel counts.
        let mut arr = NDArray::new(
            vec![NDDimension::new(120), NDDimension::new(12)],
            NDDataType::UInt8,
        );
        // EPICS timestamp: sec since 1990; pick a value with a known date.
        arr.timestamp = ad_core_rs::timestamp::EpicsTimestamp {
            sec: 0, // 1990-01-01 00:00:00
            nsec: 0,
        };
        let count_set = |arr: &NDArray, fmt: &str| -> usize {
            let ov = vec![OverlayDef {
                shape: OverlayShape::Text {
                    x: 0,
                    y: 0,
                    size_x: 120,
                    size_y: 12,
                    text: "T".to_string(),
                    font: 0,
                    timestamp_format: fmt.to_string(),
                },
                draw_mode: DrawMode::Set,
                color: [0, 255, 0],
                width_x: 1,
                width_y: 1,
            }];
            let out = draw_overlays(arr, &ov);
            if let NDDataBuffer::U8(v) = &out.data {
                v.iter().filter(|&&p| p != 0).count()
            } else {
                0
            }
        };
        let bare = count_set(&arr, "");
        let with_ts = count_set(&arr, "%Y-%m-%d");
        // The appended "1990-01-01" adds glyphs => strictly more set pixels.
        assert!(with_ts > bare, "timestamp text should add pixels");
    }

    // ---- Center/Position freeze semantics (C++ writeInt32) ----------------

    use ad_core_rs::plugin::runtime::{ParamChangeValue, ParamUpdate, PluginParamSnapshot};

    /// Drive one int32 param change on overlay slot 0 and return the updates.
    fn drive(p: &mut OverlayProcessor, reason: usize, value: i32) -> Vec<ParamUpdate> {
        let snap = PluginParamSnapshot {
            enable_callbacks: true,
            reason,
            addr: 0,
            value: ParamChangeValue::Int32(value),
        };
        p.on_param_change(reason, &snap).param_updates
    }

    fn find_int_update(updates: &[ParamUpdate], reason: usize) -> Option<i32> {
        updates.iter().find_map(|u| match u {
            ParamUpdate::Int32 {
                reason: r, value, ..
            } if *r == reason => Some(*value),
            _ => None,
        })
    }

    fn setup_processor() -> (OverlayProcessor, OverlayParamIndices) {
        let mut p = OverlayProcessor::new(vec![]);
        let mut base =
            asyn_rs::port::PortDriverBase::new("OV_TEST", 8, asyn_rs::port::PortFlags::default());
        p.register_params(&mut base).unwrap();
        let params = OverlayParamIndices {
            position_x: base.find_param("OVERLAY_POSITION_X"),
            position_y: base.find_param("OVERLAY_POSITION_Y"),
            center_x: base.find_param("OVERLAY_CENTER_X"),
            center_y: base.find_param("OVERLAY_CENTER_Y"),
            size_x: base.find_param("OVERLAY_SIZE_X"),
            size_y: base.find_param("OVERLAY_SIZE_Y"),
            ..Default::default()
        };
        (p, params)
    }

    #[test]
    fn test_freeze_position_then_resize_moves_center() {
        // Write PositionX last -> freeze ON. A later SizeX change keeps
        // PositionX fixed and moves CenterX (C++ freezePositionX == true).
        let (mut p, idx) = setup_processor();
        drive(&mut p, idx.size_x.unwrap(), 20);
        drive(&mut p, idx.position_x.unwrap(), 100);
        assert_eq!(p.slots.lock()[0].position_x, 100);
        assert_eq!(p.slots.lock()[0].center_x, 110); // 100 + 20/2

        let updates = drive(&mut p, idx.size_x.unwrap(), 40);
        // PositionX stays 100; CenterX moves to 100 + 40/2 = 120.
        assert_eq!(p.slots.lock()[0].position_x, 100);
        assert_eq!(p.slots.lock()[0].center_x, 120);
        assert_eq!(find_int_update(&updates, idx.center_x.unwrap()), Some(120));
    }

    #[test]
    fn test_freeze_center_then_resize_moves_position() {
        // Write CenterX last -> freeze OFF. A later SizeX change keeps
        // CenterX fixed and moves PositionX (C++ freezePositionX == false).
        let (mut p, idx) = setup_processor();
        drive(&mut p, idx.size_x.unwrap(), 20);
        drive(&mut p, idx.center_x.unwrap(), 200);
        assert_eq!(p.slots.lock()[0].center_x, 200);
        assert_eq!(p.slots.lock()[0].position_x, 190); // 200 - 20/2

        let updates = drive(&mut p, idx.size_x.unwrap(), 60);
        // CenterX stays 200; PositionX moves to 200 - 60/2 = 170.
        assert_eq!(p.slots.lock()[0].center_x, 200);
        assert_eq!(p.slots.lock()[0].position_x, 170);
        assert_eq!(
            find_int_update(&updates, idx.position_x.unwrap()),
            Some(170)
        );
    }

    #[test]
    fn test_freeze_y_axis_independent() {
        // The Y freeze flag is tracked independently of X.
        let (mut p, idx) = setup_processor();
        drive(&mut p, idx.size_y.unwrap(), 10);
        drive(&mut p, idx.center_y.unwrap(), 50); // freeze_y OFF
        drive(&mut p, idx.size_x.unwrap(), 10);
        drive(&mut p, idx.position_x.unwrap(), 5); // freeze_x ON
        assert!(p.slots.lock()[0].freeze_position_x);
        assert!(!p.slots.lock()[0].freeze_position_y);
    }

    #[test]
    fn test_format_epics_time_known_date() {
        // EPICS sec 0 == 1990-01-01 00:00:00 UTC.
        let ts = ad_core_rs::timestamp::EpicsTimestamp {
            sec: 0,
            nsec: 123_456_000,
        };
        assert_eq!(
            format_epics_time(ts, "%Y-%m-%d %H:%M:%S.%f"),
            "1990-01-01 00:00:00.123456"
        );
        assert_eq!(format_epics_time(ts, "100%%"), "100%");
    }
}
