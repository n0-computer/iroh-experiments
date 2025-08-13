use std::{collections::HashMap, sync::Arc};

use iroh::{
    Endpoint, NodeId,
    endpoint::{ConnectError, Connection},
};
use snafu::Snafu;
use tokio::{
    sync::mpsc,
    task::{JoinError, JoinSet},
};
use tokio_util::time::FutureExt;

pub struct Options {
    pub idle_timeout: std::time::Duration,
    pub connect_timeout: std::time::Duration,
    pub alpn: Vec<u8>,
}

type BoxedHandler = Box<
    dyn FnOnce(&ConnectResult) -> n0_future::boxed::BoxFuture<std::result::Result<(), ExecuteError>>
        + Send
        + 'static,
>;

pub enum ConnectResult {
    /// We got a connection
    Connected(Connection),
    /// Timeout during connect
    Timeout,
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
    endpoint: Endpoint,
    node_id: NodeId,
    mut rx: mpsc::Receiver<BoxedHandler>,
    main_tx: mpsc::Sender<ActorMessage>,
    options: Arc<Options>,
) {
    // Connect to the node
    let mut state = match endpoint
        .connect(node_id, &options.alpn)
        .timeout(options.connect_timeout)
        .await
    {
        Ok(Ok(conn)) => ConnectResult::Connected(conn),
        Ok(Err(e)) => ConnectResult::ConnectError(e),
        Err(_) => ConnectResult::Timeout,
    };
    if !matches!(state, ConnectResult::Connected(_)) {
        let _ = main_tx
            .send(ActorMessage::ConnectionShutdown { id: node_id })
            .await;
    }

    let mut tasks = JoinSet::new();
    let idle_timer = tokio::time::sleep(options.idle_timeout);
    tokio::pin!(idle_timer);

    loop {
        tokio::select! {
            biased;

            // Handle new work
            handler = rx.recv() => {
                match handler {
                    Some(handler) => {
                        // Reset idle timer by creating a new one
                        idle_timer.set(tokio::time::sleep(options.idle_timeout));
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
                        let _ = main_tx.send(ActorMessage::ConnectionShutdown { id: node_id }).await;
                    }
                    Some(Err(e)) => {
                        tracing::error!("Task panicked for node {}: {}", node_id, e);
                        if let ConnectResult::Connected(conn) = state {
                            conn.close(1u32.into(), b"");
                        }
                        state = ConnectResult::JoinError(e);
                        let _ = main_tx.send(ActorMessage::ConnectionShutdown { id: node_id }).await;
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
                let _ = main_tx.send(ActorMessage::ConnectionShutdown { id: node_id }).await;
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
    tx: mpsc::Sender<ActorMessage>,
    rx: mpsc::Receiver<ActorMessage>,
    endpoint: Endpoint,
    connections: HashMap<NodeId, mpsc::Sender<BoxedHandler>>,
    options: Arc<Options>,
}

impl Actor {
    pub fn new(endpoint: Endpoint, options: Options) -> (Self, mpsc::Sender<ActorMessage>) {
        let (tx, rx) = mpsc::channel(100);
        (
            Self {
                rx,
                tx: tx.clone(),
                endpoint,
                connections: HashMap::new(),
                options: Arc::new(options),
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
                    let (conn_tx, conn_rx) = mpsc::channel(100);
                    self.connections.insert(id, conn_tx.clone());

                    let endpoint = self.endpoint.clone();
                    let main_tx = self.tx.clone(); // Assuming we store the sender
                    let options = self.options.clone();

                    tokio::spawn(run_connection_actor(
                        endpoint, id, conn_rx, main_tx, options,
                    ));

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
pub enum HandlerPoolError {
    /// The handler pool has been shut down
    Shutdown,
}

#[derive(Debug, Snafu)]
pub struct ExecuteError;

pub struct HandlerPool {
    tx: mpsc::Sender<ActorMessage>,
}

impl HandlerPool {
    pub fn new(endpoint: Endpoint, options: Options) -> Self {
        let (actor, tx) = Actor::new(endpoint, options);

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
    ) -> std::result::Result<(), HandlerPoolError>
    where
        F: FnOnce(&ConnectResult) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = std::result::Result<(), ExecuteError>> + Send + 'static,
    {
        let handler = Box::new(move |conn: &ConnectResult| {
            Box::pin(f(conn)) as n0_future::boxed::BoxFuture<std::result::Result<(), ExecuteError>>
        });

        self.tx
            .send(ActorMessage::Handle { id, handler })
            .await
            .map_err(|_| HandlerPoolError::Shutdown)?;

        Ok(())
    }
}
