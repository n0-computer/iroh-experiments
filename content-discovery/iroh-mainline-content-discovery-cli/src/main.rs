pub mod args;

use std::str::FromStr;

use clap::Parser;
use iroh::endpoint;
use iroh_mainline_content_discovery::protocol::{
    AbsoluteTime, Announce, AnnounceKind, Query, QueryFlags, SignedAnnounce,
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::args::{AnnounceArgs, Args, Commands, QueryArgs};

async fn announce(args: AnnounceArgs) -> anyhow::Result<()> {
    // todo: uncomment once the connection problems are fixed
    let Ok(key) = std::env::var("ANNOUNCE_SECRET") else {
        eprintln!("ANNOUNCE_SECRET environment variable must be set to a valid secret key");
        anyhow::bail!("ANNOUNCE_SECRET env var not set");
    };
    let Ok(key) = iroh::SecretKey::from_str(&key) else {
        anyhow::bail!("ANNOUNCE_SECRET env var is not a valid secret key");
    };
    let content = args.content.hash_and_format();
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
    let signed_announce = SignedAnnounce::new(announce, &key)?;
    if !args.tracker.is_empty() {
        let iroh_endpoint = endpoint::Endpoint::builder().bind().await?;
        for tracker in args.tracker {
            println!("announcing via magicsock to {:?}: {}", tracker, content);
            let connection = iroh_endpoint
                .connect(tracker, iroh_mainline_content_discovery::protocol::ALPN)
                .await?;
            iroh_mainline_content_discovery::announce(connection, signed_announce).await?;
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
    let ep = endpoint::Endpoint::builder()
        .discovery_dht()
        .discovery_n0()
        .bind()
        .await?;
    for tracker in args.tracker {
        let conn = ep
            .connect(tracker, iroh_mainline_content_discovery::protocol::ALPN)
            .await?;
        let res = match iroh_mainline_content_discovery::query(conn, query).await {
            Ok(res) => res,
            Err(e) => {
                eprintln!("failed to query tracker {}: {}", tracker, e);
                continue;
            }
        };
        for announce in res.hosts {
            if announce.verify().is_ok() {
                println!("{}: {:?}", announce.announce.host, announce.announce.kind);
            } else {
                println!("invalid announce");
            }
        }
    }
    Ok(())
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
