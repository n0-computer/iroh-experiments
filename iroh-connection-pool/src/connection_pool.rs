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
use std::{collections::HashMap, sync::Arc, time::Duration};

use iroh::{
    Endpoint, NodeId,
    endpoint::{ConnectError, Connection},
};
use n0_future::{MaybeFuture, boxed::BoxFuture};
use snafu::Snafu;
use tokio::{
    sync::mpsc,
    task::{JoinError, JoinSet},
};
use tokio_util::time::FutureExt;

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

type BoxedHandler =
    Box<dyn FnOnce(&PoolConnectResult) -> BoxFuture<ExecuteResult> + Send + 'static>;

/// Error when a connection can not be acquired
///
/// This includes the normal iroh connection errors as well as pool specific
/// errors such as timeouts and connection limits.
pub enum PoolConnectError {
    /// Timeout during connect
    Timeout,
    /// Too many connections
    TooManyConnections,
    /// Error during connect
    ConnectError(ConnectError),
    /// Error during last execute
    ExecuteError(ExecuteError),
    /// Handler actor panicked
    JoinError(JoinError),
}

pub type PoolConnectResult = std::result::Result<Connection, PoolConnectError>;

enum ActorMessage {
    Handle { id: NodeId, handler: BoxedHandler },
    ConnectionShutdown { id: NodeId },
}

/// Run a connection actor for a single node
async fn run_connection_actor(
    node_id: NodeId,
    mut rx: mpsc::Receiver<BoxedHandler>,
    context: Arc<Context>,
) {
    // Connect to the node
    let mut state = match context
        .endpoint
        .connect(node_id, &context.alpn)
        .timeout(context.options.connect_timeout)
        .await
    {
        Ok(Ok(conn)) => Ok(conn),
        Ok(Err(e)) => Err(PoolConnectError::ConnectError(e)),
        Err(_) => Err(PoolConnectError::Timeout),
    };
    if state.is_err()
        && context.owner.close(node_id).await.is_err() {
            return;
        }

    let mut tasks = JoinSet::new();
    let idle_timer = MaybeFuture::default();
    tokio::pin!(idle_timer);

    loop {
        tokio::select! {
            biased;

            // Handle new work
            handler = rx.recv() => {
                match handler {
                    Some(handler) => {
                        // clear the idle timer
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
                        tracing::debug!("Task completed for node {}", node_id);
                    }
                    Some(Ok(Err(e))) => {
                        tracing::error!("Task failed for node {}: {}", node_id, e);
                        if let Ok(conn) = state {
                            conn.close(1u32.into(), b"");
                        }
                        state = Err(PoolConnectError::ExecuteError(e));
                        let _ = context.owner.close(node_id).await;
                    }
                    Some(Err(e)) => {
                        tracing::error!("Task panicked for node {}: {}", node_id, e);
                        if let Ok(conn) = state {
                            conn.close(1u32.into(), b"");
                        }
                        state = Err(PoolConnectError::JoinError(e));
                        let _ = context.owner.close(node_id).await;
                    }
                    None => {
                        tracing::debug!("Task was cancelled or already completed for node {}", node_id);
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
                tracing::debug!("Connection to {} idle, requesting shutdown", node_id);

                context.owner.close(node_id).await.ok();
                // Don't break here - wait for main actor to close our channel
            }
        }
    }

    // Wait for remaining tasks to complete
    while let Some(task_result) = tasks.join_next().await {
        if let Err(e) = task_result {
            tracing::error!("Task failed during shutdown for node {}: {}", node_id, e);
        }
    }

    if let Ok(connection) = &state {
        connection.close(0u32.into(), b"");
    }

    tracing::debug!("Connection actor for {} shutting down", node_id);
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
                    owner: ConnectionPool { tx: tx.clone() },
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

/// Error when calling a fn on the [`ConnectionPool`].
///
/// The only thing that can go wrong is that the connection pool is shut down.
#[derive(Debug, Snafu)]
pub enum ConnectionPoolError {
    /// The connection pool has been shut down
    Shutdown,
}

/// An error during the usage of the connection.
///
/// The connection pool will recreate the connection if a handler returns this
/// error. If you don't want this, swallow the error in the handler.
#[derive(Debug, Snafu)]
pub struct ExecuteError;

type ExecuteResult = std::result::Result<(), ExecuteError>;

/// A connection pool
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

    /// Connect to a node and execute the given handler function
    ///
    /// The connection will either be a new connection or an existing one if it is already established.
    /// If connection establishment succeeds, the handler will be called with a [`Ok`].
    /// If connection establishment fails, the handler will get passed a [`Err`] containing the error.
    ///
    /// The fn f is guaranteed to be called exactly once, unless the tokio runtime is shutting down.
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
}
