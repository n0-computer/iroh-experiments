use std::{collections::BTreeMap, time::Duration};

use iroh::{
    NodeAddr, NodeId, SecretKey, Watcher,
    discovery::static_provider::StaticProvider,
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler, Router},
};
use n0_future::{BufferedStreamExt, StreamExt, stream};
use n0_snafu::ResultExt;
use testresult::TestResult;
use tracing::trace;

use crate::connection_pool::{ConnectionPool, ConnectionPoolError, Options, PoolConnectError};

const ECHO_ALPN: &[u8] = b"echo";

#[derive(Debug, Clone)]
struct Echo;

impl ProtocolHandler for Echo {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let conn_id = connection.stable_id();
        let id = connection.remote_node_id().map_err(AcceptError::from_err)?;
        trace!(%id, %conn_id, "Accepting echo connection");
        loop {
            match connection.accept_bi().await {
                Ok((mut send, mut recv)) => {
                    trace!(%id, %conn_id, "Accepted echo request");
                    tokio::io::copy(&mut recv, &mut send).await?;
                    send.finish().map_err(AcceptError::from_err)?;
                }
                Err(e) => {
                    trace!(%id, %conn_id, "Failed to accept echo request {e}");
                    break;
                }
            }
        }
        Ok(())
    }
}

async fn echo_client(conn: Connection, text: &[u8]) -> n0_snafu::Result<Vec<u8>> {
    let conn_id = conn.stable_id();
    let id = conn.remote_node_id().e()?;
    trace!(%id, %conn_id, "Sending echo request");
    let (mut send, mut recv) = conn.open_bi().await.e()?;
    send.write_all(text).await.e()?;
    send.finish().e()?;
    let response = recv.read_to_end(1000).await.e()?;
    trace!(%id, %conn_id, "Received echo response");
    Ok(response)
}

async fn echo_server() -> TestResult<(NodeAddr, Router)> {
    let endpoint = iroh::Endpoint::builder()
        .alpns(vec![ECHO_ALPN.to_vec()])
        .bind()
        .await?;
    let addr = endpoint.node_addr().initialized().await;
    let router = iroh::protocol::Router::builder(endpoint)
        .accept(ECHO_ALPN, Echo)
        .spawn();

    Ok((addr, router))
}

async fn echo_servers(n: usize) -> TestResult<Vec<(NodeAddr, Router)>> {
    stream::iter(0..n)
        .map(|_| echo_server())
        .buffered_unordered(16)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect()
}

fn test_options() -> Options {
    Options {
        idle_timeout: Duration::from_millis(100),
        connect_timeout: Duration::from_secs(2),
        max_connections: 32,
    }
}

struct EchoClient {
    pool: ConnectionPool,
}

impl EchoClient {
    async fn echo(
        &self,
        id: NodeId,
        text: Vec<u8>,
    ) -> Result<
        Result<Result<(usize, Vec<u8>), n0_snafu::Error>, PoolConnectError>,
        ConnectionPoolError,
    > {
        self.pool
            .with_connection(id, |conn| async move {
                let id = conn.stable_id();
                let res = echo_client(conn, &text).await?;
                Ok((id, res))
            })
            .await
    }
}

#[tokio::test]
async fn connection_pool_errors() -> TestResult<()> {
    // set up static discovery for all addrs
    let discovery = StaticProvider::new();
    let endpoint = iroh::Endpoint::builder()
        .discovery(discovery.clone())
        .bind()
        .await?;
    let pool = ConnectionPool::new(endpoint, ECHO_ALPN, test_options());
    let client = EchoClient { pool };
    {
        let non_existing = SecretKey::from_bytes(&[0; 32]).public();
        let res = client.echo(non_existing, b"Hello, world!".to_vec()).await?;
        // trying to connect to a non-existing id will fail with ConnectError
        // because we don't have any information about the node
        assert!(matches!(res, Err(PoolConnectError::ConnectError(_))));
    }
    {
        let non_listening = SecretKey::from_bytes(&[0; 32]).public();
        // make up fake node info
        discovery.add_node_info(NodeAddr {
            node_id: non_listening,
            relay_url: None,
            direct_addresses: vec!["127.0.0.1:12121".parse().unwrap()]
                .into_iter()
                .collect(),
        });
        // trying to connect to an id for which we have info, but the other
        // end is not listening, will lead to a timeout.
        let res = client
            .echo(non_listening, b"Hello, world!".to_vec())
            .await?;
        assert!(matches!(res, Err(PoolConnectError::Timeout)));
    }
    Ok(())
}

#[tokio::test]
async fn connection_pool_smoke() -> TestResult<()> {
    let n = 32;
    let filter = tracing_subscriber::EnvFilter::from_default_env();
    tracing_subscriber::fmt().with_env_filter(filter).init();
    let nodes = echo_servers(n).await?;
    let ids = nodes
        .iter()
        .map(|(addr, _)| addr.node_id)
        .collect::<Vec<_>>();
    // set up static discovery for all addrs
    let discovery = StaticProvider::from_node_info(nodes.iter().map(|(addr, _)| addr.clone()));
    // build a client endpoint that can resolve all the node ids
    let endpoint = iroh::Endpoint::builder()
        .discovery(discovery.clone())
        .bind()
        .await?;
    let pool = ConnectionPool::new(endpoint.clone(), ECHO_ALPN, test_options());
    let client = EchoClient { pool };
    let mut connection_ids = BTreeMap::new();
    let msg = b"Hello, world!".to_vec();
    for id in &ids {
        let (cid1, res) = client.echo(*id, msg.clone()).await???;
        println!("First response from {}: {:?}", cid1, res);
        assert_eq!(res, msg);
        let (cid2, res) = client.echo(*id, msg.clone()).await???;
        println!("Second response from {}: {:?}", cid2, res);
        assert_eq!(res, msg);
        assert_eq!(cid1, cid2);
        connection_ids.insert(id, cid1);
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    for id in &ids {
        let cid1 = *connection_ids.get(id).expect("Connection ID not found");
        let (cid2, res) = client.echo(*id, msg.clone()).await???;
        assert_eq!(res, msg);
        assert_ne!(cid1, cid2);
    }
    Ok(())
}
