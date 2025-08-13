//! An iroh connection pool that supports 0Rtt connections
//!
//! Entry point is [`ConnectionPool0Rtt`]. You create a connection pool for a specific
//! ALPN and [`Options`]. Then the pool will manage connections for you.
//!
//! Access to connections is via the [`ConnectionPool0Rtt::connect`] method, which
//! gives you access to a connection if possible.
//!
//! It is important that you use the connection only in the future passed to
//! connect, and don't clone it out of the future.
//!
//! For what 0Rtt connections are and why you might want to use them, see this
//! [blog post](https://www.iroh.computer/blog/0rtt-api).
use std::{collections::HashMap, sync::Arc, time::Duration};

use iroh::{
    Endpoint, NodeId,
    endpoint::{ConnectOptions, ConnectWithOptsError, Connection, ConnectionError},
};
use n0_future::{MaybeFuture, boxed::BoxFuture};
use snafu::Snafu;
use tokio::{
    sync::{broadcast, mpsc},
    task::{JoinError, JoinSet},
};
use tokio_util::time::FutureExt;
use tracing::{error, trace};

/// Configuration options for the 0rtt connection pool
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

/// Part of the main actor state that is needed by the connection actors
struct Context {
    options: Options,
    alpn: Vec<u8>,
    endpoint: Endpoint,
    owner: ConnectionPool0Rtt,
}

type BoxedHandler =
    Box<dyn FnOnce(&PoolConnectResult) -> BoxFuture<ExecuteResult> + Send + 'static>;

pub type PoolConnectResult =
    std::result::Result<(Connection, broadcast::Receiver<bool>), PoolConnectError>;

/// Error when a connection can not be acquired
///
/// This includes the normal iroh connection errors as well as pool specific
/// errors such as timeouts and connection limits.
pub enum PoolConnectError {
    /// Timeout during connect
    Timeout,
    /// Too many connections
    TooManyConnections,
    /// Error in the first stage of connect
    ConnectError(ConnectWithOptsError),
    /// Error in the second stage of connect
    ConnectionError(ConnectionError),
    /// Error during usage of the connection
    ExecuteError(ExecuteError),
    /// Connection actor panicked
    JoinError(JoinError),
}

enum ActorMessage {
    Handle { id: NodeId, handler: BoxedHandler },
    ConnectionShutdown { id: NodeId },
}

fn accepted(value: bool) -> broadcast::Receiver<bool> {
    let (tx, rx) = broadcast::channel(1);
    let _ = tx.send(value);
    rx
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
            return (
                Err(PoolConnectError::ConnectError(cause)),
                MaybeFuture::None,
            );
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
                Err(cause) => Err(PoolConnectError::ConnectionError(cause)),
                Ok(connection) => Ok((connection, accepted(true))),
            };
            return (res, MaybeFuture::None);
        }
    };
    let (tx, rx) = broadcast::channel(1);
    let complete = Box::pin(async move {
        tx.send(zero_rtt_accepted.await).ok();
    });
    (Ok((conn, rx)), MaybeFuture::Some(complete))
}

/// Run a connection actor for a single node
async fn run_connection_actor(
    node_id: NodeId,
    mut rx: mpsc::Receiver<BoxedHandler>,
    context: Arc<Context>,
) {
    // Connect to the node
    let (mut state, forwarder) = match connect(&context.endpoint, node_id, &context.alpn)
        .timeout(context.options.connect_timeout)
        .await
    {
        Ok((state, forwarder)) => (state, forwarder),
        Err(_) => (Err(PoolConnectError::Timeout), MaybeFuture::None),
    };
    if state.is_err()
        && context.owner.close(node_id).await.is_err() {
            return;
        }
    let mut tasks = JoinSet::new();
    let idle_timer = MaybeFuture::default();
    tokio::pin!(idle_timer);
    tokio::pin!(forwarder);

    loop {
        tokio::select! {
            biased;

            // Handle new work
            handler = rx.recv() => {
                match handler {
                    Some(handler) => {
                        idle_timer.as_mut().set_none();
                        tasks.spawn(handler(&state));
                    }
                    None => {
                        // Channel closed - finish remaining tasks and exit
                        break;
                    }
                }
            }

            // Handle completed tasks
            task_result = tasks.join_next(), if !tasks.is_empty() => {
                match task_result {
                    Some(Ok(Ok(()))) => {
                        trace!("Task completed for node {}", node_id);
                    }
                    Some(Ok(Err(e))) => {
                        trace!("Task failed for node {}: {}", node_id, e);
                        if let Ok((conn, _)) = state {
                            conn.close(1u32.into(), b"");
                        }
                        state = Err(PoolConnectError::ExecuteError(e));
                        context.owner.close(node_id).await.ok();
                    }
                    Some(Err(e)) => {
                        error!("Task panicked for node {}: {}", node_id, e);
                        if let Ok((conn, _)) = state {
                            conn.close(1u32.into(), b"");
                        }
                        state = Err(PoolConnectError::JoinError(e));
                        context.owner.close(node_id).await.ok();
                    }
                    None => {
                        trace!("Task was cancelled or already completed for node {}", node_id);
                    }
                }

                // We are idle
                if tasks.is_empty() {
                    // If the channel is closed, we can exit
                    if rx.is_closed() {
                        break;
                    }
                    // set the idle timer
                    idle_timer.as_mut().set_future(tokio::time::sleep(context.options.idle_timeout));
                }
            }

            // Idle timeout - request shutdown
            _ = &mut idle_timer => {
                trace!("Connection to {} idle, requesting shutdown", node_id);
                context.owner.close(node_id).await.ok();
                // Don't break here - wait for main actor to close our channel
            }

            // Poll the forwarder if we have one
            _ = &mut forwarder => {}
        }
    }

    // Wait for remaining tasks to complete
    while let Some(task_result) = tasks.join_next().await {
        if let Err(e) = task_result {
            error!("Task panicked during shutdown for node {}: {}", node_id, e);
        }
    }

    if let Ok((conn, _)) = state {
        conn.close(0u32.into(), b"");
    }

    trace!("Connection actor for {} shutting down", node_id);
}

struct Actor {
    rx: mpsc::Receiver<ActorMessage>,
    connections: HashMap<NodeId, mpsc::Sender<BoxedHandler>>,
    context: Arc<Context>,
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
                context: Arc::new(Context {
                    options,
                    alpn: alpn.to_vec(),
                    endpoint,
                    owner: ConnectionPool0Rtt { tx: tx.clone() },
                }),
            },
            tx,
        )
    }

    pub async fn run(mut self) {
        while let Some(msg) = self.rx.recv().await {
            match msg {
                ActorMessage::Handle { id, mut handler } => {
                    // Try to send to existing connection actor
                    if let Some(conn_tx) = self.connections.get(&id) {
                        if let Err(tokio::sync::mpsc::error::SendError(e)) =
                            conn_tx.send(handler).await
                        {
                            handler = e;
                        } else {
                            continue;
                        }
                        // Connection actor died, remove it
                        self.connections.remove(&id);
                    }

                    // No connection actor or it died - spawn a new one
                    if self.connections.len() >= self.context.options.max_connections {
                        handler(&Err(PoolConnectError::TooManyConnections))
                            .await
                            .ok();
                        continue;
                    }
                    let (conn_tx, conn_rx) = mpsc::channel(100);
                    self.connections.insert(id, conn_tx.clone());

                    let context = self.context.clone();
                    tokio::spawn(run_connection_actor(id, conn_rx, context));

                    // Send the handler to the new actor
                    if conn_tx.send(handler).await.is_err() {
                        tracing::error!(
                            "Failed to send handler to new connection actor for {}",
                            id
                        );
                        self.connections.remove(&id);
                    }
                }
                ActorMessage::ConnectionShutdown { id } => {
                    // Remove the connection from our map - this closes the channel
                    self.connections.remove(&id);
                    tracing::debug!("Approved shutdown for connection {}", id);
                }
            }
        }
    }
}

/// Error when calling a fn on the [`ConnectionPool0Rtt`].
///
/// The only thing that can go wrong is that the connection pool is shut down.
#[derive(Debug, Snafu)]
pub enum ConnectionPoolError {
    Shutdown,
}

/// An error during the usage of the connection.
///
/// The connection pool will recreate the connection if a handler returns this
/// error. If you don't want this, swallow the error in the handler.
#[derive(Debug, Snafu)]
pub struct ExecuteError;

type ExecuteResult = std::result::Result<(), ExecuteError>;

/// A connection pool for 0-RTT connections
#[derive(Debug, Clone)]
#[repr(transparent)]
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

    /// Connect to a node and execute the given handler function
    ///
    /// The connection will either be a new connection or an existing one if it is already established.
    /// If connection establishment succeeds, the handler will be called with a [`Ok`].
    /// If connection establishment fails, the handler will get passed a [`Err`] containing the error.
    ///
    /// The fn f is guaranteed to be called exactly once, unless the tokio runtime is shutting down.
    /// If the fn returns an error, it is assumed that the connection is no longer valid. This will cause
    /// the connection to be closed and a new one to be established for future calls.
    pub async fn connect<F, Fut>(
        &self,
        id: NodeId,
        f: F,
    ) -> std::result::Result<(), ConnectionPoolError>
    where
        F: FnOnce(&PoolConnectResult) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ExecuteResult> + Send + 'static,
    {
        let handler =
            Box::new(move |conn: &PoolConnectResult| Box::pin(f(conn)) as BoxFuture<ExecuteResult>);

        self.tx
            .send(ActorMessage::Handle { id, handler })
            .await
            .map_err(|_| ConnectionPoolError::Shutdown)?;

        Ok(())
    }
}
