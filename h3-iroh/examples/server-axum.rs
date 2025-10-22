//! Demonstration of an axum server serving h3 over iroh
//!
//! run using `cargo run --features axum --example server-axum`

use anyhow::Result;
use axum::{response::Html, routing::get, Router};
use iroh_tickets::endpoint::EndpointTicket;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();

    let app = Router::new().route("/", get(handler));

    let ep = iroh::Endpoint::builder()
        .alpns(vec![b"iroh+h3".to_vec()])
        .bind()
        .await?;
    info!("accepting connections on endpoint: {}", ep.id());

    // Wait for direct addresses and a RelayUrl before printing a EndpointTicket.
    ep.online().await;
    let ticket = EndpointTicket::new(ep.addr());
    info!("node ticket: {ticket}");
    info!("run: cargo run --example client -- iroh+h3://{ticket}/");

    h3_iroh::axum::serve(ep, app).await?;

    Ok(())
}

async fn handler() -> Html<&'static str> {
    Html("<h1>Hello, World!</h1>")
}
