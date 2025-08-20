//! An iroh connection pool that supports 0Rtt connections
//!
//! Entry point is [`ConnectionPool0Rtt`]. You create a connection pool for a specific
//! ALPN and [`Options`]. Then the pool will manage connections for you.
//!
//! Access to connections is via the [`ConnectionPool0Rtt::connect`] method, which
//! gives you access to a connection if possible.
//!
//! For what 0Rtt connections are and why you might want to use them, see this
//! [blog post](https://www.iroh.computer/blog/0rtt-api).
use std::{
    collections::{HashMap, VecDeque},
    ops::Deref,
    pin::Pin,
    sync::Arc,
    task::Poll,
    time::Duration,
};

use iroh::{
    Endpoint, NodeId,
    endpoint::{ConnectOptions, ConnectWithOptsError, Connection, ConnectionError},
};
use n0_future::{FutureExt, MaybeFuture, StreamExt};
use snafu::Snafu;
use tokio::{
    sync::{
        broadcast,
        mpsc::{self, error::SendError as TokioSendError},
        oneshot,
    },
    task::JoinSet,
};
use tokio_util::time::FutureExt as TimeFutureExt;
use tracing::{debug, error, trace};

use crate::{ConnectionCounter, OneConnection};

/// Configuration options for the connection pool
#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub idle_timeout: Duration,
    pub connect_timeout: Duration,
    pub max_connections: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(1),
            max_connections: 1024,
        }
    }
}

/// A reference to a connection that is owned by a connection pool.
#[derive(Debug)]
pub struct ConnectionRef {
    connection: iroh::endpoint::Connection,
    _permit: OneConnection,
}

impl Deref for ConnectionRef {
    type Target = iroh::endpoint::Connection;

    fn deref(&self) -> &Self::Target {
        &self.connection
    }
}

impl ConnectionRef {
    fn new(connection: iroh::endpoint::Connection, counter: OneConnection) -> Self {
        Self {
            connection,
            _permit: counter,
        }
    }
}

struct Context {
    options: Options,
    endpoint: Endpoint,
    owner: ConnectionPool0Rtt,
    alpn: Vec<u8>,
}

/// Error when a connection can not be acquired
///
/// This includes the normal iroh connection errors as well as pool specific
/// errors such as timeouts and connection limits.
#[derive(Debug, Clone, Snafu)]
#[snafu(module)]
pub enum PoolConnectError {
    /// Connection pool is shut down
    Shutdown,
    /// Timeout during connect
    Timeout,
    /// Too many connections
    TooManyConnections,
    /// Error during connect
    ConnectError { source: Arc<ConnectWithOptsError> },
    /// Error during connect
    ConnectionError { source: Arc<ConnectionError> },
}

impl From<ConnectWithOptsError> for PoolConnectError {
    fn from(e: ConnectWithOptsError) -> Self {
        PoolConnectError::ConnectError {
            source: Arc::new(e),
        }
    }
}

impl From<ConnectionError> for PoolConnectError {
    fn from(e: ConnectionError) -> Self {
        PoolConnectError::ConnectionError {
            source: Arc::new(e),
        }
    }
}

pub type PoolConnectResult =
    std::result::Result<(Connection, Option<broadcast::Sender<bool>>), PoolConnectError>;

/// Future that completes when a connection is fully established
pub enum ConnectionState {
    /// The connection is in the handshake phase, the future will resolve when the handshake is complete
    Handshake(n0_future::boxed::BoxFuture<bool>),
    /// TThe connection is already fully established
    FullyEstablished,
}

impl Future for ConnectionState {
    type Output = bool;

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        match self.get_mut() {
            ConnectionState::Handshake(rx) => rx.poll(cx),
            ConnectionState::FullyEstablished => Poll::Ready(true),
        }
    }
}

enum ActorMessage {
    RequestRef(RequestRef),
    ConnectionIdle { id: NodeId },
    ConnectionShutdown { id: NodeId },
}

struct RequestRef {
    id: NodeId,
    tx: oneshot::Sender<Result<(ConnectionRef, ConnectionState), PoolConnectError>>,
}

/// Run a connection actor for a single node
async fn run_connection_actor(
    node_id: NodeId,
    mut rx: mpsc::Receiver<RequestRef>,
    context: Arc<Context>,
) {
    // Connect to the node
    let (state, forwarder) = match connect(&context.endpoint, node_id, &context.alpn)
        .timeout(context.options.connect_timeout)
        .await
    {
        Ok((state, forwarder)) => (state, forwarder),
        Err(_) => (Err(PoolConnectError::Timeout), MaybeFuture::None),
    };
    if let Err(e) = &state {
        debug!(%node_id, "Failed to connect {e:?}, requesting shutdown");
        if context.owner.close(node_id).await.is_err() {
            return;
        }
    }
    let counter = ConnectionCounter::new();
    let idle_timer = MaybeFuture::default();
    let idle_stream = counter.clone().idle_stream();

    tokio::pin!(idle_timer, idle_stream, forwarder);

    loop {
        tokio::select! {
            biased;

            // Handle new work
            handler = rx.recv() => {
                match handler {
                    Some(RequestRef { id, tx }) => {
                        assert!(id == node_id, "Not for me!");
                        match &state {
                            Ok(state) => {
                                let conn = state.0.clone();
                                let handshake_complete = match &state.1 {
                                    Some(tx) => ConnectionState::Handshake({
                                        let mut recv = tx.subscribe();
                                        Box::pin(async move { recv.recv().await.unwrap_or_default() })
                                    }),
                                    None => ConnectionState::FullyEstablished,
                                };
                                let res = ConnectionRef::new(conn, counter.get_one());

                                // clear the idle timer
                                idle_timer.as_mut().set_none();
                                tx.send(Ok((res, handshake_complete))).ok();
                            }
                            Err(cause) => {
                                tx.send(Err(cause.clone())).ok();
                            }
                        }
                    }
                    None => {
                        // Channel closed - finish remaining tasks and exit
                        break;
                    }
                }
            }

            _ = idle_stream.next() => {
                if !counter.is_idle() {
                    continue;
                };
                // notify the pool that we are idle.
                trace!(%node_id, "Idle");
                if context.owner.idle(node_id).await.is_err() {
                    // If we can't notify the pool, we are shutting down
                    break;
                }
                // set the idle timer
                idle_timer.as_mut().set_future(tokio::time::sleep(context.options.idle_timeout));
            }

            // Idle timeout - request shutdown
            _ = &mut idle_timer => {
                trace!(%node_id, "Idle timer expired, requesting shutdown");
                context.owner.close(node_id).await.ok();
                // Don't break here - wait for main actor to close our channel
            }

            _ = &mut forwarder => {}
        }
    }

    if let Ok(connection) = state {
        let reason = if counter.is_idle() { b"idle" } else { b"drop" };
        connection.0.close(0u32.into(), reason);
    }

    trace!(%node_id, "Connection actor shutting down");
}

struct Actor {
    rx: mpsc::Receiver<ActorMessage>,
    connections: HashMap<NodeId, mpsc::Sender<RequestRef>>,
    context: Arc<Context>,
    // idle set (most recent last)
    // todo: use a better data structure if this becomes a performance issue
    idle: VecDeque<NodeId>,
    tasks: JoinSet<()>,
}

impl Actor {
    pub fn new(
        endpoint: Endpoint,
        alpn: &[u8],
        options: Options,
    ) -> (Self, mpsc::Sender<ActorMessage>) {
        let (tx, rx) = mpsc::channel(100);
        (
            Self {
                rx,
                connections: HashMap::new(),
                idle: VecDeque::new(),
                context: Arc::new(Context {
                    options,
                    alpn: alpn.to_vec(),
                    endpoint,
                    owner: ConnectionPool0Rtt { tx: tx.clone() },
                }),
                tasks: JoinSet::new(),
            },
            tx,
        )
    }

    fn add_idle(&mut self, id: NodeId) {
        self.remove_idle(id);
        self.idle.push_back(id);
    }

    fn remove_idle(&mut self, id: NodeId) {
        self.idle.retain(|&x| x != id);
    }

    fn pop_oldest_idle(&mut self) -> Option<NodeId> {
        self.idle.pop_front()
    }

    fn remove_connection(&mut self, id: NodeId) {
        self.connections.remove(&id);
        self.remove_idle(id);
    }

    async fn handle_msg(&mut self, msg: ActorMessage) {
        match msg {
            ActorMessage::RequestRef(mut msg) => {
                let id = msg.id;
                self.remove_idle(id);
                // Try to send to existing connection actor
                if let Some(conn_tx) = self.connections.get(&id) {
                    if let Err(TokioSendError(e)) = conn_tx.send(msg).await {
                        msg = e;
                    } else {
                        return;
                    }
                    // Connection actor died, remove it
                    self.remove_connection(id);
                }

                // No connection actor or it died - check limits
                if self.connections.len() >= self.context.options.max_connections {
                    if let Some(idle) = self.pop_oldest_idle() {
                        // remove the oldest idle connection to make room for one more
                        trace!("removing oldest idle connection {}", idle);
                        self.connections.remove(&idle);
                    } else {
                        msg.tx.send(Err(PoolConnectError::TooManyConnections)).ok();
                        return;
                    }
                }
                let (conn_tx, conn_rx) = mpsc::channel(100);
                self.connections.insert(id, conn_tx.clone());

                let context = self.context.clone();

                tokio::spawn(run_connection_actor(id, conn_rx, context));

                // Send the handler to the new actor
                if conn_tx.send(msg).await.is_err() {
                    error!(%id, "Failed to send handler to new connection actor");
                    self.connections.remove(&id);
                }
            }
            ActorMessage::ConnectionIdle { id } => {
                self.add_idle(id);
                trace!(%id, "connection idle");
            }
            ActorMessage::ConnectionShutdown { id } => {
                // Remove the connection from our map - this closes the channel
                self.remove_connection(id);
                trace!(%id, "removed connection");
            }
        }
    }

    pub async fn run(mut self) {
        loop {
            tokio::select! {
                biased;

                msg = self.rx.recv() => {
                    if let Some(msg) = msg {
                        self.handle_msg(msg).await;
                    } else {
                        break;
                    }
                }

                res = self.tasks.join_next(), if !self.tasks.is_empty() => {
                    if let Some(Err(e)) = res {
                        // panic during either connection establishment or
                        // timeout. Message handling is outside the actor's
                        // control, so we should hopefully never get this.
                        error!("Connection actor failed: {e}");
                    }
                }
            }
        }
    }
}

/// Error when calling a fn on the [`ConnectionPool`].
///
/// The only thing that can go wrong is that the connection pool is shut down.
#[derive(Debug, Snafu)]
pub enum ConnectionPoolError {
    /// The connection pool has been shut down
    Shutdown,
}

/// A connection pool
#[derive(Debug, Clone)]
pub struct ConnectionPool0Rtt {
    tx: mpsc::Sender<ActorMessage>,
}

impl ConnectionPool0Rtt {
    pub fn new(endpoint: Endpoint, alpn: &[u8], options: Options) -> Self {
        let (actor, tx) = Actor::new(endpoint, alpn, options);

        // Spawn the main actor
        tokio::spawn(actor.run());

        Self { tx }
    }

    /// Returns either a fresh connection or a reference to an existing one.
    ///
    /// This is guaranteed to return after approximately [Options::connect_timeout]
    /// with either an error or a connection.
    pub async fn connect(
        &self,
        id: NodeId,
    ) -> std::result::Result<(ConnectionRef, ConnectionState), PoolConnectError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorMessage::RequestRef(RequestRef { id, tx }))
            .await
            .map_err(|_| PoolConnectError::Shutdown)?;
        rx.await.map_err(|_| PoolConnectError::Shutdown)?
    }

    /// Close an existing connection, if it exists
    ///
    /// This will finish pending tasks and close the connection. New tasks will
    /// get a new connection if they are submitted after this call
    pub async fn close(&self, id: NodeId) -> std::result::Result<(), ConnectionPoolError> {
        self.tx
            .send(ActorMessage::ConnectionShutdown { id })
            .await
            .map_err(|_| ConnectionPoolError::Shutdown)?;
        Ok(())
    }

    /// Notify the connection pool that a connection is idle.
    ///
    /// Should only be called from connection handlers.
    pub(crate) async fn idle(&self, id: NodeId) -> std::result::Result<(), ConnectionPoolError> {
        self.tx
            .send(ActorMessage::ConnectionIdle { id })
            .await
            .map_err(|_| ConnectionPoolError::Shutdown)?;
        Ok(())
    }
}

async fn connect(
    endpoint: &Endpoint,
    node_id: NodeId,
    alpn: &[u8],
) -> (PoolConnectResult, MaybeFuture<impl Future<Output = ()>>) {
    let connecting = match endpoint
        .connect_with_opts(node_id, alpn, ConnectOptions::default())
        .await
    {
        Ok(connecting) => connecting,
        Err(cause) => {
            trace!("Failed to connect to node {}: {}", node_id, cause);
            return (Err(PoolConnectError::from(cause)), MaybeFuture::None);
        }
    };
    let (conn, zero_rtt_accepted) = match connecting.into_0rtt() {
        Ok((conn, accepted)) => {
            trace!("Connected to node {} with 0-RTT", node_id);
            (conn, accepted)
        }
        Err(connecting) => {
            trace!("Failed to connect using 0-RTT to node {}", node_id);
            let res = match connecting.await {
                Err(cause) => Err(PoolConnectError::from(cause)),
                Ok(connection) => Ok((connection, None)),
            };
            return (res, MaybeFuture::None);
        }
    };
    let (tx, _) = broadcast::channel(1);
    let tx2 = tx.clone();
    let complete = Box::pin(async move {
        tx2.send(zero_rtt_accepted.await).ok();
    });
    (Ok((conn, Some(tx))), MaybeFuture::Some(complete))
}
