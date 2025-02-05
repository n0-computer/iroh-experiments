pub mod args;

use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use clap::Parser;
use iroh::{discovery::pkarr::dht::DhtDiscovery, Endpoint, NodeId};
use iroh_blobs::util::fs::load_secret_key;
use iroh_mainline_content_discovery::protocol::ALPN;
use iroh_mainline_tracker::{
    io::{
        self, load_from_file, setup_logging, tracker_home, tracker_path, CONFIG_DEBUG_FILE,
        CONFIG_DEFAULTS_FILE, CONFIG_FILE, SERVER_KEY_FILE,
    },
    options::Options,
    tracker::Tracker,
};

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
    ipv4_addr: SocketAddrV4,
    publish: bool,
) -> anyhow::Result<Endpoint> {
    let mainline_discovery = if publish {
        DhtDiscovery::builder().secret_key(key.clone()).build()?
    } else {
        DhtDiscovery::default()
    };
    iroh::Endpoint::builder()
        .secret_key(key)
        .discovery(Box::new(mainline_discovery))
        .alpns(vec![ALPN.to_vec()])
        .bind_addr_v4(ipv4_addr)
        .bind()
        .await
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
    tracing::info!("using options: {:#?}", options);
    // override options with args
    if let Some(quinn_port) = args.quinn_port {
        options.quinn_port = quinn_port;
    }
    if let Some(addr) = args.iroh_ipv4_addr {
        options.iroh_ipv4_addr = addr;
    }
    if let Some(udp_port) = args.udp_port {
        options.udp_port = udp_port;
    }
    log!("tracker starting using {}", home.display());
    let key_path = tracker_path(SERVER_KEY_FILE)?;
    let key = load_secret_key(key_path).await?;
    let server_config = configure_server(&key)?;
    let udp_bind_addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, options.udp_port));
    let udp_socket = tokio::net::UdpSocket::bind(udp_bind_addr).await?;
    let quinn_bind_addr =
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, options.quinn_port));
    let quinn_endpoint = iroh_quinn::Endpoint::server(server_config, quinn_bind_addr)?;
    // set the quinn port to the actual port we bound to so the DHT will announce it correctly
    options.quinn_port = quinn_endpoint.local_addr()?.port();
    let iroh_endpoint = create_endpoint(key.clone(), options.iroh_ipv4_addr, true).await?;
    let db = Tracker::new(options, iroh_endpoint.clone())?;
    await_relay_region(&iroh_endpoint).await?;
    let addr = iroh_endpoint.node_addr().await?;
    log!("listening on {:?}", addr);
    log!("tracker addr: {}\n", addr.node_id);
    log!("usage:");
    log!("tracker announce --tracker {} <tickets>", addr.node_id);
    log!(
        "tracker query --tracker {} <hash> or <ticket>",
        addr.node_id
    );
    let db2 = db.clone();
    let db3 = db.clone();
    let db4 = db.clone();
    let iroh_accept_task = tokio::spawn(db.iroh_accept_loop(iroh_endpoint));
    let quinn_accept_task = tokio::spawn(db2.quinn_accept_loop(quinn_endpoint));
    let udp_accept_task = tokio::spawn(db4.udp_accept_loop(udp_socket));
    let gc_task = tokio::spawn(db3.gc_loop());
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutting down");
        }
        res = iroh_accept_task => {
            tracing::error!("iroh accept task exited");
            res??;
        }
        res = quinn_accept_task => {
            tracing::error!("quinn accept task exited");
            res??;
        }
        res = udp_accept_task => {
            tracing::error!("udp accept task exited");
            res??;
        }
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

/// Returns default server configuration along with its certificate.
#[allow(clippy::field_reassign_with_default)] // https://github.com/rust-lang/rust-clippy/issues/6527
fn configure_server(secret_key: &iroh::SecretKey) -> anyhow::Result<iroh_quinn::ServerConfig> {
    make_server_config(secret_key, 8, 1024, vec![ALPN.to_vec()])
}

/// Create a [`quinn::ServerConfig`] with the given secret key and limits.
pub fn make_server_config(
    secret_key: &iroh::SecretKey,
    max_streams: u64,
    max_connections: u32,
    alpn_protocols: Vec<Vec<u8>>,
) -> anyhow::Result<iroh_quinn::ServerConfig> {
    let tls_server_config = tls::make_server_config(secret_key, alpn_protocols, false)?;
    let mut server_config = iroh_quinn::ServerConfig::with_crypto(Arc::new(tls_server_config));
    let mut transport_config = iroh_quinn::TransportConfig::default();
    transport_config
        .max_concurrent_bidi_streams(max_streams.try_into()?)
        .max_concurrent_uni_streams(0u32.into());

    server_config
        .transport_config(Arc::new(transport_config))
        .max_incoming(max_connections as usize);
    Ok(server_config)
}
