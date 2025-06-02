//! A library for discovering content using a tracker.
//!
//! This library contains the protocol for announcing and querying content, as
//! well as the client side implementation and an optional cli.
#[cfg(feature = "client")]
mod client;
pub mod protocol;
#[cfg(feature = "client")]
pub use client::*;
pub use protocol::ALPN;
