use asyn_rs::port::InterfaceMask;
use std::sync::Arc;

use ad_core_rs::error::ADResult;
use ad_core_rs::ndarray::{NDArray, NDDataBuffer};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, ProcessResult};
use parking_lot::Mutex;

/// Shape to draw.
#[derive(Debug, Clone)]
pub enum OverlayShape {
    Cross {
        center_x: i32,
        center_y: i32,
        /// X arm half-extent source — C draws the horizontal arm over
        /// `SizeX/2` each side of the center, independent of `SizeY`.
        size_x: i32,
        /// Y arm half-extent source — C draws the vertical arm over `SizeY/2`.
        size_y: i32,
    },
    Rectangle {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
    /// Bounded by PositionX/Y and SizeX/Y as C's NDOverlay_t; the odd
    /// SizeX bit clamps the line width (NDPluginOverlay.cpp:150).
    Ellipse {
        x: i32,
        y: i32,
        size_x: i32,
        size_y: i32,
    },
    Text {
        x: i32,
        y: i32,
        /// X extent (SizeX) — characters past `x + size_x` are not drawn,
        /// matching C++ `xmax = PositionX + SizeX`.
        size_x: i32,
        /// Y extent (SizeY) — drawing stops at `min(y + size_y, y + font.height)`.
        size_y: i32,
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
    /// Line thickness (C `int WidthX/WidthY`, NDPluginOverlay.h:34-35).
    pub width_x: i32,
    pub width_y: i32,
}

// ---------------------------------------------------------------------------
// Bitmap fonts — ported from ADCore NDPluginOverlayTextFont.cpp
// ---------------------------------------------------------------------------

use crate::overlay_font::{BitmapFont, FONTS};

/// Number of selectable bitmap fonts (C++ `NDPluginOverlayTextFontBitmapTypeN`).
pub const NUM_FONTS: usize = 4;

/// Test whether bit `col` of character `ch` row `row` is set in `font`.
///
/// Mirrors C++ `NDPluginOverlay.cpp` text rendering: each character occupies
/// `height` rows of `bytes_per_char` bytes; bit order is MSB-first within
/// each byte (`mask = 0x80`). Characters below `first_char` or above the
/// font range render blank.
fn font_pixel(font: &BitmapFont, ch: u8, row: usize, col: usize) -> bool {
    if ch < font.first_char {
        return false;
    }
    let ci = (ch - font.first_char) as usize;
    if ci >= font.num_chars || row >= font.height || col >= font.width {
        return false;
    }
    let byte_in_row = col / 8;
    let bit = 7 - (col % 8);
    let offset = (font.height * ci + row) * font.bytes_per_char + byte_in_row;
    (font.bitmap[offset] >> bit) & 1 != 0
}

/// C `epicsTimeToStrftime(tstr, sizeof(tstr)-1, ...)` with `char tstr[64]`
/// (NDPluginOverlay.cpp:189): local time, `%0nf` fractions, 62 bytes at most.
fn format_epics_time(ts: ad_core_rs::timestamp::EpicsTimestamp, fmt: &str) -> String {
    epics_base_rs::server::builtin_devices::timestamp::epics_time_to_strftime_in(
        63,
        fmt,
        ts.to_system_time().into(),
    )
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
                    let xwide = (overlay.width_x / 2) as i64;
                    let ywide = (overlay.width_y / 2) as i64;
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
                    let xmax = xmin + *width as i64;
                    let ymin = *y as i64;
                    let ymax = ymin + *height as i64;
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
                    x,
                    y,
                    size_x,
                    size_y,
                } => {
                    // C++ doOverlayT Ellipse: parametric over the first
                    // quadrant, mirrored to the other three; for each of
                    // `xwide` thickness layers shrink the radii by jj. C++
                    // sorts+uniques the resulting pixel list before drawing
                    // "or the XOR draw mode won't work because the pixel will
                    // be set and then unset". We dedup pixels here for the
                    // same reason.
                    let xsize = (*size_x / 2) as i64;
                    let ysize = (*size_y / 2) as i64;
                    let cx = *x as i64 + xsize;
                    let cy = *y as i64 + ysize;
                    let xwide = (overlay.width_x as i64).min(*size_x as i64 - 1);
                    let n_steps = 2 * (xsize + ysize);
                    let theta_step = std::f64::consts::FRAC_PI_2 / n_steps as f64;
                    let mut pixels: Vec<(i64, i64)> = Vec::new();
                    // theta accumulates as C's loop does: n_steps == 0 still
                    // runs once at theta 0 with an infinite theta_step.
                    let mut theta = 0.0f64;
                    for _ in 0..=n_steps {
                        for jj in 0..xwide {
                            let ix = (((xsize - jj) as f64) * theta.cos() + 0.5) as i64;
                            let iy = (((ysize - jj) as f64) * theta.sin() + 0.5) as i64;
                            pixels.push((cx + ix, cy + iy));
                            pixels.push((cx + ix, cy - iy));
                            pixels.push((cx - ix, cy + iy));
                            pixels.push((cx - ix, cy - iy));
                        }
                        theta += theta_step;
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
                    // An ill-defined font draws nothing (NDPluginOverlay.cpp:181-186).
                    let Some(bmp) = FONTS.get(*font) else {
                        continue;
                    };
                    // Append the formatted timestamp when a format is set
                    // (C++ epicsTimeToStrftime + DisplayText concatenation).
                    let rendered = if timestamp_format.is_empty() {
                        text.clone()
                    } else {
                        format!("{}{}", text, format_epics_time(array_ts, timestamp_format))
                    };
                    let xmin = *x as i64;
                    let xmax = xmin + *size_x as i64;
                    let ymin = *y as i64;
                    let ymax = (ymin + *size_y as i64).min(ymin + bmp.height as i64);
                    // C walks the string by `char`, so each UTF-8 byte is a cell.
                    for (ci, ch) in rendered.bytes().enumerate() {
                        // C tests `if (cp[ii] < 32) continue;` on a signed
                        // `char`, so bytes >= 128 are negative and skipped along
                        // with control codes: only printable ASCII 32..=127 is
                        // drawn. The cell still advances (C `continue`, ci++).
                        if !(32..128).contains(&ch) {
                            continue;
                        }
                        let char_x0 = xmin + (ci * bmp.width) as i64;
                        if char_x0 >= xmax {
                            break; // none of this character fits
                        }
                        for row in 0..bmp.height {
                            let iy = ymin + row as i64;
                            if iy >= ymax {
                                break;
                            }
                            for col in 0..bmp.width {
                                let ix = char_x0 + col as i64;
                                if ix >= xmax {
                                    break;
                                }
                                // addPixel drops what falls off the array.
                                if ix >= 0 && iy >= 0 && font_pixel(bmp, ch, row, col) {
                                    set_pixel(ix as usize, iy as usize);
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
///
/// The output is a copy of `src` taken from `pool`, as C's
/// `pNDArrayPool->copy(pArray, NULL, 1)` (NDPluginOverlay.cpp).
pub fn draw_overlays(
    pool: &NDArrayPool,
    src: &NDArray,
    overlays: &[OverlayDef],
) -> ADResult<NDArray> {
    let mut arr = pool.alloc_copy(src)?;
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

    Ok(arr)
}

/// Runtime overlay state — one per address in `0..maxOverlays`.
#[derive(Debug, Clone)]
struct OverlaySlot {
    use_overlay: bool,
    shape: i32,     // C NDOverlayShape_t: 0=Cross, 1=Rectangle, 2=Text, 3=Ellipse
    draw_mode: i32, // 0=Set, 1=XOR
    // C `NDOverlay_t` geometry is `int` (NDPluginOverlay.h:30-35): a shape
    // may start left of or above the array, and addPixel clips it.
    position_x: i32,
    position_y: i32,
    size_x: i32,
    size_y: i32,
    width_x: i32,
    width_y: i32,
    // C `NDOverlay_t` (NDPluginOverlay.h:38-40): `int red/green/blue`, read
    // from asynInt32 params with no range clamp.
    red: i32,
    green: i32,
    blue: i32,
    display_text: String,
    timestamp_format: String,
    font: i32,
    /// C++ `freezePositionX`: true once PositionX was written more recently
    /// than CenterX. A SizeX change then keeps PositionX fixed (moving the
    /// center); false keeps CenterX fixed (moving the position).
    freeze_position_x: bool,
    freeze_position_y: bool,
}

/// C's `prevOverlays_.resize(maxOverlays_)` (NDPluginOverlay.cpp:412)
/// value-initialises every `NDOverlay_t`: all zero, so Cross, Set, both
/// freeze flags false, until a param write replaces a field.
impl Default for OverlaySlot {
    fn default() -> Self {
        Self {
            use_overlay: false,
            shape: 0,
            draw_mode: 0,
            position_x: 0,
            position_y: 0,
            size_x: 0,
            size_y: 0,
            width_x: 0,
            width_y: 0,
            red: 0,
            green: 0,
            blue: 0,
            display_text: String::new(),
            timestamp_format: String::new(),
            font: 0,
            freeze_position_x: false,
            freeze_position_y: false,
        }
    }
}

impl OverlaySlot {
    fn to_overlay_def(&self) -> Option<OverlayDef> {
        if !self.use_overlay {
            return None;
        }
        // setPixel writes only for Set or XOR (NDPluginOverlay.cpp:42-60).
        let draw_mode = match self.draw_mode {
            0 => DrawMode::Set,
            1 => DrawMode::XOR,
            _ => return None,
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
                font: usize::try_from(self.font).ok().filter(|f| *f < NUM_FONTS)?,
                timestamp_format: self.timestamp_format.clone(),
            },
            3 => OverlayShape::Ellipse {
                x: self.position_x,
                y: self.position_y,
                size_x: self.size_x,
                size_y: self.size_y,
            },
            // doOverlayT's shape switch has no default (NDPluginOverlay.cpp:93).
            _ => return None,
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
    max_size_x: Option<usize>,
    max_size_y: Option<usize>,
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
    slots: Mutex<Vec<OverlaySlot>>,
    params: OverlayParamIndices,
}

impl OverlayProcessor {
    /// `max_overlays` is the C constructor's `maxOverlays`: one slot per
    /// address (NDPluginOverlay.cpp:412), and the port that serves them has
    /// the same number of addresses (`:403`). Address 0 always exists.
    pub fn new(max_overlays: usize, overlays: Vec<OverlayDef>) -> Self {
        let mut slots = vec![OverlaySlot::default(); max_overlays.max(1)];
        for (i, o) in overlays.into_iter().enumerate().take(slots.len()) {
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
                    slot.position_x = center_x - size_x / 2;
                    slot.position_y = center_y - size_y / 2;
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
                    x,
                    y,
                    size_x,
                    size_y,
                } => {
                    slot.shape = 3;
                    slot.position_x = x;
                    slot.position_y = y;
                    slot.size_x = size_x;
                    slot.size_y = size_y;
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
                    slot.font = i32::try_from(font).unwrap_or(-1);
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
    const ASYN_FLAGS: asyn_rs::port::PortFlags = ad_core_rs::plugin::runtime::ASYN_MULTIDEVICE;
    // NDPluginOverlay.cpp:404-405, the same mask for both.
    const INTERFACE_MASK: InterfaceMask = InterfaceMask::GENERIC_POINTER;
    const INTERRUPT_MASK: InterfaceMask = Self::INTERFACE_MASK;

    /// C `NDPluginOverlay::writeInt32` drops the base class status
    /// (`NDPluginDriver::writeInt32(pasynUser, value);`, NDPluginOverlay.cpp:508): a refused NDPluginDriver write keeps
    /// its clamp and succeeds. A Position/Center/Size write sets the other
    /// of Position/Center from the params and the freeze flag (:466-506).
    fn validate_int32(
        &self,
        reason: usize,
        addr: i32,
        value: i32,
        verdict: ad_core_rs::plugin::runtime::WriteVerdict<i32>,
        params: &mut ad_core_rs::plugin::runtime::WritePort<'_>,
    ) -> ad_core_rs::plugin::runtime::WriteOutcome<i32> {
        let outcome = ad_core_rs::plugin::runtime::WriteOutcome::from(verdict.succeed());
        let p = &self.params;
        for (x, pos, center, size) in [
            (true, p.position_x, p.center_x, p.size_x),
            (false, p.position_y, p.center_y, p.size_y),
        ] {
            let (Some(pos), Some(center), Some(size)) = (pos, center, size) else {
                continue;
            };
            if ![pos, center, size].contains(&reason) {
                continue;
            }
            let get = |r| params.get_int32_param(r, addr).unwrap_or(0);
            let mut slots = self.slots.lock();
            let Some(slot) = slots.get_mut(addr as usize) else {
                return outcome;
            };
            let (frozen, slot_pos, slot_size) = if x {
                (
                    &mut slot.freeze_position_x,
                    &mut slot.position_x,
                    &mut slot.size_x,
                )
            } else {
                (
                    &mut slot.freeze_position_y,
                    &mut slot.position_y,
                    &mut slot.size_y,
                )
            };
            let (other, other_value, position) = if reason == center {
                *frozen = false;
                let at = value.wrapping_sub(get(size) / 2);
                (pos, at, at)
            } else if reason == pos {
                *frozen = true;
                (center, value.wrapping_add(get(size) / 2), value)
            } else {
                *slot_size = value;
                if *frozen {
                    let at = get(pos);
                    (center, at.wrapping_add(value / 2), at)
                } else {
                    let at = get(center).wrapping_sub(value / 2);
                    (pos, at, at)
                }
            };
            *slot_pos = position;
            return outcome.set_int32(other, addr, other_value).settled();
        }
        outcome
    }
    fn process_array(&self, array: &Arc<NDArray>, pool: &NDArrayPool) -> ProcessResult {
        let active = self.build_active_overlays();
        let mut result = match draw_overlays(pool, array, &active) {
            Ok(out) => ProcessResult::arrays(vec![Arc::new(out)]),
            Err(e) => {
                tracing::warn!(error = %e, "overlay output allocation failed; dropping frame");
                return ProcessResult::empty();
            }
        };
        // Every frame, before any overlay is drawn (NDPluginOverlay.cpp:319-320).
        let info = array.info();
        for (reason, size) in [
            (self.params.max_size_x, info.x_size),
            (self.params.max_size_y, info.y_size),
        ] {
            if let Some(reason) = reason {
                result
                    .param_updates
                    .push(ad_core_rs::plugin::runtime::ParamUpdate::int32(
                        reason,
                        size as i32,
                    ));
            }
        }
        result
    }

    fn plugin_type(&self) -> &str {
        "NDPluginOverlay"
    }

    /// NDPluginOverlay.cpp:440
    fn array_callbacks_seed(&self) -> Option<bool> {
        Some(true)
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

        self.params.max_size_x = base.find_param("MAX_SIZE_X");
        self.params.max_size_y = base.find_param("MAX_SIZE_Y");
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
        use ad_core_rs::plugin::runtime::{ParamChangeResult, ParamChangeValue};

        let idx = params.addr as usize;
        let mut slots = self.slots.lock();
        let Some(slot) = slots.get_mut(idx) else {
            return ParamChangeResult::updates(vec![]);
        };
        if Some(reason) == self.params.use_overlay {
            slot.use_overlay = params.value.as_i32() != 0;
        } else if Some(reason) == self.params.shape {
            slot.shape = params.value.as_i32();
        } else if Some(reason) == self.params.draw_mode {
            slot.draw_mode = params.value.as_i32();
        } else if Some(reason) == self.params.width_x {
            slot.width_x = params.value.as_i32();
        } else if Some(reason) == self.params.width_y {
            slot.width_y = params.value.as_i32();
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
            slot.font = params.value.as_i32();
        }

        ParamChangeResult::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `NDPluginDriver::writeInt32(pasynUser, value);` (NDPluginOverlay.cpp:508).
    #[test]
    fn a_refused_base_write_succeeds_as_c_write_int32_does() {
        use ad_core_rs::plugin::runtime::WriteVerdict;
        let proc = OverlayProcessor::new(8, vec![]);
        let mut base = asyn_rs::port::PortDriverBase::new(
            "T",
            1,
            Default::default(),
            asyn_rs::port::InterfaceMask::UNMIGRATED_ALL,
            asyn_rs::port::InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        let clamp = WriteVerdict::Refuse {
            keep: 1,
            message: String::new(),
        };
        assert_eq!(
            proc.validate_int32(
                0,
                0,
                98,
                clamp,
                &mut ad_core_rs::plugin::runtime::WritePort::new(&mut base)
            )
            .verdict,
            WriteVerdict::Accept(1)
        );
    }
    use ad_core_rs::ndarray::{NDDataType, NDDimension};

    fn pool() -> Arc<NDArrayPool> {
        NDArrayPool::new(0)
    }

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

        let out = draw_overlays(&pool(), &arr, &overlays).unwrap();
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
        use asyn_rs::port::InterfaceMask;
        use asyn_rs::port::{PortDriverBase, PortFlags};

        let mut proc = OverlayProcessor::new(8, vec![]);
        let mut base = PortDriverBase::new(
            "R6_72",
            9,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
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

        let out = draw_overlays(&pool(), &arr, &overlays).unwrap();
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
        let out = draw_overlays(&pool(), &arr, &overlays).unwrap();
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

        let out = draw_overlays(&pool(), &arr, &overlays).unwrap();
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

        let out = draw_overlays(&pool(), &arr, &overlays).unwrap();
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
        let out = draw_overlays(&pool(), &arr, &overlays).unwrap();
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
            let out = draw_overlays(&pool(), &arr, &overlays).unwrap();
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

    /// C renders `char` by `char`: a two-byte UTF-8 letter takes two skipped
    /// cells, so the following glyph lands two cells on, not one.
    #[test]
    fn text_advances_one_cell_per_byte() {
        let render = |text: &str| {
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
                    text: text.to_string(),
                    font: 0,
                    timestamp_format: String::new(),
                },
                draw_mode: DrawMode::Set,
                color: [0, 255, 0],
                width_x: 1,
                width_y: 1,
            }];
            let NDDataBuffer::U8(v) = &draw_overlays(&pool(), &arr, &overlays).unwrap().data else {
                panic!("u8 output")
            };
            v.clone()
        };
        let (utf8, ctl) = (render("\u{e9}H"), render("\x01\x01H"));
        assert!(ctl.iter().any(|p| *p != 0));
        assert_eq!(utf8, ctl);
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

        let out = draw_overlays(&pool(), &arr, &overlays).unwrap();
        if let NDDataBuffer::U8(ref v) = out.data {
            let w = 40;
            let bmp = &FONTS[0];
            // Every drawn pixel of each glyph must agree with font_pixel().
            for (ci, ch) in "Hi".bytes().enumerate() {
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
            let out = draw_overlays(&pool(), &arr, &ov).unwrap();
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
        let out = draw_overlays(&pool(), &arr, &ov).unwrap();
        if let NDDataBuffer::U8(v) = &out.data {
            let w = 40;
            // The second glyph would start at column 6 == xmax, so nothing
            // past column 5 may be set.
            for row in 0..FONTS[0].height {
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

        let out = draw_overlays(&pool(), &arr, &overlays).unwrap();
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

        let out = draw_overlays(&pool(), &arr, &overlays).unwrap();
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
        let out = draw_overlays(&pool(), &arr, &overlays).unwrap();
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

        let out = draw_overlays(&pool(), &arr, &overlays).unwrap();
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

        let out = draw_overlays(&pool(), &arr, &overlays).unwrap();
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
        let out = draw_overlays(&pool(), &arr, &overlays).unwrap();
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
                x: 8,
                y: 12,
                size_x: 24,
                size_y: 16,
            },
            draw_mode: DrawMode::XOR,
            color: [0, 0xFF, 0],
            width_x: 3,
            width_y: 3,
        }];
        let out = draw_overlays(&pool(), &arr, &overlays).unwrap();
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
            let out = draw_overlays(&pool(), arr, &ov).unwrap();
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

    /// processCallbacks posts the array's X and Y size to MAX_SIZE_X/Y on
    /// every frame (NDPluginOverlay.cpp:319-320), the HOPR source of
    /// NDOverlayN.template's position and size sliders.
    #[test]
    fn every_frame_posts_max_size_x_and_y() {
        use ad_core_rs::plugin::runtime::ParamUpdate;
        let (p, base, _) = setup_processor();
        let arr = NDArray::new(
            vec![NDDimension::new(12), NDDimension::new(5)],
            NDDataType::UInt8,
        );
        let out = p.process_array(&Arc::new(arr), &pool());
        let (x, y) = (
            base.find_param("MAX_SIZE_X").unwrap(),
            base.find_param("MAX_SIZE_Y").unwrap(),
        );
        let posted: Vec<SideParam> = out.param_updates.into_iter().map(SideParam::from).collect();
        let want: Vec<SideParam> = [ParamUpdate::int32(x, 12), ParamUpdate::int32(y, 5)]
            .into_iter()
            .map(SideParam::from)
            .collect();
        assert_eq!(posted, want);
    }

    /// Store `value` at `reason`/`addr` as the port's param-change path does.
    fn set_param(p: &OverlayProcessor, reason: usize, addr: i32, value: i32) {
        let snap = PluginParamSnapshot {
            enable_callbacks: true,
            reason,
            addr,
            value: ParamChangeValue::Int32(value),
        };
        p.on_param_change(reason, &snap);
    }

    /// Pixels of an 8x8 mono frame the processor's overlays turn non-zero.
    fn drawn(p: &OverlayProcessor) -> Vec<(usize, usize)> {
        let out = p.process_array(&Arc::new(make_8x8()), &pool());
        let NDDataBuffer::U8(v) = &out.output_arrays[0].data else {
            panic!("u8 output")
        };
        (0..64)
            .filter(|i| v[*i] != 0)
            .map(|i| (i % 8, i / 8))
            .collect()
    }

    /// C geometry is signed (`int PositionX`, NDPluginOverlay.h:30): a
    /// rectangle at PositionX=-2, SizeX=4 spans x=-2..=2 and addPixel
    /// (NDPluginOverlay.cpp:31-33) clips what falls left of the array.
    #[test]
    fn a_shape_at_a_negative_position_is_clipped_not_moved() {
        let (p, mut base, idx) = setup_processor();
        set_param(&p, p.params.use_overlay.unwrap(), 0, 1);
        set_param(&p, p.params.shape.unwrap(), 0, 1);
        set_param(&p, p.params.green.unwrap(), 0, 9);
        drive(&p, &mut base, idx.size_x.unwrap(), 4);
        drive(&p, &mut base, idx.position_x.unwrap(), -2);
        drive(&p, &mut base, idx.size_y.unwrap(), 2);
        drive(&p, &mut base, idx.position_y.unwrap(), 1);
        set_param(&p, p.params.width_x.unwrap(), 0, 1);
        set_param(&p, p.params.width_y.unwrap(), 0, 1);
        // xwide = ywide = 1: rows 1 and 3 span x=-2..=2, row 2 holds
        // x=-2 (clipped) and x=2.
        let mut want = vec![(0, 1), (1, 1), (2, 1), (2, 2), (0, 3), (1, 3), (2, 3)];
        want.sort_by_key(|&(x, y)| (y, x));
        assert_eq!(drawn(&p), want);
    }

    /// An overlay with only USE and the geometry written draws C's
    /// zero-initialised shape (NDPluginOverlay.cpp:412): a Cross of width 0.
    #[test]
    fn an_overlay_starts_as_c_zero_initialised_one() {
        let (p, mut base, idx) = setup_processor();
        set_param(&p, p.params.use_overlay.unwrap(), 0, 1);
        set_param(&p, p.params.green.unwrap(), 0, 5);
        drive(&p, &mut base, idx.size_x.unwrap(), 2);
        drive(&p, &mut base, idx.size_y.unwrap(), 2);
        drive(&p, &mut base, idx.position_x.unwrap(), 0);
        drive(&p, &mut base, idx.position_y.unwrap(), 0);
        assert_eq!(drawn(&p), [(1, 0), (0, 1), (1, 1), (2, 1), (1, 2)]);
    }

    /// Both freeze flags start false (NDPluginOverlay.cpp:412), so a SizeX
    /// write before any Position or Center write keeps CenterX and moves
    /// PositionX (:495-497).
    #[test]
    fn a_first_size_write_keeps_the_center() {
        let (p, mut base, idx) = setup_processor();
        let (px, cx, sx) = (
            idx.position_x.unwrap(),
            idx.center_x.unwrap(),
            idx.size_x.unwrap(),
        );
        base.set_int32_param(cx, 0, 40).unwrap();
        base.set_int32_param(px, 0, 0).unwrap();
        assert_eq!(drive(&p, &mut base, sx, 10), [(px, 35)]);
    }

    /// A Shape outside the C enum draws nothing: doOverlayT's switch has
    /// no default arm (NDPluginOverlay.cpp:93).
    #[test]
    fn an_unknown_shape_draws_nothing() {
        let (p, mut base, idx) = setup_processor();
        set_param(&p, p.params.use_overlay.unwrap(), 0, 1);
        set_param(&p, p.params.shape.unwrap(), 0, 7);
        set_param(&p, p.params.green.unwrap(), 0, 5);
        set_param(&p, p.params.width_x.unwrap(), 0, 1);
        set_param(&p, p.params.width_y.unwrap(), 0, 1);
        drive(&p, &mut base, idx.size_x.unwrap(), 2);
        drive(&p, &mut base, idx.size_y.unwrap(), 2);
        assert_eq!(drawn(&p), []);
    }

    /// A DrawMode other than Set or XOR writes no pixel: setPixel has no
    /// other arm (NDPluginOverlay.cpp:42-60).
    #[test]
    fn an_unknown_draw_mode_draws_nothing() {
        let (p, mut base, idx) = setup_processor();
        set_param(&p, p.params.use_overlay.unwrap(), 0, 1);
        set_param(&p, p.params.draw_mode.unwrap(), 0, 2);
        set_param(&p, p.params.green.unwrap(), 0, 5);
        drive(&p, &mut base, idx.size_x.unwrap(), 2);
        drive(&p, &mut base, idx.size_y.unwrap(), 2);
        assert_eq!(drawn(&p), []);
    }

    /// Text with a Font outside the bitmap table draws nothing; C returns
    /// before rendering (NDPluginOverlay.cpp:181-186) instead of clamping.
    #[test]
    fn text_with_an_out_of_range_font_draws_nothing() {
        for font in [-1, 4] {
            let (p, mut base, idx) = setup_processor();
            set_param(&p, p.params.use_overlay.unwrap(), 0, 1);
            set_param(&p, p.params.shape.unwrap(), 0, 2);
            set_param(&p, p.params.font.unwrap(), 0, font);
            set_param(&p, p.params.green.unwrap(), 0, 5);
            p.slots.lock()[0].display_text = "HH".into();
            drive(&p, &mut base, idx.size_x.unwrap(), 8);
            drive(&p, &mut base, idx.size_y.unwrap(), 8);
            assert_eq!(drawn(&p), [], "font {font}");
        }
    }

    /// Ellipse line width is MIN(WidthX, SizeX-1) on the raw SizeX and the
    /// layer loop is `jj<xwide` (NDPluginOverlay.cpp:148-165): width 0 or a
    /// negative size draws nothing, and an odd SizeX allows one more layer
    /// than 2*(SizeX/2). Expected pixels come from that C loop run as-is.
    #[test]
    fn ellipse_width_clamps_to_the_raw_size_x() {
        let ellipse = |pos: i32, size: (i32, i32), width: i32| {
            let (p, mut base, idx) = setup_processor();
            set_param(&p, p.params.use_overlay.unwrap(), 0, 1);
            set_param(&p, p.params.shape.unwrap(), 0, 3);
            set_param(&p, p.params.green.unwrap(), 0, 5);
            set_param(&p, p.params.width_x.unwrap(), 0, width);
            drive(&p, &mut base, idx.position_x.unwrap(), pos);
            drive(&p, &mut base, idx.position_y.unwrap(), pos);
            drive(&p, &mut base, idx.size_x.unwrap(), size.0);
            drive(&p, &mut base, idx.size_y.unwrap(), size.1);
            drawn(&p)
        };
        let full: Vec<_> = (0..5).flat_map(|y| (0..3).map(move |x| (x, y))).collect();
        assert_eq!(ellipse(0, (3, 4), 2), full);
        assert_eq!(ellipse(0, (3, 4), 0), []);
        assert_eq!(ellipse(1, (-4, 3), 2), []);
    }

    // ---- Center/Position freeze semantics (C++ writeInt32) ----------------

    use ad_core_rs::plugin::runtime::{ParamChangeValue, PluginParamSnapshot, SideParam};

    /// Write one int32 to overlay 0 as the port does: validate, store the
    /// written value and the side params. Returns the side params.
    fn drive(
        p: &OverlayProcessor,
        base: &mut asyn_rs::port::PortDriverBase,
        reason: usize,
        value: i32,
    ) -> Vec<(usize, i32)> {
        use ad_core_rs::plugin::runtime::WriteVerdict;
        let out = p.validate_int32(
            reason,
            0,
            value,
            WriteVerdict::Accept(value),
            &mut ad_core_rs::plugin::runtime::WritePort::new(base),
        );
        assert!(out.settled);
        base.set_int32_param(reason, 0, out.verdict.kept()).unwrap();
        out.side
            .into_iter()
            .map(|s| match s {
                SideParam::Int32 { reason, value, .. } => {
                    base.set_int32_param(reason, 0, value).unwrap();
                    (reason, value)
                }
                other => panic!("unexpected {other:?}"),
            })
            .collect()
    }

    fn setup_processor() -> (
        OverlayProcessor,
        asyn_rs::port::PortDriverBase,
        OverlayParamIndices,
    ) {
        let mut p = OverlayProcessor::new(8, vec![]);
        let mut base = asyn_rs::port::PortDriverBase::new(
            "OV_TEST",
            8,
            asyn_rs::port::PortFlags::default(),
            asyn_rs::port::InterfaceMask::UNMIGRATED_ALL,
            asyn_rs::port::InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
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
        (p, base, params)
    }

    /// NDPluginOverlay.cpp:404-405 adds asynGenericPointer to both masks,
    /// already in asynNDArrayDriver's; so the overlay port takes array and
    /// NDArray I/O Intr clients and, as getInterruptPvt, refuses Float64Array.
    #[test]
    fn overlay_port_takes_cs_interrupt_clients() {
        use asyn_rs::interfaces::InterfaceType;
        let (handle, _jh) = ad_core_rs::plugin::runtime::create_plugin_runtime_multi_addr(
            &asyn_rs::asyn_record::claim_port("OVMASK").unwrap(),
            OverlayProcessor::new(2, vec![]),
            1_000_000,
            1,
            "",
            Arc::new(ad_core_rs::plugin::wiring::WiringRegistry::new()),
            2,
            ad_core_rs::plugin::runtime::DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle();
        let registers = |iface| {
            port.interrupts()
                .register_interrupt_user(
                    asyn_rs::interrupt::InterruptFilter {
                        iface: Some(iface),
                        ..Default::default()
                    },
                    0,
                )
                .is_ok()
        };
        assert!(registers(InterfaceType::Int32Array));
        assert!(registers(InterfaceType::GenericPointer));
        assert!(!registers(InterfaceType::Float64Array));
    }

    #[test]
    fn a_slot_exists_for_every_address_below_max_overlays_and_none_above() {
        // NDOverlayConfigure's maxOverlays sizes the slots; the port has the
        // same number of addresses, so the last overlay is at maxOverlays-1
        // and an address past that reaches no slot.
        let mut p = OverlayProcessor::new(3, vec![]);
        let mut base = asyn_rs::port::PortDriverBase::new(
            "OV_MAX",
            3,
            asyn_rs::port::PortFlags::default(),
            asyn_rs::port::InterfaceMask::UNMIGRATED_ALL,
            asyn_rs::port::InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        p.register_params(&mut base).unwrap();
        let use_overlay = p.params.use_overlay.unwrap();
        for (addr, want_slots) in [(2, 3), (3, 3)] {
            let snap = PluginParamSnapshot {
                enable_callbacks: true,
                reason: use_overlay,
                addr,
                value: ParamChangeValue::Int32(1),
            };
            p.on_param_change(use_overlay, &snap);
            assert_eq!(p.slots.lock().len(), want_slots);
        }
        assert!(p.slots.lock()[2].use_overlay, "address 2 is the third slot");
        assert_eq!(p.build_active_overlays().len(), 1);
        assert_eq!(
            OverlayProcessor::new(0, vec![]).slots.lock().len(),
            1,
            "address 0 always exists"
        );
    }

    /// NDPluginOverlay.cpp:484-488 and :490-494: PositionX freezes, and a
    /// SizeX write then moves CenterX inside the same write.
    #[test]
    fn test_freeze_position_then_resize_moves_center() {
        let (p, mut base, idx) = setup_processor();
        let (px, cx, sx) = (
            idx.position_x.unwrap(),
            idx.center_x.unwrap(),
            idx.size_x.unwrap(),
        );
        drive(&p, &mut base, sx, 20);
        assert_eq!(drive(&p, &mut base, px, 100), [(cx, 110)]);
        assert_eq!(drive(&p, &mut base, sx, 40), [(cx, 120)]);
        assert_eq!(p.slots.lock()[0].position_x, 100);
        assert_eq!(p.slots.lock()[0].size_x, 40);
    }

    /// :476-479 and :495-497: CenterX unfreezes, and a SizeX write then
    /// moves PositionX, negative as C's int param can be.
    #[test]
    fn test_freeze_center_then_resize_moves_position() {
        let (p, mut base, idx) = setup_processor();
        let (px, cx, sx) = (
            idx.position_x.unwrap(),
            idx.center_x.unwrap(),
            idx.size_x.unwrap(),
        );
        drive(&p, &mut base, sx, 20);
        assert_eq!(drive(&p, &mut base, cx, 200), [(px, 190)]);
        assert_eq!(drive(&p, &mut base, sx, 60), [(px, 170)]);
        assert_eq!(p.slots.lock()[0].position_x, 170);
        assert_eq!(drive(&p, &mut base, sx, 500), [(px, -50)]);
        assert_eq!(p.slots.lock()[0].position_x, -50);
    }

    #[test]
    fn test_freeze_y_axis_independent() {
        let (p, mut base, idx) = setup_processor();
        drive(&p, &mut base, idx.size_y.unwrap(), 10);
        drive(&p, &mut base, idx.center_y.unwrap(), 50);
        drive(&p, &mut base, idx.size_x.unwrap(), 10);
        drive(&p, &mut base, idx.position_x.unwrap(), 5);
        assert!(p.slots.lock()[0].freeze_position_x);
        assert!(!p.slots.lock()[0].freeze_position_y);
    }

    /// The overlay timestamp is C's epicsTimeToStrftime: the template
    /// default `%03f` is a 3-digit fraction, and the epoch stamp an NDArray
    /// starts with is `<undefined>`.
    #[test]
    fn overlay_timestamp_is_epics_time_to_strftime() {
        let ts = ad_core_rs::timestamp::EpicsTimestamp {
            sec: 1000,
            nsec: 250_000_000,
        };
        assert_eq!(format_epics_time(ts, "%03f"), "250");
        assert_eq!(format_epics_time(ts, "100%%"), "100%");
        let unset = ad_core_rs::timestamp::EpicsTimestamp::default();
        assert_eq!(format_epics_time(unset, "%Y"), "<undefined>");
    }

    #[test]
    fn overlay_output_comes_from_the_pool_and_is_reused() {
        let pool = pool();
        let mut arr = make_8x8();
        if let NDDataBuffer::U8(v) = &mut arr.data {
            v[0] = 7;
        }
        let overlays = vec![OverlayDef {
            shape: OverlayShape::Rectangle {
                x: 1,
                y: 1,
                width: 3,
                height: 3,
            },
            draw_mode: DrawMode::Set,
            color: [0, 255, 0],
            width_x: 1,
            width_y: 1,
        }];

        let first = draw_overlays(&pool, &arr, &overlays).unwrap();
        assert_eq!(first.pool_id(), pool.id());
        let NDDataBuffer::U8(v) = &first.data else {
            panic!("expected U8 buffer");
        };
        assert_eq!(v[0], 7, "the copy carries the source pixels");
        assert_eq!(v[8 + 1], 255, "the overlay is drawn on the copy");
        let ptr = v.as_ptr();
        drop(first);

        let second = draw_overlays(&pool, &arr, &overlays).unwrap();
        let NDDataBuffer::U8(v) = &second.data else {
            panic!("expected U8 buffer");
        };
        assert_eq!(v.as_ptr(), ptr, "the second frame reuses the freed buffer");
        assert_eq!(pool.num_alloc_buffers(), 1);
    }
}
