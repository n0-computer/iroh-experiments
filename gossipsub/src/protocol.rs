use crate::config::ValidationMode;
use crate::handler::HandlerEvent;
use crate::types::{self, RawMessage, Rpc};
use crate::ValidationError;
use bytes::{BufMut, Bytes, BytesMut};
use iroh::net::key::Signature;
use std::io;
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_serde::{Deserializer, Serializer};
use tokio_util::codec::Framed;

pub(crate) const SIGNING_PREFIX: &[u8] = b"libp2p-pubsub:";
pub const GOSSIPSUB_1_1_0_PROTOCOL: &[u8] = b"/meshsub/1.1.0";

/// Configuration
#[derive(Debug, Clone)]
pub struct ProtocolConfig {
    /// The Gossipsub protocol id to listen on.
    pub(crate) protocol_id: Vec<u8>,
    /// The maximum transmit size for a packet.
    pub(crate) max_transmit_size: usize,
    /// Determines the level of validation to be done on incoming messages.
    pub(crate) validation_mode: ValidationMode,
}

impl Default for ProtocolConfig {
    fn default() -> Self {
        Self {
            max_transmit_size: 65536,
            validation_mode: ValidationMode::Strict,
            protocol_id: GOSSIPSUB_1_1_0_PROTOCOL.to_vec(),
        }
    }
}

pub type GossipFramed<T> = tokio_serde::Framed<
    tokio_util::codec::Framed<T, tokio_util::codec::LengthDelimitedCodec>,
    HandlerEvent,
    types::Rpc,
    GossipsubCodec,
>;

impl ProtocolConfig {
    pub fn upgrade_connection<T>(self, socket: T) -> GossipFramed<T>
    where
        T: AsyncRead + AsyncWrite,
    {
        let mut codec = tokio_util::codec::LengthDelimitedCodec::default();
        codec.set_max_frame_length(self.max_transmit_size);
        let transport = Framed::new(socket, codec);

        tokio_serde::Framed::new(transport, GossipsubCodec::new(self.validation_mode))
    }
}

/* Gossip codec for the framing */

pub struct GossipsubCodec {
    /// Determines the level of validation performed on incoming messages.
    validation_mode: ValidationMode,
}

impl GossipsubCodec {
    pub fn new(validation_mode: ValidationMode) -> GossipsubCodec {
        GossipsubCodec { validation_mode }
    }

    /// Verifies a gossipsub message. This returns either a success or failure. All errors
    /// are logged, which prevents error handling in the codec and handler. We simply drop invalid
    /// messages and log warnings, rather than propagating errors through the codec.
    fn verify_signature(message: &types::RawMessage) -> bool {
        let Some(from) = message.source.as_ref() else {
            tracing::debug!("Signature verification failed: No source id given");
            return false;
        };

        let Some(signature) = message.signature.as_ref() else {
            tracing::debug!("Signature verification failed: No signature provided");
            return false;
        };
        let signature = match Signature::from_slice(signature) {
            Ok(sig) => sig,
            Err(_err) => {
                tracing::debug!("Signature verification failed: Invalid signature");
                return false;
            }
        };

        // Construct the signature bytes
        let message_sig: types::Message = message.clone().into();
        let buf = postcard::to_stdvec(&message_sig).unwrap();
        let mut signature_bytes = SIGNING_PREFIX.to_vec();
        signature_bytes.extend_from_slice(&buf);
        from.verify(&signature_bytes, &signature).is_ok()
    }
}

impl Serializer<types::Rpc> for GossipsubCodec {
    type Error = io::Error;

    fn serialize(self: Pin<&mut Self>, data: &types::Rpc) -> Result<Bytes, Self::Error> {
        postcard::experimental::serialized_size(data)
            .and_then(|size| postcard::to_io(data, BytesMut::with_capacity(size).writer()))
            .map(|writer| writer.into_inner().freeze())
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }
}

impl Deserializer<HandlerEvent> for GossipsubCodec {
    type Error = std::io::Error;

    fn deserialize(self: Pin<&mut Self>, src: &BytesMut) -> Result<HandlerEvent, Self::Error> {
        let rpc: types::Rpc = postcard::from_bytes(&src)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

        // Store valid messages.
        let mut messages = Vec::with_capacity(rpc.messages.len());
        // Store any invalid messages.
        let mut invalid_messages = Vec::new();

        for message in rpc.messages.into_iter() {
            // Keep track of the type of invalid message.
            let mut invalid_kind = None;
            let mut verify_signature = false;
            let mut verify_sequence_no = false;
            let mut verify_source = false;

            match self.validation_mode {
                ValidationMode::Strict => {
                    // Validate everything
                    verify_signature = true;
                    verify_sequence_no = true;
                    verify_source = true;
                }
                ValidationMode::Permissive => {
                    // If the fields exist, validate them
                    if message.signature.is_some() {
                        verify_signature = true;
                    }
                    if message.sequence_number.is_some() {
                        verify_sequence_no = true;
                    }
                    if message.source.is_some() {
                        verify_source = true;
                    }
                }
                ValidationMode::Anonymous => {
                    if message.signature.is_some() {
                        tracing::warn!(
                            "Signature field was non-empty and anonymous validation mode is set"
                        );
                        invalid_kind = Some(ValidationError::SignaturePresent);
                    } else if message.sequence_number.is_some() {
                        tracing::warn!(
                            "Sequence number was non-empty and anonymous validation mode is set"
                        );
                        invalid_kind = Some(ValidationError::SequenceNumberPresent);
                    } else if message.source.is_some() {
                        tracing::warn!("Message dropped. Message source was non-empty and anonymous validation mode is set");
                        invalid_kind = Some(ValidationError::MessageSourcePresent);
                    }
                }
                ValidationMode::None => {}
            }

            // If the initial validation logic failed, add the message to invalid messages and
            // continue processing the others.
            if let Some(validation_error) = invalid_kind.take() {
                let message = RawMessage {
                    source: None, // don't bother inform the application
                    data: message.data,
                    sequence_number: None, // don't inform the application
                    topic: message.topic,
                    signature: None, // don't inform the application
                    validated: false,
                };
                invalid_messages.push((message, validation_error));
                // proceed to the next message
                continue;
            }

            // verify message signatures if required
            if verify_signature && !GossipsubCodec::verify_signature(&message) {
                tracing::warn!("Invalid signature for received message");

                // Build the invalid message (ignoring further validation of sequence number
                // and source)
                let message = RawMessage {
                    source: None, // don't bother inform the application
                    data: message.data,
                    sequence_number: None, // don't inform the application
                    topic: message.topic,
                    signature: None, // don't inform the application
                    validated: false,
                };
                invalid_messages.push((message, ValidationError::InvalidSignature));
                // proceed to the next message
                continue;
            }

            // ensure the sequence number is a u64
            let sequence_number = if verify_sequence_no {
                if let Some(seq_no) = message.sequence_number {
                    // valid sequence number
                    Some(seq_no)
                } else {
                    // sequence number was not present
                    tracing::debug!("Sequence number not present but expected");
                    let message = RawMessage {
                        source: None, // don't bother inform the application
                        data: message.data,
                        sequence_number: None, // don't inform the application
                        topic: message.topic,
                        signature: message.signature, // don't inform the application
                        validated: false,
                    };
                    invalid_messages.push((message, ValidationError::EmptySequenceNumber));
                    continue;
                }
            } else {
                // Do not verify the sequence number, consider it empty
                None
            };

            // Verify the message source if required
            let source = if verify_source {
                if let Some(peer_id) = message.source {
                    // valid peer id
                    Some(peer_id)
                } else {
                    None
                }
            } else {
                None
            };

            // This message has passed all validation, add it to the validated messages.
            messages.push(RawMessage {
                source,
                data: message.data,
                sequence_number,
                topic: message.topic,
                signature: message.signature,
                validated: false,
            });
        }

        Ok(HandlerEvent::Message {
            rpc: Rpc {
                messages,
                subscriptions: rpc.subscriptions,
                control_msgs: rpc.control_msgs,
            },
            invalid_messages,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::{Behaviour, ConfigBuilder, TopicHash};
    use crate::{IdentTopic as Topic, Version};
    use iroh::net::key::SecretKey;
    use quickcheck::*;

    #[derive(Clone, Debug)]
    struct Message(RawMessage);

    impl Arbitrary for Message {
        fn arbitrary(g: &mut Gen) -> Self {
            let keypair = TestKeypair::arbitrary(g);

            // generate an arbitrary GossipsubMessage using the behaviour signing functionality
            let config = Config::default();
            let mut gs: Behaviour =
                Behaviour::new(crate::MessageAuthenticity::Signed(keypair.0), config).unwrap();
            let data = (0..gen_range(g, 10..10024))
                .map(|_| u8::arbitrary(g))
                .collect::<Vec<_>>();
            let topic_id = TopicId::arbitrary(g).0;
            let rt = tokio::runtime::Runtime::new().unwrap();
            let msg = rt.block_on(async { gs.build_raw_message(topic_id, data).await.unwrap() });
            Message(msg)
        }
    }

    #[derive(Clone, Debug)]
    struct TopicId(TopicHash);

    impl Arbitrary for TopicId {
        fn arbitrary(g: &mut Gen) -> Self {
            let topic_string: String = (0..gen_range(g, 20..1024))
                .map(|_| char::arbitrary(g))
                .collect::<String>();
            TopicId(Topic::new(topic_string).into())
        }
    }

    #[derive(Clone)]
    struct TestKeypair(SecretKey);

    impl Arbitrary for TestKeypair {
        fn arbitrary(_g: &mut Gen) -> Self {
            // Small enough to be inlined.
            TestKeypair(SecretKey::generate())
        }
    }

    fn gen_range(gen: &mut Gen, range: std::ops::Range<u32>) -> u32 {
        u32::arbitrary(gen) % (range.end - range.start) + range.start
    }

    impl std::fmt::Debug for TestKeypair {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("TestKeypair")
                .field("public", &self.0.public())
                .finish()
        }
    }

    // TODO
    // #[test]
    // /// Test that RPC messages can be encoded and decoded successfully.
    // fn encode_decode() {
    //     fn prop(message: Message) {
    //         let message = message.0;

    //         let rpc = Rpc {
    //             messages: vec![message.clone()],
    //             subscriptions: vec![],
    //             control_msgs: vec![],
    //         };

    //         let mut codec = GossipsubCodec::new(ValidationMode::Strict);
    //         let mut buf = BytesMut::new();
    //         codec.encode(rpc.into_protobuf(), &mut buf).unwrap();
    //         let decoded_rpc = codec.decode(&mut buf).unwrap().unwrap();
    //         // mark as validated as its a published message
    //         match decoded_rpc {
    //             HandlerEvent::Message { mut rpc, .. } => {
    //                 rpc.messages[0].validated = true;

    //                 assert_eq!(vec![message], rpc.messages);
    //             }
    //             _ => panic!("Must decode a message"),
    //         }
    //     }

    //     QuickCheck::new().quickcheck(prop as fn(_) -> _)
    // }
}
