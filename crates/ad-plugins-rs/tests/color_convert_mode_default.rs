//! R9-80 — the source color mode comes from the `ColorMode` attribute, never
//! from the dimensions.
//!
//! C `NDPluginColorConvert::convertColor` (NDPluginColorConvert.cpp:44,54-55):
//!
//! ```cpp
//! int colorMode=NDColorModeMono, bayerPattern=NDBayerRGGB;
//! ...
//! pAttribute = pArray->pAttributeList->find("ColorMode");
//! if (pAttribute) pAttribute->getValue(NDAttrInt32, &colorMode);
//! ```
//!
//! The initialiser IS the default: an array carrying no `ColorMode` attribute is
//! Mono no matter what its dimensions look like. C's Mono arm then refuses a
//! non-2-D shape outright (`case NDColorModeMono: if (pArray->ndims != 2) break;`,
//! :84) and the array falls through to the tail:
//!
//! ```cpp
//! if (!pArrayOut) pArrayOut = this->pNDArrayPool->copy(pArray, NULL, 1);
//! ```
//!
//! (:584) — the input is forwarded unchanged, and since `changedColorMode` stayed
//! 0 no ColorMode attribute is stamped on it (:589).
//!
//! The port guessed RGB1/RGB2/RGB3 from a size-3 dimension, so a plain 3-D mono
//! stack (e.g. 3 rows, or 3 frames) was converted as if it were color, and every
//! output pixel was wrong.

use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
use ad_core_rs::color::{NDBayerPattern, NDColorMode};
use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, ProcessResult};
use ad_plugins_rs::color_convert::{ColorConvertConfig, ColorConvertProcessor};

fn processor(target: NDColorMode) -> ColorConvertProcessor {
    ColorConvertProcessor::new(ColorConvertConfig {
        target_mode: target,
        bayer_pattern: NDBayerPattern::RGGB,
        false_color: 0,
    })
}

/// `[3, 4, 4]` u8, ascending samples, no ColorMode attribute.
fn stack_3x4x4() -> NDArray {
    let mut arr = NDArray::new(
        vec![
            NDDimension::new(3),
            NDDimension::new(4),
            NDDimension::new(4),
        ],
        NDDataType::UInt8,
    );
    arr.data = NDDataBuffer::U8((0..48u8).collect());
    arr
}

fn run(proc: &mut ColorConvertProcessor, arr: &NDArray) -> ProcessResult {
    proc.process_array(arr, &NDArrayPool::new(1 << 20))
}

/// The cited case: a 3-D array whose leading dimension happens to be 3. The port
/// called that RGB1; C calls it Mono.
#[test]
fn r9_80_a_leading_size_3_dimension_is_not_rgb1() {
    let arr = stack_3x4x4();
    assert_eq!(
        arr.info().color_mode,
        NDColorMode::Mono,
        "NDPluginColorConvert.cpp:44 — the attribute is absent, so the mode is the \
         initialiser NDColorModeMono"
    );
}

/// Mono source + Mono target is C's no-op: same mode, `pArrayOut` stays NULL, the
/// input is forwarded byte-for-byte. The port used to read the array as RGB1 and
/// emit a converted 4x4 luminance image instead.
#[test]
fn r9_80_a_3d_stack_is_forwarded_untouched_not_converted_as_rgb() {
    let arr = stack_3x4x4();
    let mut proc = processor(NDColorMode::Mono);

    let result = run(&mut proc, &arr);

    assert_eq!(result.output_arrays.len(), 1, "C never drops the frame");
    let out = &result.output_arrays[0];
    assert_eq!(
        out.dims.len(),
        3,
        "forwarded unchanged — an RGB1 read would have collapsed it to a 2-D image"
    );
    assert_eq!(out.dims[0].size, 3);
    assert_eq!(
        out.data.as_u8_slice(),
        arr.data.as_u8_slice(),
        "and the samples are the input's, not a luminance conversion of them"
    );
    assert!(
        out.attributes.get("ColorMode").is_none(),
        "changedColorMode stayed 0, so C stamps no ColorMode (NDPluginColorConvert.cpp:589)"
    );
}

/// The other two inference branches: a trailing (RGB3) or middle (RGB2) size-3
/// dimension was layout-shuffled into the RGB1 target. Under C the array is Mono,
/// the Mono arm rejects `ndims != 2` (:84), and the tail forwards the input
/// untouched (:584) — not shuffled, and not dropped either.
#[test]
fn r9_80_a_trailing_or_middle_size_3_dimension_is_not_rgb3_or_rgb2() {
    for dims in [
        vec![
            NDDimension::new(4),
            NDDimension::new(4),
            NDDimension::new(3),
        ],
        vec![
            NDDimension::new(4),
            NDDimension::new(3),
            NDDimension::new(4),
        ],
    ] {
        let mut arr = NDArray::new(dims.clone(), NDDataType::UInt8);
        arr.data = NDDataBuffer::U8((0..48u8).collect());
        let mut proc = processor(NDColorMode::RGB1);

        let result = run(&mut proc, &arr);

        assert_eq!(
            result.output_arrays.len(),
            1,
            "C:584 copies the input when no arm converted it — a frame is never lost"
        );
        let out = &result.output_arrays[0];
        assert_eq!(
            out.dims.iter().map(|d| d.size).collect::<Vec<_>>(),
            dims.iter().map(|d| d.size).collect::<Vec<_>>(),
            "forwarded with its shape intact — no RGB layout shuffle"
        );
        assert_eq!(
            out.data.as_u8_slice(),
            arr.data.as_u8_slice(),
            "and its samples intact"
        );
        assert!(
            out.attributes.get("ColorMode").is_none(),
            "no conversion happened, so changedColorMode stayed 0 (:589)"
        );
    }
}

/// C's tail is also what makes an unsupported *pair* harmless: `colorModeOut =
/// Bayer` matches no inner case, so `pArrayOut` stays NULL and :584 forwards the
/// input. The port returned an empty result — the frame vanished from the chain.
#[test]
fn r9_80_an_unsupported_target_forwards_the_frame_instead_of_dropping_it() {
    let mut arr = NDArray::new(
        vec![NDDimension::new(4), NDDimension::new(4)],
        NDDataType::UInt8,
    );
    arr.data = NDDataBuffer::U8((0..16u8).collect());
    let mut proc = processor(NDColorMode::Bayer);

    let result = run(&mut proc, &arr);

    assert_eq!(
        result.output_arrays.len(),
        1,
        "Mono -> Bayer has no arm in C; :584 still forwards the array"
    );
    assert_eq!(
        result.output_arrays[0].data.as_u8_slice(),
        arr.data.as_u8_slice(),
        "unchanged"
    );
}

/// With the attribute present the mode is honoured, and a genuine RGB1->Mono
/// conversion still happens — the fix removes the guess, not the feature.
#[test]
fn r9_80_the_colormode_attribute_still_drives_the_conversion() {
    let mut arr = stack_3x4x4();
    arr.attributes.add(NDAttribute::new_static(
        "ColorMode",
        "Color Mode",
        NDAttrSource::Driver,
        NDAttrValue::Int32(NDColorMode::RGB1 as i32),
    ));
    let mut proc = processor(NDColorMode::Mono);

    let result = run(&mut proc, &arr);

    assert_eq!(result.output_arrays.len(), 1);
    let out = &result.output_arrays[0];
    assert_eq!(
        out.dims.len(),
        2,
        "declared RGB1 [3,4,4] -> Mono [4,4] (NDPluginColorConvert.cpp:236-284)"
    );
    assert_eq!(out.dims[0].size, 4);
    assert_eq!(out.dims[1].size, 4);
}

/// A 2-D array with no attribute was already Mono under the old code; pin it so
/// the RGB1 target path (the common, working case) is not what the fix broke.
#[test]
fn r9_80_a_2d_mono_frame_still_converts_to_rgb1() {
    let mut arr = NDArray::new(
        vec![NDDimension::new(4), NDDimension::new(4)],
        NDDataType::UInt8,
    );
    arr.data = NDDataBuffer::U8((0..16u8).collect());
    let mut proc = processor(NDColorMode::RGB1);

    let result = run(&mut proc, &arr);

    assert_eq!(result.output_arrays.len(), 1);
    let out = &result.output_arrays[0];
    assert_eq!(out.dims.len(), 3);
    assert_eq!(out.dims[0].size, 3, "RGB1 is [3, X, Y]");
    assert_eq!(
        out.attributes
            .get("ColorMode")
            .and_then(|a| a.value.as_i64()),
        Some(NDColorMode::RGB1 as i64),
        "a real conversion sets changedColorMode, so the output IS stamped (:589)"
    );
}
