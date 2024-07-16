use crate::config::ValidationMode;
use crate::handler::HandlerEvent;
use crate::topic::TopicHash;
use crate::types::{
    self, ControlAction, MessageId, PeerInfo, RawMessage, Rpc, Subscription, SubscriptionAction,
};
use crate::ValidationError;
use byteorder::{BigEndian, ByteOrder};
use bytes::{BufMut, Bytes, BytesMut};
use iroh::net::key::Signature;
use iroh::net::NodeId;
use quick_protobuf::Writer;
use std::io;
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_serde::{Deserializer, Serializer};
use tokio_util::codec::{Decoder, Encoder, Framed};

pub(crate) const SIGNING_PREFIX: &[u8] = b"libp2p-pubsub:";
pub(crate) const GOSSIPSUB_1_1_0_PROTOCOL: &[u8] = b"/meshsub/1.1.0";

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
    types::RpcOut,
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

impl Serializer<types::RpcOut> for GossipsubCodec {
    type Error = io::Error;

    fn serialize(self: Pin<&mut Self>, data: &types::RpcOut) -> Result<Bytes, Self::Error> {
        postcard::experimental::serialized_size(data)
            .and_then(|size| postcard::to_io(data, BytesMut::with_capacity(size).writer()))
            .map(|writer| writer.into_inner().freeze())
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }
}

impl Deserializer<HandlerEvent> for GossipsubCodec {
    type Error = std::io::Error;

    fn deserialize(self: Pin<&mut Self>, src: &BytesMut) -> Result<HandlerEvent, Self::Error> {
        let rpc: types::RpcOut = postcard::from_bytes(&src)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

        todo!()
        // // Store valid messages.
        // let mut messages = Vec::with_capacity(rpc.publish.len());
        // // Store any invalid messages.
        // let mut invalid_messages = Vec::new();

        // for message in rpc.publish.into_iter() {
        //     // Keep track of the type of invalid message.
        //     let mut invalid_kind = None;
        //     let mut verify_signature = false;
        //     let mut verify_sequence_no = false;
        //     let mut verify_source = false;

        //     match self.validation_mode {
        //         ValidationMode::Strict => {
        //             // Validate everything
        //             verify_signature = true;
        //             verify_sequence_no = true;
        //             verify_source = true;
        //         }
        //         ValidationMode::Permissive => {
        //             // If the fields exist, validate them
        //             if message.signature.is_some() {
        //                 verify_signature = true;
        //             }
        //             if message.seqno.is_some() {
        //                 verify_sequence_no = true;
        //             }
        //             if message.from.is_some() {
        //                 verify_source = true;
        //             }
        //         }
        //         ValidationMode::Anonymous => {
        //             if message.signature.is_some() {
        //                 tracing::warn!(
        //                     "Signature field was non-empty and anonymous validation mode is set"
        //                 );
        //                 invalid_kind = Some(ValidationError::SignaturePresent);
        //             } else if message.seqno.is_some() {
        //                 tracing::warn!(
        //                     "Sequence number was non-empty and anonymous validation mode is set"
        //                 );
        //                 invalid_kind = Some(ValidationError::SequenceNumberPresent);
        //             } else if message.from.is_some() {
        //                 tracing::warn!("Message dropped. Message source was non-empty and anonymous validation mode is set");
        //                 invalid_kind = Some(ValidationError::MessageSourcePresent);
        //             }
        //         }
        //         ValidationMode::None => {}
        //     }

        //     // If the initial validation logic failed, add the message to invalid messages and
        //     // continue processing the others.
        //     if let Some(validation_error) = invalid_kind.take() {
        //         let message = RawMessage {
        //             source: None, // don't bother inform the application
        //             data: message.data.unwrap_or_default(),
        //             sequence_number: None, // don't inform the application
        //             topic: TopicHash::from_raw(message.topic),
        //             signature: None, // don't inform the application
        //             validated: false,
        //         };
        //         invalid_messages.push((message, validation_error));
        //         // proceed to the next message
        //         continue;
        //     }

        //     // verify message signatures if required
        //     if verify_signature && !GossipsubCodec::verify_signature(&message) {
        //         tracing::warn!("Invalid signature for received message");

        //         // Build the invalid message (ignoring further validation of sequence number
        //         // and source)
        //         let message = RawMessage {
        //             source: None, // don't bother inform the application
        //             data: message.data.unwrap_or_default(),
        //             sequence_number: None, // don't inform the application
        //             topic: TopicHash::from_raw(message.topic),
        //             signature: None, // don't inform the application
        //             validated: false,
        //         };
        //         invalid_messages.push((message, ValidationError::InvalidSignature));
        //         // proceed to the next message
        //         continue;
        //     }

        //     // ensure the sequence number is a u64
        //     let sequence_number = if verify_sequence_no {
        //         if let Some(seq_no) = message.seqno {
        //             if seq_no.is_empty() {
        //                 None
        //             } else if seq_no.len() != 8 {
        //                 tracing::debug!(
        //                     sequence_number=?seq_no,
        //                     sequence_length=%seq_no.len(),
        //                     "Invalid sequence number length for received message"
        //                 );
        //                 let message = RawMessage {
        //                     source: None, // don't bother inform the application
        //                     data: message.data.unwrap_or_default(),
        //                     sequence_number: None, // don't inform the application
        //                     topic: TopicHash::from_raw(message.topic),
        //                     signature: message.signature, // don't inform the application
        //                     validated: false,
        //                 };
        //                 invalid_messages.push((message, ValidationError::InvalidSequenceNumber));
        //                 // proceed to the next message
        //                 continue;
        //             } else {
        //                 // valid sequence number
        //                 Some(BigEndian::read_u64(&seq_no))
        //             }
        //         } else {
        //             // sequence number was not present
        //             tracing::debug!("Sequence number not present but expected");
        //             let message = RawMessage {
        //                 source: None, // don't bother inform the application
        //                 data: message.data.unwrap_or_default(),
        //                 sequence_number: None, // don't inform the application
        //                 topic: TopicHash::from_raw(message.topic),
        //                 signature: message.signature, // don't inform the application
        //                 validated: false,
        //             };
        //             invalid_messages.push((message, ValidationError::EmptySequenceNumber));
        //             continue;
        //         }
        //     } else {
        //         // Do not verify the sequence number, consider it empty
        //         None
        //     };

        //     // Verify the message source if required
        //     let source = if verify_source {
        //         if let Some(bytes) = message.from {
        //             if !bytes.is_empty() {
        //                 match NodeId::from_bytes(&bytes) {
        //                     Ok(peer_id) => Some(peer_id), // valid peer id
        //                     Err(_) => {
        //                         // invalid peer id, add to invalid messages
        //                         tracing::debug!("Message source has an invalid NodeId");
        //                         let message = RawMessage {
        //                             source: None, // don't bother inform the application
        //                             data: message.data.unwrap_or_default(),
        //                             sequence_number,
        //                             topic: TopicHash::from_raw(message.topic),
        //                             signature: message.signature, // don't inform the application
        //                             validated: false,
        //                         };
        //                         invalid_messages.push((message, ValidationError::InvalidNodeId));
        //                         continue;
        //                     }
        //                 }
        //             } else {
        //                 None
        //             }
        //         } else {
        //             None
        //         }
        //     } else {
        //         None
        //     };

        //     // This message has passed all validation, add it to the validated messages.
        //     messages.push(RawMessage {
        //         source,
        //         data: message.data.unwrap_or_default(),
        //         sequence_number,
        //         topic: TopicHash::from_raw(message.topic),
        //         signature: message.signature,
        //         validated: false,
        //     });
        // }

        // let mut control_msgs = Vec::new();

        // if let Some(rpc_control) = rpc.control {
        //     // Collect the gossipsub control messages
        //     let ihave_msgs: Vec<ControlAction> = rpc_control
        //         .ihave
        //         .into_iter()
        //         .map(|ihave| ControlAction::IHave {
        //             topic_hash: TopicHash::from_raw(ihave.topic_id.unwrap_or_default()),
        //             message_ids: ihave
        //                 .message_ids
        //                 .into_iter()
        //                 .map(MessageId::from)
        //                 .collect::<Vec<_>>(),
        //         })
        //         .collect();

        //     let iwant_msgs: Vec<ControlAction> = rpc_control
        //         .iwant
        //         .into_iter()
        //         .map(|iwant| ControlAction::IWant {
        //             message_ids: iwant
        //                 .message_ids
        //                 .into_iter()
        //                 .map(MessageId::from)
        //                 .collect::<Vec<_>>(),
        //         })
        //         .collect();

        //     let graft_msgs: Vec<ControlAction> = rpc_control
        //         .graft
        //         .into_iter()
        //         .map(|graft| ControlAction::Graft {
        //             topic_hash: TopicHash::from_raw(graft.topic_id.unwrap_or_default()),
        //         })
        //         .collect();

        //     let mut prune_msgs = Vec::new();

        //     for prune in rpc_control.prune {
        //         // filter out invalid peers
        //         let peers = prune
        //             .peers
        //             .into_iter()
        //             .filter_map(|info| {
        //                 info.peer_id
        //                     .as_ref()
        //                     .and_then(|id| NodeId::from_bytes(id).ok())
        //                     .map(|peer_id|
        //                             //TODO signedPeerRecord, see https://github.com/libp2p/specs/pull/217
        //                             PeerInfo {
        //                                 peer_id: Some(peer_id),
        //                             })
        //             })
        //             .collect::<Vec<PeerInfo>>();

        //         let topic_hash = TopicHash::from_raw(prune.topic_id.unwrap_or_default());
        //         prune_msgs.push(ControlAction::Prune {
        //             topic_hash,
        //             peers,
        //             backoff: prune.backoff,
        //         });
        //     }

        //     control_msgs.extend(ihave_msgs);
        //     control_msgs.extend(iwant_msgs);
        //     control_msgs.extend(graft_msgs);
        //     control_msgs.extend(prune_msgs);
        // }

        // Ok(Some(HandlerEvent::Message {
        //     rpc: Rpc {
        //         messages,
        //         subscriptions: rpc
        //             .subscriptions
        //             .into_iter()
        //             .map(|sub| Subscription {
        //                 action: if Some(true) == sub.subscribe {
        //                     SubscriptionAction::Subscribe
        //                 } else {
        //                     SubscriptionAction::Unsubscribe
        //                 },
        //                 topic_hash: TopicHash::from_raw(sub.topic_id.unwrap_or_default()),
        //             })
        //             .collect(),
        //         control_msgs,
        //     },
        //     invalid_messages,
        // }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::{Behaviour, ConfigBuilder};
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
            let data = (0..g.gen_range(10..10024u32))
                .map(|_| u8::arbitrary(g))
                .collect::<Vec<_>>();
            let topic_id = TopicId::arbitrary(g).0;
            Message(gs.build_raw_message(topic_id, data).unwrap())
        }
    }

    #[derive(Clone, Debug)]
    struct TopicId(TopicHash);

    impl Arbitrary for TopicId {
        fn arbitrary(g: &mut Gen) -> Self {
            let topic_string: String = (0..g.gen_range(20..1024u32))
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

    impl std::fmt::Debug for TestKeypair {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("TestKeypair")
                .field("public", &self.0.public())
                .finish()
        }
    }

    #[test]
    /// Test that RPC messages can be encoded and decoded successfully.
    fn encode_decode() {
        fn prop(message: Message) {
            let message = message.0;

            let rpc = Rpc {
                messages: vec![message.clone()],
                subscriptions: vec![],
                control_msgs: vec![],
            };

            let mut codec = GossipsubCodec::new(u32::MAX as usize, ValidationMode::Strict);
            let mut buf = BytesMut::new();
            codec.encode(rpc.into_protobuf(), &mut buf).unwrap();
            let decoded_rpc = codec.decode(&mut buf).unwrap().unwrap();
            // mark as validated as its a published message
            match decoded_rpc {
                HandlerEvent::Message { mut rpc, .. } => {
                    rpc.messages[0].validated = true;

                    assert_eq!(vec![message], rpc.messages);
                }
                _ => panic!("Must decode a message"),
            }
        }

        QuickCheck::new().quickcheck(prop as fn(_) -> _)
    }

    #[test]
    fn support_floodsub_with_custom_protocol() {
        let protocol_config = ConfigBuilder::default()
            .protocol_id("/foosub", Version::V1_1)
            .support_floodsub()
            .build()
            .unwrap()
            .protocol_config();

        assert_eq!(protocol_config.protocol_ids[0].protocol, "/foosub");
        assert_eq!(protocol_config.protocol_ids[1].protocol, "/floodsub/1.0.0");
    }
}
