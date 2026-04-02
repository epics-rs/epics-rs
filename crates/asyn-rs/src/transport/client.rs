use std::future::Future;
use std::pin::Pin;

use crate::protocol::{EventFilter, PortEvent, PortReply, PortRequest};

use super::error::TransportError;

/// Connection state of a transport client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Connected,
    Disconnected,
}

/// Trait for submitting protocol requests to a runtime.
///
/// InProcessClient is the primary implementation (no serialization, direct enum pass-through).
pub trait RuntimeClient: Send + Sync + 'static {
    fn request(
        &self,
        req: PortRequest,
    ) -> Pin<Box<dyn Future<Output = Result<PortReply, TransportError>> + Send + '_>>;

    fn request_blocking(&self, req: PortRequest) -> Result<PortReply, TransportError>;

    fn subscribe(
        &self,
        filter: EventFilter,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<tokio::sync::mpsc::Receiver<PortEvent>, TransportError>>
                + Send
                + '_,
        >,
    >;

    fn connection_state(&self) -> ConnectionState;
}
