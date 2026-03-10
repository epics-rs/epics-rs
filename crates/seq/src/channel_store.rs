use std::sync::Mutex;

use epics_base_rs::types::EpicsValue;

/// Per-channel value slot holding the latest monitor/get result.
#[derive(Debug, Clone)]
pub struct ChannelValueSlot {
    pub value: EpicsValue,
    pub status: i16,
    pub severity: i16,
}

impl Default for ChannelValueSlot {
    fn default() -> Self {
        Self {
            value: EpicsValue::Double(0.0),
            status: 0,
            severity: 0,
        }
    }
}

/// Central channel value storage shared between monitor callbacks and state sets.
///
/// Each channel has its own `Mutex<ChannelValueSlot>`. Contention is low because
/// monitors write rarely and state sets read only during sync.
pub struct ChannelStore {
    slots: Vec<Mutex<ChannelValueSlot>>,
}

impl ChannelStore {
    pub fn new(num_channels: usize) -> Self {
        let slots = (0..num_channels)
            .map(|_| Mutex::new(ChannelValueSlot::default()))
            .collect();
        Self { slots }
    }

    /// Update a channel's value (called from monitor callback or pvGet).
    pub fn set(&self, ch_id: usize, value: EpicsValue) {
        if let Some(slot) = self.slots.get(ch_id) {
            let mut s = slot.lock().unwrap();
            s.value = value;
        }
    }

    /// Update a channel's value with status/severity.
    pub fn set_full(&self, ch_id: usize, value: EpicsValue, status: i16, severity: i16) {
        if let Some(slot) = self.slots.get(ch_id) {
            let mut s = slot.lock().unwrap();
            s.value = value;
            s.status = status;
            s.severity = severity;
        }
    }

    /// Read a channel's current value.
    pub fn get(&self, ch_id: usize) -> Option<EpicsValue> {
        self.slots
            .get(ch_id)
            .map(|slot| slot.lock().unwrap().value.clone())
    }

    /// Read a channel's full slot (value + metadata).
    pub fn get_full(&self, ch_id: usize) -> Option<ChannelValueSlot> {
        self.slots
            .get(ch_id)
            .map(|slot| slot.lock().unwrap().clone())
    }

    pub fn num_channels(&self) -> usize {
        self.slots.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_and_get() {
        let store = ChannelStore::new(3);
        store.set(0, EpicsValue::Double(42.0));
        match store.get(0).unwrap() {
            EpicsValue::Double(v) => assert!((v - 42.0).abs() < 1e-10),
            _ => panic!("wrong type"),
        }
    }

    #[test]
    fn test_default_value() {
        let store = ChannelStore::new(1);
        match store.get(0).unwrap() {
            EpicsValue::Double(v) => assert!((v - 0.0).abs() < 1e-10),
            _ => panic!("wrong type"),
        }
    }

    #[test]
    fn test_set_full() {
        let store = ChannelStore::new(2);
        store.set_full(1, EpicsValue::Long(7), 1, 2);
        let slot = store.get_full(1).unwrap();
        assert_eq!(slot.status, 1);
        assert_eq!(slot.severity, 2);
        match slot.value {
            EpicsValue::Long(v) => assert_eq!(v, 7),
            _ => panic!("wrong type"),
        }
    }

    #[test]
    fn test_invalid_channel() {
        let store = ChannelStore::new(1);
        assert!(store.get(99).is_none());
        // set on invalid channel is a no-op
        store.set(99, EpicsValue::Double(1.0));
    }

    #[test]
    fn test_concurrent_access() {
        use std::sync::Arc;
        use std::thread;

        let store = Arc::new(ChannelStore::new(1));
        let store2 = store.clone();

        let writer = thread::spawn(move || {
            for i in 0..100 {
                store2.set(0, EpicsValue::Double(i as f64));
            }
        });

        let reader = thread::spawn(move || {
            for _ in 0..100 {
                let _ = store.get(0);
            }
        });

        writer.join().unwrap();
        reader.join().unwrap();
    }
}
