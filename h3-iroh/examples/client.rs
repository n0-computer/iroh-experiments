use std::future;
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use clap::Parser;
use iroh_net::ticket::NodeTicket;
use iroh_net::NodeAddr;
use tokio::io::AsyncWriteExt;
use tracing::info;

#[derive(Parser, Debug)]
#[command()]
struct Args {
    #[arg(long, help = "Use SSLKEYLOGFILE environment variable to log TLS keys")]
    keylogfile: bool,
    #[arg()]
    uri: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();

    let args = Args::parse();

    let uri: http::Uri = args.uri.parse()?;
    if uri.scheme_str() != Some("iroh+h3") {
        bail!("URI scheme must be iroh+h3");
    }
    let ticket = uri.host().context("missing hostname in URI")?;
    let ticket = NodeTicket::from_str(ticket)?;
    let addr: NodeAddr = ticket.into();

    let ep = iroh_net::Endpoint::builder()
        .keylog(args.keylogfile)
        .bind()
        .await?;

    let conn = ep.connect(addr, b"iroh+h3").await?;
    let conn = h3_iroh::Connection::new(conn);

    let (mut driver, mut send_request) = h3::client::new(conn).await?;

    let drive_fut = async move {
        future::poll_fn(|cx| driver.poll_close(cx)).await?;
        Ok::<(), anyhow::Error>(())
    };

    let req_fut = async move {
        info!("sending request");
        let req = http::Request::builder().uri(uri).body(())?;
        let mut stream = send_request.send_request(req).await?;
        stream.finish().await?;

        info!("receiving response");
        let resp = stream.recv_response().await?;
        info!(
            version = ?resp.version(),
            status = ?resp.status(),
            headers = ?resp.headers(),
            "response",
        );
        while let Some(mut chunk) = stream.recv_data().await? {
            info!("chunk!");
            let mut out = tokio::io::stdout();
            out.write_all_buf(&mut chunk).await?;
            out.flush().await?;
        }

        Ok::<(), anyhow::Error>(())
    };

    let (req_res, drive_res) = tokio::join!(req_fut, drive_fut);
    req_res?;
    drive_res?;

    info!("closing ep");
    ep.close(1u8.into(), b"ep closing").await?;
    info!("ep closed");

    Ok(())
}
