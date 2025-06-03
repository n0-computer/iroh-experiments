use std::future::Future;

use iroh::{
    endpoint::{ConnectOptions, Connection},
    Endpoint, NodeId,
};
use n0_future::{BufferedStreamExt, Stream, StreamExt};
use tracing::trace;

use crate::protocol::{
    Query, QueryResponse, Request, Response, SignedAnnounce, ALPN, REQUEST_SIZE_LIMIT,
};

/// Announce to multiple trackers in parallel.
pub fn announce_all(
    endpoint: Endpoint,
    trackers: impl IntoIterator<Item = NodeId>,
    signed_announce: SignedAnnounce,
    announce_parallelism: usize,
) -> impl Stream<Item = (NodeId, anyhow::Result<()>)> {
    n0_future::stream::iter(trackers)
        .map(move |tracker| {
            let endpoint = endpoint.clone();
            let signed_announce = signed_announce.clone();
            async move {
                let res = announce(&endpoint, tracker, signed_announce).await;
                (tracker, res)
            }
        })
        .buffered_unordered(announce_parallelism)
}

/// Announce to a tracker.
///
/// You can only announce content you yourself claim to have, to avoid spamming other nodes.
///
/// `endpoint` is the iroh endpoint to use for announcing.
/// `tracker` is the node id of the tracker to announce to. It must understand the [crate::ALPN] protocol.
/// `content` is the content to announce.
/// `kind` is the kind of the announcement. We can claim to have the complete data or only some of it.
pub async fn announce(
    endpoint: &Endpoint,
    node_id: NodeId,
    signed_announce: SignedAnnounce,
) -> anyhow::Result<()> {
    let connecting = endpoint
        .connect_with_opts(node_id, ALPN, ConnectOptions::default())
        .await?;
    match connecting.into_0rtt() {
        Ok((connection, zero_rtt_accepted)) => {
            trace!("connected to tracker using possibly 0-rtt: {node_id}");
            announce_conn(&connection, signed_announce, zero_rtt_accepted).await?;
            wait_for_session_ticket(connection);
            Ok(())
        }
        Err(connecting) => {
            let connection = connecting.await?;
            trace!("connected to tracker using 1-rtt: {node_id}");
            announce_conn(&connection, signed_announce, async { true }).await?;
            connection.close(0u32.into(), b"");
            Ok(())
        }
    }
}

/// Announce via an existing connection.
///
/// The proceed future can be used to reattempt the send, which can be useful if the connection
/// was established using 0-rtt and the tracker does not support it. If you have an existing
/// 1-rtt connection, you can pass `async { true }` to proceed immediately.
pub async fn announce_conn(
    connection: &Connection,
    signed_announce: SignedAnnounce,
    proceed: impl Future<Output = bool>,
) -> anyhow::Result<()> {
    let (mut send, recv) = connection.open_bi().await?;
    let request = Request::Announce(signed_announce);
    let request = postcard::to_stdvec(&request)?;
    trace!("sending announce");
    send.write_all(&request).await?;
    send.finish()?;
    let mut recv = if proceed.await {
        recv
    } else {
        let (mut send, recv) = connection.open_bi().await?;
        trace!("re-sending announce using 1-rtt");
        send.write_all(&request).await?;
        send.finish()?;
        recv
    };
    let _response = recv.read_to_end(REQUEST_SIZE_LIMIT).await?;
    trace!("got response");
    Ok(())
}

/// A single query to a tracker, using 0-rtt if possible.
pub async fn query(
    endpoint: &Endpoint,
    node_id: NodeId,
    args: Query,
) -> anyhow::Result<Vec<SignedAnnounce>> {
    let connecting = endpoint
        .connect_with_opts(node_id, ALPN, ConnectOptions::default())
        .await?;
    let result = match connecting.into_0rtt() {
        Ok((connection, zero_rtt_accepted)) => {
            trace!("connected to tracker using possibly 0-rtt: {node_id}");
            let res = query_conn(&connection, args, zero_rtt_accepted).await?;
            wait_for_session_ticket(connection);
            res
        }
        Err(connecting) => {
            let connection = connecting.await?;
            trace!("connected to tracker using 1-rtt: {node_id}");
            let res = query_conn(&connection, args, async { true }).await?;
            connection.close(0u32.into(), b"");
            res
        }
    };
    Ok(result.hosts)
}

/// Query multiple trackers in parallel and merge the results.
///
/// You will lose the information about which tracker the results came from, so if you need that,
/// use [`query`] instead.
pub fn query_all(
    endpoint: Endpoint,
    trackers: impl IntoIterator<Item = NodeId>,
    args: Query,
    query_parallelism: usize,
) -> impl Stream<Item = anyhow::Result<SignedAnnounce>> {
    n0_future::stream::iter(trackers)
        .map(move |tracker| {
            let endpoint = endpoint.clone();
            async move {
                let hosts = match query(&endpoint, tracker, args).await {
                    Ok(hosts) => hosts.into_iter().map(anyhow::Ok).collect(),
                    Err(cause) => vec![Err(cause)],
                };
                n0_future::stream::iter(hosts)
            }
        })
        .buffered_unordered(query_parallelism)
        .flatten()
}

/// Query via an existing connection.
///
/// The proceed future can be used to reattempt the send, which can be useful if the connection
/// was established using 0-rtt and the tracker does not support it. If you have an existing
/// 1-rtt connection, you can pass `async { true }` to proceed immediately.
pub async fn query_conn(
    connection: &Connection,
    args: Query,
    proceed: impl Future<Output = bool>,
) -> anyhow::Result<QueryResponse> {
    let request = Request::Query(args);
    let request = postcard::to_stdvec(&request)?;
    trace!("connected to {:?}", connection.remote_node_id()?);
    trace!("opened bi stream");
    let (mut send, recv) = connection.open_bi().await?;
    trace!("sending query");
    send.write_all(&request).await?;
    send.finish()?;
    let mut recv = if proceed.await {
        recv
    } else {
        let (mut send, recv) = connection.open_bi().await?;
        trace!("sending query again using 1-rtt");
        send.write_all(&request).await?;
        send.finish()?;
        recv
    };
    let response = recv.read_to_end(REQUEST_SIZE_LIMIT).await?;
    let response = postcard::from_bytes::<Response>(&response)?;
    Ok(match response {
        Response::QueryResponse(response) => response,
    })
}

fn wait_for_session_ticket(connection: Connection) {
    tokio::spawn(async move {
        // todo: use a more precise API for waiting once it is available.
        // See https://github.com/quinn-rs/quinn/pull/2257
        tokio::time::sleep(connection.rtt() * 2).await;
        connection.close(0u32.into(), b"");
    });
}
