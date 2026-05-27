use std::result;

use iroh::{
    endpoint::{
        ConnectOptions, Connection, ConnectionState, OutgoingZeroRtt, RecvStream, ZeroRttStatus,
    },
    Endpoint, EndpointId,
};
use n0_future::{BufferedStreamExt, Stream, StreamExt};
use snafu::prelude::*;
use tracing::trace;

use crate::protocol::{
    Query, QueryResponse, Request, Response, SignedAnnounce, ALPN, REQUEST_SIZE_LIMIT,
};

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Failed to connect to tracker: {}", source))]
    Connect {
        source: iroh::endpoint::ConnectWithOptsError,
        backtrace: snafu::Backtrace,
    },

    #[snafu(display("Failed connect to tracker using 1-rtt: {}", source))]
    Connect1Rtt {
        source: iroh::endpoint::ConnectingError,
        backtrace: snafu::Backtrace,
    },

    #[snafu(display("Failed to open bidi stream to tracker: {}", source))]
    OpenStream {
        source: iroh::endpoint::ConnectionError,
        backtrace: snafu::Backtrace,
    },

    #[snafu(display("Failed to serialize request: {}", source))]
    SerializeRequest {
        source: postcard::Error,
        backtrace: snafu::Backtrace,
    },

    #[snafu(display("Failed to write data: {}", source))]
    WriteRequest {
        source: iroh::endpoint::WriteError,
        backtrace: snafu::Backtrace,
    },

    #[snafu(display("Failed to finish: {}", source))]
    FinishWrite {
        source: iroh::endpoint::ClosedStream,
        backtrace: snafu::Backtrace,
    },

    #[snafu(display("Failed to read response: {}", source))]
    ReadResponse {
        source: iroh::endpoint::ReadToEndError,
        backtrace: snafu::Backtrace,
    },

    #[snafu(display("Failed to deserialize response: {}", source))]
    DeserializeResponse {
        source: postcard::Error,
        backtrace: snafu::Backtrace,
    },
}

pub type Result<T> = result::Result<T, Error>;

/// Announce to multiple trackers in parallel.
pub fn announce_all(
    endpoint: Endpoint,
    trackers: impl IntoIterator<Item = EndpointId>,
    signed_announce: SignedAnnounce,
    announce_parallelism: usize,
) -> impl Stream<Item = (EndpointId, Result<()>)> {
    n0_future::stream::iter(trackers)
        .map(move |tracker| {
            let endpoint = endpoint.clone();
            async move {
                let res = announce(&endpoint, tracker, signed_announce).await;
                (tracker, res)
            }
        })
        .buffered_unordered(announce_parallelism)
}

/// Announce to a tracker.
///
/// You can only announce content you yourself claim to have, to avoid spamming other endpoints.
///
/// `endpoint` is the iroh endpoint to use for announcing.
/// `tracker` is the endpoint id of the tracker to announce to. It must understand the [crate::ALPN] protocol.
/// `content` is the content to announce.
/// `kind` is the kind of the announcement. We can claim to have the complete data or only some of it.
pub async fn announce(
    endpoint: &Endpoint,
    endpoint_id: EndpointId,
    signed_announce: SignedAnnounce,
) -> Result<()> {
    let connecting = endpoint
        .connect_with_opts(endpoint_id, ALPN, ConnectOptions::default())
        .await
        .context(ConnectSnafu)?;
    match connecting.into_0rtt() {
        Ok(connection) => {
            trace!("connected to tracker using possibly 0-rtt: {endpoint_id}");
            let connection = announce_conn_0rtt(connection, signed_announce).await?;
            wait_for_session_ticket(connection);
            Ok(())
        }
        Err(connecting) => {
            let connection = connecting.await.context(Connect1RttSnafu)?;
            trace!("connected to tracker using 1-rtt: {endpoint_id}");
            announce_conn(&connection, signed_announce).await?;
            connection.close(0u32.into(), b"");
            Ok(())
        }
    }
}

pub fn create_announce_request(signed_announce: SignedAnnounce) -> Result<Vec<u8>> {
    let request = Request::Announce(signed_announce);
    postcard::to_stdvec(&request).context(SerializeRequestSnafu)
}

pub async fn send_announce<S: ConnectionState>(
    connection: &Connection<S>,
    request: &[u8],
) -> Result<RecvStream> {
    let (mut send, recv) = connection.open_bi().await.context(OpenStreamSnafu)?;
    trace!("sending announce");
    send.write_all(request).await.context(WriteRequestSnafu)?;
    send.finish().context(FinishWriteSnafu)?;
    Ok(recv)
}

pub async fn recv_response(mut recv: RecvStream) -> Result<()> {
    let _response = recv
        .read_to_end(REQUEST_SIZE_LIMIT)
        .await
        .context(ReadResponseSnafu)?;
    trace!("got response");
    Ok(())
}

pub async fn announce_conn(connection: &Connection, signed_announce: SignedAnnounce) -> Result<()> {
    let request = create_announce_request(signed_announce)?;
    let recv = send_announce(connection, &request).await?;
    recv_response(recv).await
}

/// Announce via an existing 0rtt connection.
pub async fn announce_conn_0rtt(
    connection: Connection<OutgoingZeroRtt>,
    signed_announce: SignedAnnounce,
) -> Result<Connection> {
    // Send via 0-RTT
    let request = create_announce_request(signed_announce)?;
    let recv = send_announce(&connection, &request).await?;

    // Check if 0-RTT was accepted
    let status = connection
        .handshake_completed()
        .await
        .context(Connect1RttSnafu)?;
    let (connection, recv) = match status {
        ZeroRttStatus::Accepted(conn) => (conn, recv),
        ZeroRttStatus::Rejected(conn) => {
            // Resend on a new stream
            let recv = send_announce(&conn, &request).await?;
            (conn, recv)
        }
    };
    recv_response(recv).await?;
    Ok(connection)
}

/// A single query to a tracker, using 0-rtt if possible.
pub async fn query(
    endpoint: &Endpoint,
    endpoint_id: EndpointId,
    args: Query,
) -> Result<Vec<SignedAnnounce>> {
    let connecting = endpoint
        .connect_with_opts(endpoint_id, ALPN, ConnectOptions::default())
        .await
        .context(ConnectSnafu)?;
    let result = match connecting.into_0rtt() {
        Ok(connection) => {
            trace!("connected to tracker using possibly 0-rtt: {endpoint_id}");
            // Send via 0-RTT
            let request = Request::Query(args);
            let request = postcard::to_stdvec(&request).context(SerializeRequestSnafu)?;
            trace!("connected to {:?}", connection.remote_id());
            let (mut send, recv) = connection.open_bi().await.context(OpenStreamSnafu)?;
            trace!("sending query");
            send.write_all(&request).await.context(WriteRequestSnafu)?;
            send.finish().context(FinishWriteSnafu)?;

            // Check if 0-RTT was accepted
            let status = connection
                .handshake_completed()
                .await
                .context(Connect1RttSnafu)?;
            let (connection, mut recv) = match status {
                ZeroRttStatus::Accepted(conn) => (conn, recv),
                ZeroRttStatus::Rejected(conn) => {
                    // Resend on a new stream
                    let (mut send, recv) = conn.open_bi().await.context(OpenStreamSnafu)?;
                    trace!("sending query again using 1-rtt");
                    send.write_all(&request).await.context(WriteRequestSnafu)?;
                    send.finish().context(FinishWriteSnafu)?;
                    (conn, recv)
                }
            };
            let response = recv
                .read_to_end(REQUEST_SIZE_LIMIT)
                .await
                .context(ReadResponseSnafu)?;
            let response =
                postcard::from_bytes::<Response>(&response).context(DeserializeResponseSnafu)?;
            let Response::QueryResponse(res) = response;
            wait_for_session_ticket(connection);
            res
        }
        Err(connecting) => {
            let connection = connecting.await.context(Connect1RttSnafu)?;
            trace!("connected to tracker using 1-rtt: {endpoint_id}");
            let res = query_conn(&connection, args).await?;
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
    trackers: impl IntoIterator<Item = EndpointId>,
    args: Query,
    query_parallelism: usize,
) -> impl Stream<Item = Result<SignedAnnounce>> {
    n0_future::stream::iter(trackers)
        .map(move |tracker| {
            let endpoint = endpoint.clone();
            async move {
                let hosts = match query(&endpoint, tracker, args).await {
                    Ok(hosts) => hosts.into_iter().map(Ok).collect(),
                    Err(cause) => vec![Err(cause)],
                };
                n0_future::stream::iter(hosts)
            }
        })
        .buffered_unordered(query_parallelism)
        .flatten()
}

/// Query via an existing connection.
pub async fn query_conn<S: ConnectionState>(
    connection: &Connection<S>,
    args: Query,
) -> Result<QueryResponse> {
    let request = Request::Query(args);
    let request = postcard::to_stdvec(&request).context(SerializeRequestSnafu)?;
    trace!("sending query");
    let (mut send, mut recv) = connection.open_bi().await.context(OpenStreamSnafu)?;
    send.write_all(&request).await.context(WriteRequestSnafu)?;
    send.finish().context(FinishWriteSnafu)?;
    let response = recv
        .read_to_end(REQUEST_SIZE_LIMIT)
        .await
        .context(ReadResponseSnafu)?;
    let response = postcard::from_bytes::<Response>(&response).context(DeserializeResponseSnafu)?;
    Ok(match response {
        Response::QueryResponse(response) => response,
    })
}

fn wait_for_session_ticket(connection: Connection<iroh::endpoint::HandshakeCompleted>) {
    tokio::spawn(async move {
        // todo: use a more precise API for waiting once it is available.
        // See https://github.com/quinn-rs/quinn/pull/2257
        let wait_time = connection
            .paths()
            .iter()
            .find(|p| p.is_selected())
            .map(|p| p.rtt() * 2)
            .unwrap_or(std::time::Duration::from_millis(100));
        tokio::time::sleep(wait_time).await;
        connection.close(0u32.into(), b"");
    });
}
