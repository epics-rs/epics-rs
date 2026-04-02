use crate::ndarray::{NDArray, NDDataBuffer};

use crate::color_layout::ColorLayout;
use crate::pixel_cast::{PixelCast, with_buffer};

/// Crop a region of interest from the source array.
/// v1: no binning or reverse. Data type and color mode are preserved.
pub fn crop_roi(
    source_data: &NDDataBuffer,
    src_layout: &ColorLayout,
    min_x: usize,
    min_y: usize,
    size_x: usize,
    size_y: usize,
) -> NDArray {
    let dst_layout = ColorLayout {
        color_mode: src_layout.color_mode,
        size_x,
        size_y,
    };
    let dims = dst_layout.make_dims();
    let data_type = source_data.data_type();

    let mut result = NDArray::new(dims, data_type);

    // Fast path: full-frame copy (no actual crop needed)
    if min_x == 0 && min_y == 0 && size_x == src_layout.size_x && size_y == src_layout.size_y {
        result.data = source_data.clone();
        return result;
    }

    let num_elements = dst_layout.num_elements();
    with_buffer!(source_data, |src_v| {
        let mut dst_data = NDDataBuffer::zeros(data_type, num_elements);
        crate::pixel_cast::with_buffer_mut!(&mut dst_data, |dst_v| {
            for y in 0..size_y {
                let src_y = min_y + y;
                for x in 0..size_x {
                    let src_x = min_x + x;
                    for ch in 0..src_layout.num_colors() {
                        let si = src_layout.index(src_x, src_y, ch);
                        let di = dst_layout.index(x, y, ch);
                        let val = PixelCast::to_f64(src_v[si]);
                        dst_v[di] = PixelCast::from_f64(val);
                    }
                }
            }
        });
        result.data = dst_data;
    });

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::ColorMode;

    #[test]
    fn test_crop_identity_mono() {
        let layout = ColorLayout {
            color_mode: ColorMode::Mono,
            size_x: 4,
            size_y: 3,
        };
        let data: Vec<f64> = (0..12).map(|x| x as f64).collect();
        let buf = NDDataBuffer::F64(data.clone());
        let result = crop_roi(&buf, &layout, 0, 0, 4, 3);
        if let NDDataBuffer::F64(v) = &result.data {
            assert_eq!(v, &data);
        }
    }

    #[test]
    fn test_crop_subregion_mono() {
        let layout = ColorLayout {
            color_mode: ColorMode::Mono,
            size_x: 4,
            size_y: 4,
        };
        // 0  1  2  3
        // 4  5  6  7
        // 8  9  10 11
        // 12 13 14 15
        let data: Vec<f64> = (0..16).map(|x| x as f64).collect();
        let buf = NDDataBuffer::F64(data);
        let result = crop_roi(&buf, &layout, 1, 1, 2, 2);
        if let NDDataBuffer::F64(v) = &result.data {
            assert_eq!(v, &[5.0, 6.0, 9.0, 10.0]);
        }
        assert_eq!(result.dims.len(), 2);
        assert_eq!(result.dims[0].size, 2); // size_x
        assert_eq!(result.dims[1].size, 2); // size_y
    }

    #[test]
    fn test_crop_identity_rgb1() {
        let layout = ColorLayout {
            color_mode: ColorMode::RGB1,
            size_x: 2,
            size_y: 2,
        };
        // 2x2 RGB1: 12 elements
        let data: Vec<u8> = (0..12).collect();
        let buf = NDDataBuffer::U8(data.clone());
        let result = crop_roi(&buf, &layout, 0, 0, 2, 2);
        if let NDDataBuffer::U8(v) = &result.data {
            assert_eq!(v, &data);
        }
    }

    #[test]
    fn test_crop_subregion_rgb1() {
        let layout = ColorLayout {
            color_mode: ColorMode::RGB1,
            size_x: 3,
            size_y: 3,
        };
        // 3x3 RGB1: 27 elements
        // pixel(x,y): R=idx*3, G=idx*3+1, B=idx*3+2 where idx=(y*3+x)
        let mut data = vec![0u8; 27];
        for y in 0..3 {
            for x in 0..3 {
                let base = (y * 3 + x) * 3;
                data[base] = (x * 10 + y) as u8; // R
                data[base + 1] = (x * 10 + y + 100) as u8; // G
                data[base + 2] = (x * 10 + y + 200) as u8; // B (wraps for u8)
            }
        }
        let buf = NDDataBuffer::U8(data);
        let result = crop_roi(&buf, &layout, 1, 1, 2, 2);
        if let NDDataBuffer::U8(v) = &result.data {
            // Expected: pixels (1,1), (2,1), (1,2), (2,2)
            // (1,1): R=11, G=111, B=211
            // (2,1): R=21, G=121, B=221
            // (1,2): R=12, G=112, B=212
            // (2,2): R=22, G=122, B=222
            assert_eq!(v.len(), 12);
            assert_eq!(v[0], 11); // (0,0) in dst = (1,1) in src, R
            assert_eq!(v[1], 111); // G
        }
        assert_eq!(result.dims.len(), 3);
        assert_eq!(result.dims[0].size, 3); // color
        assert_eq!(result.dims[1].size, 2); // size_x
        assert_eq!(result.dims[2].size, 2); // size_y
    }
}
