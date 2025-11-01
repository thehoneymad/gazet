//! Coalescing logic for combining and deduplicating query results.
//!
//! Coalescing takes raw grid matches and:
//! 1. Deduplicates features (same feature at multiple coords → keep best)
//! 2. Limits results to top contexts within relevance threshold
//! 3. Sorts by relevance, then proximity (scoredist)
//!
//! This is a simplified version of carmen-core's coalescing focused on
//! single-phrase queries. Multi-phrase stacking will be added later.

use crate::storage::{GridStore, MatchEntry, MatchKey, MatchOpts, Result};
use std::cmp::Reverse;
use std::collections::HashMap;
use ordered_float::OrderedFloat;

/// Maximum number of contexts to return from coalescing
const MAX_CONTEXTS: usize = 40;

/// Maximum relevance gap allowed (contexts must be within 0.25 of best)
const MAX_RELEVANCE_GAP: f64 = 0.25;

/// A coalesced result representing a single feature with its best match.
///
/// When a feature appears at multiple coordinates (e.g., "Main Street" in
/// different neighborhoods), we keep only the best match based on scoredist.
#[derive(Debug, Clone, PartialEq)]
pub struct CoalesceContext {
    /// The best matching entry for this feature
    pub entry: MatchEntry,
    /// Combined relevance score (for multi-phrase, this would be sum of phrase relevances)
    pub relev: f64,
}

impl CoalesceContext {
    /// Sorting key for final result ordering.
    ///
    /// Primary: relevance descending (higher is better)
    /// Secondary: scoredist descending (closer/higher score is better)
    /// Tertiary: coordinates (y, x) for deterministic ordering
    /// Quaternary: feature ID for final tiebreaker
    fn sort_key(&self) -> (OrderedFloat<f64>, OrderedFloat<f64>, u16, u16, u32) {
        (
            OrderedFloat(self.relev),
            OrderedFloat(self.entry.scoredist),
            self.entry.grid_entry.y,
            self.entry.grid_entry.x,
            self.entry.grid_entry.id,
        )
    }
}

/// Coalesces results from a single phrase query.
///
/// # Algorithm
///
/// ```text
/// Input: Stream of MatchEntry from get_matching()
///   - Already sorted by (relev desc, scoredist desc, distance asc, y asc, x asc)
///   - May contain same feature ID multiple times at different coordinates
///
/// Step 1: Deduplicate by feature ID
///   For each feature ID, keep only the entry with highest scoredist
///   Example:
///     id=1 at (10,10) scoredist=50
///     id=1 at (20,20) scoredist=60  ← Keep this one (higher scoredist)
///     id=1 at (30,30) scoredist=40
///   Result: HashMap<feature_id, best_entry>
///
/// Step 2: Apply relevance threshold
///   Find max_relevance from all entries
///   Filter out entries where (max_relevance - entry.relev) >= 0.25
///   Example:
///     max_relevance = 1.0
///     Keep: relev=1.0, relev=0.9, relev=0.8
///     Drop: relev=0.7 (gap=0.3 > 0.25)
///
/// Step 3: Limit to MAX_CONTEXTS (40)
///   Take top 40 entries after sorting
///
/// Step 4: Sort final results
///   Sort by (relev desc, scoredist desc, y asc, x asc, id asc)
///
/// Output: Vec<CoalesceContext>
///   - Deduplicated features
///   - Within relevance threshold
///   - Limited to 40 results
///   - Sorted by relevance and proximity
/// ```
///
/// # Example
///
/// ```ignore
/// let store = GridStore::new("index.rocksdb")?;
/// let match_key = MatchKey {
///     match_phrase: MatchPhrase::Exact(42),
///     lang_set: 0,
/// };
/// let match_opts = MatchOpts {
///     proximity: Some([100, 100]),
///     bbox: None,
///     zoom: 14,
/// };
///
/// let contexts = coalesce_single(&store, &match_key, &match_opts)?;
/// // Returns top 40 unique features, sorted by relevance and proximity
/// ```
pub fn coalesce_single(
    store: &GridStore,
    match_key: &MatchKey,
    match_opts: &MatchOpts,
) -> Result<Vec<CoalesceContext>> {
    // Fetch more than we need to account for deduplication
    // Carmen-core uses 2 * MAX_CONTEXTS
    let fetch_limit = 2 * MAX_CONTEXTS;

    // Get matching entries from store
    // These are already sorted by (relev desc, scoredist desc, distance asc, y asc, x asc)
    let entries = store.get_matching(match_key, match_opts, fetch_limit)?;

    // Track state during iteration
    let mut max_relevance: f64 = 0.0;
    let mut previous_id: u32 = 0;
    let mut previous_scoredist: f64 = 0.0;
    let mut feature_count: usize = 0;

    // Deduplicated entries: feature_id -> best MatchEntry
    let mut coalesced: HashMap<u32, MatchEntry> = HashMap::new();

    for entry in entries {
        let current_id = entry.grid_entry.id;
        let current_relev = entry.grid_entry.relev;
        let current_scoredist = entry.scoredist;

        // Skip if same feature as previous but lower scoredist
        // (We're iterating in scoredist descending order, so first occurrence is best)
        if previous_id == current_id && current_scoredist <= previous_scoredist {
            continue;
        }

        // Stop if we've exceeded fetch limit and relevance is dropping
        if feature_count > fetch_limit && current_relev < max_relevance {
            break;
        }

        // Stop if relevance gap exceeds threshold (0.25)
        if max_relevance - current_relev >= MAX_RELEVANCE_GAP {
            break;
        }

        // Update max relevance
        if current_relev > max_relevance {
            max_relevance = current_relev;
        }

        // Insert or update entry for this feature
        // If feature already exists with lower scoredist, replace it
        coalesced
            .entry(current_id)
            .and_modify(|existing| {
                if current_scoredist > existing.scoredist
                    && current_relev >= existing.grid_entry.relev
                {
                    *existing = entry.clone();
                }
            })
            .or_insert(entry.clone());

        // Track unique features
        if previous_id != current_id {
            feature_count += 1;
        }

        // Stop early if no proximity query and we have enough features
        if match_opts.proximity.is_none() && feature_count > fetch_limit {
            break;
        }

        previous_id = current_id;
        previous_scoredist = current_scoredist;
    }

    // Convert HashMap to Vec<CoalesceContext>
    let mut contexts: Vec<CoalesceContext> = coalesced
        .into_iter()
        .map(|(_, entry)| CoalesceContext {
            relev: entry.grid_entry.relev,
            entry,
        })
        .collect();

    // Sort by (relev desc, scoredist desc, y asc, x asc, id asc)
    contexts.sort_by_key(|ctx| Reverse(ctx.sort_key()));

    // Limit to MAX_CONTEXTS
    contexts.truncate(MAX_CONTEXTS);

    Ok(contexts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{GridEntry, GridKey, GridStoreBuilder, MatchPhrase};
    use tempfile;

    #[test]
    fn test_coalesce_deduplicates_features() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        let key = GridKey { phrase_id: 1, lang_set: 0 };

        // Same feature at 3 different coordinates
        builder.insert(&key, vec![
            GridEntry { id: 1, x: 10, y: 10, relev: 1.0, score: 5, source_phrase_hash: 0 },
            GridEntry { id: 1, x: 20, y: 20, relev: 1.0, score: 7, source_phrase_hash: 0 }, // Higher score
            GridEntry { id: 1, x: 30, y: 30, relev: 1.0, score: 3, source_phrase_hash: 0 },
        ]).unwrap();
        builder.finish().unwrap();

        let store = GridStore::new(dir.path()).unwrap();
        let match_key = MatchKey { match_phrase: MatchPhrase::Exact(1), lang_set: 0 };
        let match_opts = MatchOpts { bbox: None, proximity: None, zoom: 14 };

        let contexts = coalesce_single(&store, &match_key, &match_opts).unwrap();

        // Should return only 1 context (deduplicated)
        assert_eq!(contexts.len(), 1);
        assert_eq!(contexts[0].entry.grid_entry.id, 1);
        // Should keep the one with highest score (7)
        assert_eq!(contexts[0].entry.grid_entry.score, 7);
    }

    #[test]
    fn test_coalesce_applies_relevance_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        let key = GridKey { phrase_id: 1, lang_set: 0 };

        // Features with different relevances
        builder.insert(&key, vec![
            GridEntry { id: 1, x: 10, y: 10, relev: 1.0, score: 5, source_phrase_hash: 0 },
            GridEntry { id: 2, x: 20, y: 20, relev: 0.8, score: 5, source_phrase_hash: 0 },
            GridEntry { id: 3, x: 30, y: 30, relev: 0.6, score: 5, source_phrase_hash: 0 }, // Gap = 0.4 > 0.25
        ]).unwrap();
        builder.finish().unwrap();

        let store = GridStore::new(dir.path()).unwrap();
        let match_key = MatchKey { match_phrase: MatchPhrase::Exact(1), lang_set: 0 };
        let match_opts = MatchOpts { bbox: None, proximity: None, zoom: 14 };

        let contexts = coalesce_single(&store, &match_key, &match_opts).unwrap();

        // Should return only 2 contexts (id=3 filtered by relevance gap)
        assert_eq!(contexts.len(), 2);
        let ids: Vec<u32> = contexts.iter().map(|c| c.entry.grid_entry.id).collect();
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
        assert!(!ids.contains(&3), "id=3 should be filtered by relevance threshold");
    }

    #[test]
    fn test_coalesce_sorts_by_relevance_then_scoredist() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        let key = GridKey { phrase_id: 1, lang_set: 0 };

        builder.insert(&key, vec![
            GridEntry { id: 1, x: 10, y: 10, relev: 0.8, score: 10, source_phrase_hash: 0 },
            GridEntry { id: 2, x: 20, y: 20, relev: 1.0, score: 5, source_phrase_hash: 0 },
            GridEntry { id: 3, x: 30, y: 30, relev: 1.0, score: 8, source_phrase_hash: 0 },
        ]).unwrap();
        builder.finish().unwrap();

        let store = GridStore::new(dir.path()).unwrap();
        let match_key = MatchKey { match_phrase: MatchPhrase::Exact(1), lang_set: 0 };
        let match_opts = MatchOpts { bbox: None, proximity: None, zoom: 14 };

        let contexts = coalesce_single(&store, &match_key, &match_opts).unwrap();

        assert_eq!(contexts.len(), 3);
        
        // Should be sorted: relev=1.0 (score=8), relev=1.0 (score=5), relev=0.8 (score=10)
        assert_eq!(contexts[0].entry.grid_entry.id, 3); // relev=1.0, score=8
        assert_eq!(contexts[1].entry.grid_entry.id, 2); // relev=1.0, score=5
        assert_eq!(contexts[2].entry.grid_entry.id, 1); // relev=0.8, score=10
    }
}
