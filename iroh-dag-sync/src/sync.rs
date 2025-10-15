use anyhow::Context;
use bao_tree::{io::outboard::EmptyOutboard, BaoTree, ChunkRanges};
use iroh::endpoint::{Connecting, RecvStream, SendStream};
use iroh_blobs::store::{fs::FsStore, IROH_BLOCK_SIZE};
use iroh_io::{TokioStreamReader, TokioStreamWriter};
use multihash_codetable::MultihashDigest;
use tokio::io::AsyncReadExt;

use crate::{
    protocol::{Cid, Request, SyncRequest, SyncResponseHeader, TraversalOpts},
    tables::{ReadableTables, Tables},
    traversal::{get_inline, get_traversal, Traversal},
};

const MAX_REQUEST_SIZE: usize = 1024 * 1024 * 16;

pub async fn handle_request(
    mut connecting: Connecting,
    tables: &impl ReadableTables,
    blobs: &FsStore,
) -> anyhow::Result<()> {
    tracing::info!(
        "got connecting, {:?}",
        std::str::from_utf8(&connecting.alpn().await?)
    );
    let connection = connecting.await?;
    tracing::info!("got connection, waiting for request");
    let (mut send, mut recv) = connection.accept_bi().await?;
    tracing::info!("got request stream");
    let request = recv.read_to_end(MAX_REQUEST_SIZE).await?;
    tracing::info!("got request message: {} bytes", request.len());
    let request = postcard::from_bytes::<Request>(&request)?;
    tracing::info!("got request: {:?}", request);
    match request {
        Request::Sync(args) => {
            handle_sync_request(&mut send, args, tables, blobs).await?;
        }
    }
    Ok(())
}

pub async fn handle_sync_request(
    send: &mut SendStream,
    request: SyncRequest,
    tables: &impl ReadableTables,
    blobs: &FsStore,
) -> anyhow::Result<()> {
    let traversal = get_traversal(request.traversal, tables)?;
    let inline = get_inline(request.inline)?;
    write_sync_response(send, traversal, blobs, inline).await?;
    Ok(())
}

async fn write_sync_response<T: Traversal>(
    send: &mut SendStream,
    traversal: T,
    blobs: &FsStore,
    inline: impl Fn(&Cid) -> bool,
) -> anyhow::Result<()>
where
    T::Db: ReadableTables,
{
    let mut traversal = traversal;
    let mut send = TokioStreamWriter(send);
    while let Some(cid) = traversal.next().await? {
        let hash = traversal
            .db_mut()
            .blake3_hash(cid.hash())?
            .context("blake3 hash not found")?;
        if inline(&cid) {
            send.0
                .write_all(&SyncResponseHeader::Data(hash).as_bytes())
                .await?;

            blobs
                .export_bao(hash, ChunkRanges::all())
                .write(&mut send)
                .await?;
        } else {
            send.0
                .write_all(&SyncResponseHeader::Hash(hash).as_bytes())
                .await?;
        }
    }
    send.0.finish()?;
    Ok(())
}

pub async fn handle_sync_response(
    recv: RecvStream,
    tables: &mut Tables<'_>,
    store: &FsStore,
    traversal: TraversalOpts,
) -> anyhow::Result<()> {
    let mut reader = TokioStreamReader(recv);
    let mut traversal = get_traversal(traversal, tables)?;
    loop {
        let Some(cid) = traversal.next().await? else {
            break;
        };
        let Some(header) = SyncResponseHeader::from_stream(&mut reader.0).await? else {
            break;
        };
        println!("{cid} {header:?}");
        let blake3_hash = match header {
            SyncResponseHeader::Hash(blake3_hash) => {
                // todo: get the data via another request
                println!("just got hash mapping {cid} {blake3_hash}");
                continue;
            }
            SyncResponseHeader::Data(hash) => hash,
        };
        let size = reader.0.read_u64_le().await?;
        let outboard = EmptyOutboard {
            tree: BaoTree::new(size, IROH_BLOCK_SIZE),
            root: blake3_hash.into(),
        };
        let mut buffer = Vec::new();
        bao_tree::io::fsm::decode_ranges(&mut reader, ChunkRanges::all(), &mut buffer, outboard)
            .await?;
        let hasher = multihash_codetable::Code::try_from(cid.hash().code())?;
        let actual = hasher.digest(&buffer);
        if &actual != cid.hash() {
            anyhow::bail!("user hash mismatch");
        }
        let data = bytes::Bytes::from(buffer);
        let tag = store.add_bytes(data.clone()).with_tag().await?;
        if tag.hash != blake3_hash {
            anyhow::bail!("blake3 hash mismatch");
        }
        traversal.db_mut().insert_links(&cid, blake3_hash, &data)?;
    }
    Ok(())
}
