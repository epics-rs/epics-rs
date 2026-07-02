use asyn_rs::error::AsynError;

#[derive(Debug, thiserror::Error)]
pub enum MqttError {
    #[error("invalid topic address: {0}")]
    InvalidAddress(String),

    #[error("unsupported format/type: {0}")]
    UnsupportedType(String),

    #[error("invalid topic name: {0}")]
    InvalidTopic(String),

    #[error("JSON parse error: {0}")]
    JsonParse(#[from] serde_json::Error),

    #[error("JSON field not found: {0}")]
    JsonFieldNotFound(String),

    #[error("value conversion error: {0}")]
    ValueConversion(String),

    #[error("MQTT client error: {0}")]
    Client(String),

    #[error("MQTT client not connected")]
    NotConnected,

    #[error("publish channel closed")]
    PublishChannelClosed,
}

impl From<MqttError> for AsynError {
    fn from(e: MqttError) -> Self {
        AsynError::Status {
            status: asyn_rs::error::AsynStatus::Error,
            message: e.to_string(),
        }
    }
}

pub type MqttResult<T> = Result<T, MqttError>;
