//! Support for an axum server.

use anyhow::Result;
use axum::Router;
use bytes::{Buf, Bytes};
use h3::{quic::BidiStream, server::RequestStream};
use http::{Request, Response, Version};
use http_body_util::BodyExt;
use iroh::Endpoint;
use tower::Service;
use tracing::{debug, error, info_span, trace, warn, Instrument};

static ALPN: &[u8] = b"iroh+h3";

/// Serves an axum router over iroh using the h3 protocol.
///
/// This implementation is not production ready.  E.g. it copies the entire requests and
/// responses in memory.  It serves more as an example.
pub async fn serve(endpoint: Endpoint, router: axum::Router) -> Result<()> {
    endpoint.set_alpns(vec![ALPN.to_vec()]);
    while let Some(incoming) = endpoint.accept().await {
        trace!("accepting connection");
        let router = router.clone();
        tokio::spawn(
            async move {
                if let Err(err) = handle_connection(incoming, router).await {
                    warn!("error accepting connection: {err:#}");
                }
            }
            .instrument(info_span!("h3-connection")),
        );
    }
    endpoint.close().await;
    Ok(())
}

async fn handle_connection(incoming: iroh::endpoint::Incoming, router: Router) -> Result<()> {
    debug!("new connection established");
    let conn = incoming.await?;
    let conn = crate::Connection::new(conn);
    let mut conn = h3::server::Connection::new(conn).await?;
    loop {
        match conn.accept().await {
            Ok(Some(req_resolver)) => match req_resolver.resolve_request().await {
                Ok((req, stream)) => {
                    let router = router.clone();
                    tokio::spawn(
                        async move {
                            if let Err(err) = handle_request(req, stream, router.clone()).await {
                                warn!("handling request failed: {err}");
                            }
                        }
                        .instrument(info_span!("h3-request")),
                    );
                }
                Err(err) => {
                    error!("stream error: {err}");
                    continue;
                }
            },
            Ok(None) => {
                break; // No more streams to be recieved
            }

            Err(err) => {
                error!("connection error: {err}");
                break;
            }
        }
    }
    Ok(())
}

/// Handles a single request, buffering the entire request.
async fn handle_request<T>(
    req: Request<()>,
    mut stream: RequestStream<T, Bytes>,
    mut tower_service: Router,
) -> Result<()>
where
    T: BidiStream<Bytes>,
{
    debug!("new request: {:#?}", req);
    // TODO: All this copying is no good.
    let mut body = vec![];
    while let Ok(Some(data)) = stream.recv_data().await {
        body.extend_from_slice(data.chunk());
    }
    let body = http_body_util::Full::new(Bytes::from(body));

    let mut builder = axum::extract::Request::builder()
        .version(req.version())
        .uri(req.uri())
        .method(req.method());
    *builder.headers_mut().expect("builder invariant") = req.headers().clone();
    let req = builder.body(body)?;

    // Call Service
    let res = tower_service.call(req).await?;

    let mut builder = Response::builder()
        .status(res.status())
        .version(Version::HTTP_3);
    *builder.headers_mut().expect("builder invariant") = res.headers().clone();
    let response = builder.body(())?;

    stream.send_response(response).await.unwrap();

    // send response body and trailers
    let mut buf = res.into_body().into_data_stream();
    while let Some(chunk) = buf.frame().await {
        match chunk {
            Ok(frame) => {
                if frame.is_data() {
                    let data = frame.into_data().unwrap();
                    stream.send_data(data).await.unwrap();
                } else if frame.is_trailers() {
                    let trailers = frame.into_trailers().unwrap();
                    stream.send_trailers(trailers).await.unwrap();
                }
            }
            Err(err) => {
                warn!("Failed to read frame from response body stream: {err:#}");
            }
        }
    }
    trace!("done");
    Ok(stream.finish().await?)
}
