//! This trait allows of extended user-level decoding that can apply to message-data before a
//! message-id is calculated.
//!
//! This is primarily designed to allow applications to implement their own custom compression
//! algorithms that can be topic-specific. Once the raw data is transformed the message-id is then
//! calculated, allowing for applications to employ message-id functions post compression.

use crate::{Message, RawMessage, TopicHash};

/// A general trait of transforming a [`RawMessage`] into a [`Message`]. The
/// [`RawMessage`] is obtained from the wire and the [`Message`] is used to
/// calculate the [`crate::MessageId`] of the message and is what is sent to the application.
///
/// The inbound/outbound transforms must be inverses. Applying the inbound transform and then the
/// outbound transform MUST leave the underlying data un-modified.
///
/// By default, this is the identity transform for all fields in [`Message`].
pub trait DataTransform {
    /// Takes a [`RawMessage`] received and converts it to a [`Message`].
    fn inbound_transform(&self, raw_message: RawMessage) -> Result<Message, std::io::Error>;

    /// Takes the data to be published (a topic and associated data) transforms the data. The
    /// transformed data will then be used to create a [`crate::RawMessage`] to be sent to peers.
    fn outbound_transform(
        &self,
        topic: &TopicHash,
        data: Vec<u8>,
    ) -> Result<Vec<u8>, std::io::Error>;
}

/// The default transform, the raw data is propagated as is to the application layer gossipsub.
#[derive(Default, Clone)]
pub struct IdentityTransform;

impl DataTransform for IdentityTransform {
    fn inbound_transform(&self, raw_message: RawMessage) -> Result<Message, std::io::Error> {
        Ok(Message {
            source: raw_message.source,
            data: raw_message.data,
            sequence_number: raw_message.sequence_number,
            topic: raw_message.topic,
        })
    }

    fn outbound_transform(
        &self,
        _topic: &TopicHash,
        data: Vec<u8>,
    ) -> Result<Vec<u8>, std::io::Error> {
        Ok(data)
    }
}
