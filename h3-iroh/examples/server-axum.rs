//! Demonstration of an axum server serving h3 over iroh
//!
//! run using `cargo run --features axum --example server-axum`

use anyhow::Result;
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use futures::StreamExt;
use iroh_net::ticket::NodeTicket;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();

    let app = Router::new().route("/", get(handler));

    let ep = iroh_net::Endpoint::builder()
        .alpns(vec![b"iroh+h3".to_vec()])
        .bind()
        .await?;
    info!("accepting connections on node: {}", ep.node_id());

    // Wait for direct addresses and a RelayUrl before printing a NodeTicket.
    ep.direct_addresses().next().await;
    ep.watch_home_relay().next().await;
    let ticket = NodeTicket::new(ep.node_addr().await?);
    info!("node ticket: {ticket}");
    info!("run: cargo run --example client -- iroh+h3://{ticket}/");

    h3_iroh::axum::serve(ep, app).await?;

    Ok(())
}

async fn handler() -> Html<&'static str> {
    Html("<h1>Hello, World!</h1>")
}