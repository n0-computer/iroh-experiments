pub mod args;

use std::{
    net::{SocketAddrV4, SocketAddrV6},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use clap::Parser;
use iroh::{Endpoint, NodeId};
use iroh_blobs::util::fs::load_secret_key;
use iroh_content_discovery::protocol::ALPN;
use iroh_content_tracker::{
    io::{
        self, load_from_file, setup_logging, tracker_home, tracker_path, CONFIG_DEBUG_FILE,
        CONFIG_DEFAULTS_FILE, CONFIG_FILE, SERVER_KEY_FILE,
    },
    options::Options,
    tracker::Tracker,
};
use tracing::info;

use crate::args::Args;

static VERBOSE: AtomicBool = AtomicBool::new(false);

fn set_verbose(verbose: bool) {
    VERBOSE.store(verbose, Ordering::Relaxed);
}

pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        if $crate::verbose() {
            println!($($arg)*);
        } else {
            info!($($arg)*);
        }
    };
}

/// Wait until the endpoint has figured out it's own DERP region.
async fn await_relay_region(endpoint: &Endpoint) -> anyhow::Result<()> {
    let t0 = Instant::now();
    loop {
        let addr = endpoint.node_addr().await?;
        if addr.relay_url().is_some() {
            break;
        }
        if t0.elapsed() > Duration::from_secs(10) {
            anyhow::bail!("timeout waiting for DERP region");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

async fn create_endpoint(
    key: iroh::SecretKey,
    ipv4_addr: Option<SocketAddrV4>,
    ipv6_addr: Option<SocketAddrV6>,
) -> anyhow::Result<Endpoint> {
    let mut builder = iroh::Endpoint::builder()
        .secret_key(key)
        .discovery_dht()
        .discovery_n0()
        .alpns(vec![ALPN.to_vec()]);
    if let Some(ipv4_addr) = ipv4_addr {
        builder = builder.bind_addr_v4(ipv4_addr);
    }
    if let Some(ipv6_addr) = ipv6_addr {
        builder = builder.bind_addr_v6(ipv6_addr);
    }
    builder.bind().await
}

/// Accept an incoming connection and extract the client-provided [`NodeId`] and ALPN protocol.
pub async fn accept_conn(
    mut conn: iroh::endpoint::Connecting,
) -> anyhow::Result<(NodeId, String, iroh::endpoint::Connection)> {
    let alpn = String::from_utf8(conn.alpn().await?)?;
    let conn = conn.await?;
    let peer_id = conn.remote_node_id()?;
    Ok((peer_id, alpn, conn))
}

/// Write default options to a sample config file.
fn write_debug() -> anyhow::Result<()> {
    let default_path = tracker_path(CONFIG_DEBUG_FILE)?;
    io::save_to_file(Options::debug(), &default_path)?;
    Ok(())
}

/// Write default options to a sample config file.
fn write_defaults() -> anyhow::Result<()> {
    let default_path = tracker_path(CONFIG_DEFAULTS_FILE)?;
    io::save_to_file(Options::default(), &default_path)?;
    Ok(())
}

async fn server(args: Args) -> anyhow::Result<()> {
    set_verbose(!args.quiet);
    let home = tracker_home()?;
    tokio::fs::create_dir_all(&home).await?;
    let config_path = tracker_path(CONFIG_FILE)?;
    write_defaults()?;
    write_debug()?;
    let mut options = load_from_file::<Options>(&config_path)?;
    options.make_paths_relative(&home);
    options.ipv4_bind_addr = args.ipv4_bind_addr;
    options.ipv6_bind_addr = args.ipv6_bind_addr;
    info!("using options: {:#?}", options);
    log!("tracker starting using {}", home.display());
    let key_path = tracker_path(SERVER_KEY_FILE)?;
    let key = load_secret_key(key_path).await?;
    // let server_config = configure_server(&key)?;
    let iroh_endpoint =
        create_endpoint(key.clone(), options.ipv4_bind_addr, options.ipv6_bind_addr).await?;
    let db = Tracker::new(options, iroh_endpoint.clone())?;
    db.dump().await?;
    await_relay_region(&iroh_endpoint).await?;
    let addr = iroh_endpoint.node_addr().await?;
    println!("tracker addr: {}\n", addr.node_id);
    info!("listening on {:?}", addr);
    // let db2 = db.clone();
    let db3 = db.clone();
    // let db4 = db.clone();
    let iroh_accept_task = tokio::spawn(db.iroh_accept_loop(iroh_endpoint));
    // let quinn_accept_task = tokio::spawn(db2.quinn_accept_loop(quinn_endpoint));
    // let udp_accept_task = tokio::spawn(db4.udp_accept_loop(udp_socket));
    let gc_task = tokio::spawn(db3.gc_loop());
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("shutting down");
        }
        res = iroh_accept_task => {
            tracing::error!("iroh accept task exited");
            res??;
        }
        // res = quinn_accept_task => {
        //     tracing::error!("quinn accept task exited");
        //     res??;
        // }
        // res = udp_accept_task => {
        //     tracing::error!("udp accept task exited");
        //     res??;
        // }
        res = gc_task => {
            tracing::error!("gc task exited");
            res??;
        }
    }
    Ok(())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    setup_logging();
    let args = Args::parse();
    server(args).await
}
