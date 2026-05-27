//! A read-only iroh-blobs store that imports data from HTTP(S) URLs (such as
//! S3 buckets) into an in-memory store.
//!
//! Data is downloaded into memory at import time, the outboard is computed
//! once, and the entry is served from memory thereafter. This is a thin
//! wrapper around [`iroh_blobs::store::mem::MemStore`] that adds the
//! convenience [`S3Store::import_url`] method.

use std::ops::Deref;

use bytes::Bytes;
use iroh_blobs::{api::Store, store::mem::MemStore, Hash};
use iroh_io::{AsyncSliceReaderExt, HttpAdapter};
use url::Url;

#[derive(Debug, Clone)]
pub struct S3Store {
    inner: MemStore,
}

impl Default for S3Store {
    fn default() -> Self {
        Self {
            inner: MemStore::new(),
        }
    }
}

impl Deref for S3Store {
    type Target = Store;

    fn deref(&self) -> &Self::Target {
        self.inner.as_ref()
    }
}

impl AsRef<Store> for S3Store {
    fn as_ref(&self) -> &Store {
        self.inner.as_ref()
    }
}

impl S3Store {
    /// Download a URL into memory and add it to the store.
    pub async fn import_url(&self, url: Url) -> anyhow::Result<Hash> {
        let mut http = HttpAdapter::new(url);
        let data = http.read_to_end().await?;
        self.import_mem(data).await
    }

    /// Add an in-memory blob to the store.
    pub async fn import_mem(&self, data: Bytes) -> anyhow::Result<Hash> {
        let tag = self.inner.as_ref().add_bytes(data).await?;
        Ok(tag.hash)
    }
}
