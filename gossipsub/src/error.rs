//! Error types that can result from gossipsub.

use serde::{Deserialize, Serialize};

/// Error associated with publishing a gossipsub message.
#[derive(Debug)]
pub enum PublishError {
    /// This message has already been published.
    Duplicate,
    /// There were no peers to send this message to.
    InsufficientPeers,
    /// The overall message was too large. This could be due to excessive topics or an excessive
    /// message size.
    MessageTooLarge,
    /// The compression algorithm failed.
    TransformFailed(std::io::Error),
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for PublishError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TransformFailed(err) => Some(err),
            _ => None,
        }
    }
}

/// Error associated with subscribing to a topic.
#[derive(Debug)]
pub enum SubscriptionError {
    /// Couldn't publish our subscription
    PublishError(PublishError),
    /// We are not allowed to subscribe to this topic by the subscription filter
    NotAllowed,
}

impl std::fmt::Display for SubscriptionError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for SubscriptionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::PublishError(err) => Some(err),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ValidationError {
    /// The message has an invalid signature,
    InvalidSignature,
    /// The sequence number was empty, expected a value.
    EmptySequenceNumber,
    /// The sequence number was the incorrect size
    InvalidSequenceNumber,
    /// The NodeId was invalid
    InvalidNodeId,
    /// Signature existed when validation has been sent to
    /// [`crate::behaviour::MessageAuthenticity::Anonymous`].
    SignaturePresent,
    /// Sequence number existed when validation has been sent to
    /// [`crate::behaviour::MessageAuthenticity::Anonymous`].
    SequenceNumberPresent,
    /// Message source existed when validation has been sent to
    /// [`crate::behaviour::MessageAuthenticity::Anonymous`].
    MessageSourcePresent,
    /// The data transformation failed.
    TransformFailed,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for ValidationError {}

impl From<std::io::Error> for PublishError {
    fn from(error: std::io::Error) -> PublishError {
        PublishError::TransformFailed(error)
    }
}

/// Error associated with Config building.
#[derive(Debug)]
pub enum ConfigBuilderError {
    /// Maximum transmission size is too small.
    MaxTransmissionSizeTooSmall,
    /// History length less than history gossip length.
    HistoryLengthTooSmall,
    /// The ineauality doesn't hold mesh_outbound_min <= mesh_n_low <= mesh_n <= mesh_n_high
    MeshParametersInvalid,
    /// The inequality doesn't hold mesh_outbound_min <= self.config.mesh_n / 2
    MeshOutboundInvalid,
    /// unsubscribe_backoff is zero
    UnsubscribeBackoffIsZero,
    /// Invalid protocol
    InvalidProtocol,
}

impl std::error::Error for ConfigBuilderError {}

impl std::fmt::Display for ConfigBuilderError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::MaxTransmissionSizeTooSmall => {
                write!(f, "Maximum transmission size is too small")
            }
            Self::HistoryLengthTooSmall => write!(f, "History length less than history gossip length"),
            Self::MeshParametersInvalid => write!(f, "The ineauality doesn't hold mesh_outbound_min <= mesh_n_low <= mesh_n <= mesh_n_high"),
            Self::MeshOutboundInvalid => write!(f, "The inequality doesn't hold mesh_outbound_min <= self.config.mesh_n / 2"),
            Self::UnsubscribeBackoffIsZero => write!(f, "unsubscribe_backoff is zero"),
            Self::InvalidProtocol => write!(f, "Invalid protocol"),
        }
    }
}