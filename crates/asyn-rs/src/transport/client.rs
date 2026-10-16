use std::future::Future;
use std::pin::Pin;

use crate::interrupt::{AsyncInterruptReceiver, AsyncRecvError};
use crate::protocol::{
    EventFilter, EventPayload, EventRing, PortEvent, PortReply, PortRequest, Timestamp,
};

use super::error::TransportError;

/// Connection state of a transport client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Connected,
    Disconnected,
}

/// A subscription's consumer: the caller's own [`EventRing`], filled with what
/// the subscription filter passes. There is no queue behind it, so the ring's `ring_len`
/// is the whole depth; a full ring drops its oldest and `recv` reports it as
/// [`AsyncRecvError::Lagged`].
pub struct EventReceiver {
    pub(crate) ring: AsyncInterruptReceiver,
    pub(crate) port_name: String,
}

impl EventReceiver {
    /// The next event, in arrival order; the ring holds only what the
    /// subscription's filter passed.
    pub async fn recv(&mut self) -> Result<PortEvent, AsyncRecvError> {
        let iv = self.ring.recv().await?;
        Ok(PortEvent {
            port_name: self.port_name.clone(),
            payload: EventPayload::from(&iv),
            timestamp: Timestamp::from(iv.timestamp),
        })
    }
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
        ring: EventRing,
    ) -> Pin<Box<dyn Future<Output = Result<EventReceiver, TransportError>> + Send + '_>>;

    fn connection_state(&self) -> ConnectionState;
}
