use crate::error::{ADError, ADResult};
use crate::ndarray::{NDArray, NDDataBuffer};

use crate::color_layout::ColorLayout;
use crate::pixel_cast::{PixelCast, with_buffer};

/// Crop a region of interest from the source array.
/// Data type and color mode are preserved. ROI parameters are clamped
/// to source dimensions so the function never panics on out-of-bounds input.
pub fn crop_roi(
    source: &NDArray,
    src_layout: &ColorLayout,
    min_x: usize,
    min_y: usize,
    size_x: usize,
    size_y: usize,
) -> ADResult<NDArray> {
    // Reject compressed data
    if source.codec.is_some() {
        return Err(ADError::UnsupportedConversion(
            "crop_roi: cannot operate on compressed (codec) data".into(),
        ));
    }

    let source_data = &source.data;

    // Clamp ROI parameters to source dimensions
    let min_x = min_x.min(src_layout.size_x);
    let min_y = min_y.min(src_layout.size_y);
    let size_x = size_x.min(src_layout.size_x.saturating_sub(min_x));
    let size_y = size_y.min(src_layout.size_y.saturating_sub(min_y));

    if size_x == 0 || size_y == 0 {
        let dst_layout = ColorLayout {
            color_mode: src_layout.color_mode,
            size_x: 0,
            size_y: 0,
        };
        let dims = dst_layout.make_dims();
        return Ok(NDArray::new(dims, source_data.data_type()));
    }

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
        return Ok(result);
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

    // Update output dims to track cumulative offset from source
    let (x_dim, y_dim) = match src_layout.color_mode {
        crate::driver::ColorMode::Mono => (0, 1),
        crate::driver::ColorMode::RGB1 => (1, 2),
        crate::driver::ColorMode::RGB2 => (0, 2),
        crate::driver::ColorMode::RGB3 => (0, 1),
        _ => (0, 1),
    };
    if x_dim < result.dims.len() {
        result.dims[x_dim].offset = source.dims.get(x_dim).map(|d| d.offset).unwrap_or(0) + min_x;
    }
    if y_dim < result.dims.len() {
        result.dims[y_dim].offset = source.dims.get(y_dim).map(|d| d.offset).unwrap_or(0) + min_y;
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::ColorMode;
    use crate::ndarray::NDDimension;

    fn make_mono_array(size_x: usize, size_y: usize, data: NDDataBuffer) -> NDArray {
        let dims = vec![NDDimension::new(size_x), NDDimension::new(size_y)];
        let mut arr = NDArray::new(dims, data.data_type());
        arr.data = data;
        arr
    }

    fn make_rgb1_array(size_x: usize, size_y: usize, data: NDDataBuffer) -> NDArray {
        let dims = vec![
            NDDimension::new(3),
            NDDimension::new(size_x),
            NDDimension::new(size_y),
        ];
        let mut arr = NDArray::new(dims, data.data_type());
        arr.data = data;
        arr
    }

    #[test]
    fn test_crop_identity_mono() {
        let layout = ColorLayout {
            color_mode: ColorMode::Mono,
            size_x: 4,
            size_y: 3,
        };
        let data: Vec<f64> = (0..12).map(|x| x as f64).collect();
        let arr = make_mono_array(4, 3, NDDataBuffer::F64(data.clone()));
        let result = crop_roi(&arr, &layout, 0, 0, 4, 3).unwrap();
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
        let data: Vec<f64> = (0..16).map(|x| x as f64).collect();
        let arr = make_mono_array(4, 4, NDDataBuffer::F64(data));
        let result = crop_roi(&arr, &layout, 1, 1, 2, 2).unwrap();
        if let NDDataBuffer::F64(v) = &result.data {
            assert_eq!(v, &[5.0, 6.0, 9.0, 10.0]);
        }
        assert_eq!(result.dims.len(), 2);
        assert_eq!(result.dims[0].size, 2);
        assert_eq!(result.dims[1].size, 2);
        // Check cumulative offset
        assert_eq!(result.dims[0].offset, 1);
        assert_eq!(result.dims[1].offset, 1);
    }

    #[test]
    fn test_crop_identity_rgb1() {
        let layout = ColorLayout {
            color_mode: ColorMode::RGB1,
            size_x: 2,
            size_y: 2,
        };
        let data: Vec<u8> = (0..12).collect();
        let arr = make_rgb1_array(2, 2, NDDataBuffer::U8(data.clone()));
        let result = crop_roi(&arr, &layout, 0, 0, 2, 2).unwrap();
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
        let mut data = vec![0u8; 27];
        for y in 0..3 {
            for x in 0..3 {
                let base = (y * 3 + x) * 3;
                data[base] = (x * 10 + y) as u8;
                data[base + 1] = (x * 10 + y + 100) as u8;
                data[base + 2] = (x * 10 + y + 200) as u8;
            }
        }
        let arr = make_rgb1_array(3, 3, NDDataBuffer::U8(data));
        let result = crop_roi(&arr, &layout, 1, 1, 2, 2).unwrap();
        if let NDDataBuffer::U8(v) = &result.data {
            assert_eq!(v.len(), 12);
            assert_eq!(v[0], 11);
            assert_eq!(v[1], 111);
        }
        assert_eq!(result.dims.len(), 3);
        assert_eq!(result.dims[0].size, 3);
        assert_eq!(result.dims[1].size, 2);
        assert_eq!(result.dims[2].size, 2);
    }

    #[test]
    fn test_crop_bounds_clamping() {
        let layout = ColorLayout {
            color_mode: ColorMode::Mono,
            size_x: 4,
            size_y: 4,
        };
        let data: Vec<u8> = (0..16).collect();
        let arr = make_mono_array(4, 4, NDDataBuffer::U8(data));
        let result = crop_roi(&arr, &layout, 2, 2, 10, 10).unwrap();
        assert_eq!(result.dims[0].size, 2);
        assert_eq!(result.dims[1].size, 2);
    }

    #[test]
    fn test_crop_rejects_compressed() {
        let layout = ColorLayout {
            color_mode: ColorMode::Mono,
            size_x: 4,
            size_y: 4,
        };
        let mut arr = make_mono_array(4, 4, NDDataBuffer::U8(vec![0; 16]));
        arr.codec = Some(crate::codec::Codec {
            name: crate::codec::CodecName::LZ4,
            compressed_size: 10,
            level: 0,
            shuffle: 0,
            compressor: 0,
            original_data_type: crate::ndarray::NDDataType::UInt8,
        });
        assert!(crop_roi(&arr, &layout, 0, 0, 4, 4).is_err());
    }
}
