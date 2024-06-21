use iroh_blobs::Hash;
use libipld::{Cid, Multihash};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncRead;

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Sync(SyncRequest),
}

/// A wrapper around a Cid that can be serialized and deserialized.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Copy)]
pub struct SCid(Cid);

impl From<Cid> for SCid {
    fn from(cid: Cid) -> Self {
        Self(cid)
    }
}

impl From<SCid> for Cid {
    fn from(scid: SCid) -> Self {
        scid.0
    }
}

impl Serialize for SCid {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            serializer.serialize_str(&self.0.to_string())
        } else {
            self.0.to_bytes().serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for SCid {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if deserializer.is_human_readable() {
            let s = String::deserialize(deserializer)?;
            Ok(SCid(Cid::try_from(s).map_err(serde::de::Error::custom)?))
        } else {
            let bytes = <&[u8]>::deserialize(deserializer)?;
            Ok(SCid(Cid::try_from(bytes).map_err(serde::de::Error::custom)?))
        }
    }
}

impl std::fmt::Display for SCid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for SCid {
    type Err = libipld::cid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(SCid(Cid::try_from(s)?))
    }
}

/// A generic content identifier, basically an ipld cid.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash, Clone)]
pub struct MiniCid {
    /// ipld data format. must be one of the registered ones
    pub data_format: u64,
    /// ipld hash format. must be one of the registered ones
    pub hash_format: u64,
    /// hash bytes, must be consistent with hash_format
    pub hash: Vec<u8>,
}

impl From<Cid> for MiniCid {
    fn from(cid: Cid) -> Self {
        Self {
            hash_format: cid.hash().code(),
            data_format: cid.codec(),
            hash: cid.hash().digest().to_vec(),
        }
    }
}

impl TryFrom<MiniCid> for Cid {
    type Error = anyhow::Error;

    fn try_from(value: MiniCid) -> Result<Self, Self::Error> {
        Ok(Cid::new_v1(
            value.data_format,
            Multihash::wrap(value.hash_format, &value.hash)?,
        ))
    }
}

/// A sync request is a request to sync a DAG from a remote node.
///
/// The request contains basically a ipfs cid and a dag traversal method.
///
/// The response is a sequence of blocks consisting of a sync response header
/// and bao encoded block data.
#[derive(Debug, Serialize, Deserialize)]
pub struct SyncRequest {
    /// the root cid to sync
    pub root: MiniCid,
    /// walk method. must be one of the registered ones
    pub traversal: String,
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
