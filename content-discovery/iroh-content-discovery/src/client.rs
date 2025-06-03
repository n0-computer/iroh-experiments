use std::future::Future;

use iroh::{
    endpoint::{ConnectOptions, Connection},
    Endpoint, NodeId,
};
use n0_future::{BufferedStreamExt, Stream, StreamExt};
use tracing::{debug, info, trace};

use crate::protocol::{
    Query, QueryResponse, Request, Response, SignedAnnounce, ALPN, REQUEST_SIZE_LIMIT,
};

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
            info!("connected to tracker using possibly 0-rtt: {node_id}");
            announce_impl(connection, signed_announce, zero_rtt_accepted).await
        }
        Err(connecting) => {
            let connection = connecting.await?;
            info!("connected to tracker using 1-rtt: {node_id}");
            announce_impl(connection, signed_announce, async { true }).await
        }
    }
}

async fn announce_impl(
    connection: Connection,
    signed_announce: SignedAnnounce,
    proceed: impl Future<Output = bool>,
) -> anyhow::Result<()> {
    let (mut send, recv) = connection.open_bi().await?;
    debug!("opened bi stream");
    let request = Request::Announce(signed_announce);
    let request = postcard::to_stdvec(&request)?;
    debug!("sending announce");
    send.write_all(&request).await?;
    send.finish()?;
    let mut recv = if proceed.await {
        recv
    } else {
        let (mut send, recv) = connection.open_bi().await?;
        debug!("re-sending announce using 1-rtt");
        send.write_all(&request).await?;
        send.finish()?;
        recv
    };
    let _response = recv.read_to_end(REQUEST_SIZE_LIMIT).await?;
    Ok(())
}

/// Query multiple trackers in parallel and merge the results.
pub fn query_trackers(
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
            info!("connected to tracker using possibly 0-rtt: {node_id}");
            query_impl(connection, args, zero_rtt_accepted).await
        }
        Err(connecting) => {
            let connection = connecting.await?;
            info!("connected to tracker using 1-rtt: {node_id}");
            query_impl(connection, args, async { true }).await
        }
    }?;
    Ok(result.hosts)
}

/// Assume an existing connection to a tracker and query it for peers for some content.
async fn query_impl(
    connection: iroh::endpoint::Connection,
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
