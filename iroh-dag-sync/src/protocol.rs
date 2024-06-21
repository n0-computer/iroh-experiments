use std::{collections::BTreeSet, ops::Deref, str::FromStr};

use iroh_blobs::Hash;
use libipld::DagCbor;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncRead;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub enum Request {
    Sync(SyncRequest),
}

/// A wrapper around a Cid that can be serialized and deserialized.
///
/// Yes, I know cid has a feature for serde, but I had some issues with DagCbor
/// derive when using it, probably because libipld does not have the latest cid.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Copy, DagCbor)]
pub struct Cid(libipld::Cid);

impl Serialize for Cid {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            serializer.serialize_str(&self.0.to_string())
        } else {
            serializer.serialize_bytes(&self.0.to_bytes())
        }
    }
}

impl<'de> Deserialize<'de> for Cid {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if deserializer.is_human_readable() {
            let s = String::deserialize(deserializer)?;
            Ok(Cid(
                libipld::Cid::try_from(s).map_err(serde::de::Error::custom)?
            ))
        } else {
            let bytes = serde_bytes::ByteBuf::deserialize(deserializer)?;
            Ok(Cid(
                libipld::Cid::try_from(bytes.into_vec()).map_err(serde::de::Error::custom)?
            ))
        }
    }
}

impl From<libipld::Cid> for Cid {
    fn from(cid: libipld::Cid) -> Self {
        Self(cid)
    }
}

impl From<Cid> for libipld::Cid {
    fn from(scid: Cid) -> Self {
        scid.0
    }
}

impl Deref for Cid {
    type Target = libipld::Cid;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::fmt::Display for Cid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for Cid {
    type Err = libipld::cid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Cid(libipld::Cid::try_from(s)?))
    }
}

/// A sync request is a request to sync a DAG from a remote node.
///
/// The request contains basically a ipfs cid and a dag traversal method.
///
/// The response is a sequence of blocks consisting of a sync response header
/// and bao encoded block data.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub struct SyncRequest {
    /// walk method. must be one of the registered ones
    pub traversal: TraversalOpts,
    /// which data to send inline
    pub inline: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum SyncResponseHeader {
    Hash(Hash),
    Data(Hash),
}

impl SyncResponseHeader {
    pub fn as_bytes(&self) -> [u8; 33] {
        let mut slice = [0u8; 33];
        let res = postcard::to_slice(self, &mut slice).unwrap();
        assert!(res.len() == slice.len());
        slice
    }

    pub async fn from_stream(mut stream: impl AsyncRead + Unpin) -> anyhow::Result<Option<Self>> {
        use tokio::io::AsyncReadExt;
        let mut buf = [0u8; 33];
        match stream.read_exact(&mut buf).await {
            Ok(_) => Ok(Some(postcard::from_bytes(&buf)?)),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[repr(u8)]
pub enum DataMode {
    /// no data is included
    None = 0,
    /// data is included
    Inline = 1,
}

/// Options to configure a traversal
///
/// The exact traversal options will probably be project specific, since we don't
/// want to come up with a generic language to specify graph traversals.
///
/// Having a small number of highly optimized traversal methods under a custom
/// protocol/APLN is probably best for specialized use cases.
///
/// This is just an example of possible traversal options.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub enum TraversalOpts {
    /// A sequence of individual cids.
    ///
    /// This can be used to sync a single block or a sequence of unrelated blocks.
    /// Note that since we are getting individual blocks, the codec part of the cids
    /// is not relevant.
    Sequence(SequenceTraversalOpts),
    /// A full traversal of a DAG, with a set of already visited cids.
    Full(FullTraversalOpts),
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub struct SequenceTraversalOpts(
    /// The sequence of cids to traverse, in order.
    pub Vec<Cid>,
);

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub struct FullTraversalOpts {
    /// The root of the traversal.
    ///
    /// The codec part of the cid is relevant. E.g. for a cid with codec raw,
    /// 0x55, the traversal would always be just the root.
    pub root: Cid,
    /// The set of already visited cids. This can be used to abort a traversal
    /// once data that is already known is encountered.
    ///
    /// E.g. in case of a linked list shaped dag, you would insert here
    /// the cid of the last element that you have locally.
    pub visited: Option<BTreeSet<Cid>>,
    /// The order in which to traverse the DAG.
    ///
    /// Since a traversal will abort once a cid is encountered that is not
    /// present, this can influence how much data is fetched.
    #[serde(default)]
    pub order: Option<TraversalOrder>,
    /// Filter to apply to the traversal.
    #[serde(default)]
    pub filter: Option<TraversalFilter>,
}

#[derive(Debug, Serialize, Deserialize, Default, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub enum TraversalOrder {
    #[default]
    DepthFirstPreOrderLeftToRight,
}

#[derive(Debug, Serialize, Deserialize, Default, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub enum TraversalFilter {
    /// Include all cids.
    #[default]
    All,
    /// Exclude raw cids.
    NoRaw,
    /// Exclude cids with a specific codec.
    Excude(BTreeSet<u64>),
}

pub fn ron_parser() -> ::ron::Options {
    ron::Options::default()
        .with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME)
        .with_default_extension(ron::extensions::Extensions::UNWRAP_NEWTYPES)
        .with_default_extension(ron::extensions::Extensions::UNWRAP_VARIANT_NEWTYPES)
}

impl ToString for TraversalOpts {
    fn to_string(&self) -> String {
        ron_parser().to_string(self).unwrap()
    }
}

impl FromStr for TraversalOpts {
    type Err = ron::de::SpannedError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ron_parser().from_str(s)
    }
}

impl TraversalOpts {
    pub fn from_args(root: &Option<Cid>, traversal: &Option<String>) -> anyhow::Result<Self> {
        match (root, traversal) {
            (Some(root), None) => Ok(TraversalOpts::Full(FullTraversalOpts {
                root: root.clone(),
                visited: Default::default(),
                order: Default::default(),
                filter: Default::default(),
            })),
            (None, Some(traversal)) => Ok(TraversalOpts::from_str(traversal)?),
            (Some(_), Some(_)) => {
                anyhow::bail!("Either root or traversal must be specified, not both")
            }
            (None, None) => anyhow::bail!("Either root or traversal must be specified"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use crate::protocol::{
        ron_parser, Cid, FullTraversalOpts, Request, SyncRequest, TraversalOpts,
    };

    #[test]
    fn cid_json_roundtrip() {
        let ron = ron_parser();
        let cid = Cid::from_str("QmWyLtd4WEJe45UBqCZG94gYY9B8qF3k4DKFX3o2bodHmV").unwrap();
        let json = ron.to_string(&cid).unwrap();
        let cid2 = ron.from_str(&json).unwrap();
        assert_eq!(cid, cid2);
    }

    #[test]
    fn cid_postcard_roundtrip() {
        let cid = Cid::from_str("QmWyLtd4WEJe45UBqCZG94gYY9B8qF3k4DKFX3o2bodHmV").unwrap();
        let bytes = postcard::to_allocvec(&cid).unwrap();
        let cid2 = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(cid, cid2);
    }

    #[test]
    fn opts_postcard_roundtrip() {
        let cid = Cid::from_str("QmWyLtd4WEJe45UBqCZG94gYY9B8qF3k4DKFX3o2bodHmV").unwrap();
        let opts = TraversalOpts::Full(FullTraversalOpts {
            root: cid,
            visited: Default::default(),
            order: Default::default(),
            filter: Default::default(),
        });
        let bytes = postcard::to_allocvec(&opts).unwrap();
        let opts2: TraversalOpts = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(opts, opts2);
    }

    #[test]
    fn request_postcard_roundtrip() {
        let cid = Cid::from_str("QmWyLtd4WEJe45UBqCZG94gYY9B8qF3k4DKFX3o2bodHmV").unwrap();
        let opts = TraversalOpts::Full(FullTraversalOpts {
            root: cid,
            visited: Default::default(),
            order: Default::default(),
            filter: Default::default(),
        });
        let request = Request::Sync(SyncRequest {
            traversal: opts,
            inline: "always".to_owned(),
        });
        let bytes = postcard::to_allocvec(&request).unwrap();
        let request2: Request = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(request, request2);
    }
}
