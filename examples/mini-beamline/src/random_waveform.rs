//! Random-waveform PVs for benchmarking bulk array transfers.
//!
//! Each record exposes a fixed-length `Vec<f64>` populated at IOC startup
//! with deterministic per-record random data. The VAL field is
//! written once during `init()` and not changed after — this is intentional:
//! the bench measures wire transfer + client-side decode cost, not
//! IOC-side update throughput.
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

    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    use epics_base_rs::error::CaResult;
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

    /// Per-record DeviceSupport. Selects pre-generated random data by
    /// trailing record index (`mini:wf1` -> bundle field `wf1`) and writes it
    /// into the waveform record's VAL field at init time.
    pub struct RandomWaveformDeviceSupport {
        datasets: Arc<Vec<Arc<[f64]>>>,
        fallback_seed: u64,
        len: usize,
        data: Option<Arc<[f64]>>,
    }

    impl RandomWaveformDeviceSupport {
        /// `len` should match the record's NELM. `seed` makes each
        /// record's data unique so that 10× same-data compression
        /// artifacts don't skew the bench.
        pub fn new(len: usize, seed: u64, datasets: Arc<Vec<Arc<[f64]>>>) -> Self {
            Self {
                datasets,
                fallback_seed: seed,
                len,
                data: None,
            }
        }

        fn selected_data(&mut self) -> Arc<[f64]> {
            self.data
                .get_or_insert_with(|| random_doubles(self.len, self.fallback_seed).into())
                .clone()
        }
    }

    impl DeviceSupport for RandomWaveformDeviceSupport {
        fn dtyp(&self) -> &str {
            "miniRandomWaveform"
        }

        fn set_record_info(&mut self, name: &str, _scan: ScanType) {
            if let Some(idx) = trailing_index(name)
                && let Some(data) = idx.checked_sub(1).and_then(|i| self.datasets.get(i))
            {
                self.data = Some(data.clone());
            }
        }

        fn init(&mut self, record: &mut dyn Record) -> CaResult<()> {
            record.put_field(
                "VAL",
                EpicsValue::DoubleArray(self.selected_data().to_vec()),
            )?;
            Ok(())
        }

        fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
            // VAL was populated at init() and never changes — the CA
            // server returns the current VAL directly without calling
            // read() on PINI gets, so this body is only entered if the
            // record is forced to process. Return computed() so the
            // value already in the record is preserved.
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
        datasets: Arc<Vec<Arc<[f64]>>>,
    }

    impl RandomWaveformFactory {
        pub fn new(len: usize) -> Self {
            Self::new_with_count(len, 0)
        }

        pub fn new_with_count(len: usize, count: usize) -> Self {
            let datasets = (0..count)
                .map(|idx| Arc::<[f64]>::from(random_doubles(len, idx as u64)))
                .collect();
            Self {
                counter: Arc::new(AtomicU64::new(0)),
                len,
                datasets: Arc::new(datasets),
            }
        }

        pub fn make(&self) -> Box<dyn DeviceSupport> {
            let seed = self.counter.fetch_add(1, Ordering::Relaxed);
            Box::new(RandomWaveformDeviceSupport::new(
                self.len,
                seed,
                self.datasets.clone(),
            ))
        }

        pub fn register_pva_bundle(&self, pv_name: &str, field_prefix: &str) {
            let mut pv = PvStructure::new("mini:waveforms/v1");

            for (idx, data) in self.datasets.iter().enumerate() {
                pv.fields.push((
                    format!("{field_prefix}{}", idx + 1),
                    PvField::scalar_array_double(data.clone()),
                ));
            }
            pv.fields.push((
                "nelm".into(),
                PvField::Scalar(ScalarValue::Int(self.len as i32)),
            ));
            pv.fields
                .push(("version".into(), PvField::Scalar(ScalarValue::Int(1))));

            let latest = Arc::new(parking_lot::Mutex::new(Some(PvField::Structure(pv))));
            let subscribers = Arc::new(parking_lot::Mutex::new(Vec::new()));
            epics_bridge_rs::qsrv::register_pva_pv_global(
                pv_name,
                epics_bridge_rs::qsrv::PvaPvHandle {
                    latest,
                    subscribers,
                },
            );
        }
    }
}
