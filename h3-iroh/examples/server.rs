//! Demonstration of using h3-iroh as a server without framework.
//!
//! run using `cargo run --example server -- --root .`

use std::{path::PathBuf, sync::Arc};

use anyhow::{bail, Result};
use bytes::{Bytes, BytesMut};
use clap::Parser;
use h3::{error::ErrorLevel, quic::BidiStream, server::RequestStream};
use http::{Request, StatusCode};
use iroh::{endpoint::Incoming, Watcher};
use iroh_base::ticket::NodeTicket;
use tokio::{fs::File, io::AsyncReadExt};
use tracing::{debug, error, field, info, info_span, Instrument, Span};

#[derive(Parser, Debug)]
#[command()]
struct Args {
    #[arg(
        short,
        long,
        name = "DIR",
        help = "Root directory to server files from, if omitted server will only respond OK"
    )]
    root: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();

    let args = Args::parse();

    let root = if let Some(root) = args.root {
        if !root.is_dir() {
            bail!("{}: is not a readable directory", root.display());
        } else {
            info!("serving {}", root.display());
            Arc::new(Some(root))
        }
    } else {
        Arc::new(None)
    };

    let ep = iroh::Endpoint::builder()
        .alpns(vec![b"iroh+h3".to_vec()])
        .bind()
        .await?;
    info!("accepting connections on node: {}", ep.node_id());

    // Wait for direct addresses and a RelayUrl before printing a NodeTicket.
    ep.direct_addresses().initialized().await?;
    ep.home_relay().initialized().await?;
    let ticket = NodeTicket::new(ep.node_addr().initialized().await?);
    info!("node ticket: {ticket}");
    info!("run e.g.: cargo run --example client -- iroh+h3://{ticket}/Cargo.toml");

    // Handle incoming connections
    while let Some(incoming) = ep.accept().await {
        tokio::spawn({
            let root = root.clone();
            async move {
                if let Err(err) = handle_connection(incoming, root).await {
                    error!("failed connection: {err:#}");
                }
            }
            .instrument(info_span!("conn", remote_node_id = field::Empty))
        });
    }
    ep.close().await;

    Ok(())
}

async fn handle_connection(incoming: Incoming, root: Arc<Option<PathBuf>>) -> Result<()> {
    let conn = incoming.accept()?.await?;
    let remote_node_id = conn.remote_node_id()?;
    let span = Span::current();
    span.record("remote_node_id", remote_node_id.fmt_short());
    info!("new connection");

    let mut h3_conn = h3::server::Connection::new(h3_iroh::Connection::new(conn)).await?;
    loop {
        match h3_conn.accept().await {
            Ok(Some((req, stream))) => {
                info!(?req, "new request");
                tokio::spawn({
                    let root = root.clone();
                    async move {
                        if let Err(err) = handle_request(req, stream, root).await {
                            error!("request failed: {err:#}");
                        }
                    }
                    .instrument(info_span!("req"))
                });
            }
            Ok(None) => {
                break;
            }
            Err(err) => {
                error!("accept error: {err:#}");
                match err.get_error_level() {
                    ErrorLevel::ConnectionError => break,
                    ErrorLevel::StreamError => continue,
                }
            }
        }
    }

    Ok(())
}

async fn handle_request<T>(
    req: Request<()>,
    mut stream: RequestStream<T, Bytes>,
    serve_root: Arc<Option<PathBuf>>,
) -> Result<()>
where
    T: BidiStream<Bytes>,
{
    let (status, file) = match serve_root.as_deref() {
        None => (StatusCode::OK, None),
        Some(_) if req.uri().path().contains("..") => (StatusCode::NOT_FOUND, None),
        Some(root) => {
            let path = root.join(req.uri().path().strip_prefix('/').unwrap_or(""));
            debug!(path = %path.display(), "Opening file");
            match File::open(&path).await {
                Ok(file) => (StatusCode::OK, Some(file)),
                Err(err) => {
                    error!(path = %path.to_string_lossy(), "failed to open file: {err:#}");
                    (StatusCode::NOT_FOUND, None)
                }
            }
        }
    };

    let resp = http::Response::builder().status(status).body(())?;
    match stream.send_response(resp).await {
        Ok(_) => info!("success"),
        Err(err) => error!("unable to send response: {err:#}"),
    }
    if let Some(mut file) = file {
        loop {
            let mut buf = BytesMut::with_capacity(4096 * 10);
            if file.read_buf(&mut buf).await? == 0 {
                break;
            }
            stream.send_data(buf.freeze()).await?;
        }
    }
    stream.finish().await?;
    Ok(())
}
