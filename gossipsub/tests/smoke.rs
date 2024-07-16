use futures::{stream::SelectAll, StreamExt};
use gossipsub::{MessageAuthenticity, ValidationMode, GOSSIPSUB_1_1_0_PROTOCOL};
use quickcheck::{QuickCheck, TestResult};
use rand::{seq::SliceRandom, SeedableRng};
use std::{sync::Arc, task::Poll, time::Duration};
use tokio_stream::wrappers::ReceiverStream;
use tracing_subscriber::EnvFilter;

struct Graph {
    nodes: Vec<iroh::node::MemNode>,
    subscriptions: SelectAll<ReceiverStream<gossipsub::Event>>,
}

impl Graph {
    async fn new_connected(num_nodes: usize, seed: u64) -> Graph {
        assert!(num_nodes > 0, "expecting at least one node");

        let mut rng = rand::rngs::StdRng::seed_from_u64(seed);

        let mut subscriptions = SelectAll::new();
        let mut not_connected_nodes = Vec::new();
        for _i in 0..num_nodes {
            let (node, sub) = build_node().await;
            not_connected_nodes.push((node, sub));
        }
        let mut connected_nodes = vec![not_connected_nodes.pop().unwrap().0];

        for (next, sub) in not_connected_nodes {
            let connected = connected_nodes
                .choose_mut(&mut rng)
                .expect("at least one connected node");

            let next_gossip = next
                .get_protocol::<gossipsub::Behaviour>(GOSSIPSUB_1_1_0_PROTOCOL)
                .unwrap();
            next_gossip
                .connect(connected.node_addr().await.unwrap())
                .await
                .unwrap();
            subscriptions.push(sub);
            connected_nodes.push(next);
        }

        Graph {
            nodes: connected_nodes,
            subscriptions,
        }
    }

    /// Polls the graph and passes each event into the provided FnMut until the closure returns
    /// `true`.
    ///
    /// Returns [`true`] on success and [`false`] on timeout.
    async fn wait_for<F: FnMut(&gossipsub::Event) -> bool>(&mut self, mut f: F) -> bool {
        let condition = async {
            loop {
                let event = self.subscriptions.select_next_some().await;
                if f(&event) {
                    break;
                }
            }
        };

        match tokio::time::timeout(Duration::from_secs(10), condition).await {
            Ok(()) => true,
            Err(_) => false,
        }
    }

    /// Polls the graph until Poll::Pending is obtained, completing the underlying polls.
    async fn drain_events(&mut self) {
        let fut = futures::future::poll_fn(|cx| loop {
            match self.subscriptions.poll_next_unpin(cx) {
                Poll::Ready(_) => {}
                Poll::Pending => return Poll::Ready(()),
            }
        });
        tokio::time::timeout(Duration::from_secs(10), fut)
            .await
            .unwrap();
    }
}

async fn build_node() -> (iroh::node::MemNode, ReceiverStream<gossipsub::Event>) {
    // NOTE: The graph of created nodes can be disconnected from the mesh point of view as nodes
    // can reach their d_lo value and not add other nodes to their mesh. To speed up this test, we
    // reduce the default values of the heartbeat, so that all nodes will receive gossip in a
    // timely fashion.

    let mut builder = iroh::node::Node::memory().build().await.unwrap();

    let endpoint = builder.endpoint().clone();
    let node_id = endpoint.node_id();

    let config = gossipsub::ConfigBuilder::default()
        .heartbeat_initial_delay(Duration::from_millis(100))
        .heartbeat_interval(Duration::from_millis(200))
        .history_length(10)
        .history_gossip(10)
        .validation_mode(ValidationMode::Permissive)
        .build()
        .unwrap();

    let gossip: gossipsub::Behaviour =
        gossipsub::Behaviour::new(MessageAuthenticity::Author(node_id), config).unwrap();
    let sub = ReceiverStream::new(gossip.subscribe_events().unwrap());
    let gossip = Arc::new(gossip);
    let node = builder
        .accept(GOSSIPSUB_1_1_0_PROTOCOL, gossip.clone())
        .spawn()
        .await
        .unwrap();
    (node, sub)
}

#[test]
fn multi_hop_propagation() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init();

    fn prop(num_nodes: u8, seed: u64) -> TestResult {
        if !(2..=50).contains(&num_nodes) {
            return TestResult::discard();
        }

        tracing::debug!(number_of_nodes=%num_nodes, seed=%seed);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let mut graph = Graph::new_connected(num_nodes as usize, seed).await;
            let number_nodes = graph.nodes.len();

            // Subscribe each node to the same topic.
            let topic = gossipsub::IdentTopic::new("test-net");
            for node in &mut graph.nodes {
                node.get_protocol::<gossipsub::Behaviour>(GOSSIPSUB_1_1_0_PROTOCOL)
                    .unwrap()
                    .subscribe(&topic)
                    .unwrap();
            }

            // Wait for all nodes to be subscribed.
            let mut subscribed = 0;

            let all_subscribed = graph
                .wait_for(move |ev| {
                    if let gossipsub::Event::Subscribed { .. } = ev {
                        subscribed += 1;
                        if subscribed == (number_nodes - 1) * 2 {
                            return true;
                        }
                    }

                    false
                })
                .await;

            if !all_subscribed {
                return TestResult::error(format!(
                    "Timed out waiting for all nodes to subscribe but only have {subscribed:?}/{num_nodes:?}.",
                ));
            }

            // It can happen that the publish occurs before all grafts have completed causing this test
            // to fail. We drain all the poll messages before publishing.
            graph.drain_events().await;

            // Publish a single message.
            graph
                .nodes
                .iter()
                .next()
                .unwrap()
                .get_protocol::<gossipsub::Behaviour>(GOSSIPSUB_1_1_0_PROTOCOL)
                .unwrap()
                .publish(topic, vec![1, 2, 3])
                .unwrap();

            // Wait for all nodes to receive the published message.
            let mut received_msgs = 0;
            let all_received = graph
                .wait_for(move |ev| {
                    if let gossipsub::Event::Message { .. } = ev {
                        received_msgs += 1;
                        if received_msgs == number_nodes - 1 {
                            return true;
                        }
                    }

                    false
                })
                .await;

            if !all_received {
                return TestResult::error(format!(
                    "Timed out waiting for all nodes to receive the msg but only have {received_msgs:?}/{num_nodes:?}.",
                ));
            }

            TestResult::passed()
        })
    }

    QuickCheck::new()
        .max_tests(5)
        .quickcheck(prop as fn(u8, u64) -> TestResult)
}
