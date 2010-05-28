use serde::{Deserialize, Serialize};

/// Unique identifier for a phrase (supports up to 4 billion phrases).
pub type PhraseId = u32;

/// 128-bit bitfield representing language support.
/// Each bit represents a language. Empty (0) means all languages.
pub type LanguageSet = u128;

/// Default zoom level for tile coordinate system (approximately 150m resolution with S2 Level 16)
pub const DEFAULT_ZOOM: u16 = 16;

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