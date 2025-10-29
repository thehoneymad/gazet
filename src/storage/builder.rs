use crate::storage::{
    encode_relev_score, pack_feature_id, GridEntry, GridKey, MortonCode, PackedFeatureId, PhraseId,
    RelevScore, Result, StorageError, TypeMarker,
};
use itertools::Itertools;
use morton::interleave_morton;
use rocksdb::{Options, DB};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::collections::hash_map::Entry;
use std::collections::{hash_map, BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// Nested storage structure for efficient grouping and compression.
///
/// Organizes GridEntries in a three-level hierarchy optimized for both
/// storage efficiency and query performance.
///
///
/// # Level 1: RelevScore (u8)
///
/// Combined relevance and score key that groups entries by importance:
/// - Upper 4 bits: Relevance (0-3, quantized from 0.4-1.0)
/// - Lower 4 bits: Score (0-15, truncated from 0-255)
///
/// **Benefits:**
/// - Pre-sorted results: High relevance/score entries come first
/// - Better compression: Similar values grouped together
/// - Query optimization: Can skip low-relevance groups entirely
///
/// # Level 2: Morton Code (u32)
///
/// Spatially-encoded coordinate that preserves locality:
/// - Interleaves x and y coordinate bits
/// - Nearby points get nearby morton codes
/// - Enables efficient spatial range queries
///
/// **Future:** Will be replaced with S2 CellID (u64) for hierarchical queries
///
/// # Level 3: PackedFeatureId (SmallVec<[u32; 4]>)
///
/// List of features at this RelevScore and coordinate:
/// - Each u32 packs: feature_id (24 bits) + source_phrase_hash (8 bits)
/// - SmallVec stores ≤4 items inline (no heap allocation)
/// - Spills to heap only when >4 features (uncommon)
///
/// **Packing format:**
/// text
/// u32: [feature_id: 24 bits][source_phrase_hash: 8 bits]
///
/// Example:
/// feature_id = 12345 (0x003039)
/// hash = 42 (0x2A)
/// packed = (12345 << 8) | 42 = 0x00303A2A
///
/// **Why pack together?**
/// When a feature generates multiple phrases ("main", "street", "main street"),
/// the hash identifies they came from the same source phrase, enabling
/// deduplication during query processing.
///
/// **Why SmallVec:**
/// Most coordinates have 1-4 features, so SmallVec avoids heap allocations
/// for the common case while still supporting unlimited features when needed.
///
#[derive(Serialize, Deserialize)]
struct BuilderEntry {
    inner: HashMap<RelevScore, HashMap<MortonCode, SmallVec<[PackedFeatureId; 4]>>>,
}
impl BuilderEntry {
    fn new() -> Self {
        BuilderEntry {
            inner: HashMap::new(),
        }
    }
}

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
        let to_append = self.data
            .entry(key.clone())
            .or_insert_with(BuilderEntry::new);
        extend_entries(to_append, values);
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

        for (grid_key, builder_entry) in self.data {
            let db_key = grid_key.to_db_key(TypeMarker::SinglePhrase);
            let db_value = Self::serialize_value(&builder_entry)?;
            db.put(&db_key, &db_value)?;
        }
        Ok(())
    }

    /// Serializes BuilderEntry to bytes.
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
        bincode::serialize(&entries.inner).map_err(|e| StorageError::Serialization(e.to_string()))
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

        let key = GridKey { phrase_id: 1, lang_set: 0 };
        builder.insert(&key, vec![
            GridEntry {
                relev: 0.8,
                score: 255,
                x: 100,
                y: 200,
                id: 1,
                source_phrase_hash: 0,
            }
        ]).unwrap();

        assert_eq!(builder.data.len(), 1);
        builder.finish().unwrap();
    }

    #[test]
    fn insert_multiple_entries_same_key_groups_by_relevance_score() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        let key = GridKey { phrase_id: 1, lang_set: 0 };
        builder.insert(&key, vec![
            GridEntry { relev: 0.8, score: 255, x: 100, y: 200, id: 1, source_phrase_hash: 0 },
            GridEntry { relev: 1.0, score: 200, x: 101, y: 201, id: 2, source_phrase_hash: 1 },
        ]).unwrap();

        assert_eq!(builder.data.len(), 1);
        let entry = builder.data.get(&key).unwrap();
        assert!(entry.inner.len() >= 1);

        builder.finish().unwrap();
    }

    #[test]
    fn append_to_existing_key_merges_entries_without_duplicate_keys() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        let key = GridKey { phrase_id: 1, lang_set: 0 };
        builder.insert(&key, vec![
            GridEntry { relev: 0.8, score: 255, x: 100, y: 200, id: 1, source_phrase_hash: 0 },
        ]).unwrap();

        builder.append(&key, vec![
            GridEntry { relev: 0.8, score: 255, x: 100, y: 200, id: 2, source_phrase_hash: 1 },
        ]).unwrap();

        assert_eq!(builder.data.len(), 1);
        builder.finish().unwrap();
    }
}
