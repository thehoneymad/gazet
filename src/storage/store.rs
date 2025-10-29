//! Read-only interface for querying GridStore indexes.
//!
//! Use `GridStore` at query time to retrieve spatial phrase data.
//! See `GridStoreBuilder` for creating indexes.

use crate::storage::{
    decode_relev_score, encode_relev_score, pack_feature_id, unpack_feature_id, BuilderEntry,
    GridEntry, GridKey, Result, StorageError, TypeMarker,
};
use morton::deinterleave_morton;
use rocksdb::{Options, DB};
use std::path::Path;

/// Read-only interface to a GridStore database.
pub struct GridStore {
    db: DB,
}

impl GridStore {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let mut opts = Options::default();
        opts.set_allow_mmap_reads(true);

        let db = DB::open_for_read_only(&opts, path, false)?;
        Ok(GridStore { db })
    }

    pub fn get(&self, key: &GridKey) -> Result<Option<Vec<GridEntry>>> {
        let db_key = key.to_db_key(TypeMarker::SinglePhrase);
        match self.db.get(&db_key)? {
            Some(value) => {
                let entries = decode_value(&value)?;
                Ok(Some(entries))
            }
            None => Ok(None),
        }
    }
}

/// Deserializes stored BuilderEntry back to flat Vec<GridEntry>.
fn decode_value(value: &[u8]) -> Result<Vec<GridEntry>> {
    let builder_entry: BuilderEntry =
        bincode::deserialize(value).map_err(|e| StorageError::Serialization(e.to_string()))?;
    let mut entries = Vec::new();

    for (relev_score, morton_map) in builder_entry.inner {
        let (relev, score) = decode_relev_score(relev_score);

        for (morton, packed_feature_ids) in morton_map {
            let (x, y) = deinterleave_morton(morton);
            for packed_feature_id in packed_feature_ids {
                let (id, source_phrase_hash) = unpack_feature_id(packed_feature_id);
                entries.push(GridEntry {
                    relev,
                    score,
                    x,
                    y,
                    id,
                    source_phrase_hash,
                });
            }
        }
    }

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{truncate_score, GridStoreBuilder};
    use tempfile;

    #[test]
    fn test_get_returns_none_for_missing_key() {
        let dir = tempfile::tempdir().unwrap();
        let builder = GridStoreBuilder::new(dir.path()).unwrap();
        builder.finish().unwrap();

        let store = GridStore::new(dir.path()).unwrap();
        let key = GridKey {
            phrase_id: 999,
            lang_set: 0,
        };

        assert_eq!(store.get(&key).unwrap(), None);
    }

    #[test]
    fn test_get_retrieves_inserted_entries() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        let key = GridKey {
            phrase_id: 1,
            lang_set: 0,
        };
        let entries = vec![
            GridEntry {
                relev: 0.8,
                score: 255,
                x: 100,
                y: 200,
                id: 1,
                source_phrase_hash: 0,
            },
            GridEntry {
                relev: 1.0,
                score: 200,
                x: 101,
                y: 201,
                id: 2,
                source_phrase_hash: 1,
            },
        ];

        builder.insert(&key, entries.clone()).unwrap();
        builder.finish().unwrap();

        let store = GridStore::new(dir.path()).unwrap();
        let retrieved = store.get(&key).unwrap().unwrap();

        assert_eq!(retrieved.len(), 2);
        // Note: Order may differ due to HashMap iteration
        assert!(retrieved.iter().any(|e| e.id == 1 && e.x == 100));
        assert!(retrieved.iter().any(|e| e.id == 2 && e.x == 101));
    }

    #[test]
    fn test_roundtrip_preserves_data() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        let key = GridKey {
            phrase_id: 42,
            lang_set: 0,
        };
        let entry = GridEntry {
            relev: 0.6,
            score: 128,
            x: 500,
            y: 600,
            id: 12345,
            source_phrase_hash: 99,
        };

        builder.insert(&key, vec![entry.clone()]).unwrap();
        builder.finish().unwrap();

        let store = GridStore::new(dir.path()).unwrap();
        let retrieved = store.get(&key).unwrap().unwrap();

        assert_eq!(retrieved.len(), 1);
        let r = &retrieved[0];
        assert_eq!(r.relev, entry.relev);
        assert_eq!(r.score, truncate_score(entry.score));
        assert_eq!(r.x, entry.x);
        assert_eq!(r.y, entry.y);
        assert_eq!(r.id, entry.id);
        assert_eq!(r.source_phrase_hash, entry.source_phrase_hash);
    }
}
