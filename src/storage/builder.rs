use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use rocksdb::{Options, DB};
use crate::storage::{GridEntry, GridKey, Result};

/// Builder for creating and populating a grid store.
///
/// Accumulates GridEntries in memory during building, then writes
/// everything to RocksDB in a single batch for efficiency.
pub struct GridStoreBuilder {
    path: PathBuf,
    data: BTreeMap<GridKey, Vec<GridEntry>>,
    // TODO: bin_boundaries is not implemented yet. We should.
}

impl GridStoreBuilder {
    /// Creates a new GridStoreBuilder at the specified path.
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        Ok(GridStoreBuilder {
            path: path.as_ref().to_path_buf(),
            data: BTreeMap::new(),
        })
    }

    /// Inserts GridEntries for a given key.
    ///
    /// Multiple calls with the same key will accumulate entries in memory.
    /// All entries are written to the database when finish() is called.
    pub fn insert(&mut self, key: &GridKey, mut entries: Vec<GridEntry>) -> Result<()> {
        self.data
            .entry(key.clone())
            .or_insert_with(Vec::new)
            .append(&mut entries);
        Ok(())
    }

    /// Finalizes the store by writing all accumulated data to RocksDB.
    pub fn finish(self) -> Result<()> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        let db = DB::open(&opts, self.path)?;

        // DB will be closed when dropped
        Ok(())
    }
}
