use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use futures_lite::StreamExt;
use iroh_blobs::store::{Map, MapEntry};
use iroh_blobs::{store::Store, BlobFormat};
use iroh_car::CarReader;
use iroh_io::AsyncSliceReaderExt;
use iroh_net::discovery::{dns::DnsDiscovery, pkarr::PkarrPublisher, ConcurrentDiscovery};
use iroh_net::ticket::NodeTicket;
use iroh_net::NodeAddr;
use libipld::{cbor::DagCborCodec, codec::Codec};
use libipld::{DagCbor, Ipld, IpldCodec};
use protocol::{ron_parser, Cid, Request};
use sync::{handle_request, handle_sync_response};
use tables::{ReadOnlyTables, ReadableTables, Tables};
use tokio::io::AsyncWriteExt;
use tokio_util::task::LocalPoolHandle;
use traversal::{get_traversal, Traversal};
use util::wait_for_relay;

mod args;
mod protocol;
mod sync;
mod tables;
mod traversal;
mod util;

use args::Args;

const SYNC_ALPN: &[u8] = b"DAG_SYNC/1";

async fn create_endpoint(port: Option<u16>) -> anyhow::Result<iroh_net::Endpoint> {
    let secret_key = util::get_or_create_secret()?;
    let discovery = Box::new(ConcurrentDiscovery::from_services(vec![
        Box::new(DnsDiscovery::n0_dns()),
        Box::new(PkarrPublisher::n0_dns(secret_key.clone())),
    ]));

    let endpoint = iroh_net::Endpoint::builder()
        .secret_key(secret_key)
        .alpns(vec![SYNC_ALPN.to_vec()])
        .discovery(discovery)
        .bind(port.unwrap_or_default())
        .await?;

    Ok(endpoint)
}

fn init_mapping_db(db: &redb::Database) -> anyhow::Result<()> {
    let tx = db.begin_write()?;
    tables::Tables::new(&tx)?;
    tx.commit()?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let store = iroh_blobs::store::fs::Store::load("blobs.db").await?;
    let mapping_store = redb::Database::create("dag.db")?;
    init_mapping_db(&mapping_store)?;
    let rt = LocalPoolHandle::new(1);

    match args.cmd {
        args::SubCommand::Import(import_args) => {
            let tx = mapping_store.begin_write()?;
            let mut tables = tables::Tables::new(&tx)?;
            let file = tokio::fs::File::open(import_args.path).await?;
            let reader = CarReader::new(file).await?;
            let stream = reader.stream().enumerate();
            tokio::pin!(stream);
            let mut first = None;
            while let Some((i, block)) = stream.next().await {
                let (cid, data) = block?;
                if first.is_none() {
                    first = Some(cid);
                }
                let mut links = Vec::new();
                IpldCodec::try_from(cid.codec())?.references::<Ipld, _>(&data, &mut links)?;
                let tag = store.import_bytes(data.into(), BlobFormat::Raw).await?;
                let hash = tag.hash();
                if !links.is_empty() {
                    println!("{} {} {}", i, cid, links.len());
                    let links = DagCborCodec.encode(&links)?;
                    tables.data_to_links.insert((cid.codec(), *hash), links)?;
                } else {
                    println!("{} {}", i, cid);
                }
                tables
                    .hash_to_blake3
                    .insert((cid.hash().code(), cid.hash().digest()), hash)?;
            }
            drop(tables);
            tx.commit()?;
            store.sync().await?;
            if let Some(first) = first {
                println!("root: {}", first);
            }
        }
        args::SubCommand::Export(args) => {
            let tx = mapping_store.begin_read()?;
            let tables = tables::ReadOnlyTables::new(&tx)?;
            let opts = protocol::TraversalOpts::from_args(&args.cid, &args.traversal)?;
            println!("using traversal: '{}'", ron_parser().to_string(&opts)?);
            let traversal = get_traversal(opts, &tables)?;
            match args.target {
                Some(target) => {
                    let file = tokio::fs::File::create(target).await?;
                    export_traversal(traversal, &store, file).await?
                }
                None => print_traversal(traversal, &store).await?,
            }
        }
        args::SubCommand::Node(args) => {
            let endpoint = create_endpoint(args.net.iroh_port).await?;
            wait_for_relay(&endpoint).await?;
            let addr = endpoint.node_addr().await?;
            println!("Node id:\n{}", addr.node_id);
            println!("Listening on {:#?}", addr.info);
            println!("ticket:\n{}", NodeTicket::new(addr.clone())?);
            while let Some(mut connecting) = endpoint.accept().await {
                let alpn = connecting.alpn().await?;
                match alpn.as_ref() {
                    SYNC_ALPN => {
                        let tx = mapping_store.begin_read()?;
                        let tables = ReadOnlyTables::new(&tx)?;
                        let store = store.clone();
                        rt.spawn_pinned(move || async move {
                            handle_request(connecting, &tables, &store).await?;
                            anyhow::Ok(())
                        });
                    }
                    _ => {
                        eprintln!("Unknown ALPN: {:?}", alpn);
                    }
                }
            }
        }
        args::SubCommand::Sync(args) => {
            let endpoint = create_endpoint(args.net.iroh_port).await?;
            let traversal = protocol::TraversalOpts::from_args(&args.root, &args.traversal)?;
            println!("using traversal: '{}'", ron_parser().to_string(&traversal)?);
            let inline = protocol::InlineOpts::from_args(&args.inline)?;
            println!("using inline: '{}'", ron_parser().to_string(&inline)?);
            let tx = mapping_store.begin_write()?;
            let mut tables = Tables::new(&tx)?;
            let store = store.clone();
            let endpoint = Arc::new(endpoint);
            let node = NodeAddr::from(args.from);
            let connection = endpoint.connect(node, SYNC_ALPN).await?;
            let request = protocol::Request::Sync(protocol::SyncRequest {
                traversal: traversal.clone(),
                inline,
            });
            tracing::info!("sending request: {:?}", request);
            let request = postcard::to_allocvec(&request)?;
            tracing::info!("sending request: {} bytes", request.len());
            tracing::info!("sending request: {}", hex::encode(&request));
            let roundtrip: Request = postcard::from_bytes(&request).unwrap();
            tracing::info!("roundtrip: {:?}", roundtrip);
            let (mut send, recv) = connection.open_bi().await?;
            send.write_all(&request).await?;
            send.finish().await?;
            handle_sync_response(recv, &mut tables, &store, traversal).await?;
            drop(tables);
            tx.commit()?;
        }
    }

    Ok(())
}

async fn print_traversal<T>(
    traversal: T,
    store: &iroh_blobs::store::fs::Store,
) -> anyhow::Result<()>
where
    T: Traversal,
    T::Db: ReadableTables,
{
    let mut traversal = traversal;
    let mut first = None;
    let mut n = 0;
    while let Some(cid) = traversal.next().await? {
        if first.is_none() {
            first = Some(cid);
        }
        let blake3_hash = traversal
            .db_mut()
            .blake3_hash(cid.hash())?
            .context("blake3 hash not found")?;
        let data = store.get(&blake3_hash).await?.context("data not found")?;
        println!("{} {:x} {} {}", cid, cid.codec(), data.size().value(), n);
        n += 1;
    }

    if let Some(first) = first {
        println!("root: {}", first);
    }
    Ok(())
}

async fn export_traversal<T>(
    traversal: T,
    store: &iroh_blobs::store::fs::Store,
    mut file: tokio::fs::File,
) -> anyhow::Result<()>
where
    T: Traversal,
    T::Db: ReadableTables,
{
    #[derive(DagCbor)]
    struct CarFileHeader {
        version: u64,
        roots: Vec<Cid>,
    }

    let header = CarFileHeader {
        version: 1,
        roots: traversal.roots(),
    };
    let header_bytes = DagCborCodec.encode(&header)?;
    file.write_all(&postcard::to_allocvec(&(header_bytes.len() as u64))?)
        .await?;
    file.write_all(&header_bytes).await?;
    let mut traversal = traversal;
    let mut buffer = [0u8; 9];
    while let Some(cid) = traversal.next().await? {
        let blake3_hash = traversal
            .db_mut()
            .blake3_hash(cid.hash())?
            .context("blake3 hash not found")?;
        let handle = store.get(&blake3_hash).await?.context("data not found")?;
        let data = handle.data_reader().read_to_end().await?;
        let mut block_bytes = cid.to_bytes(); // postcard::to_extend(&RawCidHeader::from_cid(&cid), Vec::new())?;
                                              // block_bytes.extend_from_slice(&cid.hash().digest()); // hash
        block_bytes.extend_from_slice(&data);
        let size: u64 = block_bytes.len() as u64;
        file.write_all(postcard::to_slice(&size, &mut buffer)?)
            .await?;
        file.write_all(&block_bytes).await?;
    }
    file.sync_all().await?;
    Ok(())
}
