//! A software prefetch of an object's cache lines, ahead of its first touch.

const LINE: usize = 64;

/// The lines of the `len` bytes at `start`, as the first and a count: every
/// line of `[start & !63, start + len)`, so an unaligned object's last line
/// is covered too.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64", test))]
#[inline(always)]
fn span(start: usize, len: usize) -> (usize, usize) {
    let first = start & !(LINE - 1);
    (first, (start + len - first).div_ceil(LINE))
}

/// Hint every cache line of the `T` at `p` into L1; its constant size lets
/// the hints unroll. Nothing is read: a prefetch never faults.
#[inline(always)]
pub(crate) fn prefetch_val<T>(p: *const T) {
    prefetch_span(p.cast(), size_of::<T>(), align_of::<T>());
}

/// Hint every cache line of the `size` bytes at `p`, which is aligned to
/// `align`; both constant at the call, so the hints unroll.
#[inline(always)]
pub(crate) fn prefetch_span(p: *const u8, size: usize, align: usize) {
    points(p.addr(), size, align, |a| prefetch_line(p.with_addr(a)));
}

/// One address in each line of `[start, start + size)` for a `start` aligned
/// to `align`: a point every line from `start`, then the last byte unless the
/// alignment already puts it in the last point's line.
#[inline(always)]
fn points(start: usize, size: usize, align: usize, mut f: impl FnMut(usize)) {
    for k in 0..size.div_ceil(LINE) {
        f(start + k * LINE);
    }
    if size % LINE != 1 && align % LINE != 0 {
        f(start + size - 1);
    }
}

/// An address range a walk prefetches for a record whose hold it does not
/// have: written under the hold, read back as a plain integer and never
/// dereferenced, so a stale one costs one wasted hint.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[derive(Default)]
pub(crate) struct Hint(core::sync::atomic::AtomicU64);

/// No prefetch to feed on this target, so nothing to keep.
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
#[derive(Default)]
pub(crate) struct Hint;

/// Both prefetching targets keep user addresses below bit 48; the line count
/// rides above it, so a hint is one word, decoded once when it is named.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
mod word {
    const ADDR_BITS: u32 = 48;
    const ADDR: u64 = (1 << ADDR_BITS) - 1;

    #[inline(always)]
    pub(super) fn pack(first: usize, lines: usize) -> u64 {
        (first as u64 & ADDR) | ((lines.min(0xffff) as u64) << ADDR_BITS)
    }

    #[inline(always)]
    pub(super) fn unpack(word: u64) -> (usize, usize) {
        ((word & ADDR) as usize, (word >> ADDR_BITS) as usize)
    }
}

impl Hint {
    /// Name the lines of the `len` bytes at `p`.
    #[inline(always)]
    pub(crate) fn set(&self, p: *const u8, len: usize) {
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        {
            let (first, lines) = span(p.addr(), len);
            self.0.store(
                word::pack(first, lines),
                core::sync::atomic::Ordering::Relaxed,
            );
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        let _ = (p, len);
    }

    /// Name `a`'s value and the reference counts `Arc` keeps just before it.
    #[inline(always)]
    pub(crate) fn set_arc<T>(&self, a: &std::sync::Arc<T>) {
        const COUNTS: usize = 2 * std::mem::size_of::<usize>();
        self.set(
            std::sync::Arc::as_ptr(a).cast::<u8>().wrapping_sub(COUNTS),
            std::mem::size_of::<T>() + COUNTS,
        );
    }

    /// Name nothing.
    #[inline(always)]
    pub(crate) fn clear(&self) {
        self.set(core::ptr::null(), 0);
    }

    /// The `(first line, line count)` last named.
    #[cfg(all(test, any(target_arch = "x86_64", target_arch = "aarch64")))]
    pub(crate) fn named(&self) -> (usize, usize) {
        word::unpack(self.0.load(core::sync::atomic::Ordering::Relaxed))
    }

    /// Hint the lines last named into L1. Nothing is read, so they may have
    /// been freed since.
    #[inline(always)]
    pub(crate) fn prefetch(&self) {
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        {
            let (first, lines) = word::unpack(self.0.load(core::sync::atomic::Ordering::Relaxed));
            for k in 0..lines {
                prefetch_line(core::ptr::without_provenance(first + k * LINE));
            }
        }
    }
}

#[inline(always)]
fn prefetch_line(p: *const u8) {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: a prefetch hint has no architectural effect and cannot fault.
    unsafe {
        core::arch::x86_64::_mm_prefetch::<{ core::arch::x86_64::_MM_HINT_T0 }>(p.cast())
    };
    #[cfg(target_arch = "aarch64")]
    // SAFETY: as above; `prfm` neither faults nor writes.
    unsafe {
        core::arch::asm!("prfm pldl1keep, [{0}]", in(reg) p, options(nostack, preserves_flags, readonly))
    };
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    let _ = p;
}

#[cfg(test)]
mod tests {
    fn set(start: usize, len: usize) -> Vec<usize> {
        let (first, lines) = super::span(start, len);
        (0..lines).map(|k| first + k * 64).collect()
    }

    #[test]
    fn aligned_start_end_on_a_line() {
        assert_eq!(set(0x1000, 128), [0x1000, 0x1040]);
    }

    #[test]
    fn aligned_start_end_mid_line() {
        assert_eq!(set(0x1000, 129), [0x1000, 0x1040, 0x1080]);
    }

    #[test]
    fn unaligned_start_end_on_a_line() {
        assert_eq!(set(0x1010, 0x70), [0x1000, 0x1040]);
    }

    #[test]
    fn unaligned_start_end_mid_line() {
        assert_eq!(set(0x1010, 0x80), [0x1000, 0x1040, 0x1080]);
    }

    #[test]
    fn short_object_straddling_two_lines() {
        assert_eq!(set(0x103c, 8), [0x1000, 0x1040]);
    }

    #[test]
    fn short_object_inside_one_line() {
        assert_eq!(set(0x1008, 8), [0x1000]);
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    #[test]
    fn a_hint_keeps_its_address_and_length() {
        use super::word::{pack, unpack};
        assert_eq!(unpack(pack(0x7fff_1234_5678, 200)), (0x7fff_1234_5678, 200));
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    #[test]
    fn a_cleared_hint_names_no_line() {
        let h = super::Hint::default();
        h.set(0x1010 as *const u8, 8);
        h.clear();
        assert_eq!(h.named().1, 0);
    }

    /// Every start alignment against every size up to three lines: the fixed
    /// points name exactly the lines [`span`](super::span) does.
    #[test]
    fn fixed_points_name_the_same_lines() {
        for align in [1, 8, 16, 64] {
            for size in (1..=192).filter(|n| n % align == 0) {
                for start in (0x1000..0x1040).step_by(align) {
                    let mut got = Vec::new();
                    super::points(start, size, align, |a| got.push(a & !63));
                    got.dedup();
                    assert_eq!(got, set(start, size), "start {start:#x} size {size}");
                }
            }
        }
    }

    #[test]
    fn prefetch_of_a_live_object_runs() {
        let v = [0u8; 200];
        let h = super::Hint::default();
        h.set(v[3..].as_ptr(), 197);
        h.prefetch();
        super::prefetch_val(&v);
    }
}
