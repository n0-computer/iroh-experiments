//! A simple iroh connection pool
//!
//! Entry point is [`ConnectionPool`]. You create a connection pool for a specific
//! ALPN and [`Options`]. Then the pool will manage connections for you.
//!
//! Access to connections is via the [`ConnectionPool::connect`] method, which
//! gives you access to a connection if possible.
//!
//! It is important that you use the connection only in the future passed to
//! connect, and don't clone it out of the future.
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use iroh::{
    Endpoint, NodeId,
    endpoint::{ConnectError, Connection},
};
use n0_future::{MaybeFuture, StreamExt};
use snafu::Snafu;
use tokio::{
    sync::{
        mpsc::{self, error::SendError as TokioSendError},
        oneshot,
    },
    task::JoinError,
};
use tokio_util::time::FutureExt as TimeFutureExt;
use tracing::{debug, error, trace};

use crate::{ConnectionCounter, ConnectionRef};

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

struct Context {
    options: Options,
    endpoint: Endpoint,
    owner: ConnectionPool,
    alpn: Vec<u8>,
}

/// Error when a connection can not be acquired
///
/// This includes the normal iroh connection errors as well as pool specific
/// errors such as timeouts and connection limits.
#[derive(Debug, Clone)]
pub enum PoolConnectError {
    /// Connection pool is shut down
    Shutdown,
    /// Timeout during connect
    Timeout,
    /// Too many connections
    TooManyConnections,
    /// Error during connect
    ConnectError(Arc<ConnectError>),
    /// Handler actor panicked
    JoinError(Arc<JoinError>),
}

impl std::fmt::Display for PoolConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PoolConnectError::Shutdown => write!(f, "Connection pool is shut down"),
            PoolConnectError::Timeout => write!(f, "Connection timed out"),
            PoolConnectError::TooManyConnections => write!(f, "Too many connections"),
            PoolConnectError::ConnectError(e) => write!(f, "Connection error: {}", e),
            PoolConnectError::JoinError(e) => write!(f, "Join error: {}", e),
        }
    }
}

pub type PoolConnectResult = std::result::Result<Connection, PoolConnectError>;

enum ActorMessage {
    RequestRef(RequestRef),
    ConnectionIdle { id: NodeId },
    ConnectionShutdown { id: NodeId },
}

struct RequestRef {
    id: NodeId,
    tx: oneshot::Sender<Result<ConnectionRef, PoolConnectError>>,
}

/// Run a connection actor for a single node
async fn run_connection_actor(
    node_id: NodeId,
    mut rx: mpsc::Receiver<RequestRef>,
    context: Arc<Context>,
) {
    // Connect to the node
    let state = match context
        .endpoint
        .connect(node_id, &context.alpn)
        .timeout(context.options.connect_timeout)
        .await
    {
        Ok(Ok(conn)) => Ok(conn),
        Ok(Err(e)) => Err(PoolConnectError::ConnectError(Arc::new(e))),
        Err(_) => Err(PoolConnectError::Timeout),
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

    tokio::pin!(idle_timer, idle_stream);

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
                                let res = ConnectionRef::new(state.clone(), counter.get_one());

                                // clear the idle timer
                                idle_timer.as_mut().set_none();
                                tx.send(Ok(res)).ok();
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
        }
    }

    if let Ok(connection) = state {
        let reason = if counter.is_idle() { b"idle" } else { b"drop" };
        connection.close(0u32.into(), reason);
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
                    owner: ConnectionPool { tx: tx.clone() },
                }),
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

    pub async fn run(mut self) {
        while let Some(msg) = self.rx.recv().await {
            match msg {
                ActorMessage::RequestRef(mut msg) => {
                    let id = msg.id;
                    self.remove_idle(id);
                    // Try to send to existing connection actor
                    if let Some(conn_tx) = self.connections.get(&id) {
                        if let Err(TokioSendError(e)) = conn_tx.send(msg).await {
                            msg = e;
                        } else {
                            continue;
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
                            continue;
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
pub struct ConnectionPool {
    tx: mpsc::Sender<ActorMessage>,
}

impl ConnectionPool {
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
    ) -> std::result::Result<ConnectionRef, PoolConnectError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorMessage::RequestRef(RequestRef { id, tx }))
            .await
            .map_err(|_| PoolConnectError::Shutdown)?;
        Ok(rx.await.map_err(|_| PoolConnectError::Shutdown)??)
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
