use anyhow::Context;
use ipld_core::codec::Links;
use iroh_blobs::Hash;
use multihash::Multihash;
use redb::{ReadableTable, TableDefinition};

/// Table mapping ipld hash to blake3 hash.
const HASH_TO_BLAKE3: TableDefinition<(u64, &[u8]), Hash> = TableDefinition::new("hash_to_blake3");
/// Table mapping ipld format and blake3 hash to contained links
///
/// For blobs containing no links, there should not be an entry in this table.
const DATA_TO_LINKS: TableDefinition<(u64, Hash), Vec<u8>> = TableDefinition::new("data_to_links");

pub trait ReadableTables {
    fn hash_to_blake3(&self) -> &impl redb::ReadableTable<(u64, &'static [u8]), Hash>;
    fn data_to_links(&self) -> &impl redb::ReadableTable<(u64, Hash), Vec<u8>>;

    #[allow(dead_code)]
    fn has_links(&self, cid: &cid::Cid) -> anyhow::Result<bool> {
        let hash = self
            .hash_to_blake3()
            .get((cid.hash().code(), cid.hash().digest()))?
            .context("blake3 hash not found")?;
        Ok(self
            .data_to_links()
            .get((cid.codec(), hash.value()))?
            .is_some())
    }

    /// Get the stored links for a given ipld hash.
    fn links(&self, cid: &cid::Cid) -> anyhow::Result<Option<Vec<cid::Cid>>> {
        let hash = self
            .hash_to_blake3()
            .get((cid.hash().code(), cid.hash().digest()))?
            .context("blake3 hash not found")?;
        let Some(links) = self.data_to_links().get((cid.codec(), hash.value()))? else {
            return Ok(None);
        };
        Ok(Some(serde_ipld_dagcbor::from_slice(&links.value())?))
    }

    /// Get the blake3 hash for a given ipld hash.
    fn blake3_hash<const N: usize>(&self, hash: &Multihash<N>) -> anyhow::Result<Option<Hash>> {
        Ok(self
            .hash_to_blake3()
            .get((hash.code(), hash.digest()))?
            .map(|x| x.value()))
    }
}

impl<T: ReadableTables> ReadableTables for &T {
    fn hash_to_blake3(&self) -> &impl redb::ReadableTable<(u64, &'static [u8]), Hash> {
        (*self).hash_to_blake3()
    }

    fn data_to_links(&self) -> &impl redb::ReadableTable<(u64, Hash), Vec<u8>> {
        (*self).data_to_links()
    }
}

impl<T: ReadableTables> ReadableTables for &mut T {
    fn hash_to_blake3(&self) -> &impl redb::ReadableTable<(u64, &'static [u8]), Hash> {
        ReadableTables::hash_to_blake3(*self)
    }

    fn data_to_links(&self) -> &impl redb::ReadableTable<(u64, Hash), Vec<u8>> {
        ReadableTables::data_to_links(*self)
    }
}

pub struct Tables<'tx> {
    pub hash_to_blake3: redb::Table<'tx, (u64, &'static [u8]), Hash>,
    pub data_to_links: redb::Table<'tx, (u64, Hash), Vec<u8>>,
}

impl<'tx> Tables<'tx> {
    pub fn new(tx: &'tx redb::WriteTransaction) -> std::result::Result<Self, redb::TableError> {
        Ok(Self {
            hash_to_blake3: tx.open_table(HASH_TO_BLAKE3)?,
            data_to_links: tx.open_table(DATA_TO_LINKS)?,
        })
    }

    pub fn insert_links(&mut self, cid: &cid::Cid, hash: Hash, data: &[u8]) -> anyhow::Result<()> {
        let links: Vec<_> = serde_ipld_dagcbor::codec::DagCborCodec::links(data)?.collect();
        self.hash_to_blake3
            .insert((cid.hash().code(), cid.hash().digest()), hash)?;
        if !links.is_empty() {
            let links = serde_ipld_dagcbor::to_vec(&links)?;
            self.data_to_links.insert((cid.codec(), hash), links)?;
        }
        Ok(())
    }
}

impl ReadableTables for Tables<'_> {
    fn hash_to_blake3(&self) -> &impl redb::ReadableTable<(u64, &'static [u8]), Hash> {
        &self.hash_to_blake3
    }

    fn data_to_links(&self) -> &impl redb::ReadableTable<(u64, Hash), Vec<u8>> {
        &self.data_to_links
    }
}

pub struct ReadOnlyTables {
    pub hash_to_blake3: redb::ReadOnlyTable<(u64, &'static [u8]), Hash>,
    pub data_to_links: redb::ReadOnlyTable<(u64, Hash), Vec<u8>>,
}

impl ReadOnlyTables {
    pub fn new(tx: &redb::ReadTransaction) -> std::result::Result<Self, redb::TableError> {
        Ok(Self {
            hash_to_blake3: tx.open_table(HASH_TO_BLAKE3)?,
            data_to_links: tx.open_table(DATA_TO_LINKS)?,
        })
    }
}

impl ReadableTables for ReadOnlyTables {
    fn hash_to_blake3(&self) -> &impl redb::ReadableTable<(u64, &'static [u8]), Hash> {
        &self.hash_to_blake3
    }

    fn data_to_links(&self) -> &impl redb::ReadableTable<(u64, Hash), Vec<u8>> {
        &self.data_to_links
    }
}
