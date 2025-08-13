use std::{collections::HashMap, sync::Arc};

use iroh::{
    Endpoint, NodeId,
    endpoint::{ConnectOptions, ConnectWithOptsError, Connection},
};
use snafu::Snafu;
use tokio::{
    sync::{broadcast, mpsc},
    task::{JoinError, JoinSet},
};
use tokio_util::time::FutureExt;
use tracing::{error, trace};

pub struct Options {
    pub idle_timeout: std::time::Duration,
    pub connect_timeout: std::time::Duration,
    pub max_connections: usize,
    pub alpn: Vec<u8>,
}

type BoxedHandler = Box<
    dyn FnOnce(&ConnectResult) -> n0_future::boxed::BoxFuture<std::result::Result<(), ExecuteError>>
        + Send
        + 'static,
>;

pub enum ConnectResult {
    Connected(Connection, broadcast::Receiver<bool>),
    Timeout,
    TooManyConnections,
    ConnectError(ConnectWithOptsError),
    ConnectionError(iroh::endpoint::ConnectionError),
    ExecuteError(ExecuteError),
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
    endpoint: Endpoint,
    node_id: NodeId,
    alpn: &[u8],
) -> (ConnectResult, Option<impl Future<Output = ()>>) {
    let connecting = match endpoint
        .connect_with_opts(node_id, alpn, ConnectOptions::default())
        .await
    {
        Ok(connecting) => connecting,
        Err(cause) => {
            trace!("Failed to connect to node {}: {}", node_id, cause);
            return (ConnectResult::ConnectError(cause), None);
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
                Err(cause) => ConnectResult::ConnectionError(cause),
                Ok(connection) => ConnectResult::Connected(connection, accepted(true)),
            };
            return (res, None);
        }
    };
    let (tx, rx) = broadcast::channel(1);
    let complete = Box::pin(async move {
        tx.send(zero_rtt_accepted.await).ok();
    });
    (ConnectResult::Connected(conn, rx), Some(complete))
}

/// Run a connection actor for a single node
async fn run_connection_actor(
    endpoint: Endpoint,
    node_id: NodeId,
    mut rx: mpsc::Receiver<BoxedHandler>,
    owner: HandlerPool0Rtt,
    options: Arc<Options>,
) {
    // Connect to the node
    let (mut state, mut forwarder) = match connect(endpoint, node_id, &options.alpn)
        .timeout(options.connect_timeout)
        .await
    {
        Ok((state, forwarder)) => (state, forwarder),
        Err(_) => (ConnectResult::Timeout, None),
    };
    if !matches!(state, ConnectResult::Connected(_, _)) {
        owner.close(node_id).await.ok();
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
                        trace!("Task completed for node {}", node_id);
                    }
                    Some(Ok(Err(e))) => {
                        trace!("Task failed for node {}: {}", node_id, e);
                        if let ConnectResult::Connected(conn, _) = state {
                            conn.close(1u32.into(), b"");
                        }
                        state = ConnectResult::ExecuteError(e);
                        owner.close(node_id).await.ok();
                    }
                    Some(Err(e)) => {
                        error!("Task panicked for node {}: {}", node_id, e);
                        if let ConnectResult::Connected(conn, _) = state {
                            conn.close(1u32.into(), b"");
                        }
                        state = ConnectResult::JoinError(e);
                        owner.close(node_id).await.ok();
                    }
                    None => {
                        trace!("Task was cancelled or already completed for node {}", node_id);
                    }
                }

                // If no tasks left and channel is closed, we can exit
                if tasks.is_empty() && rx.is_closed() {
                    break;
                }
            }

            // Idle timeout - request shutdown
            _ = &mut idle_timer, if tasks.is_empty() => {
                trace!("Connection to {} idle, requesting shutdown", node_id);
                owner.close(node_id).await.ok();
                // Don't break here - wait for main actor to close our channel
            }

            _ = forwarder.as_mut().unwrap(), if forwarder.is_some() => {
                forwarder = None;
            }
        }
    }

    // Wait for remaining tasks to complete
    while let Some(task_result) = tasks.join_next().await {
        if let Err(e) = task_result {
            error!("Task panicked during shutdown for node {}: {}", node_id, e);
        }
    }

    if let ConnectResult::Connected(conn, _) = state {
        conn.close(0u32.into(), b"");
    }

    trace!("Connection actor for {} shutting down", node_id);
}

struct Actor {
    tx: HandlerPool0Rtt,
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
                tx: HandlerPool0Rtt { tx: tx.clone() },
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
                    if self.connections.len() >= self.options.max_connections {
                        handler(&ConnectResult::TooManyConnections).await.ok();
                        continue;
                    }
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
    Shutdown,
}

#[derive(Debug, Snafu)]
pub struct ExecuteError;

#[derive(Debug, Clone)]
#[repr(transparent)]
pub struct HandlerPool0Rtt {
    tx: mpsc::Sender<ActorMessage>,
}

impl HandlerPool0Rtt {
    pub fn new(endpoint: Endpoint, options: Options) -> Self {
        let (actor, tx) = Actor::new(endpoint, options);

        // Spawn the main actor
        tokio::spawn(actor.run());

        Self { tx }
    }

    /// Close an existing connection, if it exists
    ///
    /// This will finish pending tasks and close the connection. New tasks will
    /// get a new connection if they are submitted after this call
    pub async fn close(&self, id: NodeId) -> std::result::Result<(), HandlerPoolError> {
        self.tx
            .send(ActorMessage::ConnectionShutdown { id })
            .await
            .map_err(|_| HandlerPoolError::Shutdown)?;
        Ok(())
    }

    /// Connect to a node and execute the given handler function
    ///
    /// The connection will either be a new connection or an existing one if it is already established.
    /// If connection establishment succeeds, the handler will be called with a [`ConnectResult::Connected`].
    /// If connection establishment fails, the handler will get passed a [`ConnectResult`] containing the error.
    ///
    /// The fn f is guaranteed to be called exactly once, unless the tokio runtime is shutting down.
    /// If the fn returns an error, it is assumed that the connection is no longer valid. This will cause
    /// the connection to be closed and a new one to be established for future calls.
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
