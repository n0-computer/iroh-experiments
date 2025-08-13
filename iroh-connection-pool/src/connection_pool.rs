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
use n0_future::boxed::BoxFuture;
use snafu::Snafu;
use tokio::{
    sync::mpsc,
    task::{JoinError, JoinSet},
};
use tokio_util::time::FutureExt;

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

type BoxedHandler = Box<dyn FnOnce(&ConnectResult) -> BoxFuture<ExecuteResult> + Send + 'static>;

pub enum ConnectResult {
    /// We got a connection
    Connected(Connection),
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
        Ok(Ok(conn)) => ConnectResult::Connected(conn),
        Ok(Err(e)) => ConnectResult::ConnectError(e),
        Err(_) => ConnectResult::Timeout,
    };
    if !matches!(state, ConnectResult::Connected(_)) {
        context.owner.close(node_id).await.ok();
    }

    let mut tasks = JoinSet::new();
    let idle_timer = tokio::time::sleep(context.options.idle_timeout);
    tokio::pin!(idle_timer);

    loop {
        tokio::select! {
            biased;

            // Handle new work
            handler = rx.recv() => {
                match handler {
                    Some(handler) => {
                        // Reset idle timer by creating a new one
                        idle_timer.set(tokio::time::sleep(context.options.idle_timeout));
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
                        if let ConnectResult::Connected(conn) = state {
                            conn.close(1u32.into(), b"");
                        }
                        state = ConnectResult::ExecuteError(e);
                        let _ = context.owner.close(node_id).await;
                    }
                    Some(Err(e)) => {
                        tracing::error!("Task panicked for node {}: {}", node_id, e);
                        if let ConnectResult::Connected(conn) = state {
                            conn.close(1u32.into(), b"");
                        }
                        state = ConnectResult::JoinError(e);
                        let _ = context.owner.close(node_id).await;
                    }
                    None => {
                        tracing::debug!("Task was cancelled or already completed for node {}", node_id);
                    }
                }

                // If no tasks left and channel is closed, we can exit
                if tasks.is_empty() && rx.is_closed() {
                    break;
                }
            }

            // Idle timeout - request shutdown
            _ = &mut idle_timer, if tasks.is_empty() => {
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

    if let ConnectResult::Connected(connection) = &state {
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
                        handler(&ConnectResult::TooManyConnections).await.ok();
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

#[derive(Debug, Snafu)]
pub enum ConnectionPoolError {
    /// The connection pool has been shut down
    Shutdown,
}

#[derive(Debug, Snafu)]
pub struct ExecuteError;

pub type ExecuteResult = std::result::Result<(), ExecuteError>;

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
    /// If connection establishment succeeds, the handler will be called with a [`ConnectResult::Connected`].
    /// If connection establishment fails, the handler will get passed a [`ConnectResult`] containing the error.
    ///
    /// The fn f is guaranteed to be called exactly once, unless the tokio runtime is shutting down.
    pub async fn connect<F, Fut>(
        &self,
        id: NodeId,
        f: F,
    ) -> std::result::Result<(), ConnectionPoolError>
    where
        F: FnOnce(&ConnectResult) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ExecuteResult> + Send + 'static,
    {
        let handler =
            Box::new(move |conn: &ConnectResult| Box::pin(f(conn)) as BoxFuture<ExecuteResult>);

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
