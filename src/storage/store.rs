//! Read-only interface for querying GridStore indexes.
//!
//! Use `GridStore` at query time to retrieve spatial phrase data.
//! See `GridStoreBuilder` for creating indexes.

use crate::storage::{
    decode_boundaries, decode_relev_score, encode_relev_score, pack_feature_id, unpack_feature_id,
    BuilderEntry, GridEntry, GridKey, MatchEntry, MatchKey, MatchOpts, MatchPhrase, PhraseId,
    Result, StorageError, TypeMarker, BOUNDS_KEY,
};
use morton::deinterleave_morton;
use rocksdb::{Direction, IteratorMode, Options, DB};
use std::collections::HashSet;
use std::path::Path;

/// Read-only interface to a GridStore database.
pub struct GridStore {
    db: DB,
    /// Bin boundaries for prefix bin optimization.
    /// Contains phrase_id values where prefix bins start.
    /// Empty if no prefix bins were created during indexing.
    pub bin_boundaries: HashSet<PhraseId>,
}

impl GridStore {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let mut opts = Options::default();
        opts.set_allow_mmap_reads(true);

        let db = DB::open_for_read_only(&opts, path, false)?;

        // Read bin boundaries from database
        let bin_boundaries: HashSet<PhraseId> = match db.get(BOUNDS_KEY)? {
            Some(entry) => decode_boundaries(entry.as_ref()).into_iter().collect(),
            None => HashSet::new(),
        };

        Ok(GridStore { db, bin_boundaries })
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

    /// Queries the grid store for matching phrases with optional spatial filtering.
    ///
    /// Returns an iterator of matching entries, supporting both exact phrase lookups
    /// and range queries. Automatically uses prefix bins when available for efficient
    /// range queries.
    ///
    /// # Arguments
    /// * `match_key` - Query specification (phrase range and language filter)
    /// * `match_opts` - Spatial filtering options (bbox, proximity, zoom)
    ///
    /// # Returns
    /// Iterator of [`MatchEntry`] containing grid entries with language match metadata.
    ///
    /// # Query Types
    ///
    /// ## Exact Query
    /// ```ignore
    /// let match_key = MatchKey {
    ///     match_phrase: MatchPhrase::Exact(42),
    ///     lang_set: 0,
    /// };
    /// ```
    /// Looks up a single phrase_id.
    ///
    /// ## Range Query
    /// ```ignore
    /// let match_key = MatchKey {
    ///     match_phrase: MatchPhrase::Range { start: 100, end: 200 },
    ///     lang_set: 0,
    /// };
    /// ```
    /// Looks up multiple phrase_ids. Uses prefix bins if range aligns with bin boundaries.
    ///
    /// # Prefix Bin Optimization
    ///
    /// Range queries automatically use prefix bins when:
    /// - Both `start` and `end` exist in `bin_boundaries`
    /// - Enables O(1) bin lookups instead of O(n) individual phrase lookups
    ///
    /// # Error Handling
    ///
    /// Corrupted database entries are silently skipped to allow partial results.
    /// TODO: Add proper error logging for skipped entries.
    ///
    /// # Spatial Filtering
    ///
    /// TODO: Implement spatial filtering using `match_opts` (bbox, proximity, zoom).
    /// Currently returns all matching entries regardless of spatial constraints.
    pub fn get_matching(
        &self,
        match_key: &MatchKey,
        _match_opts: &MatchOpts, // TODO: Implement spatial filtering
    ) -> Result<Vec<MatchEntry>> {
        // Determine query strategy: exact vs range, prefix bins vs individual lookups
        let (fetch_start, fetch_end, fetch_type_marker) = match match_key.match_phrase {
            // Convert exact lookup to half-open range [id, id+1) for uniform iteration logic
            MatchPhrase::Exact(id) => (id, id + 1, TypeMarker::SinglePhrase),
            
            MatchPhrase::Range { start, end } => {
                // Use prefix bins only if query range exactly aligns with bin boundaries
                // Partial bin fetches are not supported - bins are pre-aggregated at write time
                if self.bin_boundaries.contains(&start) && self.bin_boundaries.contains(&end) {
                    (start, end, TypeMarker::PrefixBin)
                } else {
                    (start, end, TypeMarker::SinglePhrase)
                }
            }
        };

        // Normalize query to range format for uniform iteration
        let mut range_key = match_key.clone();
        range_key.match_phrase = MatchPhrase::Range {
            start: fetch_start,
            end: fetch_end,
        };

        // Create starting database key for iteration
        let start_key = range_key.to_start_key(fetch_type_marker)?;
        
        // Clone range_key for use in both closures (moved into each)
        let range_key_for_take = range_key.clone();
        let range_key_for_filter = range_key;
        
        // Iterate from start_key forward, stopping when keys no longer match range
        let db_iter = self
            .db
            .iterator(IteratorMode::From(&start_key, Direction::Forward))
            .take_while(move |result| {
                match result {
                    Ok((key, _value)) => {
                        // Stop iteration when key falls outside our phrase_id range
                        range_key_for_take
                            .matches_key(fetch_type_marker, key)
                            .unwrap_or(false)
                    }
                    Err(_) => false, // Stop on database error
                }
            });

        // Filter by language and decode values
        let results = db_iter
            .filter_map(move |result| {
                // Skip corrupted entries silently (TODO: add proper error logging)
                let (key, value) = result.ok()?;
                
                // Check if entry's language overlaps with query language filter
                let matches_language = range_key_for_filter.matches_language(&key).ok()?;
                
                // Decode stored BuilderEntry to flat Vec<GridEntry>
                let entries = decode_value(&value).ok()?;

                // Wrap each GridEntry with language match metadata
                Some(entries.into_iter().map(move |grid_entry| MatchEntry {
                    grid_entry,
                    matches_language,
                }))
            })
            .flatten()
            .collect(); // TODO: Return iterator instead of Vec (see README for investigation)
            
        Ok(results)
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

    #[test]
    fn test_gridstore_reads_bin_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        // Add some phrases
        for i in 0..10 {
            let key = GridKey {
                phrase_id: i,
                lang_set: 0,
            };
            builder
                .insert(
                    &key,
                    vec![GridEntry {
                        relev: 1.0,
                        score: 10,
                        x: i as u16,
                        y: 1,
                        id: i,
                        source_phrase_hash: 0,
                    }],
                )
                .unwrap();
        }

        // Set boundaries
        builder.load_bin_boundaries(vec![3, 7]).unwrap();
        builder.finish().unwrap();

        // Open store and verify boundaries were read
        let store = GridStore::new(dir.path()).unwrap();
        assert_eq!(store.bin_boundaries.len(), 2);
        assert!(store.bin_boundaries.contains(&3));
        assert!(store.bin_boundaries.contains(&7));
    }

    #[test]
    fn test_gridstore_empty_boundaries_when_none_set() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        let key = GridKey {
            phrase_id: 1,
            lang_set: 0,
        };
        builder
            .insert(
                &key,
                vec![GridEntry {
                    relev: 1.0,
                    score: 10,
                    x: 1,
                    y: 1,
                    id: 1,
                    source_phrase_hash: 0,
                }],
            )
            .unwrap();

        // Don't set boundaries
        builder.finish().unwrap();

        // Open store and verify boundaries are empty
        let store = GridStore::new(dir.path()).unwrap();
        assert_eq!(store.bin_boundaries.len(), 0);
    }
}
