use std::path::Path;
use rocksdb::{Options, DB};
use crate::storage::{GridEntry, GridKey, Result};

pub struct GridStoreBuilder {
    db: DB,
}

impl GridStoreBuilder {
    /// Creates a new GridStoreBuilder at the specified path.
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let mut opts = Options::default();
        opts.create_if_missing(true);

        let db = DB::open(&opts, path)?;

        Ok(GridStoreBuilder { db })
    }

    /// Inserts GridEntries for a given key.
    ///
    /// Multiple entries can be stored for the same phrase_id at different locations.
    pub fn insert(&mut self, key: &GridKey, entries: Vec<GridEntry>) -> Result<()> {
        // TODO: Serialize key and entries, write to DB
        todo!("Implement serialization and storage")
    }

    /// Finalizes the store, making it ready for queries.
    pub fn finish(self) -> Result<()> {
        // DB will be closed when dropped
        Ok(())
    }
}
