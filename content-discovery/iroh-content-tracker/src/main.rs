pub mod args;

use std::{
    net::{SocketAddrV4, SocketAddrV6},
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use clap::Parser;
use iroh::{endpoint::BindError, Endpoint, Watcher};
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
        let addr = endpoint.node_addr().initialized().await;
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
) -> Result<Endpoint, BindError> {
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
    let endpoint =
        create_endpoint(key.clone(), options.ipv4_bind_addr, options.ipv6_bind_addr).await?;
    let db = Tracker::new(options, endpoint.clone())?;
    db.dump().await?;
    await_relay_region(&endpoint).await?;
    let addr = endpoint.node_addr().initialized().await;
    println!("tracker addr: {}\n", addr.node_id);
    info!("listening on {:?}", addr);
    // let db2 = db.clone();
    let db3 = db.clone();
    // let db4 = db.clone();
    let accept_task = tokio::spawn(db.accept_loop(endpoint));
    // let quinn_accept_task = tokio::spawn(db2.quinn_accept_loop(quinn_endpoint));
    // let udp_accept_task = tokio::spawn(db4.udp_accept_loop(udp_socket));
    let gc_task = tokio::spawn(db3.gc_loop());
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("shutting down");
        }
        res = accept_task => {
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

pub async fn load_secret_key(key_path: PathBuf) -> anyhow::Result<iroh::SecretKey> {
    use anyhow::Context;
    use iroh::SecretKey;
    use tokio::io::AsyncWriteExt;

    if key_path.exists() {
        let keystr = tokio::fs::read(key_path).await?;

        let ser_key = ssh_key::private::PrivateKey::from_openssh(keystr)?;
        let ssh_key::private::KeypairData::Ed25519(kp) = ser_key.key_data() else {
            anyhow::bail!("invalid key format");
        };
        let secret_key = SecretKey::from_bytes(&kp.private.to_bytes());
        Ok(secret_key)
    } else {
        let secret_key = SecretKey::generate(rand::rngs::OsRng);
        let ckey = ssh_key::private::Ed25519Keypair {
            public: secret_key.public().public().into(),
            private: secret_key.secret().into(),
        };
        let ser_key =
            ssh_key::private::PrivateKey::from(ckey).to_openssh(ssh_key::LineEnding::default())?;

        // Try to canonicalize if possible
        let key_path = key_path.canonicalize().unwrap_or(key_path);
        let key_path_parent = key_path.parent().ok_or_else(|| {
            anyhow::anyhow!("no parent directory found for '{}'", key_path.display())
        })?;
        tokio::fs::create_dir_all(&key_path_parent).await?;

        // write to tempfile
        let (file, temp_file_path) = tempfile::NamedTempFile::new_in(key_path_parent)
            .context("unable to create tempfile")?
            .into_parts();
        let mut file = tokio::fs::File::from_std(file);
        file.write_all(ser_key.as_bytes())
            .await
            .context("unable to write keyfile")?;
        file.flush().await?;
        drop(file);

        // move file
        tokio::fs::rename(temp_file_path, key_path)
            .await
            .context("failed to rename keyfile")?;

        Ok(secret_key)
    }
}
