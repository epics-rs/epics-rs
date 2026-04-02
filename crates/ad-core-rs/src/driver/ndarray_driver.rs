use std::path::Path;
use std::sync::Arc;

use asyn_rs::error::AsynResult;
use asyn_rs::port::{PortDriverBase, PortFlags};

use crate::ndarray::NDArray;
use crate::ndarray_pool::NDArrayPool;
use crate::params::ndarray_driver::NDArrayDriverParams;
use crate::plugin::channel::{NDArrayOutput, NDArraySender, QueuedArrayCounter};

/// Base state for asynNDArrayDriver (file handling, attribute mgmt, pool).
pub struct NDArrayDriverBase {
    pub port_base: PortDriverBase,
    pub params: NDArrayDriverParams,
    pub pool: Arc<NDArrayPool>,
    pub array_output: NDArrayOutput,
    pub queued_counter: Arc<QueuedArrayCounter>,
}

impl NDArrayDriverBase {
    pub fn new(port_name: &str, max_memory: usize) -> AsynResult<Self> {
        let mut port_base = PortDriverBase::new(
            port_name,
            1,
            PortFlags {
                can_block: true,
                ..Default::default()
            },
        );

        let params = NDArrayDriverParams::create(&mut port_base)?;

        port_base.set_int32_param(params.array_callbacks, 0, 1)?;
        port_base.set_float64_param(params.pool_max_memory, 0, max_memory as f64 / 1_048_576.0)?;

        let pool = Arc::new(NDArrayPool::new(max_memory));

        Ok(Self {
            port_base,
            params,
            pool,
            array_output: NDArrayOutput::new(),
            queued_counter: Arc::new(QueuedArrayCounter::new()),
        })
    }

    /// Connect a downstream channel-based receiver.
    pub fn connect_downstream(&mut self, mut sender: NDArraySender) {
        sender.set_queued_counter(self.queued_counter.clone());
        self.array_output.add(sender);
    }

    /// Number of connected downstream channels.
    pub fn num_plugins(&self) -> usize {
        self.array_output.num_senders()
    }

    /// Publish an array: update counters, push to plugins and channel outputs.
    pub fn publish_array(&mut self, array: Arc<NDArray>) -> AsynResult<()> {
        let counter = self
            .port_base
            .get_int32_param(self.params.array_counter, 0)?
            + 1;
        self.port_base
            .set_int32_param(self.params.array_counter, 0, counter)?;

        let info = array.info();
        self.port_base
            .set_int32_param(self.params.array_size_x, 0, info.x_size as i32)?;
        self.port_base
            .set_int32_param(self.params.array_size_y, 0, info.y_size as i32)?;
        self.port_base
            .set_int32_param(self.params.array_size_z, 0, info.color_size as i32)?;
        self.port_base
            .set_int32_param(self.params.array_size, 0, info.total_bytes as i32)?;
        self.port_base
            .set_int32_param(self.params.unique_id, 0, array.unique_id)?;

        // Update pool stats
        self.port_base.set_float64_param(
            self.params.pool_used_memory,
            0,
            self.pool.allocated_bytes() as f64 / 1_048_576.0,
        )?;
        self.port_base.set_int32_param(
            self.params.pool_free_buffers,
            0,
            self.pool.num_free_buffers() as i32,
        )?;
        self.port_base.set_int32_param(
            self.params.pool_alloc_buffers,
            0,
            self.pool.num_alloc_buffers() as i32,
        )?;

        let callbacks_enabled = self
            .port_base
            .get_int32_param(self.params.array_callbacks, 0)?
            != 0;

        if callbacks_enabled {
            self.port_base.set_generic_pointer_param(
                self.params.ndarray_data,
                0,
                array.clone() as Arc<dyn std::any::Any + Send + Sync>,
            )?;

            self.array_output.publish(array);
        }

        self.port_base.call_param_callbacks(0)?;

        Ok(())
    }

    /// Construct a file path from template, path, name, and number.
    pub fn create_file_name(&mut self) -> AsynResult<String> {
        let path = self.port_base.get_string_param(self.params.file_path, 0)?;
        let name = self.port_base.get_string_param(self.params.file_name, 0)?;
        let number = self.port_base.get_int32_param(self.params.file_number, 0)?;
        let template = self
            .port_base
            .get_string_param(self.params.file_template, 0)?;

        let full = if template.is_empty() {
            format!("{}{}{:04}", path, name, number)
        } else {
            // Simple template: replace %s with path+name, %d with number
            template
                .replace("%s%s", &format!("{}{}", path, name))
                .replace("%d", &number.to_string())
        };

        self.port_base
            .set_string_param(self.params.full_file_name, 0, full.clone())?;

        Ok(full)
    }

    /// Check if the file path directory exists.
    pub fn check_path(&mut self) -> AsynResult<bool> {
        let path = self.port_base.get_string_param(self.params.file_path, 0)?;
        let exists = Path::new(&path).is_dir();
        self.port_base
            .set_int32_param(self.params.file_path_exists, 0, exists as i32)?;
        Ok(exists)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::channel::ndarray_channel;

    #[test]
    fn test_new_sets_callbacks_enabled() {
        let drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
        assert_eq!(
            drv.port_base
                .get_int32_param(drv.params.array_callbacks, 0)
                .unwrap(),
            1,
        );
    }

    #[test]
    fn test_publish_array() {
        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
        let arr = drv
            .pool
            .alloc(
                vec![
                    crate::ndarray::NDDimension::new(64),
                    crate::ndarray::NDDimension::new(64),
                ],
                crate::ndarray::NDDataType::UInt8,
            )
            .unwrap();
        drv.publish_array(Arc::new(arr)).unwrap();
        assert_eq!(
            drv.port_base
                .get_int32_param(drv.params.array_counter, 0)
                .unwrap(),
            1,
        );
    }

    #[test]
    fn test_publish_updates_size_info() {
        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
        let arr = drv
            .pool
            .alloc(
                vec![
                    crate::ndarray::NDDimension::new(320),
                    crate::ndarray::NDDimension::new(240),
                ],
                crate::ndarray::NDDataType::UInt16,
            )
            .unwrap();
        drv.publish_array(Arc::new(arr)).unwrap();
        assert_eq!(
            drv.port_base
                .get_int32_param(drv.params.array_size_x, 0)
                .unwrap(),
            320,
        );
        assert_eq!(
            drv.port_base
                .get_int32_param(drv.params.array_size_y, 0)
                .unwrap(),
            240,
        );
    }

    #[test]
    fn test_create_file_name_default() {
        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
        drv.port_base
            .set_string_param(drv.params.file_path, 0, "/tmp/".into())
            .unwrap();
        drv.port_base
            .set_string_param(drv.params.file_name, 0, "test_".into())
            .unwrap();
        drv.port_base
            .set_int32_param(drv.params.file_number, 0, 42)
            .unwrap();
        drv.port_base
            .set_string_param(drv.params.file_template, 0, "".into())
            .unwrap();

        let name = drv.create_file_name().unwrap();
        assert_eq!(name, "/tmp/test_0042");
    }

    #[test]
    fn test_check_path_exists() {
        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
        drv.port_base
            .set_string_param(drv.params.file_path, 0, "/tmp".into())
            .unwrap();
        assert!(drv.check_path().unwrap());
    }

    #[test]
    fn test_check_path_not_exists() {
        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
        drv.port_base
            .set_string_param(drv.params.file_path, 0, "/nonexistent_path_xyz".into())
            .unwrap();
        assert!(!drv.check_path().unwrap());
    }

    #[test]
    fn test_connect_downstream() {
        let mut drv = NDArrayDriverBase::new("TEST", 1_000_000).unwrap();
        let (sender, mut receiver) = ndarray_channel("DOWNSTREAM", 10);
        drv.connect_downstream(sender);
        assert_eq!(drv.num_plugins(), 1);

        let arr = drv
            .pool
            .alloc(
                vec![crate::ndarray::NDDimension::new(8)],
                crate::ndarray::NDDataType::UInt8,
            )
            .unwrap();
        let id = arr.unique_id;
        drv.publish_array(Arc::new(arr)).unwrap();

        let received = receiver.blocking_recv().unwrap();
        assert_eq!(received.unique_id, id);
    }
}
