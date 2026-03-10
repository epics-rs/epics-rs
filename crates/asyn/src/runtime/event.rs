/// Events emitted by a runtime actor.
#[derive(Debug, Clone)]
pub enum RuntimeEvent {
    /// Runtime started.
    Started { port_name: String },
    /// Runtime stopped.
    Stopped { port_name: String },
    /// Port connected.
    Connected { port_name: String },
    /// Port disconnected.
    Disconnected { port_name: String },
    /// An error occurred.
    Error { port_name: String, message: String },
}
