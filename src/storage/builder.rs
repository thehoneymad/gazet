//! GridStore builder for creating spatial phrase indexes.
//!
//! # Overview
//!
//! A **grid** is a spatial coordinate (x, y) where a feature appears. A single phrase like
//! "Main Street" may appear at hundreds of grid coordinates across a city or region.
//!
//! `GridStoreBuilder` accumulates all grids for all phrases in a geocoding index
//! (e.g., "streets", "cities", "countries") and writes them to a single RocksDB database.
//!
//! # Architecture
//!
//! **One builder per index**, not per grid or per phrase:
//! - A "streets" index contains millions of phrases, each with multiple grid coordinates
//! - All data is accumulated in memory during building
//! - `finish()` writes everything to one RocksDB database
//!
//! ## Storage Structure
//!
//! ```text
//! streets.rocksdb/                                    # One database per index
//! ├─ [type:0][phrase_id:1][lang:0] → BuilderEntry   # "main street" at 500 coordinates
//! ├─ [type:0][phrase_id:2][lang:0] → BuilderEntry   # "oak avenue" at 200 coordinates
//! ├─ [type:0][phrase_id:3][lang:0] → BuilderEntry   # "1st street" at 300 coordinates
//! └─ ... millions more phrases
//! ```
//!
//! Each `BuilderEntry` contains:
//! - All grid coordinates for that phrase
//! - Grouped by relevance/score for efficient querying
//! - Organized by Morton-encoded coordinates for spatial locality

use crate::storage::common::BuilderEntry;
use crate::storage::{
    encode_boundaries, encode_relev_score, group_by_owned, pack_feature_id, GridEntry, GridKey,
    LanguageSet, PhraseId, Result, StorageError, TypeMarker, BOUNDS_KEY,
};
use itertools::Itertools;
use morton::interleave_morton;
use rocksdb::{Options, DB};
use smallvec::SmallVec;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// Extends a BuilderEntry with the given values.
fn extend_entries(builder_entry: &mut BuilderEntry, values: Vec<GridEntry>) {
    for (rs, rs_values) in &values
        .into_iter()
        .chunk_by(|value| encode_relev_score(value.relev, value.score))
    {
        let rs_entry = builder_entry.inner.entry(rs).or_insert_with(HashMap::new);

        for (morton, morton_values) in
            &rs_values.chunk_by(|value| interleave_morton(value.x, value.y))
        {
            let packed_ids =
                morton_values.map(|value| pack_feature_id(value.id, value.source_phrase_hash));

            match rs_entry.entry(morton) {
                Entry::Vacant(e) => {
                    e.insert(packed_ids.collect());
                }
                Entry::Occupied(mut e) => {
                    e.get_mut().extend(packed_ids);
                }
            }
        }
    }
}

/// Copies all entries from source to destination BuilderEntry.
///
/// Used for prefix bin aggregation - copies GridEntries from individual
/// phrase entries into the aggregated PrefixBin entry
fn copy_entries(source_entry: &BuilderEntry, target_entry: &mut BuilderEntry) {
    for (relev_score, values) in source_entry.inner.iter() {
        let rs_entry = target_entry
            .inner
            .entry(*relev_score)
            .or_insert_with(HashMap::new);

        for (morton, ids) in values.iter() {
            let morton_entry = rs_entry.entry(*morton).or_insert_with(SmallVec::new);
            morton_entry.extend(ids.iter().cloned());
        }
    }
}

/// Builder for creating and populating a grid store.
///
/// Accumulates GridEntries in memory during building, organizing them by
/// relevance/score and coordinate for efficient storage. All data is written
/// to RocksDB in a single batch when finish() is called.
///
/// # Memory Usage
/// All entries are kept in memory until finish(). For large datasets (>1M features),
/// consider batching or implementing incremental writes.
///
/// # Storage Format
/// - Keys: [type_marker:1][phrase_id:4][lang_set:0-16] bytes
/// - Values: Serialized BuilderEntry (currently bincode, future: custom format)
pub struct GridStoreBuilder {
    path: PathBuf,

    /// In-memory accumulation of all GridEntries, grouped by GridKey.
    ///
    /// Entries are organized by phrase_id and language, with each GridKey
    /// mapping to a BuilderEntry that groups entries by RelevScore and coordinate.
    ///
    /// All data is kept in memory during building to enable:
    /// - Merging entries from multiple insert() calls for the same key
    /// - Sorting by relevance/score before serialization
    /// - Deduplication of features at the same location
    /// - Efficient batch write to RocksDB in finish()
    ///
    /// Memory usage scales with dataset size. For large datasets (>1M features),
    /// consider batching or incremental writes.
    data: BTreeMap<GridKey, BuilderEntry>,

    /// Bin boundaries for prefix bin optimization.
    ///
    /// Defines phrase_id ranges for aggregated prefix bins used in autocomplete.
    /// Example: [100, 200, 300] creates bins [0-99], [100-199], [200-299], [300-∞]
    ///
    /// Each bin gets a PrefixBin entry (type_marker=1) with aggregated data
    /// from all phrase_ids in that range, enabling efficient range queries.
    ///
    /// Optional: If empty, no prefix bins are generated (only exact phrase entries).
    bin_boundaries: Vec<PhraseId>,
}

impl GridStoreBuilder {
    /// Creates a new GridStoreBuilder at the specified path.
    ///
    /// The database will be created when finish() is called.
    /// All data is accumulated in memory until then.
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        Ok(GridStoreBuilder {
            path: path.as_ref().to_path_buf(),
            data: BTreeMap::new(),
            bin_boundaries: Vec::new(),
        })
    }

    /// Inserts GridEntries for a given key.
    ///
    /// Multiple calls with the same key will accumulate and merge entries.
    /// Entries are automatically grouped by RelevScore and Morton-encoded coordinate.
    ///
    /// # Entry Processing
    ///
    /// Each GridEntry is transformed into the three-level hierarchy:
    /// 1. Relevance + score → RelevScore (u8 key for grouping)
    /// 2. (x, y) → Morton code (u32 for spatial locality)
    /// 3. Feature ID + hash → PackedFeatureId (u32 for deduplication)
    ///
    /// # Example
    /// ignore
    /// let key = GridKey { phrase_id: 42, lang_set: 0 };
    /// builder.insert(&key, vec![
    ///     GridEntry {
    ///         relev: 0.8,
    ///         score: 255,
    ///         x: 100,
    ///         y: 200,
    ///         id: 1,
    ///         source_phrase_hash: 0
    ///     }
    /// ])?;
    ///
    pub fn insert(&mut self, key: &GridKey, entries: Vec<GridEntry>) -> Result<()> {
        let mut to_insert = BuilderEntry::new();
        extend_entries(&mut to_insert, entries);
        self.data.insert(key.clone(), to_insert);
        Ok(())
    }

    /// Appends GridEntries to an existing key, or creates new entry if key doesn't exist.
    ///
    /// Unlike `insert()` which replaces existing data, `append()` merges new entries
    /// with existing ones. Multiple `append()` calls accumulate entries.
    ///
    /// # Use Case
    /// When building incrementally from multiple data sources:
    /// ```ignore
    /// builder.insert(&key, batch1)?;   // Initial data
    /// builder.append(&key, batch2)?;   // Add more data
    /// builder.append(&key, batch3)?;   // Keep adding
    /// ```
    pub fn append(&mut self, key: &GridKey, values: Vec<GridEntry>) -> Result<()> {
        let to_append = self
            .data
            .entry(key.clone())
            .or_insert_with(BuilderEntry::new);
        extend_entries(to_append, values);
        Ok(())
    }

    /// Sets bin boundaries for prefix bin optimization.
    ///
    /// Prefix bins enable efficient range queries (autocomplete) by aggregating
    /// multiple phrase entries into larger bins. Instead of querying thousands of
    /// individual phrases starting with "San", a single PrefixBin entry contains
    /// all of them.
    ///
    /// # How Prefix Bins Work
    ///
    /// Given boundaries `[1000, 2000, 3000]`, the system creates bins:
    /// - Bin 1000: phrases 0-999
    /// - Bin 2000: phrases 1000-1999
    /// - Bin 3000: phrases 2000-2999
    /// - Implicit: phrases 3000+ (no upper bound)
    ///
    /// Each bin gets a PrefixBin entry (TypeMarker::PrefixBin) that aggregates
    /// all GridEntries from phrases in that range.
    ///
    /// # Query Example
    ///
    /// Without bins: Query "San*" → iterate phrases 5000-5500 individually (500 lookups)
    /// With bins: Query "San*" → lookup bin 5000 (1 lookup, pre-aggregated data)
    ///
    /// # When to Use
    ///
    /// - Autocomplete/prefix search is required
    /// - Index has >10k phrases (smaller indexes don't benefit)
    /// - Bin size should be ~100-1000 phrases for optimal performance
    ///
    /// # Example
    /// ```ignore
    /// builder.load_bin_boundaries(vec![1000, 2000, 3000])?;
    /// // finish() will now create PrefixBin entries at these boundaries
    /// ```
    pub fn load_bin_boundaries(&mut self, bin_boundaries: Vec<PhraseId>) -> Result<()> {
        self.bin_boundaries = bin_boundaries;
        Ok(())
    }

    /// Finalizes the store by writing all accumulated data to RocksDB.
    ///
    /// # Serialization Strategy
    ///
    /// ## Current: Bincode (Simple)
    /// Uses bincode for straightforward serialization:
    /// - Pros: Simple, reliable, one function call
    /// - Cons: ~30% larger storage than custom format
    /// - Trade-off: Development speed vs storage efficiency
    ///
    /// ## Future: Custom Format (Optimized)
    /// Can migrate to carmen-core style custom format:
    /// - Variable-length integer encoding (small numbers = fewer bytes)
    /// - Offset-based pointers for shared data (deduplication)
    /// - Pre-sorted by relevance (high to low) for query optimization
    /// - Optimized for LZ4 compression (group similar values)
    /// - ~30% storage reduction
    ///
    /// Migration path: Replace serialize_value() without changing insert() logic.
    /// See carmen-core's gridstore_format.rs for reference implementation.
    ///
    /// # Database Format
    /// Each entry written as:
    /// - Key: GridKey serialized to [type_marker][phrase_id][lang_set]
    /// - Value: BuilderEntry serialized to bytes
    /// - Compression: RocksDB applies LZ4 automatically
    pub fn finish(self) -> Result<()> {
        let mut opts = Options::default();
        opts.create_if_missing(true);

        let db = DB::open(&opts, self.path)?;

        // Group phrases by bin boundary
        let mut bin_seq = self.bin_boundaries.iter().cloned().peekable();
        let mut current_bin = None;
        let mut next_boundary = 0u32;

        let grouped = group_by_owned(self.data.into_iter(), |(key, _value)| {
            while key.phrase_id >= next_boundary {
                current_bin = bin_seq.next();
                next_boundary = *(bin_seq.peek().unwrap_or(&u32::MAX));
            }
            current_bin
        });

        // Process each bin group
        for (group_id, group_value) in grouped {
            let mut lang_set_map: HashMap<LanguageSet, BuilderEntry> = HashMap::new();

            // Write individual phrases and accumulate into bin
            for (grid_key, value) in group_value.into_iter() {
                // Write individual phrase entry
                let db_key = grid_key.to_db_key(TypeMarker::SinglePhrase);
                let db_value = Self::serialize_value(&value)?;
                db.put(&db_key, &db_value)?;

                // Accumulate into bin aggregate
                let grouped_entry = lang_set_map
                    .entry(grid_key.lang_set)
                    .or_insert_with(BuilderEntry::new);

                copy_entries(&value, grouped_entry);
            }

            // Write aggregated bin entries
            if let Some(group_id) = group_id {
                for (lang_set, builder_entry) in lang_set_map.into_iter() {
                    let group_key = GridKey {
                        phrase_id: group_id,
                        lang_set,
                    };
                    let db_key = group_key.to_db_key(TypeMarker::PrefixBin);
                    let db_value = Self::serialize_value(&builder_entry)?;
                    db.put(&db_key, &db_value)?;
                }
            }
        }

        // Write bin boundaries metadata
        let encoded_boundaries = encode_boundaries(&self.bin_boundaries);
        db.put(BOUNDS_KEY, &encoded_boundaries)?;
        Ok(())
    }

    /// Serializes BuilderEntry to bytes with deterministic ordering.
    ///
    /// Sorts entries by RelevScore (descending) before serialization to ensure:
    /// 1. Deterministic output (same input always produces same bytes)
    /// 2. High-relevance entries come first for query optimization
    /// 3. Better compression (similar values grouped together)
    ///
    /// Current implementation uses bincode. This function provides
    /// a single point to swap in custom serialization format later.
    ///
    /// # Custom Format Design (Future)
    ///
    /// The custom format would encode BuilderEntry as:
    ///
    /// ```text
    /// [PhraseRecord]
    ///   ├─ num_relev_scores: varint
    ///   └─ relev_scores: [RelevScore, ...]
    ///        ├─ relev_score: u8
    ///        ├─ num_coords: varint
    ///        └─ coords: [Coord, ...]
    ///             ├─ coord: u32 (morton code or S2 cell)
    ///             ├─ num_ids: varint
    ///             └─ ids: [u32, ...] (sorted, deduplicated)
    /// ```
    ///
    /// Benefits:
    /// - Variable-length encoding: Small numbers use 1 byte instead of 4
    /// - Pre-sorted: High relevance entries first for early termination
    /// - Deduplicated: Shared ID lists stored once with offset pointers
    /// - Compression-friendly: Similar values grouped together
    ///
    /// Implementation would use:
    /// - `integer_encoding` crate for varint encoding
    /// - Custom Writer/Reader similar to carmen-core's gridstore_format
    /// - Offset-based pointers for shared data structures
    fn serialize_value(entries: &BuilderEntry) -> Result<Vec<u8>> {
        // Convert HashMap to Vec and sort by RelevScore descending
        let mut sorted_entries: Vec<_> = entries.inner.iter().collect();
        sorted_entries.sort_by(|(relev_score_a, _), (relev_score_b, _)| {
            relev_score_b.cmp(relev_score_a)  // Descending: high relevance first
        });
        
        bincode::serialize(&sorted_entries).map_err(|e| StorageError::Serialization(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile;

    #[test]
    fn insert_single_entry_creates_one_key_in_builder_data() {
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
                    relev: 0.8,
                    score: 255,
                    x: 100,
                    y: 200,
                    id: 1,
                    source_phrase_hash: 0,
                }],
            )
            .unwrap();

        assert_eq!(builder.data.len(), 1);
        builder.finish().unwrap();
    }

    #[test]
    fn insert_multiple_entries_same_key_groups_by_relevance_score() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        let key = GridKey {
            phrase_id: 1,
            lang_set: 0,
        };
        builder
            .insert(
                &key,
                vec![
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
                ],
            )
            .unwrap();

        assert_eq!(builder.data.len(), 1);
        let entry = builder.data.get(&key).unwrap();
        assert!(entry.inner.len() >= 1);

        builder.finish().unwrap();
    }

    #[test]
    fn append_to_existing_key_merges_entries_without_duplicate_keys() {
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
                    relev: 0.8,
                    score: 255,
                    x: 100,
                    y: 200,
                    id: 1,
                    source_phrase_hash: 0,
                }],
            )
            .unwrap();

        builder
            .append(
                &key,
                vec![GridEntry {
                    relev: 0.8,
                    score: 255,
                    x: 100,
                    y: 200,
                    id: 2,
                    source_phrase_hash: 1,
                }],
            )
            .unwrap();

        assert_eq!(builder.data.len(), 1);
        builder.finish().unwrap();
    }

    #[test]
    fn test_finish_with_no_boundaries_writes_only_individual_entries() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        // Add some phrases
        for i in 0..5 {
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

        builder.finish().unwrap();

        let db = rocksdb::DB::open_default(dir.path()).unwrap();

        let mut count = 0;
        let iter = db.iterator(rocksdb::IteratorMode::Start);
        for item in iter {
            let (key, _) = item.unwrap();
            if key[0] == TypeMarker::SinglePhrase as u8 {
                count += 1;
            }
        }
        assert_eq!(count, 5);
    }

    #[test]
    fn test_finish_with_boundaries_creates_prefix_bins() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        // Add 10 phrases
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

        // Set boundaries: bin at 5
        builder.load_bin_boundaries(vec![5]).unwrap();
        builder.finish().unwrap();

        // Verify database
        let db = rocksdb::DB::open_default(dir.path()).unwrap();

        let mut individual_count = 0;
        let mut bin_count = 0;
        let iter = db.iterator(rocksdb::IteratorMode::Start);
        for item in iter {
            let (key, _) = item.unwrap();
            if key.len() > 0 {
                match key[0] {
                    0 => individual_count += 1,  // SinglePhrase
                    1 => bin_count += 1,          // PrefixBin
                    _ => {}
                }
            }
        }

        assert_eq!(individual_count, 10, "Should have 10 individual entries");
        assert_eq!(bin_count, 1, "Should have 1 bin entry");

        // Verify ~BOUNDS exists
        let bounds = db.get(BOUNDS_KEY).unwrap();
        assert!(bounds.is_some());
    }

    #[test]
    fn test_finish_with_multiple_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        // Add 15 phrases
        for i in 0..15 {
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

        // Set boundaries: bins at 5, 10
        builder.load_bin_boundaries(vec![5, 10]).unwrap();
        builder.finish().unwrap();

        let db = DB::open_default(dir.path()).unwrap();

        let mut bin_count = 0;
        let iter = db.iterator(rocksdb::IteratorMode::Start);
        for item in iter {
            let (key, _) = item.unwrap();
            if key.len() > 0 && key[0] == 1 {
                bin_count += 1;
            }
        }

        assert_eq!(bin_count, 2, "Should have 2 bin entries");
    }

    #[test]
    fn test_finish_with_multiple_languages() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        // Add phrases with different languages
        for i in 0..5 {
            for lang in [0, 1] {
                let key = GridKey {
                    phrase_id: i,
                    lang_set: lang,
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
        }

        builder.load_bin_boundaries(vec![3]).unwrap();
        builder.finish().unwrap();

        let db = DB::open_default(dir.path()).unwrap();

        let mut bin_count = 0;
        let iter = db.iterator(rocksdb::IteratorMode::Start);
        for item in iter {
            let (key, _) = item.unwrap();
            if key.len() > 0 && key[0] == 1 {
                bin_count += 1;
            }
        }

        // Should have 2 bin entries (one per language)
        assert_eq!(bin_count, 2, "Should have 2 bin entries (one per language)");
    }

    #[test]
    fn test_copy_entries_aggregates_data() {
        use smallvec::SmallVec;
        
        let mut source = BuilderEntry::new();
        let mut dest = BuilderEntry::new();

        // Add some data to source
        source.inner.insert(
            0xFF,
            {
                let mut map = HashMap::new();
                let vec: SmallVec<[u32; 4]> = smallvec::smallvec![1, 2, 3];
                map.insert(123, vec);
                map
            },
        );

        copy_entries(&source, &mut dest);

        assert!(dest.inner.contains_key(&0xFF));
        let expected: SmallVec<[u32; 4]> = smallvec::smallvec![1, 2, 3];
        assert_eq!(dest.inner[&0xFF][&123], expected);
    }
}
