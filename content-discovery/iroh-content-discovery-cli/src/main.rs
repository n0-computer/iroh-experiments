pub mod args;

use std::str::FromStr;

use anyhow::bail;
use clap::Parser;
use iroh::endpoint::{self, BindError};
use iroh_content_discovery::protocol::{
    AbsoluteTime, Announce, AnnounceKind, Query, QueryFlags, SignedAnnounce,
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::args::{AnnounceArgs, Args, Commands, ContentArg, QueryArgs};

async fn announce(args: AnnounceArgs) -> anyhow::Result<()> {
    // todo: uncomment once the connection problems are fixed
    let Ok(key) = std::env::var("ANNOUNCE_SECRET") else {
        eprintln!("ANNOUNCE_SECRET environment variable must be set to a valid secret key");
        bail!("ANNOUNCE_SECRET env var not set");
    };
    let Ok(key) = iroh::SecretKey::from_str(&key) else {
        bail!("ANNOUNCE_SECRET env var is not a valid secret key");
    };
    let content = args.content.hash_and_format();
    if let ContentArg::Ticket(ticket) = &args.content {
        if ticket.addr().id != key.public() {
            bail!("ticket does not match the announce secret");
        }
    }
    let kind = if args.partial {
        AnnounceKind::Partial
    } else {
        AnnounceKind::Complete
    };
    let timestamp = AbsoluteTime::now();
    let announce = Announce {
        host: key.public(),
        kind,
        content,
        timestamp,
    };
    let endpoint = create_client_endpoint().await?;
    let signed_announce = SignedAnnounce::new(announce, &key)?;
    if !args.tracker.is_empty() {
        for tracker in args.tracker {
            println!("announcing to {tracker}: {content}");
            iroh_content_discovery::announce(&endpoint, tracker, signed_announce).await?;
        }
    }

    println!("done");
    Ok(())
}

async fn query(args: QueryArgs) -> anyhow::Result<()> {
    let query = Query {
        content: args.content.hash_and_format(),
        flags: QueryFlags {
            complete: !args.partial,
            verified: args.verified,
        },
    };
    let ep = create_client_endpoint().await?;
    for tracker in args.tracker {
        let announces: Vec<SignedAnnounce> =
            match iroh_content_discovery::query(&ep, tracker, query).await {
                Ok(announces) => announces,
                Err(e) => {
                    eprintln!("failed to query tracker {tracker}: {e}");
                    continue;
                }
            };
        for announce in announces {
            if announce.verify().is_ok() {
                println!("{}: {:?}", announce.announce.host, announce.announce.kind);
            } else {
                println!("invalid announce");
            }
        }
    }
    Ok(())
}

/// Create an endpoint that does look up discovery info via DNS or the DHT, but does not
/// announce. The client node id is ephemeral and will not be dialed by anyone.
async fn create_client_endpoint() -> Result<endpoint::Endpoint, BindError> {
    let discovery = iroh::discovery::pkarr::dht::DhtDiscovery::builder()
        .dht(true)
        .n0_dns_pkarr_relay();
    endpoint::Endpoint::builder()
        .discovery(discovery)
        .bind()
        .await
}

// set the RUST_LOG env var to one of {debug,info,warn} to see logging info
pub fn setup_logging() {
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(EnvFilter::from_default_env())
        .try_init()
        .ok();
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    setup_logging();
    let args = Args::parse();
    match args.command {
        Commands::Announce(args) => announce(args).await,
        Commands::Query(args) => query(args).await,
    }
}
