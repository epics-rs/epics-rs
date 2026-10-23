//! A 1-D FFT frame the size of the last runs in the buffers the last one
//! grew: it allocates its waveforms and its output, one `f64` per padded
//! sample at most, and no complex row or plan scratch, which is twice that.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDimension};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::NDPluginProcess;
use ad_plugins_rs::fft::{FFTConfig, FFTDirection, FFTProcessor};

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static LARGEST: Cell<usize> = const { Cell::new(0) };
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            LARGEST.with(|n| n.set(n.get().max(layout.size())));
        }
        // SAFETY: forwarded unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

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
        LARGEST.with(|n| n.set(0));
        COUNTING.with(|c| c.set(true));
        let second = fft.process_array(&frame, &pool);
        COUNTING.with(|c| c.set(false));
        drop(second);
        let largest = LARGEST.with(Cell::get);
        assert!(
            largest <= N * size_of::<f64>(),
            "{direction:?}: a {largest}-byte allocation"
        );
    }
}
