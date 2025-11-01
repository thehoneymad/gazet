//! Read-only interface for querying GridStore indexes.
//!
//! Use `GridStore` at query time to retrieve spatial phrase data.
//! See `GridStoreBuilder` for creating indexes.

use crate::storage::group_by_owned;
use crate::storage::{
    decode_boundaries, decode_relev_score, proximity_radius, scoredist, tile_dist,
    unpack_feature_id, GridEntry, GridKey, MatchEntry, MatchKey, MatchOpts, MatchPhrase, PhraseId,
    Result, StorageError, TypeMarker, BOUNDS_KEY,
};
use interval_heap::IntervalHeap;
use itertools::Itertools;
use morton::deinterleave_morton;
use ordered_float::OrderedFloat;
use rocksdb::{Direction, IteratorMode, Options, DB};
use smallvec::SmallVec;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::Path;

struct QueueElement<T: Iterator<Item = MatchEntry>> {
    next_entry: MatchEntry,
    entry_iter: T,
}

impl<T: Iterator<Item = MatchEntry>> Eq for QueueElement<T> {}

impl<T: Iterator<Item = MatchEntry>> PartialEq<Self> for QueueElement<T> {
    fn eq(&self, other: &Self) -> bool {
        self.sort_key() == other.sort_key()
    }
}

impl<T: Iterator<Item = MatchEntry>> PartialOrd<Self> for QueueElement<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<T: Iterator<Item = MatchEntry>> Ord for QueueElement<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.sort_key().cmp(&other.sort_key())
    }
}

impl<T: Iterator<Item = MatchEntry>> QueueElement<T> {
    fn sort_key(&self) -> (OrderedFloat<f64>, OrderedFloat<f64>, bool, u16, u16, u32) {
        (
            OrderedFloat(self.next_entry.grid_entry.relev),
            OrderedFloat(self.next_entry.scoredist),
            self.next_entry.matches_language,
            self.next_entry.grid_entry.x,
            self.next_entry.grid_entry.y,
            self.next_entry.grid_entry.id,
        )
    }
}

/// Default zoom level for GridStore (low detail, suitable for testing)
const DEFAULT_ZOOM: u16 = 6;

/// Default coalesce radius in miles (0.0 = no proximity search)
const DEFAULT_COALESCE_RADIUS: f64 = 0.0;

/// Read-only interface to a GridStore database.
pub struct GridStore {
    db: DB,
    /// Bin boundaries for prefix bin optimization.
    /// Contains phrase_id values where prefix bins start.
    /// Empty if no prefix bins were created during indexing.
    pub bin_boundaries: HashSet<PhraseId>,
    pub zoom: u16,
    pub coalesce_radius: f64,
}

impl GridStore {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::new_with_options(path, DEFAULT_ZOOM, DEFAULT_COALESCE_RADIUS)
    }

    pub fn new_with_options<P: AsRef<Path>>(
        path: P,
        zoom: u16,
        coalesce_radius: f64,
    ) -> Result<Self> {
        let mut opts = Options::default();
        opts.set_allow_mmap_reads(true);

        let db = DB::open_for_read_only(&opts, path, false)?;

        // Read bin boundaries from database
        let bin_boundaries: HashSet<PhraseId> = match db.get(BOUNDS_KEY)? {
            Some(entry) => decode_boundaries(entry.as_ref()).into_iter().collect(),
            None => HashSet::new(),
        };

        Ok(GridStore {
            db,
            bin_boundaries,
            zoom,
            coalesce_radius,
        })
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
        match_opts: &MatchOpts, // TODO: Implement spatial filtering
        max_values: usize,
    ) -> Result<impl Iterator<Item = MatchEntry>> {
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

        let mut pri_queue = IntervalHeap::<QueueElement<_>>::new();
        for result in db_iter {
            let (key, value) = result.ok().unwrap();
            let matches_language = range_key_for_filter.matches_language(&key).ok().unwrap();

            let mut entry_iter =
                decode_matching_value(value, &match_opts, matches_language, self.coalesce_radius)
                    .ok()
                    .unwrap();

            if let Some(next_entry) = entry_iter.next() {
                let queue_element = QueueElement {
                    next_entry,
                    entry_iter,
                };

                if pri_queue.len() >= max_values {
                    if let Some(worst_entry) = pri_queue.min() {
                        if worst_entry >= &queue_element {
                            continue;
                        } else {
                            pri_queue.pop_min();
                            pri_queue.push(queue_element);
                        }
                    }
                } else {
                    pri_queue.push(queue_element);
                }
            }
        }

        let mut count = 0;
        let iter = std::iter::from_fn(move || {
            if count >= max_values {
                return None;
            }
            pri_queue.pop_max().map(|mut queue_elem| {
                count += 1;
                let result = queue_elem.next_entry;
                if let Some(next_entry) = queue_elem.entry_iter.next() {
                    queue_elem.next_entry = next_entry;
                    pri_queue.push(queue_elem);
                }
                result
            })
        });

        Ok(iter)
    }
}

/// Deserializes stored BuilderEntry back to flat Vec<GridEntry>.
///
/// The stored format is a Vec of (RelevScore, HashMap<MortonCode, SmallVec<PackedFeatureId>>)
/// sorted by RelevScore in descending order (high relevance first).
#[inline]
fn decode_value(value: &[u8]) -> Result<Vec<GridEntry>> {
    let sorted_entries: Vec<(u8, HashMap<u32, SmallVec<[u32; 4]>>)> =
        bincode::deserialize(value).map_err(|e| StorageError::Serialization(e.to_string()))?;

    let mut entries = Vec::new();

    for (relev_score, morton_map) in sorted_entries {
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

/// Deserializes and filters GridEntries with spatial matching.
///
/// Unlike decode_value(), this applies spatial filtering during iteration
/// and calculates distance/scoredist for proximity ranking.
///
/// Groups by relevance, then within each relevance group, merges coords from
/// different scores sorted by scoredist (highest first).
///
/// # Algorithm Overview
///
/// ```text
/// Input: Serialized data with structure:
///   [(relev_score_byte, HashMap<morton, feature_ids>), ...]
///   Already sorted by relev_score descending
///
/// Step 1: Deserialize and flatten
///   [(1.0, 7, morton1, [id1, id2]), (1.0, 5, morton2, [id3]), (0.8, 7, morton3, [id4])]
///    └relev└score
///
/// Step 2: Group by relevance
///   Relevance 1.0: [(1.0, 7, morton1, [id1,id2]), (1.0, 5, morton2, [id3])]
///   Relevance 0.8: [(0.8, 7, morton3, [id4])]
///
/// Step 3: Within each relevance group, process each coord
///   For relevance 1.0:
///     Score 7: (distance=10, scoredist=50, x, y, [id1,id2])
///     Score 5: (distance=5,  scoredist=60, x, y, [id3])
///
/// Step 4: kmerge by scoredist (highest first)
///   Score 5 with scoredist=60 comes before Score 7 with scoredist=50
///   Result: [id3, id1, id2] (all at relevance 1.0)
///
/// Step 5: Move to next relevance group (0.8) and repeat
///
/// Final output order:
///   [id3 (relev=1.0, scoredist=60),
///    id1 (relev=1.0, scoredist=50),
///    id2 (relev=1.0, scoredist=50),
///    id4 (relev=0.8, scoredist=...)]
/// ```
#[inline]
fn decode_matching_value<T: AsRef<[u8]>>(
    value: T,
    match_opts: &MatchOpts,
    matches_language: bool,
    coalesce_radius: f64,
) -> Result<impl Iterator<Item = MatchEntry>> {
    // Deserialize: Vec<(relev_score_byte, HashMap<morton, feature_ids>)>
    // Already sorted by relev_score descending from builder
    let sorted_entries: Vec<(u8, HashMap<u32, SmallVec<[u32; 4]>>)> =
        bincode::deserialize(value.as_ref())
            .map_err(|e| StorageError::Serialization(e.to_string()))?;

    let match_opts = match_opts.clone();

    // STEP 1: Flatten to (relev, score, morton, packed_ids) tuples
    // Example: [(1.0, 7, morton1, [id1, id2]), (1.0, 5, morton2, [id3]), (0.8, 7, morton3, [id4])]
    let relevs = sorted_entries
        .into_iter()
        .flat_map(|(relev_score, morton_map)| {
            let (relev, score) = decode_relev_score(relev_score);
            morton_map
                .into_iter()
                .map(move |(morton, ids)| (relev, score, morton, ids))
        });

    // STEP 2: Group consecutive entries by relevance
    // Example: Relevance 1.0 → [(1.0, 7, ...), (1.0, 5, ...)], Relevance 0.8 → [(0.8, 7, ...)]
    let iter =
        group_by_owned(relevs, |(relev, _, _, _)| *relev).flat_map(move |(relev, score_groups)| {
            let match_opts = match_opts.clone();

            // STEP 3: Within each relevance group, process each coord
            // Convert each coord to an iterator so kmerge can merge them
            // Match carmen-core's 4-case spatial filtering structure
            let coords_per_score =
                score_groups
                    .into_iter()
                    .filter_map(move |(_, score, morton, ids)| {
                        let (x, y) = deinterleave_morton(morton);

                        // 4-case spatial filtering:
                        // Case 1: No spatial filtering
                        // Case 2: Bbox only
                        // Case 3: Proximity only
                        // Case 4: Both bbox and proximity
                        let (distance, within_radius, scoredist) =
                            match (&match_opts.bbox, &match_opts.proximity) {
                                // Case 1: No spatial filtering - accept all coords
                                (None, None) => (0.0, false, score as f64),

                                // Case 2: Bbox only - filter by bounding box
                                (Some(bbox), None) => {
                                    if !(x >= bbox[0]
                                        && x <= bbox[2]
                                        && y >= bbox[1]
                                        && y <= bbox[3])
                                    {
                                        return None; // Outside bbox
                                    }
                                    (0.0, false, score as f64)
                                }

                                // Case 3: Proximity only - calculate distance and scoredist
                                (None, Some(prox_pt)) => {
                                    let distance = tile_dist(prox_pt[0], prox_pt[1], x, y);
                                    let within_radius = distance
                                        <= proximity_radius(match_opts.zoom, coalesce_radius);
                                    let scoredist = scoredist(
                                        match_opts.zoom,
                                        distance,
                                        score,
                                        coalesce_radius,
                                    );
                                    (distance, within_radius, scoredist)
                                }

                                // Case 4: Both bbox and proximity - filter by bbox, then calculate distance
                                (Some(bbox), Some(prox_pt)) => {
                                    if !(x >= bbox[0]
                                        && x <= bbox[2]
                                        && y >= bbox[1]
                                        && y <= bbox[3])
                                    {
                                        return None; // Outside bbox
                                    }
                                    let distance = tile_dist(prox_pt[0], prox_pt[1], x, y);
                                    let within_radius = distance
                                        <= proximity_radius(match_opts.zoom, coalesce_radius);
                                    let scoredist = scoredist(
                                        match_opts.zoom,
                                        distance,
                                        score,
                                        coalesce_radius,
                                    );
                                    (distance, within_radius, scoredist)
                                }
                            };

                        // Wrap tuple in once() iterator for kmerge compatibility
                        Some(std::iter::once((
                            distance,
                            within_radius,
                            score,
                            scoredist,
                            x,
                            y,
                            ids,
                        )))
                    });

            // STEP 4: kmerge - merge iterators sorted by scoredist (highest first)
            // Example: Score 7 [scoredist=50, 45], Score 5 [scoredist=60, 40]
            //       → kmerge → [60, 50, 45, 40]
            // This ensures best proximity-adjusted results come first within each relevance group
            let all_coords = coords_per_score.kmerge_by(
                |a: &(f64, bool, u8, f64, u16, u16, SmallVec<[u32; 4]>),
                 b: &(f64, bool, u8, f64, u16, u16, SmallVec<[u32; 4]>)| {
                    // Primary: scoredist descending (higher is better)
                    match a.3.partial_cmp(&b.3).unwrap() {
                        Ordering::Greater => true,
                        Ordering::Less => false,
                        Ordering::Equal => {
                            // Secondary: distance ascending (closer is better)
                            match a.0.partial_cmp(&b.0).unwrap() {
                                Ordering::Less => true,
                                Ordering::Greater => false,
                                Ordering::Equal => {
                                    // Tertiary: y coordinate ascending, then x ascending
                                    match a.5.cmp(&b.5) {
                                        Ordering::Less => true,
                                        Ordering::Greater => false,
                                        Ordering::Equal => a.4.cmp(&b.4) == Ordering::Less,
                                    }
                                }
                            }
                        }
                    }
                },
            );

            // STEP 5: Expand each coord to individual feature IDs
            // One coord may have multiple features: (x, y, [id1, id2]) → [MatchEntry(id1), MatchEntry(id2)]
            all_coords.flat_map(
                move |(distance, within_radius, score, scoredist, x, y, ids)| {
                    ids.into_iter().map(move |packed_id| {
                        let (id, source_phrase_hash) = unpack_feature_id(packed_id);
                        MatchEntry {
                            grid_entry: GridEntry {
                                // Apply 4% penalty if wrong language AND outside search radius
                                relev: relev
                                    * if matches_language || within_radius {
                                        1.0
                                    } else {
                                        0.96
                                    },
                                score,
                                x,
                                y,
                                id,
                                source_phrase_hash,
                            },
                            matches_language,
                            distance,
                            scoredist,
                        }
                    })
                },
            )
        });

    Ok(iter)
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
    fn test_proximity_ordering() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        let key = GridKey {
            phrase_id: 1,
            lang_set: 0,
        };

        // Four entries at different coordinates, all same relev/score
        let entries = vec![
            GridEntry {
                id: 1,
                x: 2,
                y: 2,
                relev: 1.0,
                score: 1,
                source_phrase_hash: 0,
            },
            GridEntry {
                id: 2,
                x: 2,
                y: 0,
                relev: 1.0,
                score: 1,
                source_phrase_hash: 0,
            },
            GridEntry {
                id: 3,
                x: 0,
                y: 0,
                relev: 1.0,
                score: 1,
                source_phrase_hash: 0,
            },
            GridEntry {
                id: 4,
                x: 0,
                y: 2,
                relev: 1.0,
                score: 1,
                source_phrase_hash: 0,
            },
        ];
        builder.insert(&key, entries).unwrap();
        builder.finish().unwrap();

        let store = GridStore::new_with_options(dir.path(), 14, 0.0).unwrap();

        // Query with proximity point at (2, 2) - should return id=1 first (closest)
        let match_key = MatchKey {
            match_phrase: MatchPhrase::Exact(1),
            lang_set: 0,
        };
        let match_opts = MatchOpts {
            bbox: None,
            proximity: Some([2, 2]),
            zoom: 14,
        };

        let results: Vec<MatchEntry> = store
            .get_matching(&match_key, &match_opts, 10)
            .unwrap()
            .collect();

        assert_eq!(results.len(), 4);

        let result_ids: Vec<u32> = results.iter().map(|r| r.grid_entry.id).collect();
        // Matches carmen-core's coalesce_single_test_proximity_basic
        assert_eq!(
            result_ids,
            [1, 2, 4, 3],
            "Results ordered by distance, then coordinates"
        );

        // Verify distances match expected values
        assert_eq!(results[0].distance, 0.0);
        assert_eq!(results[1].distance, 2.0);
        assert_eq!(results[2].distance, 2.0);
        assert!((results[3].distance - 2.83).abs() < 0.01);
    }

    #[test]
    fn test_bbox_filtering() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        let key = GridKey {
            phrase_id: 1,
            lang_set: 0,
        };

        // Entries inside and outside bbox
        let entries = vec![
            GridEntry {
                id: 1,
                x: 5,
                y: 5,
                relev: 1.0,
                score: 1,
                source_phrase_hash: 0,
            }, // inside
            GridEntry {
                id: 2,
                x: 15,
                y: 15,
                relev: 1.0,
                score: 1,
                source_phrase_hash: 0,
            }, // outside
            GridEntry {
                id: 3,
                x: 8,
                y: 8,
                relev: 1.0,
                score: 1,
                source_phrase_hash: 0,
            }, // inside
        ];
        builder.insert(&key, entries).unwrap();
        builder.finish().unwrap();

        let store = GridStore::new(dir.path()).unwrap();

        let match_key = MatchKey {
            match_phrase: MatchPhrase::Exact(1),
            lang_set: 0,
        };
        let match_opts = MatchOpts {
            bbox: Some([0, 0, 10, 10]), // Only includes id=1 and id=3
            proximity: None,
            zoom: 14,
        };

        let results: Vec<MatchEntry> = store
            .get_matching(&match_key, &match_opts, 10)
            .unwrap()
            .collect();

        assert_eq!(results.len(), 2);
        let result_ids: Vec<u32> = results.iter().map(|r| r.grid_entry.id).collect();
        assert!(result_ids.contains(&1));
        assert!(result_ids.contains(&3));
        assert!(
            !result_ids.contains(&2),
            "id=2 outside bbox should be filtered"
        );
    }

    #[test]
    fn test_exact_phrase_matching() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        // Insert entries for multiple phrases
        let key1 = GridKey { phrase_id: 1, lang_set: 0 };
        let key2 = GridKey { phrase_id: 2, lang_set: 0 };
        
        builder.insert(&key1, vec![
            GridEntry { id: 1, x: 10, y: 10, relev: 1.0, score: 5, source_phrase_hash: 0 },
        ]).unwrap();
        
        builder.insert(&key2, vec![
            GridEntry { id: 2, x: 20, y: 20, relev: 1.0, score: 5, source_phrase_hash: 0 },
        ]).unwrap();
        
        builder.finish().unwrap();
        let store = GridStore::new(dir.path()).unwrap();

        // Query for exact phrase_id=1
        let match_key = MatchKey {
            match_phrase: MatchPhrase::Exact(1),
            lang_set: 0,
        };
        let match_opts = MatchOpts { bbox: None, proximity: None, zoom: 14 };
        
        let results: Vec<MatchEntry> = store.get_matching(&match_key, &match_opts, 10).unwrap().collect();
        
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].grid_entry.id, 1);
    }

    #[test]
    fn test_range_query_without_prefix_bins() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        // Insert entries for phrase_ids 1, 2, 3
        for phrase_id in 1..=3 {
            let key = GridKey { phrase_id, lang_set: 0 };
            builder.insert(&key, vec![
                GridEntry { id: phrase_id, x: 10, y: 10, relev: 1.0, score: 5, source_phrase_hash: 0 },
            ]).unwrap();
        }
        
        builder.finish().unwrap();
        let store = GridStore::new(dir.path()).unwrap();

        // Query for range [1, 3) - should return phrase_ids 1 and 2
        let match_key = MatchKey {
            match_phrase: MatchPhrase::Range { start: 1, end: 3 },
            lang_set: 0,
        };
        let match_opts = MatchOpts { bbox: None, proximity: None, zoom: 14 };
        
        let results: Vec<MatchEntry> = store.get_matching(&match_key, &match_opts, 10).unwrap().collect();
        
        assert_eq!(results.len(), 2);
        let result_ids: Vec<u32> = results.iter().map(|r| r.grid_entry.id).collect();
        assert!(result_ids.contains(&1));
        assert!(result_ids.contains(&2));
        assert!(!result_ids.contains(&3), "phrase_id=3 outside range [1,3)");
    }

    #[test]
    fn test_range_query_with_prefix_bins() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        // Set bin boundaries at 100 and 200
        builder.load_bin_boundaries(vec![100, 200]).unwrap();

        // Insert entries for phrase_ids in first bin [0, 100)
        for phrase_id in [10, 20, 30] {
            let key = GridKey { phrase_id, lang_set: 0 };
            builder.insert(&key, vec![
                GridEntry { id: phrase_id, x: 10, y: 10, relev: 1.0, score: 5, source_phrase_hash: 0 },
            ]).unwrap();
        }
        
        builder.finish().unwrap();
        let store = GridStore::new(dir.path()).unwrap();

        // Query using prefix bin range [0, 100)
        let match_key = MatchKey {
            match_phrase: MatchPhrase::Range { start: 0, end: 100 },
            lang_set: 0,
        };
        let match_opts = MatchOpts { bbox: None, proximity: None, zoom: 14 };
        
        let results: Vec<MatchEntry> = store.get_matching(&match_key, &match_opts, 10).unwrap().collect();
        
        // Should return all 3 entries from the prefix bin
        assert_eq!(results.len(), 3);
        let result_ids: Vec<u32> = results.iter().map(|r| r.grid_entry.id).collect();
        assert!(result_ids.contains(&10));
        assert!(result_ids.contains(&20));
        assert!(result_ids.contains(&30));
    }

    #[test]
    fn test_language_filtering() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        let key = GridKey {
            phrase_id: 1,
            lang_set: 0b0001, // Language bit 0 set
        };
        
        builder.insert(&key, vec![
            GridEntry { id: 1, x: 10, y: 10, relev: 1.0, score: 5, source_phrase_hash: 0 },
        ]).unwrap();
        
        builder.finish().unwrap();
        let store = GridStore::new(dir.path()).unwrap();

        // Query with matching language
        let match_key = MatchKey {
            match_phrase: MatchPhrase::Exact(1),
            lang_set: 0b0001, // Same language
        };
        let match_opts = MatchOpts { bbox: None, proximity: None, zoom: 14 };
        
        let results: Vec<MatchEntry> = store.get_matching(&match_key, &match_opts, 10).unwrap().collect();
        
        assert_eq!(results.len(), 1);
        assert!(results[0].matches_language, "Language should match");
    }

    #[test]
    fn test_language_penalty_outside_radius() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        let key = GridKey {
            phrase_id: 1,
            lang_set: 0b0001, // Language bit 0
        };
        
        builder.insert(&key, vec![
            GridEntry { id: 1, x: 100, y: 100, relev: 1.0, score: 5, source_phrase_hash: 0 },
        ]).unwrap();
        
        builder.finish().unwrap();
        let store = GridStore::new_with_options(dir.path(), 14, 1.0).unwrap(); // Small radius

        // Query with different language and proximity far from result
        let match_key = MatchKey {
            match_phrase: MatchPhrase::Exact(1),
            lang_set: 0b0010, // Different language bit
        };
        let match_opts = MatchOpts {
            bbox: None,
            proximity: Some([0, 0]), // Far from (100, 100)
            zoom: 14,
        };
        
        let results: Vec<MatchEntry> = store.get_matching(&match_key, &match_opts, 10).unwrap().collect();
        
        assert_eq!(results.len(), 1);
        assert!(!results[0].matches_language, "Language should not match");
        // 4% penalty applied: 1.0 * 0.96 = 0.96
        assert_eq!(results[0].grid_entry.relev, 0.96, "Should have 4% language penalty");
    }

    #[test]
    fn test_language_no_penalty_within_radius() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        let key = GridKey {
            phrase_id: 1,
            lang_set: 0b0001,
        };
        
        builder.insert(&key, vec![
            GridEntry { id: 1, x: 2, y: 2, relev: 1.0, score: 5, source_phrase_hash: 0 },
        ]).unwrap();
        
        builder.finish().unwrap();
        let store = GridStore::new_with_options(dir.path(), 14, 100.0).unwrap(); // Large radius

        // Query with different language but proximity close to result
        let match_key = MatchKey {
            match_phrase: MatchPhrase::Exact(1),
            lang_set: 0b0010, // Different language
        };
        let match_opts = MatchOpts {
            bbox: None,
            proximity: Some([2, 2]), // Same location
            zoom: 14,
        };
        
        let results: Vec<MatchEntry> = store.get_matching(&match_key, &match_opts, 10).unwrap().collect();
        
        assert_eq!(results.len(), 1);
        assert!(!results[0].matches_language, "Language should not match");
        // No penalty because within radius: 1.0 * 1.0 = 1.0
        assert_eq!(results[0].grid_entry.relev, 1.0, "No penalty within radius");
    }

    #[test]
    fn test_multiple_features_same_coordinate() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        let key = GridKey { phrase_id: 1, lang_set: 0 };

        // Multiple features at exact same coordinate (x=10, y=10)
        let entries = vec![
            GridEntry { id: 1, x: 10, y: 10, relev: 1.0, score: 5, source_phrase_hash: 0 },
            GridEntry { id: 2, x: 10, y: 10, relev: 1.0, score: 5, source_phrase_hash: 0 },
            GridEntry { id: 3, x: 10, y: 10, relev: 1.0, score: 5, source_phrase_hash: 0 },
        ];
        builder.insert(&key, entries).unwrap();
        builder.finish().unwrap();

        let store = GridStore::new(dir.path()).unwrap();
        let match_key = MatchKey { match_phrase: MatchPhrase::Exact(1), lang_set: 0 };
        let match_opts = MatchOpts { bbox: None, proximity: None, zoom: 14 };

        let results: Vec<MatchEntry> = store.get_matching(&match_key, &match_opts, 10).unwrap().collect();

        // All 3 features should be returned
        assert_eq!(results.len(), 3);
        let result_ids: Vec<u32> = results.iter().map(|r| r.grid_entry.id).collect();
        assert!(result_ids.contains(&1));
        assert!(result_ids.contains(&2));
        assert!(result_ids.contains(&3));
        
        // All should have same coordinates
        for result in &results {
            assert_eq!(result.grid_entry.x, 10);
            assert_eq!(result.grid_entry.y, 10);
        }
    }

    #[test]
    fn test_multiple_features_same_coordinate_different_scores() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        let key = GridKey { phrase_id: 1, lang_set: 0 };

        // Multiple features at same coordinate but different scores
        let entries = vec![
            GridEntry { id: 1, x: 10, y: 10, relev: 1.0, score: 10, source_phrase_hash: 0 },
            GridEntry { id: 2, x: 10, y: 10, relev: 1.0, score: 5, source_phrase_hash: 0 },
            GridEntry { id: 3, x: 10, y: 10, relev: 1.0, score: 15, source_phrase_hash: 0 },
        ];
        builder.insert(&key, entries).unwrap();
        builder.finish().unwrap();

        let store = GridStore::new(dir.path()).unwrap();
        let match_key = MatchKey { match_phrase: MatchPhrase::Exact(1), lang_set: 0 };
        let match_opts = MatchOpts { bbox: None, proximity: None, zoom: 14 };

        let results: Vec<MatchEntry> = store.get_matching(&match_key, &match_opts, 10).unwrap().collect();

        assert_eq!(results.len(), 3);
        
        // Should be ordered by score descending (15, 10, 5)
        let result_scores: Vec<u8> = results.iter().map(|r| r.grid_entry.score).collect();
        assert_eq!(result_scores[0], 15);
        assert_eq!(result_scores[1], 10);
        assert_eq!(result_scores[2], 5);
    }

    #[test]
    fn test_multiple_features_same_coordinate_with_proximity() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        let key = GridKey { phrase_id: 1, lang_set: 0 };

        // Multiple features at same coordinate
        let entries = vec![
            GridEntry { id: 1, x: 10, y: 10, relev: 1.0, score: 5, source_phrase_hash: 0 },
            GridEntry { id: 2, x: 10, y: 10, relev: 1.0, score: 5, source_phrase_hash: 1 },
            GridEntry { id: 3, x: 10, y: 10, relev: 1.0, score: 5, source_phrase_hash: 2 },
        ];
        builder.insert(&key, entries).unwrap();
        builder.finish().unwrap();

        let store = GridStore::new_with_options(dir.path(), 14, 0.0).unwrap();
        let match_key = MatchKey { match_phrase: MatchPhrase::Exact(1), lang_set: 0 };
        let match_opts = MatchOpts {
            bbox: None,
            proximity: Some([10, 10]), // Same location
            zoom: 14,
        };

        let results: Vec<MatchEntry> = store.get_matching(&match_key, &match_opts, 10).unwrap().collect();

        assert_eq!(results.len(), 3);
        
        // All should have distance=0 (same location as proximity point)
        for result in &results {
            assert_eq!(result.distance, 0.0);
        }
    }

    #[test]
    fn test_morton_collision_different_phrases() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        // Two different phrases, both have features at (10, 10)
        let key1 = GridKey { phrase_id: 1, lang_set: 0 };
        let key2 = GridKey { phrase_id: 2, lang_set: 0 };

        builder.insert(&key1, vec![
            GridEntry { id: 1, x: 10, y: 10, relev: 1.0, score: 5, source_phrase_hash: 0 },
            GridEntry { id: 2, x: 10, y: 10, relev: 1.0, score: 5, source_phrase_hash: 0 },
        ]).unwrap();

        builder.insert(&key2, vec![
            GridEntry { id: 3, x: 10, y: 10, relev: 1.0, score: 5, source_phrase_hash: 0 },
            GridEntry { id: 4, x: 10, y: 10, relev: 1.0, score: 5, source_phrase_hash: 0 },
        ]).unwrap();

        builder.finish().unwrap();
        let store = GridStore::new(dir.path()).unwrap();

        // Query phrase 1 - should only get ids 1 and 2
        let match_key1 = MatchKey { match_phrase: MatchPhrase::Exact(1), lang_set: 0 };
        let match_opts = MatchOpts { bbox: None, proximity: None, zoom: 14 };
        let results1: Vec<MatchEntry> = store.get_matching(&match_key1, &match_opts, 10).unwrap().collect();
        
        assert_eq!(results1.len(), 2);
        let ids1: Vec<u32> = results1.iter().map(|r| r.grid_entry.id).collect();
        assert!(ids1.contains(&1));
        assert!(ids1.contains(&2));

        // Query phrase 2 - should only get ids 3 and 4
        let match_key2 = MatchKey { match_phrase: MatchPhrase::Exact(2), lang_set: 0 };
        let results2: Vec<MatchEntry> = store.get_matching(&match_key2, &match_opts, 10).unwrap().collect();
        
        assert_eq!(results2.len(), 2);
        let ids2: Vec<u32> = results2.iter().map(|r| r.grid_entry.id).collect();
        assert!(ids2.contains(&3));
        assert!(ids2.contains(&4));
    }

    #[test]
    fn test_multiple_coords_per_score() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        let key = GridKey {
            phrase_id: 1,
            lang_set: 0,
        };

        // Multiple entries with same score but different coordinates
        let entries = vec![
            GridEntry {
                id: 1,
                x: 100,
                y: 100,
                relev: 0.8,
                score: 7,
                source_phrase_hash: 0,
            },
            GridEntry {
                id: 2,
                x: 200,
                y: 200,
                relev: 0.8,
                score: 7,
                source_phrase_hash: 0,
            },
            GridEntry {
                id: 3,
                x: 300,
                y: 300,
                relev: 0.8,
                score: 7,
                source_phrase_hash: 0,
            },
        ];
        builder.insert(&key, entries).unwrap();
        builder.finish().unwrap();

        let store = GridStore::new(dir.path()).unwrap();

        let match_key = MatchKey {
            match_phrase: MatchPhrase::Exact(1),
            lang_set: 0,
        };
        let match_opts = MatchOpts {
            bbox: None,
            proximity: None,
            zoom: 14,
        };

        let results: Vec<MatchEntry> = store
            .get_matching(&match_key, &match_opts, 10)
            .unwrap()
            .collect();

        // All 3 coords should be returned
        assert_eq!(results.len(), 3);
        let result_ids: Vec<u32> = results.iter().map(|r| r.grid_entry.id).collect();
        assert!(result_ids.contains(&1));
        assert!(result_ids.contains(&2));
        assert!(result_ids.contains(&3));
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

    #[test]
    fn test_get_matching_basic_range_query() {
        // Step 1: Create temp directory
        let dir = tempfile::tempdir().unwrap();

        // Step 2: Build test data
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        // Insert entries for phrase_id 1 with language 1
        let key = GridKey {
            phrase_id: 1,
            lang_set: 1,
        };

        let entries = vec![
            GridEntry {
                relev: 1.0,
                score: 7,
                x: 10,
                y: 20,
                id: 100,
                source_phrase_hash: 0,
            },
            GridEntry {
                relev: 1.0,
                score: 5,
                x: 11,
                y: 21,
                id: 101,
                source_phrase_hash: 0,
            },
        ];

        builder.insert(&key, entries).unwrap();
        builder.finish().unwrap();

        // Step 3: Query the data
        let store = GridStore::new(dir.path()).unwrap();

        let match_key = MatchKey {
            match_phrase: MatchPhrase::Exact(1), // Query for phrase_id 1
            lang_set: 1,                         // Match language 1
        };

        let match_opts = MatchOpts {
            bbox: None,
            proximity: None,
            zoom: 16,
        };

        let results: Vec<MatchEntry> = store
            .get_matching(&match_key, &match_opts, 10) // max_values = 10
            .unwrap()
            .collect();

        assert_eq!(results.len(), 2, "Should return 2 entries");

        // First result should be highest score (7)
        assert_eq!(results[0].grid_entry.id, 100);
        assert_eq!(results[0].grid_entry.score, 7);
        assert_eq!(results[0].matches_language, true);

        // Second result should be lower score (5)
        assert_eq!(results[1].grid_entry.id, 101);
        assert_eq!(results[1].grid_entry.score, 5);
        assert_eq!(results[1].matches_language, true);
    }
}
