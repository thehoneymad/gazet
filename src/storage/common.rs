use byteorder::{BigEndian, WriteBytesExt};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::collections::HashMap;

/// Unique identifier for a phrase (supports up to 4 billion phrases).
pub type PhraseId = u32;

/// 128-bit bitfield representing language support.
/// Each bit represents a language. Empty (0) means all languages.
pub type LanguageSet = u128;

/// Default zoom level for tile coordinate system (approximately 150m resolution with S2 Level 16)
pub const DEFAULT_ZOOM: u16 = 16;

/// Maximum expected size of a database key in bytes.
/// Format: 1 (type_marker) + 4 (phrase_id) + 16 (max lang_set)
pub const MAX_DB_KEY_SIZE: usize = 21;

/// Language set value indicating all languages are supported.
pub const ALL_LANGUAGES: LanguageSet = u128::MAX;

/// Language set value indicating no specific languages.
pub const NO_LANGUAGES: LanguageSet = 0;

/// Type marker for database key entries.
///
/// Distinguishes between exact phrase lookups and prefix bin aggregations.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TypeMarker {
    /// Exact phrase entry - stores all GridEntries for a specific phrase_id
    SinglePhrase = 0,
    /// Prefix bin entry - aggregated GridEntries for a range of phrase_ids
    PrefixBin = 1,
}

/// Key for indexing phrases in the spatial grid store.
///
/// Combines a phrase identifier with a language set to enable
/// multi-language phrase lookups. Identical to carmen-core's GridKey.
///
/// # Fields
/// * `phrase_id` - Unique identifier for the phrase (supports up to 4 billion phrases)
/// * `lang_set` - 128-bit bitfield representing supported languages (empty = all languages)
#[derive(Serialize, Deserialize, Debug, PartialOrd, Ord, PartialEq, Eq, Clone)]
pub struct GridKey {
    pub phrase_id: PhraseId,
    pub lang_set: LanguageSet,
}

impl GridKey {
    /// Serializes the key to database format.
    ///
    /// # Format
    /// `[type_marker: 1 byte][phrase_id: 4 bytes][lang_set: 0-16 bytes]`
    ///
    /// ## Type Marker
    /// - `TypeMarker::SinglePhrase` (0) - Exact phrase entry
    /// - `TypeMarker::PrefixBin` (1) - Prefix bin for range queries
    ///
    /// ## Phrase ID
    /// - 4 bytes, big-endian u32
    /// - Fixed width for lexicographic sorting
    ///
    /// ## Language Set
    /// - Variable length (0-16 bytes), compressed by skipping leading zeros
    /// - `u128::MAX` - All languages (writes nothing, empty = universal)
    /// - `0` - No languages (writes single 0 byte)
    /// - Other values - Specific language bits (compressed)
    ///
    pub fn to_db_key(&self, type_marker: TypeMarker) -> Vec<u8> {
        let mut key = Vec::with_capacity(MAX_DB_KEY_SIZE); // 1 + 4 + 16 max
        key.push(type_marker as u8);
        key.write_u32::<BigEndian>(self.phrase_id).unwrap();

        match self.lang_set {
            ALL_LANGUAGES => {
                // All languages - write nothing (empty = universal)
            }
            NO_LANGUAGES => {
                // Empty language set
                key.push(0);
            }
            _ => {
                // Specific languages - compress
                let bytes = self.lang_set.to_be_bytes();
                let start = bytes.iter().position(|&b| b != 0).unwrap_or(16);
                key.extend_from_slice(&bytes[start..]);
            }
        }

        debug_assert!(
            key.len() <= MAX_DB_KEY_SIZE,
            "Key size {} exceeds maximum {}",
            key.len(),
            MAX_DB_KEY_SIZE
        );

        key
    }
}

/// Specifies which phrase(s) to match in a query.
///
/// Supports both exact phrase lookups and range queries for efficient
/// prefix matching and autocomplete functionality.
///
/// # Variants
/// * `Exact(PhraseId)` - Single phrase lookup. Used when the exact phrase_id is known.
///   Example: searching "main street" → phrase_id 42 → `Exact(42)`
///
/// * `Range { start, end }` - Multiple phrase lookup via range scan [start, end).
///   Used for prefix matching and autocomplete where multiple phrase_ids match.
///   Example: autocomplete "mai..." → phrase_ids 100-150 → `Range { start: 100, end: 150 }`
///   More efficient than multiple individual lookups.
#[derive(Serialize, Deserialize, Debug, PartialOrd, Ord, PartialEq, Eq, Clone)]
pub enum MatchPhrase {
    /// Match a single exact phrase_id
    Exact(PhraseId),
    /// Match a range of phrase_ids [start, end) - half-open interval
    Range { start: PhraseId, end: PhraseId },
}

/// Query key combining phrase matching with language filtering.
///
/// Used to search the grid store for phrases in specific languages.
/// The lang_set bitfield enables efficient multi-language queries.
///
/// # Examples
/// * Search for exact phrase in English: `MatchKey { match_phrase: Exact(42), lang_set: english_bit }`
/// * Search for phrase range in any language: `MatchKey { match_phrase: Range{start: 100, end: 200}, lang_set: 0 }`
#[derive(Serialize, Deserialize, Debug, PartialOrd, Ord, PartialEq, Eq, Clone)]
pub struct MatchKey {
    pub match_phrase: MatchPhrase,
    pub lang_set: LanguageSet,
}

/// Query options for spatial filtering and proximity ranking.
///
/// Controls geographic constraints and zoom level for grid queries.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub struct MatchOpts {
    /// Optional bounding box [min_x, min_y, max_x, max_y] in tile coordinates
    pub bbox: Option<[u16; 4]>,
    /// Optional proximity point [x, y] for distance-based ranking
    pub proximity: Option<[u16; 2]>,
    /// Zoom level for tile coordinate system (default: 16)
    pub zoom: u16,
}

/// Unique identifier for a feature (truncated to 24 bits in storage).
pub type FeatureId = u32;

/// Entry in the spatial grid store representing a feature at a location.
///
/// Stores spatial coordinates, relevance scoring, and feature identification.
/// Uses tile coordinates (x, y) at a specific zoom level.
///
/// # Deduplication
/// The `source_phrase_hash` enables deduplication when the same feature matches
/// multiple query phrases. For example, a feature "123 Main Street" generates
/// phrases like "main street", "main st", "main street seattle" - all pointing
/// to the same feature. The hash identifies these as originating from the same
/// source phrase to avoid duplicate results.
///
/// # Storage Optimization
/// - `relev` is quantized to 4 bits (0.4, 0.6, 0.8, 1.0)
/// - `id` is truncated to 24 bits (supports ~16 million features)
/// - `source_phrase_hash` uses 8 bits (256 values, collisions acceptable)
#[derive(Serialize, Deserialize, Debug, PartialOrd, PartialEq, Clone)]
pub struct GridEntry {
    /// Relevance score (0.0-1.0), quantized to 4 bits in storage
    pub relev: f64,
    /// Importance score (0-255)
    pub score: u8,
    /// X coordinate in tile space
    pub x: u16,
    /// Y coordinate in tile space
    pub y: u16,
    /// Feature ID, truncated to 24 bits in storage
    pub id: FeatureId,
    /// Hash of source phrase for deduplication (8 bits)
    pub source_phrase_hash: u8,
}

/// Converts float relevance to 2-bit integer (0-3).
///
/// Relevance values are quantized to 4 discrete levels during indexing
/// for storage efficiency (2 bits instead of 64 bits for f64).
///
/// # Quantization Levels
/// - 0.4 → 0 (very common terms, low relevance)
/// - 0.6 → 1 (common terms, medium-low relevance)
/// - 0.8 → 2 (uncommon terms, medium-high relevance)
/// - 1.0 → 3 (rare terms, high relevance)
///
/// # Why Quantization?
/// - Storage: 2 bits vs 64 bits per entry
/// - Compression: Fewer unique values compress better with LZ4
/// - Precision: Acceptable loss for approximate text matching
#[inline]
pub fn relev_float_to_int(relev: f64) -> u8 {
    if relev <= 0.4 {
        0
    } else if relev <= 0.6 {
        1
    } else if relev <= 0.8 {
        2
    } else {
        3
    }
}

/// Converts quantized relevance back to float.
#[inline]
pub fn relev_int_to_float(relev: u8) -> f64 {
    match relev {
        0 => 0.4,
        1 => 0.6,
        2 => 0.8,
        _ => 1.0,
    }
}

/// Combined relevance and score key (4 bits each, stored in u8).
/// Upper 4 bits: relevance (0-3)
/// Lower 4 bits: score (0-15)
pub type RelevScore = u8;

/// Combines relevance and score into a single byte for efficient grouping.
///
/// Creates a composite key used to group GridEntries by importance:
/// - Upper 4 bits: relevance (0-3, quantized from 0.4-1.0)
/// - Lower 4 bits: score (0-15, truncated from 0-255)
///
/// This grouping enables:
/// 1. Pre-sorted storage (high relevance/score first)
/// 2. Better compression (similar values grouped together)
/// 3. Efficient query filtering (skip low-relevance groups)
///
/// # Example
///
/// let key = encode_relev_score(0.8, 255);
/// // relev 0.8 → 2 (0010 in binary)
/// // score 255 → 15 (truncated to 1111 in binary)
/// // result: 0010_1111 = 47
///
#[inline]
pub fn encode_relev_score(relev: f64, score: u8) -> RelevScore {
    let relev_bits = relev_float_to_int(relev);
    let score_bits = score & 0x0F;
    (relev_bits << 4) | score_bits
}

/// Decodes relevance and score from packed RelevScore.
#[inline]
pub fn decode_relev_score(relev_score: RelevScore) -> (f64, u8) {
    let relev = relev_int_to_float(relev_score >> 4);
    let score = relev_score & 0x0F;
    (relev, score)
}

/// Packed feature identifier with source phrase hash for deduplication.
///
/// Combines feature ID (24 bits) and source phrase hash (8 bits) into a single u32.
/// The hash enables deduplication when the same feature matches multiple query phrases.
///
/// Format: [feature_id: 24 bits][source_phrase_hash: 8 bits]
pub type PackedFeatureId = u32;

/// Morton-encoded spatial coordinate.
///
/// A u32 that encodes (x, y) tile coordinates by interleaving their bits.
/// Preserves spatial locality: nearby coordinates have nearby morton codes.
///
/// Created by: `interleave_morton(x: u16, y: u16) -> MortonCode`
///
/// # Future Migration
/// Will be replaced with S2 CellID (u64) for hierarchical spatial queries.
pub type MortonCode = u32;

/// Packs feature ID (24 bits) and source phrase hash (8 bits) into u32.
#[inline]
pub fn pack_feature_id(id: FeatureId, source_phrase_hash: u8) -> PackedFeatureId {
    (id << 8) | (source_phrase_hash as u32)
}

/// Unpacks feature ID and source phrase hash from PackedFeatureId.
#[inline]
pub fn unpack_feature_id(packed: PackedFeatureId) -> (FeatureId, u8) {
    let id = packed >> 8;
    let source_phrase_hash = (packed & 0xFF) as u8;
    (id, source_phrase_hash)
}

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
pub struct BuilderEntry {
    pub(crate) inner: HashMap<RelevScore, HashMap<MortonCode, SmallVec<[PackedFeatureId; 4]>>>,
}

impl BuilderEntry {
    pub(crate) fn new() -> Self {
        BuilderEntry {
            inner: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{encode_relev_score, relev_float_to_int};
    use crate::storage::{decode_relev_score, pack_feature_id, unpack_feature_id};

    #[test]
    fn test_relev_float_to_int() {
        assert_eq!(relev_float_to_int(0.0), 0);
        assert_eq!(relev_float_to_int(0.4), 0);
        assert_eq!(relev_float_to_int(0.5), 1);
        assert_eq!(relev_float_to_int(0.6), 1);
        assert_eq!(relev_float_to_int(0.7), 2);
        assert_eq!(relev_float_to_int(0.8), 2);
        assert_eq!(relev_float_to_int(0.9), 3);
        assert_eq!(relev_float_to_int(1.0), 3);
        assert_eq!(relev_float_to_int(4.0), 3);
    }

    #[test]
    fn test_encode_relev_score() {
        // Test all relevance levels with max score
        assert_eq!(encode_relev_score(0.4, 255), 0b0000_1111);
        assert_eq!(encode_relev_score(0.6, 255), 0b0001_1111);
        assert_eq!(encode_relev_score(0.8, 255), 0b0010_1111);
        assert_eq!(encode_relev_score(1.0, 255), 0b0011_1111);

        // Test score truncation
        assert_eq!(encode_relev_score(1.0, 0), 0b0011_0000);
        assert_eq!(encode_relev_score(1.0, 15), 0b0011_1111);
        assert_eq!(encode_relev_score(1.0, 16), 0b0011_0000);
        assert_eq!(encode_relev_score(1.0, 240), 0b0011_0000);

        // Test both together
        assert_eq!(encode_relev_score(0.6, 2), 0b0001_0010);
    }

    #[test]
    fn test_to_db_key() {
        use super::{GridKey, TypeMarker, ALL_LANGUAGES, NO_LANGUAGES};

        // Test with all languages
        let key = GridKey {
            phrase_id: 42,
            lang_set: ALL_LANGUAGES,
        };
        let db_key = key.to_db_key(TypeMarker::SinglePhrase);
        assert_eq!(
            db_key,
            vec![
                0b0000_0000, // type marker
                0b0000_0000,
                0b0000_0000,
                0b0000_0000,
                0b0010_1010 // phrase_id = 42
            ]
        );

        // Test with no languages
        let key = GridKey {
            phrase_id: 42,
            lang_set: NO_LANGUAGES,
        };
        let db_key = key.to_db_key(TypeMarker::SinglePhrase);
        assert_eq!(
            db_key,
            vec![
                0b0000_0000, // type marker
                0b0000_0000,
                0b0000_0000,
                0b0000_0000,
                0b0010_1010, // phrase_id = 42
                0b0000_0000  // NO_LANGUAGES marker
            ]
        );

        // Test with specific language (bit 0 set)
        let key = GridKey {
            phrase_id: 42,
            lang_set: 1,
        };
        let db_key = key.to_db_key(TypeMarker::SinglePhrase);
        assert_eq!(
            db_key,
            vec![
                0b0000_0000, // type marker
                0b0000_0000,
                0b0000_0000,
                0b0000_0000,
                0b0010_1010, // phrase_id = 42
                0b0000_0001  // lang_set = 1 (compressed)
            ]
        );

        // Test prefix bin type marker
        let key = GridKey {
            phrase_id: 100,
            lang_set: ALL_LANGUAGES,
        };
        let db_key = key.to_db_key(TypeMarker::PrefixBin);
        assert_eq!(
            db_key,
            vec![
                0b0000_0001, // type marker = 1
                0b0000_0000,
                0b0000_0000,
                0b0000_0000,
                0b0110_0100 // phrase_id = 100
            ]
        );
    }

    #[test]
    fn test_decode_relev_score() {
        // Test all relevance levels
        assert_eq!(decode_relev_score(0b0000_1111), (0.4, 15));
        assert_eq!(decode_relev_score(0b0001_1111), (0.6, 15));
        assert_eq!(decode_relev_score(0b0010_1111), (0.8, 15));
        assert_eq!(decode_relev_score(0b0011_1111), (1.0, 15));

        // Test different scores
        assert_eq!(decode_relev_score(0b0011_0000), (1.0, 0));
        assert_eq!(decode_relev_score(0b0011_0101), (1.0, 5));

        // Test round-trip
        let encoded = encode_relev_score(0.8, 10);
        let (relev, score) = decode_relev_score(encoded);
        assert_eq!(relev, 0.8);
        assert_eq!(score, 10);
    }

    #[test]
    fn test_pack_unpack_feature_id() {
        // Test basic packing/unpacking
        let packed = pack_feature_id(12345, 42);
        let (id, hash) = unpack_feature_id(packed);
        assert_eq!(id, 12345);
        assert_eq!(hash, 42);

        // Test edge cases
        let packed = pack_feature_id(0, 0);
        assert_eq!(unpack_feature_id(packed), (0, 0));

        let packed = pack_feature_id(16777215, 255); // Max 24-bit id, max 8-bit hash
        assert_eq!(unpack_feature_id(packed), (16777215, 255));

        // Test round-trip
        let original = (999999, 123);
        let packed = pack_feature_id(original.0, original.1);
        assert_eq!(unpack_feature_id(packed), original);
    }
}
