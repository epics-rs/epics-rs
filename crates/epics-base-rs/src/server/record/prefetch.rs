//! A software prefetch of an object's cache lines, ahead of its first touch.

const LINE: usize = 64;

/// The lines of the `len` bytes at `start`, as the first and a count: every
/// line from `start`'s to the last byte's, and none for an empty span.
#[cfg(test)]
fn span(start: usize, len: usize) -> (usize, usize) {
    let first = start & !(LINE - 1);
    match len.checked_sub(1) {
        Some(last) => (first, (start + last) / LINE - start / LINE + 1),
        None => (first, 0),
    }
}

/// The lines an object of `len` bytes aligned to `align` spans from its worst
/// start in a line. A hint names this many from its first line, so the walk's
/// prefetch loop runs a trip count fixed by the type, not by where the
/// allocator put the object: with an exact count, a longout body moving from
/// three lines to four moved the scan walk by 6 instr/cycle.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64", test))]
#[inline(always)]
fn cover(len: usize, align: usize) -> usize {
    match len.checked_sub(1) {
        Some(last) => (LINE - align.min(LINE) + last) / LINE + 1,
        None => 0,
    }
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
/// alignment already puts it in the last point's line. An empty object has
/// no last byte, so no point.
#[inline(always)]
fn points(start: usize, size: usize, align: usize, mut f: impl FnMut(usize)) {
    let Some(last) = size.checked_sub(1) else {
        return;
    };
    for k in 0..size.div_ceil(LINE) {
        f(start + k * LINE);
    }
    if last % LINE != 0 && align % LINE != 0 {
        f(start + last);
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
    /// Name the lines of the `len` bytes at `p`, which is aligned to `align`:
    /// [`cover`]'s count from `p`'s line, every line of the object and at
    /// most one past it.
    #[inline(always)]
    pub(crate) fn set(&self, p: *const u8, len: usize, align: usize) {
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        {
            self.0.store(
                word::pack(p.addr() & !(LINE - 1), cover(len, align)),
                core::sync::atomic::Ordering::Relaxed,
            );
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        let _ = (p, len, align);
    }

    /// Name `a`'s value and the reference counts `Arc` keeps just before it.
    #[inline(always)]
    pub(crate) fn set_arc<T>(&self, a: &std::sync::Arc<T>) {
        self.set_arc_body(std::sync::Arc::as_ptr(a));
    }

    /// [`Self::set_arc`] for a pinned `Arc`.
    #[inline(always)]
    pub(crate) fn set_pin_arc<T>(&self, a: &core::pin::Pin<std::sync::Arc<T>>) {
        self.set_arc_body(core::ptr::from_ref::<T>(a));
    }

    #[inline(always)]
    fn set_arc_body<T>(&self, body: *const T) {
        const COUNTS: usize = 2 * std::mem::size_of::<usize>();
        self.set(
            body.cast::<u8>().wrapping_sub(COUNTS),
            std::mem::size_of::<T>() + COUNTS,
            std::mem::align_of::<T>().max(std::mem::align_of::<usize>()),
        );
    }

    /// Name nothing.
    #[inline(always)]
    pub(crate) fn clear(&self) {
        self.set(core::ptr::null(), 0, 1);
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
            each_line(first, lines, |a| {
                prefetch_line(core::ptr::without_provenance(a))
            });
        }
    }
}

/// Call `f` with each of the `lines` lines from `at`. Most hints name nothing
/// (a record without monitors), so they leave first; the rest run in blocks
/// of four and a two- and one-line tail, because a counted loop's remainder
/// trip cost four instructions a line, more than the line [`cover`] adds.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64", test))]
#[inline(always)]
fn each_line(mut at: usize, lines: usize, mut f: impl FnMut(usize)) {
    if lines == 0 {
        return;
    }
    for _ in 0..lines / 4 {
        (0..4).for_each(|k| f(at + k * LINE));
        at += 4 * LINE;
    }
    if lines & 2 != 0 {
        (0..2).for_each(|k| f(at + k * LINE));
        at += 2 * LINE;
    }
    if lines & 1 != 0 {
        f(at);
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
        h.set(0x1010 as *const u8, 8, 8);
        h.clear();
        assert_eq!(h.named().1, 0);
    }

    /// An empty span, a zero-sized type's included, names no line, even at
    /// address 0 where its last byte would wrap.
    #[test]
    fn an_empty_span_names_no_line() {
        for start in [0, 0x1000, 0x1010] {
            assert_eq!(set(start, 0), [] as [usize; 0], "start {start:#x}");
            for align in [1, 8, 16, 64] {
                super::points(start, 0, align, |a| panic!("empty span named {a:#x}"));
            }
        }
        super::prefetch_val(core::ptr::null::<()>());
    }

    /// Every start alignment against every size up to three lines: the fixed
    /// points name exactly the lines [`span`](super::span) does.
    #[test]
    fn fixed_points_name_the_same_lines() {
        for align in [1, 8, 16, 64] {
            for size in (0..=192).filter(|n| n % align == 0) {
                for start in (0..0x40).chain(0x1000..0x1040).step_by(align) {
                    let mut got = Vec::new();
                    super::points(start, size, align, |a| got.push(a & !63));
                    got.dedup();
                    assert_eq!(got, set(start, size), "start {start:#x} size {size}");
                }
            }
        }
    }

    /// Every aligned start of every size up to three lines: a hint names the
    /// same count wherever the object sits, and that count covers its lines.
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    #[test]
    fn a_hint_names_the_same_count_at_every_start() {
        for align in [1, 8, 16, 64, 128] {
            for size in (0..=192).filter(|n| n % align == 0) {
                let mut counts = Vec::new();
                for start in (0x1000..0x1080).step_by(align) {
                    let h = super::Hint::default();
                    h.set(start as *const u8, size, align);
                    let (first, lines) = h.named();
                    let exact = set(start, size);
                    assert_eq!(first, start & !63, "start {start:#x} size {size}");
                    assert!(lines >= exact.len(), "start {start:#x} size {size}");
                    assert!(lines <= exact.len() + 1, "start {start:#x} size {size}");
                    counts.push(lines);
                }
                counts.dedup();
                assert_eq!(counts.len(), 1, "align {align} size {size}: {counts:?}");
            }
        }
    }

    #[test]
    fn each_line_visits_every_named_line_once() {
        for lines in 0..=13 {
            let mut got = Vec::new();
            super::each_line(0x1000, lines, |a| got.push(a));
            let want: Vec<usize> = (0..lines).map(|k| 0x1000 + k * 64).collect();
            assert_eq!(got, want, "lines {lines}");
        }
    }

    #[test]
    fn prefetch_of_a_live_object_runs() {
        let v = [0u8; 200];
        let h = super::Hint::default();
        h.set(v[3..].as_ptr(), 197, 1);
        h.prefetch();
        super::prefetch_val(&v);
    }
}
