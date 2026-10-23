use crate::error::{ADError, ADResult};
use crate::ndarray::NDArray;
use crate::ndarray_pool::NDArrayPool;

use crate::color_layout::ColorLayout;
use crate::convert::output_dims;
use crate::driver::ColorMode;

/// Crop a region of interest from the source array.
/// Data type and color mode are preserved. ROI parameters are clamped
/// to source dimensions so the function never panics on out-of-bounds input.
/// The crop is allocated from `pool` and copied into the buffer it hands
/// back, as C's `NDArrayPool::convert` fills the array its `alloc` returns
/// (NDArrayPool.cpp:647-716).
pub fn crop_roi(
    pool: &NDArrayPool,
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
        return pool.alloc(dims, source_data.data_type());
    }

    let dst_layout = ColorLayout {
        color_mode: src_layout.color_mode,
        size_x,
        size_y,
    };
    let data_type = source_data.data_type();

    // Fast path: full-frame copy (no actual crop needed). The dims are the
    // plain ones, as C convert's same-type copy returns before its dims
    // epilogue (NDArrayPool.cpp:662-672).
    if min_x == 0 && min_y == 0 && size_x == src_layout.size_x && size_y == src_layout.size_y {
        let mut result = pool.alloc(dst_layout.make_dims(), data_type)?;
        result.data.copy_from(source_data);
        return Ok(result);
    }

    // `output_dims` adds the source's offsets and multiplies its binning in
    // on every dim, C convert's epilogue (NDArrayPool.cpp:719-723).
    let (x_dim, y_dim) = match src_layout.color_mode {
        crate::driver::ColorMode::Mono => (0, 1),
        crate::driver::ColorMode::RGB1 => (1, 2),
        crate::driver::ColorMode::RGB2 => (0, 2),
        crate::driver::ColorMode::RGB3 => (0, 1),
        _ => (0, 1),
    };
    let mut dims_out = dst_layout.make_dims();
    dims_out[x_dim].offset = min_x;
    dims_out[y_dim].offset = min_y;
    let mut result = pool.alloc(output_dims(source, &dims_out)?, data_type)?;

    // One run per row of the region: all three colours of it in RGB1, one
    // colour plane's row in RGB2 and RGB3. The output is the source's data
    // type, so a run is C convertDim's same-type copy, byte for byte.
    let (planes, run) = match src_layout.color_mode {
        ColorMode::RGB1 => (1, 3 * size_x),
        ColorMode::RGB2 | ColorMode::RGB3 => (3, size_x),
        _ => (1, size_x),
    };
    let es = data_type.element_size();
    let src = source_data.as_u8_slice();
    let dst = result.data.as_u8_slice_mut();
    for y in 0..size_y {
        for c in 0..planes {
            let s = src_layout.index(min_x, min_y + y, c) * es;
            let d = dst_layout.index(0, y, c) * es;
            dst[d..d + run * es].copy_from_slice(&src[s..s + run * es]);
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ndarray::{NDDataBuffer, NDDataType, NDDataView, NDDimension};

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
        let arr = make_mono_array(4, 3, NDDataBuffer::from_vec::<f64>(data.clone()));
        let result = crop_roi(&NDArrayPool::new(0), &arr, &layout, 0, 0, 4, 3).unwrap();
        if let NDDataView::F64(v) = result.data.view() {
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
        let arr = make_mono_array(4, 4, NDDataBuffer::from_vec::<f64>(data));
        let result = crop_roi(&NDArrayPool::new(0), &arr, &layout, 1, 1, 2, 2).unwrap();
        if let NDDataView::F64(v) = result.data.view() {
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
        let arr = make_rgb1_array(2, 2, NDDataBuffer::from_vec::<u8>(data.clone()));
        let result = crop_roi(&NDArrayPool::new(0), &arr, &layout, 0, 0, 2, 2).unwrap();
        if let NDDataView::U8(v) = result.data.view() {
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
        let arr = make_rgb1_array(3, 3, NDDataBuffer::from_vec::<u8>(data));
        let result = crop_roi(&NDArrayPool::new(0), &arr, &layout, 1, 1, 2, 2).unwrap();
        if let NDDataView::U8(v) = result.data.view() {
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
        let arr = make_mono_array(4, 4, NDDataBuffer::from_vec::<u8>(data));
        let result = crop_roi(&NDArrayPool::new(0), &arr, &layout, 2, 2, 10, 10).unwrap();
        assert_eq!(result.dims[0].size, 2);
        assert_eq!(result.dims[1].size, 2);
    }

    /// Each colour mode's sub-region and full frame against a per-element
    /// walk of `ColorLayout::index`.
    #[test]
    fn crop_matches_an_element_walk_in_every_color_mode() {
        for mode in [
            ColorMode::Mono,
            ColorMode::RGB1,
            ColorMode::RGB2,
            ColorMode::RGB3,
        ] {
            let src_layout = ColorLayout {
                color_mode: mode,
                size_x: 5,
                size_y: 4,
            };
            let n = src_layout.num_elements() as u16;
            let mut src = NDArray::new(src_layout.make_dims(), NDDataType::UInt16);
            src.data = NDDataBuffer::from_vec::<u16>((0..n).collect());
            let pool = NDArrayPool::new(0);
            for (min_x, min_y, size_x, size_y) in [(1, 2, 3, 2), (2, 1, 3, 2), (0, 0, 5, 4)] {
                let dst_layout = ColorLayout {
                    color_mode: mode,
                    size_x,
                    size_y,
                };
                let mut want = vec![0u16; dst_layout.num_elements()];
                for y in 0..size_y {
                    for x in 0..size_x {
                        for ch in 0..src_layout.num_colors() {
                            want[dst_layout.index(x, y, ch)] =
                                src_layout.index(min_x + x, min_y + y, ch) as u16;
                        }
                    }
                }
                let out = crop_roi(&pool, &src, &src_layout, min_x, min_y, size_x, size_y).unwrap();
                let NDDataView::U16(got) = out.data.view() else {
                    panic!("{:?}", out.data.data_type());
                };
                assert_eq!(got, &want, "{mode:?} ({min_x},{min_y}) {size_x}x{size_y}");
            }
        }
    }

    /// C's convert fills the array `alloc` handed it (NDArrayPool.cpp:647,
    /// :671, :716), so a crop after a released one reuses its buffer.
    #[test]
    fn a_crop_fills_the_buffer_the_pool_reuses() {
        let layout = ColorLayout {
            color_mode: ColorMode::Mono,
            size_x: 4,
            size_y: 4,
        };
        let arr = make_mono_array(4, 4, NDDataBuffer::from_vec::<u16>((0..16).collect()));
        for (min_x, size_x) in [(0, 4), (1, 2)] {
            let pool = NDArrayPool::new(0);
            let first = crop_roi(&pool, &arr, &layout, min_x, 0, size_x, 4).unwrap();
            let ptr = first.data.as_u8_slice().as_ptr();
            drop(first);
            let second = crop_roi(&pool, &arr, &layout, min_x, 0, size_x, 4).unwrap();
            assert_eq!(second.data.as_u8_slice().as_ptr(), ptr, "size_x {size_x}");
        }
    }

    /// The full-frame crop fills the frame the pool charged for, not a
    /// longer source buffer.
    #[test]
    fn a_full_frame_crop_stops_at_the_frame() {
        let layout = ColorLayout {
            color_mode: ColorMode::Mono,
            size_x: 2,
            size_y: 2,
        };
        let arr = make_mono_array(2, 2, NDDataBuffer::from_vec::<u8>((1..=6).collect()));
        let pool = NDArrayPool::new(0);
        let out = crop_roi(&pool, &arr, &layout, 0, 0, 2, 2).unwrap();
        assert_eq!(out.data.as_slice::<u8>(), Some(&[1, 2, 3, 4][..]));
        assert_eq!(pool.allocated_bytes(), 4);
    }

    /// A crop copies elements as C's same-type `convertDim` does, so 64-bit
    /// values past 2^53 come through exactly.
    #[test]
    fn a_crop_copies_64_bit_values_exactly() {
        let layout = ColorLayout {
            color_mode: ColorMode::Mono,
            size_x: 2,
            size_y: 2,
        };
        let big = (1u64 << 53) + 1;
        let arr = make_mono_array(
            2,
            2,
            NDDataBuffer::from_vec::<u64>(vec![1, big, 3, u64::MAX]),
        );
        let out = crop_roi(&NDArrayPool::new(0), &arr, &layout, 1, 0, 1, 2).unwrap();
        let NDDataView::U64(v) = out.data.view() else {
            panic!("{:?}", out.data.data_type());
        };
        assert_eq!(v, &[big, u64::MAX]);
    }

    /// A crop of a binned, offset, reversed source carries them into every
    /// dim as C convert's epilogue (NDArrayPool.cpp:719-723); the full frame
    /// returns before it with plain dims, as C's same-type copy.
    #[test]
    fn a_crop_carries_the_source_binning() {
        let layout = ColorLayout {
            color_mode: ColorMode::RGB1,
            size_x: 4,
            size_y: 4,
        };
        let mut arr = make_rgb1_array(4, 4, NDDataBuffer::from_vec::<u8>(vec![0; 48]));
        for (d, (offset, binning)) in arr.dims.iter_mut().zip([(1, 1), (8, 2), (4, 3)]) {
            d.offset = offset;
            d.binning = binning;
        }
        arr.dims[2].reverse = true;
        let pool = NDArrayPool::new(0);

        let out = crop_roi(&pool, &arr, &layout, 1, 2, 2, 2).unwrap();
        let got: Vec<_> = out
            .dims
            .iter()
            .map(|d| (d.size, d.offset, d.binning, d.reverse))
            .collect();
        assert_eq!(got, [(3, 1, 1, false), (2, 9, 2, false), (2, 6, 3, true)]);

        let full = crop_roi(&pool, &arr, &layout, 0, 0, 4, 4).unwrap();
        assert!(
            full.dims
                .iter()
                .all(|d| (d.offset, d.binning, d.reverse) == (0, 1, false))
        );
    }

    #[test]
    fn test_crop_rejects_compressed() {
        let layout = ColorLayout {
            color_mode: ColorMode::Mono,
            size_x: 4,
            size_y: 4,
        };
        let mut arr = make_mono_array(4, 4, NDDataBuffer::from_vec::<u8>(vec![0; 16]));
        arr.codec = Some(crate::codec::Codec {
            name: crate::codec::CodecName::LZ4,
            compressed_size: 10,
            level: 0,
            shuffle: 0,
            compressor: 0,
            original_data_type: crate::ndarray::NDDataType::UInt8,
        });
        assert!(crop_roi(&NDArrayPool::new(0), &arr, &layout, 0, 0, 4, 4).is_err());
    }
}
