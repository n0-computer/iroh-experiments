use std::{
    pin::Pin,
    task::{Context, Poll},
};

use futures::{stream::FusedStream, Stream, StreamExt};
use iroh::{Endpoint, NodeId};

use crate::protocol::{
    Query, QueryResponse, Request, Response, SignedAnnounce, ALPN, REQUEST_SIZE_LIMIT,
};

/// Announce to a tracker.
///
/// You can only announce content you yourself claim to have, to avoid spamming other nodes.
///
/// `endpoint` is the iroh endpoint to use for announcing.
/// `tracker` is the node id of the tracker to announce to. It must understand the [TRACKER_ALPN] protocol.
/// `content` is the content to announce.
/// `kind` is the kind of the announcement. We can claim to have the complete data or only some of it.
pub async fn announce_iroh(
    connection: iroh::endpoint::Connection,
    signed_announce: SignedAnnounce,
) -> anyhow::Result<()> {
    let (mut send, mut recv) = connection.open_bi().await?;
    tracing::debug!("opened bi stream");
    let request = Request::Announce(signed_announce);
    let request = postcard::to_stdvec(&request)?;
    tracing::debug!("sending announce");
    send.write_all(&request).await?;
    send.finish()?;
    let _response = recv.read_to_end(REQUEST_SIZE_LIMIT).await?;
    Ok(())
}

async fn query_iroh_one(
    endpoint: Endpoint,
    node_id: NodeId,
    args: Query,
) -> anyhow::Result<Vec<SignedAnnounce>> {
    let connection = endpoint.connect(node_id, ALPN).await?;
    let result = query_iroh(connection, args).await?;
    Ok(result.hosts)
}

/// Query multiple trackers in parallel and merge the results.
pub fn query_trackers(
    endpoint: Endpoint,
    trackers: impl IntoIterator<Item = NodeId>,
    args: Query,
    query_parallelism: usize,
) -> impl Stream<Item = anyhow::Result<SignedAnnounce>> {
    futures::stream::iter(trackers)
        .map(move |tracker| {
            let endpoint = endpoint.clone();
            async move {
                let hosts = match query_iroh_one(endpoint, tracker, args).await {
                    Ok(hosts) => hosts.into_iter().map(anyhow::Ok).collect(),
                    Err(cause) => vec![Err(cause)],
                };
                futures::stream::iter(hosts)
            }
        })
        .buffer_unordered(query_parallelism)
        .flatten()
}

/// Assume an existing connection to a tracker and query it for peers for some content.
pub async fn query_iroh(
    connection: iroh::endpoint::Connection,
    args: Query,
) -> anyhow::Result<QueryResponse> {
    tracing::info!("connected to {:?}", connection.remote_node_id()?);
    let (mut send, mut recv) = connection.open_bi().await?;
    tracing::info!("opened bi stream");
    let request = Request::Query(args);
    let request = postcard::to_stdvec(&request)?;
    tracing::info!("sending query");
    send.write_all(&request).await?;
    send.finish()?;
    let response = recv.read_to_end(REQUEST_SIZE_LIMIT).await?;
    let response = postcard::from_bytes::<Response>(&response)?;
    Ok(match response {
        Response::QueryResponse(response) => response,
    })
}

pub enum Connection {
    Iroh(iroh::endpoint::Connection),
    Quinn(quinn::Connection),
}

/// Create a iroh endpoint and connect to a tracker using the [crate::protocol::ALPN] protocol.
pub async fn connect(tracker: NodeId) -> anyhow::Result<iroh::endpoint::Connection> {
    // todo: uncomment once the connection problems are fixed
    // for now, a random node id is more reliable.
    // let key = load_secret_key(tracker_path(CLIENT_KEY)?).await?;
    let key = iroh::SecretKey::generate(rand::thread_rng());
    let endpoint = Endpoint::builder().secret_key(key).bind().await?;
    tracing::info!("trying to connect to tracker at {:?}", tracker);
    let connection = endpoint.connect(tracker, ALPN).await?;
    Ok(connection)
}

pub struct MergeUntilFirstEnds<S1, S2> {
    stream1: S1,
    stream2: S2,
}

impl<S1, S2> MergeUntilFirstEnds<S1, S2>
where
    S1: Stream + Unpin,
    S2: Stream<Item = S1::Item> + Unpin,
{
    pub fn new(stream1: S1, stream2: S2) -> Self {
        Self { stream1, stream2 }
    }
}

impl<S1, S2> Stream for MergeUntilFirstEnds<S1, S2>
where
    S1: Stream + Unpin,
    S2: Stream<Item = S1::Item> + Unpin,
{
    type Item = S1::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let poll1 = Pin::new(&mut self.stream1).poll_next(cx);
        let poll2 = Pin::new(&mut self.stream2).poll_next(cx);

        match (poll1, poll2) {
            (Poll::Ready(None), _) | (_, Poll::Ready(None)) => Poll::Ready(None),
            (Poll::Ready(Some(item)), _) | (_, Poll::Ready(Some(item))) => Poll::Ready(Some(item)),
            (Poll::Pending, Poll::Pending) => Poll::Pending,
        }
    }
}

impl<S1, S2> FusedStream for MergeUntilFirstEnds<S1, S2>
where
    S1: Stream + FusedStream + Unpin,
    S2: Stream<Item = S1::Item> + FusedStream + Unpin,
{
    fn is_terminated(&self) -> bool {
        self.stream1.is_terminated() && self.stream2.is_terminated()
    }
}
