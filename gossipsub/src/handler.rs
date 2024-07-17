use crate::protocol::{GossipFramed, ProtocolConfig};
use crate::types::{self, RawMessage};
use crate::ValidationError;
use futures::{SinkExt, StreamExt};
use iroh::net::endpoint::{RecvStream, SendStream};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::mpsc;
use web_time::Instant;

/// The event emitted by the Handler. This informs the behaviour of various events created
/// by the handler.
#[derive(Debug, Serialize, Deserialize)]
pub enum HandlerEvent {
    /// A GossipsubRPC message has been received. This also contains a list of invalid messages (if
    /// any) that were received.
    Message {
        /// The GossipsubRPC message excluding any invalid messages.
        rpc: types::Rpc,
        /// Any invalid messages that were received in the RPC, along with the associated
        /// validation error.
        invalid_messages: Vec<(RawMessage, ValidationError)>,
    },
}

#[derive(Debug)]
pub struct Stream {
    send: SendStream,
    recv: RecvStream,
}

impl AsyncWrite for Stream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        Pin::new(&mut self.send).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.send).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.send).poll_shutdown(cx)
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.recv).poll_read(cx, buf)
    }
}

/// A message sent from the behaviour to the handler.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum HandlerIn {
    /// A gossipsub message to send.
    Message(types::RpcOut),
    /// The peer has joined the mesh.
    JoinedMesh,
    /// The peer has left the mesh.
    LeftMesh,
}

/// The maximum number of inbound or outbound substreams attempts we allow.
///
/// Gossipsub is supposed to have a single long-lived inbound and outbound substream. On failure we
/// attempt to recreate these. This imposes an upper bound of new substreams before we consider the
/// connection faulty and disable the handler. This also prevents against potential substream
/// creation loops.
const MAX_SUBSTREAM_ATTEMPTS: usize = 5;

/// Protocol Handler that manages a single long-lived substream with a peer.
pub struct Handler {
    handler_sender: mpsc::Sender<HandlerEvent>,
    handler_receiver: mpsc::Receiver<HandlerIn>,

    /// Upgrade configuration for the gossipsub protocol.
    listen_protocol: ProtocolConfig,

    /// The single long-lived outbound substream.
    outbound_substream: Option<OutboundSubstreamState>,

    /// The single long-lived inbound substream.
    inbound_substream: Option<InboundSubstreamState>,

    /// Queue of values that we want to send to the remote.
    send_queue: SmallVec<[types::Rpc; 16]>,

    /// Flag indicating that an outbound substream is being established to prevent duplicate
    /// requests.
    outbound_substream_establishing: bool,

    /// The number of outbound substreams we have requested.
    outbound_substream_attempts: usize,

    /// The number of inbound substreams that have been created by the peer.
    inbound_substream_attempts: usize,

    last_io_activity: Instant,

    /// Keeps track of whether this connection is for a peer in the mesh. This is used to make
    /// decisions about the keep alive state for this connection.
    in_mesh: bool,
}

/// State of the inbound substream, opened either by us or by the remote.
enum InboundSubstreamState {
    /// Waiting for a message from the remote. The idle state for an inbound substream.
    WaitingInput(GossipFramed<Stream>),
    /// The substream is being closed.
    Closing(GossipFramed<Stream>),
    /// An error occurred during processing.
    Poisoned,
}

/// State of the outbound substream, opened either by us or by the remote.
enum OutboundSubstreamState {
    /// Waiting for the user to send a message. The idle state for an outbound substream.
    WaitingOutput(GossipFramed<Stream>),
    /// Waiting to send a message to the remote.
    PendingSend(GossipFramed<Stream>, types::Rpc),
    /// Waiting to flush the substream so that the data arrives to the remote.
    PendingFlush(GossipFramed<Stream>),
    /// An error occurred during processing.
    Poisoned,
}

impl Handler {
    /// Builds a new [`Handler`].
    pub fn new(
        protocol_config: ProtocolConfig,
        handler_sender: mpsc::Sender<HandlerEvent>,
        handler_receiver: mpsc::Receiver<HandlerIn>,
    ) -> Self {
        Handler {
            handler_sender,
            handler_receiver,
            listen_protocol: protocol_config,
            inbound_substream: None,
            outbound_substream: None,
            outbound_substream_establishing: false,
            outbound_substream_attempts: 0,
            inbound_substream_attempts: 0,
            send_queue: SmallVec::new(),
            last_io_activity: Instant::now(),
            in_mesh: false,
        }
    }

    fn on_fully_negotiated_inbound(&mut self, substream: GossipFramed<Stream>) {
        // new inbound substream. Replace the current one, if it exists.
        tracing::trace!("New inbound substream request");
        self.inbound_substream = Some(InboundSubstreamState::WaitingInput(substream));
    }

    fn on_fully_negotiated_outbound(&mut self, substream: GossipFramed<Stream>) {
        assert!(
            self.outbound_substream.is_none(),
            "Established an outbound substream with one already available"
        );
        self.outbound_substream = Some(OutboundSubstreamState::WaitingOutput(substream));
    }

    async fn run(mut self) {
        // TODO: use select!

        if let Ok(event) = self.handler_receiver.try_recv() {
            match event {
                HandlerIn::Message(m) => self.send_queue.push(m.into()),
                HandlerIn::JoinedMesh => {
                    self.in_mesh = true;
                }
                HandlerIn::LeftMesh => {
                    self.in_mesh = false;
                }
            }
        }

        // process outbound stream
        loop {
            match std::mem::replace(
                &mut self.outbound_substream,
                Some(OutboundSubstreamState::Poisoned),
            ) {
                // outbound idle state
                Some(OutboundSubstreamState::WaitingOutput(substream)) => {
                    if let Some(message) = self.send_queue.pop() {
                        self.send_queue.shrink_to_fit();
                        self.outbound_substream =
                            Some(OutboundSubstreamState::PendingSend(substream, message));
                        continue;
                    }

                    self.outbound_substream =
                        Some(OutboundSubstreamState::WaitingOutput(substream));
                    break;
                }
                Some(OutboundSubstreamState::PendingSend(mut substream, message)) => {
                    match substream.send(message).await {
                        Ok(()) => {
                            self.outbound_substream =
                                Some(OutboundSubstreamState::PendingFlush(substream))
                        }
                        Err(e) => {
                            tracing::debug!("Failed to send message on outbound stream: {e}");
                            self.outbound_substream = None;
                            break;
                        }
                    }
                }
                Some(OutboundSubstreamState::PendingFlush(mut substream)) => {
                    match substream.flush().await {
                        Ok(()) => {
                            self.last_io_activity = Instant::now();
                            self.outbound_substream =
                                Some(OutboundSubstreamState::WaitingOutput(substream))
                        }
                        Err(e) => {
                            tracing::debug!("Failed to flush outbound stream: {e}");
                            self.outbound_substream = None;
                            break;
                        }
                    }
                }
                None => {
                    self.outbound_substream = None;
                    break;
                }
                Some(OutboundSubstreamState::Poisoned) => {
                    unreachable!("Error occurred during outbound stream processing")
                }
            }
        }

        loop {
            match std::mem::replace(
                &mut self.inbound_substream,
                Some(InboundSubstreamState::Poisoned),
            ) {
                // inbound idle state
                Some(InboundSubstreamState::WaitingInput(mut substream)) => {
                    match substream.next().await {
                        Some(Ok(message)) => {
                            self.last_io_activity = Instant::now();
                            self.inbound_substream =
                                Some(InboundSubstreamState::WaitingInput(substream));
                            self.handler_sender.send(message).await;
                            break;
                        }
                        Some(Err(error)) => {
                            tracing::debug!("Failed to read from inbound stream: {error}");
                            // Close this side of the stream. If the
                            // peer is still around, they will re-establish their
                            // outbound stream i.e. our inbound stream.
                            self.inbound_substream =
                                Some(InboundSubstreamState::Closing(substream));
                        }
                        // peer closed the stream
                        None => {
                            tracing::debug!("Inbound stream closed by remote");
                            self.inbound_substream =
                                Some(InboundSubstreamState::Closing(substream));
                        }
                    }
                }
                Some(InboundSubstreamState::Closing(mut substream)) => {
                    match substream.close().await {
                        Err(e) => {
                            // Don't close the connection but just drop the inbound substream.
                            // In case the remote has more to send, they will open up a new
                            // substream.
                            tracing::debug!("Inbound substream error while closing: {e}");
                        }
                        Ok(_) => {}
                    }
                    self.inbound_substream = None;
                    break;
                }
                None => {
                    self.inbound_substream = None;
                    break;
                }
                Some(InboundSubstreamState::Poisoned) => {
                    unreachable!("Error occurred during inbound stream processing")
                }
            }
        }
    }
}
