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
    sync::{mpsc, oneshot},
    task::{JoinError, JoinSet},
};
use tokio_util::time::FutureExt;
use tracing::{debug, error, trace};

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

type BoxedHandler = Box<dyn FnOnce(PoolConnectResult) -> BoxFuture<ExecuteResult> + Send + 'static>;

/// Error when a connection can not be acquired
///
/// This includes the normal iroh connection errors as well as pool specific
/// errors such as timeouts and connection limits.
#[derive(Debug, Clone)]
pub enum PoolConnectError {
    /// Timeout during connect
    Timeout,
    /// Too many connections
    TooManyConnections,
    /// Error during connect
    ConnectError(Arc<ConnectError>),
    /// Error during last execute
    ExecuteError(Arc<ExecuteError>),
    /// Handler actor panicked
    JoinError(Arc<JoinError>),
}

impl std::fmt::Display for PoolConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PoolConnectError::Timeout => write!(f, "Connection timed out"),
            PoolConnectError::TooManyConnections => write!(f, "Too many connections"),
            PoolConnectError::ConnectError(e) => write!(f, "Connection error: {}", e),
            PoolConnectError::ExecuteError(e) => write!(f, "Execution error: {}", e),
            PoolConnectError::JoinError(e) => write!(f, "Join error: {}", e),
        }
    }
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
        Ok(Err(e)) => Err(PoolConnectError::ConnectError(Arc::new(e))),
        Err(_) => Err(PoolConnectError::Timeout),
    };
    if let Err(e) = &state {
        debug!(%node_id, "Failed to connect {e:?}, requesting shutdown");
        if context.owner.close(node_id).await.is_err() {
            return;
        }
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
                        trace!(%node_id, "Received new task");
                        // clear the idle timer
                        idle_timer.as_mut().set_none();
                        tasks.spawn(handler(state.clone()));
                    }
                    None => {
                        // Channel closed - finish remaining tasks and exit
                        break;
                    }
                }
            }

            // Handle completed tasks
            Some(task_result) = tasks.join_next(), if !tasks.is_empty() => {
                match task_result {
                    Ok(Ok(())) => {
                        debug!(%node_id, "Task completed");
                    }
                    Ok(Err(e)) => {
                        error!(%node_id, "Task failed: {}", e);
                        if let Ok(conn) = state {
                            conn.close(1u32.into(), b"error");
                        }
                        state = Err(PoolConnectError::ExecuteError(Arc::new(e)));
                        let _ = context.owner.close(node_id).await;
                    }
                    Err(e) => {
                        error!(%node_id, "Task panicked: {}", e);
                        if let Ok(conn) = state {
                            conn.close(1u32.into(), b"panic");
                        }
                        state = Err(PoolConnectError::JoinError(Arc::new(e)));
                        let _ = context.owner.close(node_id).await;
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
                debug!(%node_id, "Connection idle, requesting shutdown");
                context.owner.close(node_id).await.ok();
                // Don't break here - wait for main actor to close our channel
            }
        }
    }

    // Wait for remaining tasks to complete
    while let Some(task_result) = tasks.join_next().await {
        if let Err(e) = task_result {
            error!(%node_id, "Task failed during shutdown: {}", e);
        }
    }

    if let Ok(connection) = &state {
        connection.close(0u32.into(), b"idle");
    }

    debug!(%node_id, "Connection actor shutting down");
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
                        handler(Err(PoolConnectError::TooManyConnections))
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

impl From<PoolConnectError> for ExecuteError {
    fn from(_: PoolConnectError) -> Self {
        ExecuteError
    }
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
        F: FnOnce(PoolConnectResult) -> Fut + Send + 'static,
        Fut: Future<Output = ExecuteResult> + Send + 'static,
    {
        let handler =
            Box::new(move |conn: PoolConnectResult| Box::pin(f(conn)) as BoxFuture<ExecuteResult>);

        self.tx
            .send(ActorMessage::Handle { id, handler })
            .await
            .map_err(|_| ConnectionPoolError::Shutdown)?;

        Ok(())
    }

    pub async fn with_connection<F, Fut, I, E>(
        &self,
        id: NodeId,
        f: F,
    ) -> Result<Result<Result<I, E>, PoolConnectError>, ConnectionPoolError>
    where
        F: FnOnce(Connection) -> Fut + Send + 'static,
        Fut: Future<Output = Result<I, E>> + Send + 'static,
        I: Send + 'static,
        E: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.connect(id, |conn| async move {
            let (res, ret) = match conn {
                Ok(connection) => {
                    let res = f(connection).await;
                    let ret = match &res {
                        Ok(_) => Ok(()),
                        Err(_) => Err(ExecuteError),
                    };
                    (Ok(res), ret)
                }
                Err(e) => (Err(e), Err(ExecuteError)),
            };
            tx.send(res).ok();
            ret
        })
        .await?;
        rx.await.map_err(|_| ConnectionPoolError::Shutdown)
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
