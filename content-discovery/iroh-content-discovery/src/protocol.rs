//! The protocol for communicating with the tracker.
use std::{
    ops::{Deref, Sub},
    result,
    time::{Duration, SystemTime},
};

use iroh::NodeId;
use iroh_blobs::HashAndFormat;
use serde::{Deserialize, Serialize};
use serde_big_array::BigArray;
use snafu::prelude::*;

/// The ALPN string for this protocol
pub const ALPN: &[u8] = b"n0/tracker/1";
/// Maximum size of a request
pub const REQUEST_SIZE_LIMIT: usize = 1024 * 16;

/// Announce kind
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum AnnounceKind {
    /// The peer supposedly has some of the data.
    Partial = 0,
    /// The peer supposedly has the complete data.
    Complete,
}

impl AnnounceKind {
    pub fn from_complete(complete: bool) -> Self {
        if complete {
            Self::Complete
        } else {
            Self::Partial
        }
    }
}

/// An absolute time in microseconds since the UNIX epoch.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct AbsoluteTime(u64);

impl AbsoluteTime {
    pub fn now() -> Self {
        Self::try_from(SystemTime::now()).unwrap()
    }

    pub fn from_micros(micros: u64) -> Self {
        Self(micros)
    }

    pub fn as_micros(&self) -> u64 {
        self.0
    }
}

impl Sub for AbsoluteTime {
    type Output = Duration;

    fn sub(self, rhs: Self) -> Self::Output {
        Duration::from_micros(self.0 - rhs.0)
    }
}

impl TryFrom<SystemTime> for AbsoluteTime {
    type Error = &'static str;

    fn try_from(value: SystemTime) -> Result<Self, Self::Error> {
        Ok(Self(
            value
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| "SystemTime before UNIX EPOCH")?
                .as_micros()
                .try_into()
                .map_err(|_| "SystemTime too large")?,
        ))
    }
}

impl From<AbsoluteTime> for SystemTime {
    fn from(value: AbsoluteTime) -> Self {
        std::time::UNIX_EPOCH + Duration::from_micros(value.0)
    }
}

/// Announce that a peer claims to have some blobs or set of blobs.
///
/// A peer can announce having some data, but it should also be able to announce
/// that another peer has the data. This is why the peer is included.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Announce {
    /// The peer that supposedly has the data.
    pub host: NodeId,
    /// The content that the peer claims to have.
    pub content: HashAndFormat,
    /// The kind of the announcement.
    pub kind: AnnounceKind,
    /// The timestamp of the announce.
    pub timestamp: AbsoluteTime,
}

/// A signed announce.
#[derive(derive_more::Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SignedAnnounce {
    /// Announce.
    pub announce: Announce,
    /// Signature of the announce, signed by the host of the announce.
    ///
    /// The signature is over the announce, serialized with postcard.
    #[serde(with = "BigArray")]
    #[debug("{}", hex::encode(self.signature))]
    pub signature: [u8; 64],
}

impl Deref for SignedAnnounce {
    type Target = Announce;

    fn deref(&self) -> &Self::Target {
        &self.announce
    }
}

#[derive(Debug, Snafu)]
pub enum VerifyError {
    SignatureError {
        source: ed25519_dalek::SignatureError,
    },
    SerializationError {
        source: postcard::Error,
    },
}

impl SignedAnnounce {
    /// Create a new signed announce.
    pub fn new(
        announce: Announce,
        secret_key: &iroh::SecretKey,
    ) -> result::Result<Self, postcard::Error> {
        let announce_bytes = postcard::to_allocvec(&announce)?;
        let signature = secret_key.sign(&announce_bytes).to_bytes();
        Ok(Self {
            announce,
            signature,
        })
    }

    /// Verify the announce, and return the announce if it's valid.
    pub fn verify(&self) -> result::Result<(), VerifyError> {
        let announce_bytes = postcard::to_allocvec(&self.announce).context(SerializationSnafu)?;
        let signature = iroh_base::Signature::from_bytes(&self.signature);
        self.announce
            .host
            .verify(&announce_bytes, &signature)
            .context(SignatureSnafu)?;
        Ok(())
    }
}

/// Flags for a query.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct QueryFlags {
    /// Only return peers that supposedly have the complete data.
    ///
    /// If this is false, the response might contain peers that only have some of the data.
    pub complete: bool,

    /// Only return hosts that have been verified.
    ///
    /// In case of a partial query, verification just means a check that the host exists
    /// and returns the size for the data.
    ///
    /// In case of a complete query, verification means that the host has been randomly
    /// probed for the data.
    pub verified: bool,
}

/// Query a peer for a blob or set of blobs.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Query {
    /// The content we want to find.
    ///
    /// It's a difference if a peer has a blob or a hash seq and all of its children.
    pub content: HashAndFormat,
    /// The mode of the query.
    pub flags: QueryFlags,
}

/// A response to a query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResponse {
    /// The hosts that supposedly have the content.
    ///
    /// If there are any addrs, they are as seen from the tracker,
    /// so they might or might not be useful.
    pub hosts: Vec<SignedAnnounce>,
}

/// A request to the tracker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    /// Announce info
    Announce(SignedAnnounce),
    /// Query info
    Query(Query),
}

/// A response from the tracker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    /// Response to a query
    QueryResponse(QueryResponse),
}

#[cfg(test)]
mod tests {}
