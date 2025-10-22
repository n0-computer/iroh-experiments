//! Table definitions and accessors for the redb database.
use redb::{ReadableTable, TableDefinition, TableError};

use super::{AnnouncePath, AnnounceValue, ProbeValue};

pub(super) const ANNOUNCES_TABLE: TableDefinition<AnnouncePath, AnnounceValue> =
    TableDefinition::new("announces-0");
pub(super) const PROBES_TABLE: TableDefinition<AnnouncePath, ProbeValue> =
    TableDefinition::new("probes-0");

pub(super) trait ReadableTables {
    fn announces(&self) -> &impl ReadableTable<AnnouncePath, AnnounceValue>;
    fn probes(&self) -> &impl ReadableTable<AnnouncePath, ProbeValue>;
}

pub(super) struct Tables<'a> {
    pub announces: redb::Table<'a, AnnouncePath, AnnounceValue>,
    pub probes: redb::Table<'a, AnnouncePath, ProbeValue>,
}

impl<'db> Tables<'db> {
    pub fn new(tx: &'db redb::WriteTransaction) -> std::result::Result<Self, TableError> {
        Ok(Self {
            announces: tx.open_table(ANNOUNCES_TABLE)?,
            probes: tx.open_table(PROBES_TABLE)?,
        })
    }
}

impl ReadableTables for Tables<'_> {
    fn announces(&self) -> &impl ReadableTable<AnnouncePath, AnnounceValue> {
        &self.announces
    }
    fn probes(&self) -> &impl ReadableTable<AnnouncePath, ProbeValue> {
        &self.probes
    }
}

/// A struct similar to [`redb::ReadOnlyTable`] but for all tables that make up
/// the blob store.
pub(super) struct ReadOnlyTables {
    pub announces: redb::ReadOnlyTable<AnnouncePath, AnnounceValue>,
    pub probes: redb::ReadOnlyTable<AnnouncePath, ProbeValue>,
}

impl ReadOnlyTables {
    pub fn new(tx: &redb::ReadTransaction) -> std::result::Result<Self, TableError> {
        Ok(Self {
            announces: tx.open_table(ANNOUNCES_TABLE)?,
            probes: tx.open_table(PROBES_TABLE)?,
        })
    }
}

impl ReadableTables for ReadOnlyTables {
    fn announces(&self) -> &impl ReadableTable<AnnouncePath, AnnounceValue> {
        &self.announces
    }
    fn probes(&self) -> &impl ReadableTable<AnnouncePath, ProbeValue> {
        &self.probes
    }
}
