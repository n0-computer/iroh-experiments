//! A collection of types using the Gossipsub system.
use crate::{handler::HandlerIn, TopicHash};
use iroh::net::NodeId;
use std::fmt;
use std::fmt::Debug;
use tokio::{sync::mpsc, task::JoinHandle};

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
/// Validation kinds from the application for received messages.
pub enum MessageAcceptance {
    /// The message is considered valid, and it should be delivered and forwarded to the network.
    Accept,
    /// The message is considered invalid, and it should be rejected and trigger the P₄ penalty.
    Reject,
    /// The message is neither delivered nor forwarded to the network, but the router does not
    /// trigger the P₄ penalty.
    Ignore,
}

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MessageId(pub Vec<u8>);

impl MessageId {
    pub fn new(value: &[u8]) -> Self {
        Self(value.to_vec())
    }
}

impl<T: Into<Vec<u8>>> From<T> for MessageId {
    fn from(value: T) -> Self {
        Self(value.into())
    }
}

impl std::fmt::Display for MessageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", hex_fmt::HexFmt(&self.0))
    }
}

impl std::fmt::Debug for MessageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MessageId({})", hex_fmt::HexFmt(&self.0))
    }
}

#[derive(Debug)]
pub(crate) struct PeerConnections {
    /// Its current connections.
    pub(crate) connections: Vec<usize>,
    pub(crate) connection_task: JoinHandle<()>,
    pub(crate) connection_sender: mpsc::Sender<HandlerIn>,
}

/// A message received by the gossipsub system and stored locally in caches..
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct RawMessage {
    /// Id of the peer that published this message.
    pub source: Option<NodeId>,

    /// Content of the message. Its meaning is out of scope of this library.
    pub data: Vec<u8>,

    /// A random sequence number.
    pub sequence_number: Option<u64>,

    /// The topic this message belongs to
    pub topic: TopicHash,

    /// The signature of the message if it's signed.
    pub signature: Option<Vec<u8>>,

    /// Flag indicating if this message has been validated by the application or not.
    pub validated: bool,
}

impl From<RawMessage> for Message {
    fn from(value: RawMessage) -> Self {
        Message {
            source: value.source,
            data: value.data,
            sequence_number: value.sequence_number,
            topic: value.topic,
        }
    }
}

/// The message sent to the user after a [`RawMessage`] has been transformed by a
/// [`crate::DataTransform`].
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Message {
    /// Id of the peer that published this message.
    pub source: Option<NodeId>,

    /// Content of the message.
    pub data: Vec<u8>,

    /// A random sequence number.
    pub sequence_number: Option<u64>,

    /// The topic this message belongs to
    pub topic: TopicHash,
}

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Message")
            .field(
                "data",
                &format_args!("{:<20}", &hex_fmt::HexFmt(&self.data)),
            )
            .field("source", &self.source)
            .field("sequence_number", &self.sequence_number)
            .field("topic", &self.topic)
            .finish()
    }
}

/// A subscription received by the gossipsub system.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Subscription {
    /// Action to perform.
    pub action: SubscriptionAction,
    /// The topic from which to subscribe or unsubscribe.
    pub topic_hash: TopicHash,
}

/// Action that a subscription wants to perform.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SubscriptionAction {
    /// The remote wants to subscribe to the given topic.
    Subscribe,
    /// The remote wants to unsubscribe from the given topic.
    Unsubscribe,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PeerInfo {
    pub peer_id: Option<NodeId>,
    //TODO add this when RFC: Signed Address Records got added to the spec (see pull request
    // https://github.com/libp2p/specs/pull/217)
    //pub signed_peer_record: ?,
}

/// A Control message received by the gossipsub system.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ControlAction {
    /// Node broadcasts known messages per topic - IHave control message.
    IHave {
        /// The topic of the messages.
        topic_hash: TopicHash,
        /// A list of known message ids (peer_id + sequence _number) as a string.
        message_ids: Vec<MessageId>,
    },
    /// The node requests specific message ids (peer_id + sequence _number) - IWant control message.
    IWant {
        /// A list of known message ids (peer_id + sequence _number) as a string.
        message_ids: Vec<MessageId>,
    },
    /// The node has been added to the mesh - Graft control message.
    Graft {
        /// The mesh topic the peer should be added to.
        topic_hash: TopicHash,
    },
    /// The node has been removed from the mesh - Prune control message.
    Prune {
        /// The mesh topic the peer should be removed from.
        topic_hash: TopicHash,
        /// A list of peers to be proposed to the removed peer as peer exchange
        peers: Vec<PeerInfo>,
        /// The backoff time in seconds before we allow to reconnect
        backoff: Option<u64>,
    },
}

/// A Gossipsub RPC message sent.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RpcOut {
    /// Publish a Gossipsub message on network.
    Publish(RawMessage),
    /// Forward a Gossipsub message to the network.
    Forward(RawMessage),
    /// Subscribe a topic.
    Subscribe(TopicHash),
    /// Unsubscribe a topic.
    Unsubscribe(TopicHash),
    /// List of Gossipsub control messages.
    Control(ControlAction),
}

impl From<RpcOut> for Rpc {
    fn from(rpc: RpcOut) -> Self {
        match rpc {
            RpcOut::Publish(message) => Rpc {
                subscriptions: Vec::new(),
                messages: vec![message],
                control_msgs: Vec::new(),
            },
            RpcOut::Forward(message) => Rpc {
                messages: vec![message],
                subscriptions: Vec::new(),
                control_msgs: Vec::new(),
            },
            RpcOut::Subscribe(topic) => Rpc {
                messages: Vec::new(),
                subscriptions: vec![Subscription {
                    action: SubscriptionAction::Subscribe,
                    topic_hash: topic,
                }],
                control_msgs: Vec::new(),
            },
            RpcOut::Unsubscribe(topic) => Rpc {
                messages: Vec::new(),
                subscriptions: vec![Subscription {
                    action: SubscriptionAction::Unsubscribe,
                    topic_hash: topic,
                }],
                control_msgs: Vec::new(),
            },
            RpcOut::Control(action) => Rpc {
                messages: Vec::new(),
                subscriptions: Vec::new(),
                control_msgs: vec![action],
            },
        }
    }
}

/// An RPC received/sent.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rpc {
    /// List of messages that were part of this RPC query.
    pub messages: Vec<RawMessage>,
    /// List of subscriptions.
    pub subscriptions: Vec<Subscription>,
    /// List of Gossipsub control messages.
    pub control_msgs: Vec<ControlAction>,
}

impl fmt::Debug for Rpc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut b = f.debug_struct("GossipsubRpc");
        if !self.messages.is_empty() {
            b.field("messages", &self.messages);
        }
        if !self.subscriptions.is_empty() {
            b.field("subscriptions", &self.subscriptions);
        }
        if !self.control_msgs.is_empty() {
            b.field("control_msgs", &self.control_msgs);
        }
        b.finish()
    }
}
