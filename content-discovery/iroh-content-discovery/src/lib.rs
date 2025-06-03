//! A library for discovering content using a tracker.
//!
//! This library contains the protocol for announcing and querying content, as
//! well as a simple client implementation.
//!
//! # Protocol
//!
//! The protocol is defined in the [protocol] module.
//!
//! # Client
//!
//! Client functionality uses the `client` feature flag, which is enabled by
//! default. Disable it if you only need the protocol and want to minimize
//! dependencies.
//!
//! The client implementation is using 0-rtt to minimize latency when announcing
//! or querying content. For details about how to use 0-rtt in iroh, see this
//! [blog post](https://www.iroh.computer/blog/0rtt-api/).
//!
//! Note that both announce and query will dial the tracker using just a node ID,
//! so the endpoint must have node discovery enabled, and the tracker must
//! announce itself to that node discovery mechanism.
//!
//! Use [announce] to announce content to a tracker, and
//! [query] to query a tracker for content.
//!
//! The higher level functions [announce_all] and [query_all] can be used to
//! announce to or or query from multiple trackers at once. They use
//! [futures_buffered](https://docs.rs/futures-buffered/latest/futures_buffered/)
//! for concurrency and are intended for the common case of working with multiple
//! trackers.
//!
//! The lower level functions [announce_conn] and [query_conn] can be used to
//! announce to or query a tracker using an existing connection.
#[cfg(feature = "client")]
mod client;
pub mod protocol;
#[cfg(feature = "client")]
pub use client::*;
pub use protocol::ALPN;
