//! Random-waveform PVs for benchmarking bulk array transfers.
//!
//! Each record exposes a fixed-length `Vec<f64>` updated once per second
//! with deterministic per-record random data. The 10 individual waveform
//! records and the native PVA bundle share the same backing arrays.
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use parking_lot::Mutex;
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    use epics_base_rs::error::CaResult;
    use epics_base_rs::runtime::sync::mpsc;
    use epics_base_rs::server::device_support::{
        DeviceReadOutcome, DeviceSupport, WriteCompletion,
    };
    use epics_base_rs::server::record::{Record, ScanType};
    use epics_base_rs::types::EpicsValue;
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
        field_prefix: Mutex<String>,
        latest: Arc<Mutex<Option<PvField>>>,
        subscribers: Arc<Mutex<Vec<mpsc::Sender<PvField>>>>,
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
                field_prefix: Mutex::new("wf".to_string()),
                latest: Arc::new(Mutex::new(None)),
                subscribers: Arc::new(Mutex::new(Vec::new())),
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

        fn set_field_prefix(&self, field_prefix: &str) {
            *self.field_prefix.lock() = field_prefix.to_string();
        }

        fn publish_bundle(&self) {
            let payload = {
                let field_prefix = self.field_prefix.lock().clone();
                let datasets = self.datasets.lock();
                build_bundle_payload(&datasets, self.len, &field_prefix)
            };

            *self.latest.lock() = Some(payload.clone());

            let mut subscribers = self.subscribers.lock();
            subscribers.retain(|tx| !tx.is_closed());
            for tx in subscribers.iter() {
                let _ = tx.try_send(payload.clone());
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

        fn init(&mut self, record: &mut dyn Record) -> CaResult<()> {
            record.put_field("VAL", EpicsValue::DoubleArray(self.current_data().to_vec()))?;
            Ok(())
        }

        fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
            let data = self.update_data();
            record.put_field("VAL", EpicsValue::DoubleArray(data.to_vec()))?;
            if let Some(index) = self.index {
                self.shared.mark_processed(index);
            }
            Ok(DeviceReadOutcome::computed())
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
        pub fn new(len: usize) -> Self {
            Self::new_with_count(len, 0)
        }

        pub fn new_with_count(len: usize, count: usize) -> Self {
            Self {
                counter: Arc::new(AtomicU64::new(0)),
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
            self.shared.publish_bundle();
            epics_bridge_rs::qsrv::register_pva_pv_global(
                pv_name,
                epics_bridge_rs::qsrv::PvaPvHandle {
                    latest: self.shared.latest.clone(),
                    subscribers: self.shared.subscribers.clone(),
                },
            );
        }
    }
}
