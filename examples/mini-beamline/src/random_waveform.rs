//! Random-waveform PVs for benchmarking bulk array transfers.
//!
//! Each record exposes a fixed-length `Vec<f64>` updated once per second
//! with deterministic per-record random data. The 10 individual waveform
//! records and the native PVA bundle share the same backing arrays.
//!
//! The bundle (`mini:wf:bundle`) is published *atomically* per cycle:
//! all 10 records refresh their slot, then the bundle re-emits with the
//! consistent set. CA per-record monitors stream independently as each
//! record finishes its scan, so CA and PVA observers see different
//! interleavings — that is intentional for the benchmark workload.
//!
//! Used by `examples/bench_flyscan_waveform.py` (in ophyd-epicsrs) to
//! reproduce a realistic flyscan workload (N waveforms × M elements
//! per scan step).
//!
//! IOC config (st.cmd):
//!   dbLoadRecords("$(MINI_BEAMLINE)/db/random_waveform.template",
//!                 "P=mini:wf,N=1,NELM=10000")
//!   ... × 10

#[cfg(feature = "ioc")]
pub mod ioc_support {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, OnceLock};

    use parking_lot::Mutex;
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    use epics_base_rs::error::CaResult;
    use epics_base_rs::server::device_support::{
        DeviceInitOutcome, DeviceReadOutcome, DeviceSupport, DeviceUdf, WriteCompletion,
    };
    use epics_base_rs::server::record::{Record, ScanType};
    use epics_base_rs::types::EpicsValue;
    use epics_bridge_rs::qsrv::PvaPvHandle;
    use epics_pva_rs::pvdata::{PvField, PvStructure, ScalarValue};

    /// Generate `len` deterministic-random doubles in `[0.0, 1.0)`
    /// from a 64-bit seed.
    fn random_doubles(len: usize, seed: u64) -> Vec<f64> {
        let mut rng = StdRng::seed_from_u64(seed);
        (0..len).map(|_| rng.random::<f64>()).collect()
    }

    fn trailing_index(name: &str) -> Option<usize> {
        let digits_start = name
            .char_indices()
            .rev()
            .find_map(|(idx, ch)| (!ch.is_ascii_digit()).then_some(idx + ch.len_utf8()))
            .unwrap_or(0);
        let digits = &name[digits_start..];
        if digits.is_empty() {
            None
        } else {
            digits.parse().ok()
        }
    }

    fn build_bundle_payload(datasets: &[Arc<[f64]>], len: usize, field_prefix: &str) -> PvField {
        let mut pv = PvStructure::new("mini:waveforms/v1");

        for (idx, data) in datasets.iter().enumerate() {
            pv.fields.push((
                format!("{field_prefix}{}", idx + 1),
                PvField::scalar_array_double(data.clone()),
            ));
        }
        pv.fields
            .push(("nelm".into(), PvField::Scalar(ScalarValue::Int(len as i32))));
        pv.fields
            .push(("version".into(), PvField::Scalar(ScalarValue::Int(1))));

        PvField::Structure(pv)
    }

    struct RandomWaveformShared {
        len: usize,
        datasets: Mutex<Vec<Arc<[f64]>>>,
        next_seed: AtomicU64,
        processed_mask: AtomicU64,
        expected_mask: u64,
        /// Set once via [`Self::set_field_prefix`] before the bundle is
        /// first published; immutable thereafter so subscribers' negotiated
        /// `FieldDesc` (which embeds the field names) stays stable for the
        /// lifetime of the PV. Mirrors pvxs `SharedPV::post`'s requirement
        /// that subsequent posts match the type of `open()`.
        field_prefix: OnceLock<String>,
        /// The single validating write owner for this PV (pvxs
        /// `SharedPV::post`). Created lazily on the first publish from the
        /// payload's descriptor — the field names depend on the runtime
        /// `field_prefix`, so the canonical shape is not known until then.
        /// Every later publish posts through this same handle, which rejects
        /// a payload whose shape drifted from the first and is the only path
        /// that may write `latest`/`subscribers`.
        handle: OnceLock<PvaPvHandle>,
    }

    impl RandomWaveformShared {
        fn new(len: usize, count: usize) -> Self {
            let datasets = (0..count)
                .map(|idx| Arc::<[f64]>::from(random_doubles(len, idx as u64)))
                .collect();
            let expected_mask = if count >= u64::BITS as usize {
                u64::MAX
            } else {
                (1u64 << count) - 1
            };
            Self {
                len,
                datasets: Mutex::new(datasets),
                next_seed: AtomicU64::new(count as u64),
                processed_mask: AtomicU64::new(0),
                expected_mask,
                field_prefix: OnceLock::new(),
                handle: OnceLock::new(),
            }
        }

        fn count(&self) -> usize {
            self.datasets.lock().len()
        }

        fn current_data(&self, index: usize) -> Option<Arc<[f64]>> {
            self.datasets.lock().get(index).cloned()
        }

        fn update_data(&self, index: usize) -> Option<Arc<[f64]>> {
            let seed = self.next_seed.fetch_add(1, Ordering::Relaxed);
            let data = Arc::<[f64]>::from(random_doubles(self.len, seed));
            let mut datasets = self.datasets.lock();
            let slot = datasets.get_mut(index)?;
            *slot = data.clone();
            Some(data)
        }

        /// Lock the bundle's field-name prefix. Calling this twice with
        /// different values is a programming error — the second prefix
        /// would change the wire `FieldDesc` mid-flight and break any
        /// monitors already negotiated with the first shape.
        fn set_field_prefix(&self, field_prefix: &str) {
            match self.field_prefix.set(field_prefix.to_string()) {
                Ok(()) => {}
                Err(_) => {
                    let existing = self.field_prefix.get().map(String::as_str).unwrap_or("");
                    if existing != field_prefix {
                        panic!(
                            "RandomWaveformShared::set_field_prefix called twice with different \
                             prefixes ({existing:?} then {field_prefix:?}); the bundle's \
                             FieldDesc must remain stable after the first publish"
                        );
                    }
                }
            }
        }

        fn field_prefix(&self) -> &str {
            self.field_prefix.get().map(String::as_str).unwrap_or("wf")
        }

        /// The latest published snapshot, read through the validating
        /// handle (`None` until the first publish creates it). Used by the
        /// unit tests to observe publish state without reaching into the
        /// handle's private fields.
        #[cfg(test)]
        fn current_value(&self) -> Option<PvField> {
            self.handle.get().and_then(|h| h.current_value())
        }

        fn publish_bundle(&self) {
            let payload = {
                let datasets = self.datasets.lock();
                build_bundle_payload(&datasets, self.len, self.field_prefix())
            };

            // Route every publish through the single validating owner. The
            // handle's descriptor is captured from the first payload (field
            // names depend on the runtime `field_prefix`); `post` then
            // rejects any later payload whose shape drifted from the first —
            // pvxs `SharedPV::post`'s type-stability invariant — and never
            // lets a mismatched value become the served snapshot or reach
            // monitor subscribers.
            let handle = self
                .handle
                .get_or_init(|| PvaPvHandle::new(Some(payload.descriptor())));
            if let Err(err) = handle.post(payload) {
                eprintln!(
                    "mini:wf:bundle: dropping payload whose shape changed since the first \
                     publish ({err}); keeping already-connected monitors valid"
                );
            }
        }

        fn mark_processed(&self, index: usize) {
            if index >= u64::BITS as usize || self.expected_mask == 0 {
                return;
            }
            let bit = 1u64 << index;
            let processed = self.processed_mask.fetch_or(bit, Ordering::AcqRel) | bit;
            if processed & self.expected_mask == self.expected_mask {
                self.processed_mask.store(0, Ordering::Release);
                self.publish_bundle();
            }
        }
    }

    /// Per-record DeviceSupport. Selects pre-generated random data by
    /// trailing record index (`mini:wf1` -> bundle field `wf1`) and writes it
    /// into the waveform record's VAL field on each scan.
    pub struct RandomWaveformDeviceSupport {
        shared: Arc<RandomWaveformShared>,
        fallback_data: Arc<[f64]>,
        index: Option<usize>,
    }

    impl RandomWaveformDeviceSupport {
        /// `len` should match the record's NELM. `seed` makes each
        /// record's data unique so that 10× same-data compression
        /// artifacts don't skew the bench.
        fn new(len: usize, seed: u64, shared: Arc<RandomWaveformShared>) -> Self {
            Self {
                shared,
                fallback_data: Arc::<[f64]>::from(random_doubles(len, seed)),
                index: None,
            }
        }

        fn current_data(&self) -> Arc<[f64]> {
            self.index
                .and_then(|idx| self.shared.current_data(idx))
                .unwrap_or_else(|| self.fallback_data.clone())
        }

        fn update_data(&self) -> Arc<[f64]> {
            self.index
                .and_then(|idx| self.shared.update_data(idx))
                .unwrap_or_else(|| self.fallback_data.clone())
        }
    }

    impl DeviceSupport for RandomWaveformDeviceSupport {
        fn dtyp(&self) -> &str {
            "miniRandomWaveform"
        }

        fn set_record_info(&mut self, name: &str, _scan: ScanType) {
            if let Some(idx) = trailing_index(name)
                && let Some(index) = idx.checked_sub(1)
                && index < self.shared.count()
            {
                self.index = Some(index);
            }
        }

        fn init(&mut self, record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
            record.put_field("VAL", EpicsValue::DoubleArray(self.current_data().to_vec()))?;
            Ok(DeviceInitOutcome::Live)
        }

        fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
            let data = self.update_data();
            record.put_field("VAL", EpicsValue::DoubleArray(data.to_vec()))?;
            if let Some(index) = self.index {
                self.shared.mark_processed(index);
            }
            Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
        }

        fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
            Ok(())
        }

        fn write_begin(
            &mut self,
            _record: &mut dyn Record,
        ) -> CaResult<Option<Box<dyn WriteCompletion>>> {
            Ok(None)
        }

        fn last_alarm(&self) -> Option<(u16, u16)> {
            None
        }

        fn last_timestamp(&self) -> Option<std::time::SystemTime> {
            None
        }

        fn io_intr_receiver(&mut self) -> Option<epics_base_rs::runtime::sync::mpsc::Receiver<()>> {
            None
        }
    }

    /// Factory state shared by individual waveform records and the native
    /// PVA bundle PV.
    pub struct RandomWaveformFactory {
        counter: Arc<AtomicU64>,
        len: usize,
        shared: Arc<RandomWaveformShared>,
    }

    impl RandomWaveformFactory {
        /// `count` must be > 0 and must equal the number of waveform records
        /// that will be wired through this factory — otherwise the bundle's
        /// `expected_mask` will never be reached and it will never publish.
        pub fn new_with_count(len: usize, count: usize) -> Self {
            assert!(
                count > 0,
                "RandomWaveformFactory needs count > 0 — bundle never publishes otherwise"
            );
            Self {
                // Start the per-instance fallback-data seed range above the
                // shared dataset's seed range (0..count) so that an
                // unrecognised record name doesn't accidentally land on the
                // same sequence as datasets[i].
                counter: Arc::new(AtomicU64::new(count as u64)),
                len,
                shared: Arc::new(RandomWaveformShared::new(len, count)),
            }
        }

        pub fn make(&self) -> Box<dyn DeviceSupport> {
            let seed = self.counter.fetch_add(1, Ordering::Relaxed);
            Box::new(RandomWaveformDeviceSupport::new(
                self.len,
                seed,
                self.shared.clone(),
            ))
        }

        pub fn register_pva_bundle(&self, pv_name: &str, field_prefix: &str) {
            self.shared.set_field_prefix(field_prefix);
            // The first publish creates the validating handle from the
            // payload's descriptor; register a clone so the qsrv adapter
            // shares the same `latest`/`subscribers` state and serves the
            // canonical descriptor for introspection.
            self.shared.publish_bundle();
            let handle = self
                .shared
                .handle
                .get()
                .expect("publish_bundle initializes the handle")
                .clone();
            epics_bridge_rs::qsrv::register_pva_pv_global(pv_name, handle);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn trailing_index_basic() {
            assert_eq!(trailing_index("mini:wf1"), Some(1));
            assert_eq!(trailing_index("mini:wf10"), Some(10));
            assert_eq!(trailing_index("wf01"), Some(1));
            assert_eq!(trailing_index("123"), Some(123));
        }

        #[test]
        fn trailing_index_no_digits() {
            assert_eq!(trailing_index("mini:wf:bundle"), None);
            assert_eq!(trailing_index("name"), None);
            assert_eq!(trailing_index(""), None);
        }

        #[test]
        fn mark_processed_publishes_when_all_bits_set() {
            let shared = RandomWaveformShared::new(4, 3);
            // No publish before any record processes.
            assert!(shared.current_value().is_none());
            shared.mark_processed(0);
            shared.mark_processed(1);
            assert!(shared.current_value().is_none());
            shared.mark_processed(2);
            assert!(shared.current_value().is_some());
            // Mask should reset for the next cycle.
            assert_eq!(shared.processed_mask.load(Ordering::Relaxed), 0);
        }

        #[test]
        fn mark_processed_idempotent_within_cycle() {
            let shared = RandomWaveformShared::new(2, 2);
            shared.mark_processed(0);
            shared.mark_processed(0); // duplicate — should not advance cycle
            assert!(shared.current_value().is_none());
            shared.mark_processed(1);
            assert!(shared.current_value().is_some());
        }

        #[test]
        fn mark_processed_zero_count_is_noop() {
            // count=0 is rejected by the factory, but the shared layer
            // must still be safe if invoked directly with expected_mask=0.
            let shared = RandomWaveformShared::new(1, 0);
            shared.mark_processed(0);
            assert!(shared.current_value().is_none());
        }

        #[test]
        fn set_field_prefix_locks_after_first_call() {
            let shared = RandomWaveformShared::new(2, 1);
            shared.set_field_prefix("wf");
            // Idempotent for the same prefix.
            shared.set_field_prefix("wf");
            assert_eq!(shared.field_prefix(), "wf");
        }

        #[test]
        #[should_panic(expected = "set_field_prefix called twice")]
        fn set_field_prefix_rejects_conflict() {
            let shared = RandomWaveformShared::new(2, 1);
            shared.set_field_prefix("wf");
            shared.set_field_prefix("ch"); // panics — would change shape
        }

        #[test]
        fn publish_bundle_descriptor_is_stable() {
            let shared = RandomWaveformShared::new(2, 2);
            shared.set_field_prefix("wf");
            shared.publish_bundle();
            let first = shared.current_value().map(|v| v.descriptor()).unwrap();
            // Bump some data and republish — descriptor must match.
            shared.update_data(0);
            shared.publish_bundle();
            let second = shared.current_value().map(|v| v.descriptor()).unwrap();
            assert_eq!(first, second);
        }

        #[test]
        #[should_panic(expected = "count > 0")]
        fn factory_rejects_zero_count() {
            let _ = RandomWaveformFactory::new_with_count(8, 0);
        }
    }
}
