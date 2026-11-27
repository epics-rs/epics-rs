//! A 1-D FFT frame the size of the last runs in the buffers the last one
//! grew: it allocates its waveforms and its output, one `f64` per padded
//! sample at most, and no complex row or plan scratch, which is twice that.

use std::sync::Arc;

use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDimension};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::NDPluginProcess;
use ad_plugins_rs::fft::{FFTConfig, FFTDirection, FFTProcessor};
use alloc_count::thread;

const N: usize = 1 << 16;

#[test]
fn a_second_1d_frame_allocates_no_complex_buffer() {
    let frame = Arc::new(NDArray::with_data(
        vec![NDDimension::new(N)],
        NDDataBuffer::from_vec::<u16>((0..N).map(|i| (i * 7919 % 4093) as u16).collect()),
    ));
    let pool = NDArrayPool::new(0);
    for direction in [FFTDirection::Forward, FFTDirection::Inverse] {
        let fft = FFTProcessor::with_config(FFTConfig {
            direction,
            ..FFTConfig::default()
        });
        drop(fft.process_array(&frame, &pool));
        let (second, counts) = thread::counted(|| fft.process_array(&frame, &pool));
        drop(second);
        let largest = counts.largest;
        assert!(
            largest <= N * size_of::<f64>(),
            "{direction:?}: a {largest}-byte allocation"
        );
    }
}
