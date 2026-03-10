use epics_base_rs::types::EpicsValue;

/// Trait implemented by generated program variable structs.
///
/// Each SNL program generates a struct holding all program variables.
/// This trait provides channel-value accessors so the runtime can
/// transfer values between the channel store and the local snapshot.
pub trait ProgramVars: Clone + Send + Sync + 'static {
    /// Read the value of a channel-assigned variable.
    fn get_channel_value(&self, ch_id: usize) -> EpicsValue;

    /// Write a channel value into the local variable.
    fn set_channel_value(&mut self, ch_id: usize, value: &EpicsValue);
}

/// Channel definition: static metadata from the compiled program.
#[derive(Debug, Clone)]
pub struct ChannelDef {
    /// Variable name in the SNL source.
    pub var_name: String,
    /// PV name template (may contain macros like {P}).
    pub pv_name: String,
    /// Whether this channel has a `monitor` declaration.
    pub monitored: bool,
    /// Event flag id synced to this channel (via `sync`), if any.
    pub sync_ef: Option<usize>,
}

/// Static program metadata provided by generated code.
pub trait ProgramMeta {
    const NUM_CHANNELS: usize;
    const NUM_EVENT_FLAGS: usize;
    const NUM_STATE_SETS: usize;

    fn channel_defs() -> Vec<ChannelDef>;

    /// Maps event flag id → list of synced channel ids.
    fn event_flag_sync_map() -> Vec<Vec<usize>>;
}
