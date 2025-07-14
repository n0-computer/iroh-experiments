use std::{time::Duration, vec};

use iroh::{protocol::RouterBuilder, Endpoint};
use iroh_blobs::{store::mem::MemStore, BlobsProtocol};
use iroh_content_discovery::{
    announce,
    protocol::{AbsoluteTime, Announce, AnnounceKind, Query, QueryFlags, SignedAnnounce},
    query,
};
use iroh_content_tracker::tracker::Tracker;

#[tokio::test]
async fn smoke_test() -> anyhow::Result<()> {
    tracing_subscriber::fmt().init();
    let tempdir = tempfile::tempdir()?;
    let tracker_ep = Endpoint::builder()
        .alpns(vec![iroh_content_discovery::ALPN.to_vec()])
        .discovery_n0()
        .bind()
        .await?;
    let provider_ep = Endpoint::builder().discovery_n0().bind().await?;
    let client_ep = Endpoint::builder().discovery_n0().bind().await?;
    let mut options = iroh_content_tracker::options::Options::debug();
    options.announce_data_path = tempdir.path().join("announce.redb");
    let tracker = Tracker::new(options, tracker_ep.clone())?;
    let accept_task = tokio::spawn(tracker.clone().accept_loop(tracker_ep.clone()));
    let tracker_id = tracker_ep.node_id();
    let store = MemStore::new();
    let blobs = BlobsProtocol::new(&store, provider_ep.clone(), None);
    let provider_router = RouterBuilder::new(provider_ep.clone())
        .accept(iroh_blobs::ALPN, blobs.clone())
        .spawn();
    let hash = blobs
        .store()
        .add_bytes(b"some content".to_vec())
        .await?
        .hash;
    println!("added content with hash: {hash}");
    tokio::time::sleep(Duration::from_secs(1)).await; // wait for the tracker to be ready
    println!("querying tracker {tracker_id} for content with hash: {hash}");
    let q = Query {
        content: hash.into(),
        flags: QueryFlags {
            complete: true,
            verified: true,
        },
    };
    let res = query(&client_ep, tracker_id, q).await?;
    println!("query successful, content found on tracker {tracker_id} {res:?}");
    assert!(res.is_empty(), "we have not published anything yet!");
    let signed_announce = SignedAnnounce::new(
        Announce {
            content: hash.into(),
            kind: AnnounceKind::Complete,
            host: provider_ep.node_id(),
            timestamp: AbsoluteTime::now(),
        },
        provider_ep.secret_key(),
    )?;
    println!("announcing content to tracker {tracker_id}: {signed_announce:?}");
    announce(&client_ep, tracker_id, signed_announce).await?;
    println!("querying tracker {tracker_id} for content with hash: {hash}");
    tokio::time::sleep(Duration::from_secs(1)).await; // give the tracker some time to do the probe
    let res = query(&client_ep, tracker_id, q).await?;
    println!("query successful, content found on tracker {tracker_id} {res:?}");
    assert_eq!(res.len(), 1, "we should have one announce for the content");
    provider_router.shutdown().await?;
    accept_task.abort();
    Ok(())
}
