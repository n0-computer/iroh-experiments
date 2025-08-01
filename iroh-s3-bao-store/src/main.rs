use anyhow::Context;
use clap::{Parser, Subcommand};
use indicatif::{
    HumanBytes, HumanDuration, MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle,
};
use iroh::{Endpoint, NodeAddr, SecretKey};
use iroh_blobs::{
    provider::{self, handle_connection, CustomEventSender, EventSender},
    ticket::BlobTicket,
    util::local_pool::LocalPool,
    BlobFormat,
};
use iroh_io::{AsyncSliceReaderExt, HttpAdapter};
use iroh_s3_bao_store::S3Store;
use serde::Deserialize;
use std::net::{SocketAddrV4, SocketAddrV6};
use std::{
    fmt::{Display, Formatter},
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use url::Url;

/// Send a file or directory between two machines, using blake3 verified streaming.
///
/// For all subcommands, you can specify a secret key using the IROH_SECRET
/// environment variable. If you don't, a random one will be generated.
///
/// You can also specify the address for the iroh socket. If you don't, a random one
/// will be chosen.
#[derive(Parser, Debug)]
pub struct Args {
    #[clap(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    #[default]
    Hex,
    Cid,
}

impl FromStr for Format {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "hex" => Ok(Format::Hex),
            "cid" => Ok(Format::Cid),
            _ => Err(anyhow::anyhow!("invalid format")),
        }
    }
}

impl Display for Format {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Format::Hex => write!(f, "hex"),
            Format::Cid => write!(f, "cid"),
        }
    }
}

fn print_hash(hash: &iroh_blobs::Hash, format: Format) -> String {
    match format {
        Format::Hex => hash.to_hex().to_string(),
        Format::Cid => hash.to_string(),
    }
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Send a file or directory.
    ServeS3(ServeS3Args),

    /// Receive a file or directory.
    ServeUrls(ImportS3Args),
}

#[derive(Parser, Debug)]
pub struct CommonArgs {
    /// The IPv4 addr for the iroh socket to listen on.
    ///
    /// Defauls to a random free port, but it can be useful to specify a fixed
    /// port, e.g. to configure a firewall rule.
    #[clap(long)]
    pub iroh_ipv4_addr: Option<SocketAddrV4>,

    /// The IPv6 addr for the iroh socket to listen on.
    #[clap(long)]
    pub iroh_ipv6_addr: Option<SocketAddrV6>,

    #[clap(long, default_value_t = Format::Hex)]
    pub format: Format,

    #[clap(short = 'v', long, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

#[derive(Parser, Debug)]
pub struct ServeS3Args {
    /// Url to the s3 bucket root.
    pub url: Url,

    /// Top level directory name.
    #[clap(long)]
    pub name: Option<String>,

    #[clap(flatten)]
    pub common: CommonArgs,
}

#[derive(Parser, Debug)]
pub struct ImportS3Args {
    /// Url to the s3 bucket root.
    pub url: Vec<Url>,

    #[clap(flatten)]
    pub common: CommonArgs,
}

/// Get the secret key or generate a new one.
///
/// Print the secret key to stderr if it was generated, so the user can save it.
fn get_or_create_secret(print: bool) -> anyhow::Result<SecretKey> {
    match std::env::var("IROH_SECRET") {
        Ok(secret) => SecretKey::from_str(&secret).context("invalid secret"),
        Err(_) => {
            let key = SecretKey::generate(rand::thread_rng());
            if print {
                eprintln!("using secret key {key}");
            }
            Ok(key)
        }
    }
}

#[derive(Debug, Clone)]
struct SendStatus {
    /// the multiprogress bar
    mp: MultiProgress,
}

impl SendStatus {
    fn new() -> Self {
        let mp = MultiProgress::new();
        mp.set_draw_target(ProgressDrawTarget::stderr());
        Self { mp }
    }

    fn new_client(&self) -> ClientStatus {
        let current = self.mp.add(ProgressBar::hidden());
        current.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.green} [{elapsed_precise}] {msg}")
                .unwrap(),
        );
        current.enable_steady_tick(Duration::from_millis(100));
        current.set_message("waiting for requests");
        ClientStatus {
            current: current.into(),
        }
    }
}

#[derive(Debug, Clone)]
struct ClientStatus {
    current: Arc<ProgressBar>,
}

impl Drop for ClientStatus {
    fn drop(&mut self) {
        if Arc::strong_count(&self.current) == 1 {
            self.current.finish_and_clear();
        }
    }
}

impl CustomEventSender for ClientStatus {
    fn send(&self, event: iroh_blobs::provider::Event) -> futures_lite::future::Boxed<()> {
        self.try_send(event);
        Box::pin(std::future::ready(()))
    }

    fn try_send(&self, event: iroh_blobs::provider::Event) {
        tracing::info!("{:?}", event);
        let msg = match event {
            provider::Event::ClientConnected { connection_id } => {
                Some(format!("{connection_id} got connection"))
            }
            provider::Event::TransferBlobCompleted {
                connection_id,
                hash,
                index,
                size,
                ..
            } => Some(format!(
                "{} transfer blob completed {} {} {}",
                connection_id,
                hash,
                index,
                HumanBytes(size)
            )),
            provider::Event::TransferCompleted {
                connection_id,
                stats,
                ..
            } => Some(format!(
                "{} transfer completed {} {}",
                connection_id,
                stats.send.write_bytes.size,
                HumanDuration(stats.send.write_bytes.stats.duration)
            )),
            provider::Event::TransferAborted { connection_id, .. } => {
                Some(format!("{connection_id} transfer completed"))
            }
            _ => None,
        };
        if let Some(msg) = msg {
            self.current.set_message(msg);
        }
    }
}

async fn serve_db(
    db: S3Store,
    iroh_ipv4_addr: Option<SocketAddrV4>,
    iroh_ipv6_addr: Option<SocketAddrV6>,
    on_addr: impl FnOnce(NodeAddr) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let secret_key = get_or_create_secret(true)?;
    // create an iroh endpoint
    let mut builder = Endpoint::builder()
        .alpns(vec![iroh_blobs::protocol::ALPN.to_vec()])
        .secret_key(secret_key);

    if let Some(addr) = iroh_ipv4_addr {
        builder = builder.bind_addr_v4(addr);
    }
    if let Some(addr) = iroh_ipv6_addr {
        builder = builder.bind_addr_v6(addr);
    }
    // wait for the endpoint to be ready
    let endpoint = builder.bind().await?;
    // wait for the endpoint to figure out its address before making a ticket
    endpoint.home_relay().initialized().await?;
    // make a ticket
    let addr = endpoint.node_addr().await?;
    on_addr(addr)?;
    let lp = LocalPool::single();
    let ps = SendStatus::new();
    let sc = Arc::new(ps.new_client());
    loop {
        let Some(connecting) = endpoint.accept().await else {
            tracing::info!("no more incoming connections, exiting");
            break;
        };
        let db = db.clone();
        let lph = lp.handle().clone();
        let sc = sc.clone();
        let conn = connecting.await?;
        tokio::spawn(handle_connection(conn, db, EventSender::new(Some(sc)), lph));
    }
    Ok(())
}

async fn serve_s3(args: ServeS3Args) -> anyhow::Result<()> {
    let root = args.url;
    let xml = HttpAdapter::new(root.clone()).read_to_end().await?;
    let xml = String::from_utf8_lossy(&xml);
    tracing::debug!("{}", xml);
    let bucket: ListBucketResult = serde_xml_rs::from_str(&xml)?;
    let db = S3Store::default();
    let mut hashes = Vec::new();
    let safe_bucket_name = || root.to_string().replace('/', "_");
    let prefix = args.name.unwrap_or_else(safe_bucket_name);
    for path in bucket.contents.iter().map(|c| c.key.clone()) {
        let url = root.join(&path)?;
        let hash = db.import_url(url).await?;
        let name = format!("{prefix}/{path}");
        hashes.push((name, hash));
    }
    let collection = hashes
        .iter()
        .cloned()
        .collect::<iroh_blobs::format::collection::Collection>();
    let blobs = collection.to_blobs();
    let mut last_hash = None;
    for blob in blobs {
        last_hash = Some(db.import_mem(blob).await?);
    }

    serve_db(
        db,
        args.common.iroh_ipv4_addr,
        args.common.iroh_ipv6_addr,
        |addr| {
            if let Some(hash) = last_hash {
                let ticket = BlobTicket::new(addr.clone(), hash, BlobFormat::HashSeq)?;
                println!("collection: {ticket}");
            }
            Ok(())
        },
    )
    .await?;
    Ok(())
}

async fn serve_urls(args: ImportS3Args) -> anyhow::Result<()> {
    let db = S3Store::default();
    let mut hashes = Vec::new();
    for url in args.url {
        let hash = db.import_url(url.clone()).await?;
        println!("added {}, {}", url, print_hash(&hash, args.common.format));
        let name = url.to_string().replace('/', "_");
        hashes.push((name, hash));
    }
    let collection = hashes
        .iter()
        .cloned()
        .collect::<iroh_blobs::format::collection::Collection>();
    let blobs = collection.to_blobs();
    let mut last_hash = None;
    for blob in blobs {
        last_hash = Some(db.import_mem(blob).await?);
    }

    serve_db(
        db,
        args.common.iroh_ipv4_addr,
        args.common.iroh_ipv6_addr,
        |addr| {
            for (name, hash) in &hashes {
                let ticket = BlobTicket::new(addr.clone(), *hash, BlobFormat::Raw)?;
                println!("{name} {ticket}");
            }
            if let Some(hash) = last_hash {
                let ticket = BlobTicket::new(addr.clone(), hash, BlobFormat::HashSeq)?;
                println!("collection: {ticket}");
            }
            Ok(())
        },
    )
    .await?;
    Ok(())
}

/// The ListBucketResult xml structure returned by s3.
#[derive(Debug, Deserialize)]
struct ListBucketResult {
    #[serde(rename = "Contents", default)]
    contents: Vec<Contents>,
}

/// The Contents xml structure returned by s3.
#[derive(Debug, Deserialize)]
struct Contents {
    #[serde(rename = "Key", default)]
    key: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    match args.command {
        Commands::ServeS3(args) => serve_s3(args).await,
        Commands::ServeUrls(args) => serve_urls(args).await,
    }
}
