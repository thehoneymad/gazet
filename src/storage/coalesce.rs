//! Multi-phrase coalescing logic from carmen-core.
//!
//! Combines results from multiple phrase queries (e.g., "Main" + "Street" + "Seattle")
//! based on spatial overlap and type hierarchy rules.

use crate::storage::{adjust_bbox_zoom, CoalesceEntry, ConstrainedPriorityQueue, GridStore, MatchEntry, MatchKey, MatchOpts, MatchPhrase, PhrasematchSubquery, Result, StackableNode, StackableTree, MAX_CONTEXTS};
use std::borrow::Borrow;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use ordered_float::OrderedFloat;
use itertools::Itertools;

/// Coalesced context with multiple phrase entries
#[derive(Debug, Clone, PartialEq)]
pub struct CoalesceContext {
    pub entries: Vec<CoalesceEntry>,
    pub mask: u32,
    pub relev: f64,
}

impl CoalesceContext {
    #[inline(always)]
    fn sort_key(&self) -> (OrderedFloat<f64>, OrderedFloat<f64>, Reverse<u16>, u16, u16, u32) {
        (
            OrderedFloat(self.relev),
            OrderedFloat(self.entries[0].scoredist),
            Reverse(self.entries[0].idx),
            self.entries[0].grid_entry.x,
            self.entries[0].grid_entry.y,
            self.entries[0].grid_entry.id,
        )
    }
}

impl Eq for CoalesceContext {}

impl PartialOrd for CoalesceContext {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CoalesceContext {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.sort_key().cmp(&other.sort_key())
    }
}

/// Maximum relevance gap allowed (0.25)
const MAX_RELEVANCE_GAP: f64 = 0.25;

/// Maximum grids to fetch per phrase
const MAX_GRIDS_PER_PHRASE: usize = 2000;

/// Main coalesce entry point - delegates to single or multi
pub fn coalesce<T: Borrow<GridStore> + Clone + Debug>(
    stack: Vec<PhrasematchSubquery<T>>,
    match_opts: &MatchOpts,
) -> Result<Vec<CoalesceContext>> {
    let contexts = if stack.len() <= 1 {
        coalesce_single(&stack[0], match_opts)?
    } else {
        coalesce_multi(stack, match_opts)?
    };

    // Deduplicate and limit results
    let mut out = Vec::with_capacity(MAX_CONTEXTS);
    if !contexts.is_empty() {
        let max_relevance = contexts[0].relev;
        let mut sets: HashSet<u64> = HashSet::new();
        for context in contexts {
            if out.len() >= MAX_CONTEXTS {
                break;
            }
            if max_relevance - context.relev >= MAX_RELEVANCE_GAP {
                break;
            }
            let inserted = sets.insert(context.entries[0].tmp_id.into());
            if inserted {
                out.push(context);
            }
        }
    }
    Ok(out)
}

fn grid_to_coalesce_entry<T: Borrow<GridStore> + Clone>(
    grid: &MatchEntry,
    subquery: &PhrasematchSubquery<T>,
    match_opts: &MatchOpts,
    phrasematch_id: u32,
) -> CoalesceEntry {
    debug_assert!(match_opts.zoom == subquery.store.borrow().zoom);
    let relevance = grid.grid_entry.relev * subquery.weight;

    CoalesceEntry {
        grid_entry: crate::storage::GridEntry { relev: relevance, ..grid.grid_entry },
        matches_language: grid.matches_language,
        idx: subquery.idx,
        tmp_id: ((subquery.idx as u32) << 24) + grid.grid_entry.id,
        mask: subquery.mask,
        distance: grid.distance,
        scoredist: grid.scoredist,
        phrasematch_id,
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
pub fn coalesce_single<T: Borrow<GridStore> + Clone>(
    subquery: &PhrasematchSubquery<T>,
    match_opts: &MatchOpts,
) -> Result<Vec<CoalesceContext>> {
    // Fetch more than we need to account for deduplication
    // Carmen-core uses 2 * MAX_CONTEXTS
    let bigger_max = 2 * MAX_CONTEXTS;

    let grids = subquery.store.borrow().get_matching(
        &subquery.match_keys[0].key,
        match_opts,
        bigger_max,
    )?;

    let mut max_relevance: f64 = 0.;
    let mut previous_id: u32 = 0;
    let mut previous_relevance: f64 = 0.;
    let mut previous_scoredist: f64 = 0.;
    let mut min_scoredist = std::f64::MAX;
    let mut feature_count: usize = 0;

    let mut coalesced: HashMap<u32, CoalesceEntry> = HashMap::new();

    for grid in grids {
        let coalesce_entry = grid_to_coalesce_entry(&grid, subquery, match_opts, 0);

        if previous_id == coalesce_entry.grid_entry.id
            && coalesce_entry.scoredist <= previous_scoredist
        {
            continue;
        }

        if feature_count > bigger_max {
            if coalesce_entry.scoredist < min_scoredist {
                continue;
            } else if coalesce_entry.grid_entry.relev < previous_relevance {
                break;
            }
        }

        if max_relevance - coalesce_entry.grid_entry.relev >= MAX_RELEVANCE_GAP {
            break;
        }
        if coalesce_entry.grid_entry.relev > max_relevance {
            max_relevance = coalesce_entry.grid_entry.relev;
        }

        let current_id = coalesce_entry.grid_entry.id;
        let current_relev = coalesce_entry.grid_entry.relev;
        let current_scoredist = coalesce_entry.scoredist;

        coalesced
            .entry(current_id)
            .and_modify(|existing| {
                if current_scoredist > existing.scoredist && current_relev >= existing.grid_entry.relev {
                    *existing = coalesce_entry.clone();
                }
            })
            .or_insert(coalesce_entry);

        if previous_id != current_id {
            feature_count += 1;
        }
        if match_opts.proximity.is_none() && feature_count > bigger_max {
            break;
        }
        if current_scoredist < min_scoredist {
            min_scoredist = current_scoredist;
        }
        previous_id = current_id;
        previous_relevance = current_relev;
        previous_scoredist = current_scoredist;
    }

    let mut contexts: Vec<CoalesceContext> = coalesced
        .iter()
        .map(|(_, entry)| CoalesceContext {
            entries: vec![entry.clone()],
            mask: entry.mask,
            relev: entry.grid_entry.relev,
        })
        .collect();

    contexts.sort_by_key(|context| {
        Reverse((
            OrderedFloat(context.relev),
            OrderedFloat(context.entries[0].scoredist),
            context.entries[0].grid_entry.x,
            context.entries[0].grid_entry.y,
            context.entries[0].grid_entry.id,
        ))
    });

    contexts.truncate(MAX_CONTEXTS);
    Ok(contexts)
}

/// Coalesces results from multiple phrase queries with zoom-based stacking.
///
/// # Algorithm Overview
///
/// Multi-phrase coalescing combines results from different phrase queries
/// (e.g., "Main" + "Street" + "Seattle") by finding features that spatially
/// overlap at their respective zoom levels.
///
/// ## Key Concepts
///
/// **Zoom-based coordinate matching:**
/// - Different indexes may be at different zoom levels
/// - Higher zoom = more detailed tiles (zoom 14 has 2^14 tiles per side)
/// - Lower zoom = coarser tiles (zoom 6 has 2^6 tiles per side)
/// - To match coordinates across zooms, scale down from higher to lower:
///   `coord_at_lower_zoom = coord_at_higher_zoom / 2^(zoom_diff)`
///
/// **Example:**
/// ```text
/// Index A (zoom 14): "Main Street" at tile (1000, 2000)
/// Index B (zoom 6):  "Seattle" at tile (3, 7)
///
/// To check if they overlap:
/// 1. Scale A's coordinates down to zoom 6:
///    scale_factor = 2^(14-6) = 2^8 = 256
///    A_at_zoom6 = (1000/256, 2000/256) = (3, 7)
/// 2. Compare: (3,7) == (3,7) ✓ They overlap!
/// ```
///
/// ## Stacking Rules
///
/// Phrases can stack if:
/// 1. **Different masks**: `(mask_a & mask_b) == 0`
///    - Each phrase has a unique bit in the mask
///    - Prevents stacking same phrase twice
/// 2. **Spatial overlap**: Coordinates match at lower zoom level
/// 3. **Compatible types**: Type hierarchy allows stacking
///    - Streets can stack with cities
///    - Cities can stack with states
///    - But streets cannot stack directly with countries
///
/// ## Processing Order
///
/// Stack is sorted by (zoom, idx) so we process:
/// 1. Lowest zoom first (coarsest, like countries)
/// 2. Then higher zooms (finer detail, like streets)
///
/// This ensures parent features (cities) are available when
/// processing child features (streets).
///
/// ## Example Execution
///
/// ```text
/// Query: "Main Street Seattle"
/// Stack:
///   [0] zoom=6, idx=2, mask=0b100: "Seattle" (city)
///   [1] zoom=14, idx=1, mask=0b010: "Street" (suffix)
///   [2] zoom=14, idx=0, mask=0b001: "Main" (name)
///
/// Iteration 0: Process "Seattle" at zoom 6
///   - Fetch grids for "Seattle"
///   - Grid: id=100 at (3,7) relev=0.8
///   - Store in coalesced[(6,3,7)] = [Context{entries=[Seattle], mask=0b100, relev=0.8}]
///
/// Iteration 1: Process "Street" at zoom 14
///   - Fetch grids for "Street"
///   - Grid: id=200 at (1000,2000) relev=0.6
///   - Scale down to zoom 6: (1000/256, 2000/256) = (3,7)
///   - Check coalesced[(6,3,7)]: Found "Seattle"!
///   - Stack them: entries=[Street, Seattle], mask=0b110, relev=1.4
///   - Store in coalesced[(14,1000,2000)]
///
/// Iteration 2: Process "Main" at zoom 14
///   - Fetch grids for "Main"
///   - Grid: id=300 at (1000,2000) relev=0.7
///   - Check coalesced[(14,1000,2000)]: Found "Street+Seattle"!
///   - Stack them: entries=[Main, Street, Seattle], mask=0b111, relev=2.1
///   - This is the final iteration, add to contexts
///
/// Result: One context with all three phrases stacked
/// ```
///
/// ## Penalties
///
/// On the final iteration, apply small penalties:
/// - Single-entry contexts: -0.01 (no stacking occurred)
/// - Ascending mask order: -0.01 (less natural phrase order)
///
/// This slightly favors contexts where phrases stack in natural order.
fn coalesce_multi<T: Borrow<GridStore> + Clone>(
    mut stack: Vec<PhrasematchSubquery<T>>,
    match_opts: &MatchOpts,
) -> Result<Vec<CoalesceContext>> {
    // Sort by zoom (lowest first) then idx for deterministic processing
    stack.sort_by_key(|subquery| (subquery.store.borrow().zoom, subquery.idx));

    let mut coalesced: HashMap<(u16, u16, u16), Vec<CoalesceContext>> = HashMap::new();
    let mut contexts: Vec<CoalesceContext> = Vec::new();
    let mut max_relevance: f64 = 0.;
    let mut zoom_adjusted_match_options = match_opts.clone();

    for (i, subquery) in stack.iter().enumerate() {
        let mut to_add_to_coalesced: HashMap<(u16, u16, u16), Vec<CoalesceContext>> = HashMap::new();
        
        // Find compatible zooms: all zooms higher than current
        // (we can scale down from high zoom to low zoom)
        let compatible_zooms: Vec<u16> = stack
            .iter()
            .filter_map(|subquery_b| {
                if subquery.idx == subquery_b.idx
                    || subquery.store.borrow().zoom < subquery_b.store.borrow().zoom
                {
                    None // Skip same phrase or lower zooms
                } else {
                    Some(subquery_b.store.borrow().zoom)
                }
            })
            .dedup()
            .collect();

        // Adjust match options to current zoom level
        if zoom_adjusted_match_options.zoom != subquery.store.borrow().zoom {
            zoom_adjusted_match_options = match_opts.adjust_to_zoom(subquery.store.borrow().zoom);
        }

        let grids = subquery.store.borrow().get_matching(
            &subquery.match_keys[0].key,
            &zoom_adjusted_match_options,
            MAX_GRIDS_PER_PHRASE,
        )?;

        for grid in grids.take(MAX_GRIDS_PER_PHRASE) {
            let coalesce_entry = grid_to_coalesce_entry(&grid, subquery, &zoom_adjusted_match_options, 0);
            let zxy = (subquery.store.borrow().zoom, grid.grid_entry.x, grid.grid_entry.y);

            let mut context_mask = coalesce_entry.mask;
            let mut context_relevance = coalesce_entry.grid_entry.relev;
            let mut entries: Vec<CoalesceEntry> = vec![coalesce_entry];

            // Check if this grid overlaps with grids from compatible (lower) zooms
            for other_zoom in compatible_zooms.iter() {
                // Scale current coordinates down to the lower zoom level
                // Example: zoom 14 coord 1000 → zoom 6 coord 3 (1000 / 2^8 = 3)
                let scale_factor: u16 = 1 << (subquery.store.borrow().zoom - *other_zoom);
                let other_zxy = (
                    *other_zoom,
                    entries[0].grid_entry.x / scale_factor,
                    entries[0].grid_entry.y / scale_factor,
                );

                // Look for previously coalesced entries at this lower zoom coordinate
                if let Some(already_coalesced) = coalesced.get(&other_zxy) {
                    let mut prev_mask = 0;
                    let mut prev_relev: f64 = 0.;
                    
                    // Try to stack with each parent context
                    for parent_context in already_coalesced {
                        for parent_entry in &parent_context.entries {
                            // Replace if same mask but higher relevance
                            if parent_entry.mask == prev_mask
                                && parent_entry.grid_entry.relev > prev_relev
                            {
                                entries.pop();
                                entries.push(parent_entry.clone());
                                context_relevance -= prev_relev;
                                context_relevance += parent_entry.grid_entry.relev;
                                prev_mask = parent_entry.mask;
                                prev_relev = parent_entry.grid_entry.relev;
                            } 
                            // Stack if masks don't overlap (different phrases)
                            else if (context_mask & parent_entry.mask) == 0 {
                                entries.push(parent_entry.clone());
                                context_relevance += parent_entry.grid_entry.relev;
                                context_mask = context_mask | parent_entry.mask;
                                prev_mask = parent_entry.mask;
                                prev_relev = parent_entry.grid_entry.relev;
                            }
                        }
                    }
                }
            }
            
            if context_relevance > max_relevance {
                max_relevance = context_relevance;
            }

            // Final iteration: add to output contexts with penalties
            if i == (stack.len() - 1) {
                // Penalize single-entry contexts (no stacking occurred)
                if entries.len() == 1 {
                    context_relevance -= 0.01;
                } 
                // Penalize ascending mask order (less natural phrase order)
                // Example: mask 0b001 before 0b010 is ascending (1 < 2)
                else if entries[0].mask > entries[1].mask {
                    context_relevance -= 0.01
                }

                // Only keep contexts within relevance threshold
                if max_relevance - context_relevance < MAX_RELEVANCE_GAP {
                    contexts.push(CoalesceContext {
                        entries,
                        mask: context_mask,
                        relev: context_relevance,
                    });
                }
            } 
            // Non-final iterations: store for future stacking
            // Only store if first iteration OR successfully stacked (len > 1)
            else if i == 0 || entries.len() > 1 {
                if let Some(already_coalesced) = to_add_to_coalesced.get_mut(&zxy) {
                    already_coalesced.push(CoalesceContext {
                        entries,
                        mask: context_mask,
                        relev: context_relevance,
                    });
                } else {
                    to_add_to_coalesced.insert(
                        zxy,
                        vec![CoalesceContext {
                            entries,
                            mask: context_mask,
                            relev: context_relevance,
                        }],
                    );
                }
            }
        }
        
        // Merge this iteration's results into the main coalesced map
        for (to_add_zxy, to_add_context) in to_add_to_coalesced {
            if let Some(existing_vector) = coalesced.get_mut(&to_add_zxy) {
                existing_vector.extend(to_add_context);
            } else {
                coalesced.insert(to_add_zxy, to_add_context);
            }
        }
    }

    // Add any remaining coalesced contexts that weren't added in final iteration
    for (_, matched) in coalesced {
        for context in matched {
            if max_relevance - context.relev < MAX_RELEVANCE_GAP {
                contexts.push(context);
            }
        }
    }

    contexts.sort_by_key(|context| {
        (
            Reverse(OrderedFloat(context.relev)),
            Reverse(OrderedFloat(context.entries[0].scoredist)),
            context.entries[0].idx,
            Reverse(context.entries[0].grid_entry.x),
            Reverse(context.entries[0].grid_entry.y),
            Reverse(context.entries[0].grid_entry.id),
        )
    });

    Ok(contexts)
}

// Tree-based coalesce using stackable trees and KDBush spatial indexing
//
// TODO: Full implementation requires:
// 1. ConstrainedPriorityQueue wrapper around MinMaxHeap
// 2. TreeCoalesceState with KDBush spatial index
// 3. CoalesceStep priority queue walker
// 4. Parallel processing with rayon
// 5. Query quotas (one-letter, high-zoom, etc.)
// 6. Spatial overlap checks with KDBush
//
// For now, fall back to coalesce_multi which provides similar functionality
// without the tree optimization.
struct TreeCoalesceState {
    contexts: Vec<CoalesceContext>,
    bush: static_bushes::KDBush<u16>,
}

impl TreeCoalesceState {
    fn new(contexts: Vec<CoalesceContext>) -> TreeCoalesceState {
        let mut builder: static_bushes::KDBushBuilder<u16> = static_bushes::KDBushBuilder::new();
        for context in contexts.iter() {
            let point = [context.entries[0].grid_entry.x, context.entries[0].grid_entry.y];
            builder.add(&point);
        }
        let bush = builder.finish();
        TreeCoalesceState { contexts, bush }
    }
}

struct CoalesceStep<'a, T: Borrow<GridStore> + Clone + Debug> {
    node: &'a StackableNode<'a, T>,
    prev_state: Option<std::sync::Arc<TreeCoalesceState>>,
    prev_zoom: u16,
    match_opts: MatchOpts,
    possible_relev: f64,
    contains_prox: bool,
}

impl<T: Borrow<GridStore> + Clone + Debug> CoalesceStep<'_, T> {
    fn new<'a>(
        node: &'a StackableNode<'a, T>,
        prev_state: Option<std::sync::Arc<TreeCoalesceState>>,
        prev_zoom: u16,
        match_opts: &MatchOpts,
        possible_relev: f64,
    ) -> CoalesceStep<'a, T> {
        let subquery = node.phrasematch.expect("phrasematch required");
        let match_opts = if match_opts.zoom == subquery.store.borrow().zoom {
            match_opts.clone()
        } else {
            match_opts.adjust_to_zoom(subquery.store.borrow().zoom)
        };

        let contains_prox = if let Some(prox) = match_opts.proximity {
            subquery.store.borrow().bboxes.iter().any(|bbox| {
                bbox[0] <= prox[0] && bbox[2] >= prox[0] && bbox[1] <= prox[1] && bbox[3] >= prox[1]
            })
        } else {
            false
        };

        CoalesceStep { node, prev_state, prev_zoom, match_opts, possible_relev, contains_prox }
    }

    #[inline(always)]
    fn cmp_key(&self) -> (OrderedFloat<f64>, bool, OrderedFloat<f64>) {
        let subquery = self.node.phrasematch.expect("phrasematch required");
        (
            OrderedFloat(self.node.max_relev),
            self.contains_prox,
            OrderedFloat(subquery.store.borrow().max_score),
        )
    }
}

impl<T: Borrow<GridStore> + Clone + Debug> Ord for CoalesceStep<'_, T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.cmp_key().cmp(&other.cmp_key())
    }
}
impl<T: Borrow<GridStore> + Clone + Debug> PartialOrd for CoalesceStep<'_, T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<T: Borrow<GridStore> + Clone + Debug> PartialEq for CoalesceStep<'_, T> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp_key() == other.cmp_key()
    }
}
impl<T: Borrow<GridStore> + Clone + Debug> Eq for CoalesceStep<'_, T> {}

struct KeyFetchStep<T: Borrow<GridStore> + Clone + Debug> {
    key_id: u32,
    subquery: PhrasematchSubquery<T>,
    key: MatchKey,
    match_opts: MatchOpts,
    is_single: bool,
}

enum KeyFetchResult {
    Single(ConstrainedPriorityQueue<CoalesceContext>),
    Multi((u32, Vec<MatchEntry>)),
}

fn penalize_multi_context(context: &mut CoalesceContext) {
    if context.entries.len() == 1 || context.entries[0].mask > context.entries[1].mask {
        context.relev -= 0.01
    }
}

pub const COALESCE_CHUNK_SIZE: usize = 8;
pub const ONE_LETTER_RANGE_QUOTA: usize = 8;
pub const ONE_WORD_HIGH_ZOOM_RANGE_QUOTA: usize = 8;
pub const ONE_WORD_RANGE_QUOTA: usize = 40;
pub const ALL_HIGH_ZOOM_RANGE_QUOTA: usize = 40;
pub const ALL_HIGH_ZOOM_QUOTA: usize = 600;

pub fn tree_coalesce<T: Borrow<GridStore> + Clone + Debug>(
    stack_tree: &StackableTree<T>,
    match_opts: &MatchOpts,
) -> Result<Vec<CoalesceContext>> {
    debug_assert!(stack_tree.root.phrasematch.is_none(), "no phrasematch on root node");

    let mut contexts: ConstrainedPriorityQueue<CoalesceContext> =
        ConstrainedPriorityQueue::new(MAX_CONTEXTS * 20);
    let mut steps: interval_heap::IntervalHeap<CoalesceStep<T>> = interval_heap::IntervalHeap::new();
    let mut data_cache: HashMap<u32, Vec<MatchEntry>> = HashMap::new();

    let mut one_letter_range_count: usize = 0;
    let mut one_word_range_count: usize = 0;
    let mut one_word_high_zoom_range_count: usize = 0;
    let mut all_high_zoom_range_count: usize = 0;
    let mut all_high_zoom_count: usize = 0;

    for child_idx in &stack_tree.root.children {
        if let Some(node) = stack_tree.arena.get(*child_idx) {
            let weight = node
                .phrasematch
                .as_ref()
                .expect("phrasematch must be set on non-root tree nodes")
                .weight;
            steps.push(CoalesceStep::new(node, None, 0, match_opts, weight));
        }
    }

    let mut complete = false;
    while !steps.is_empty() && !complete {
        let mut step_chunk = Vec::with_capacity(COALESCE_CHUNK_SIZE);
        let mut keys = Vec::new();
        let mut unique_keys = std::collections::HashSet::new();

        let mut added_in_this_chunk = 0;
        while added_in_this_chunk < COALESCE_CHUNK_SIZE {
            let mut enqueued_work_in_this_iter = false;

            if let Some(step) = steps.pop_max() {
                if contexts.len() >= contexts.max_size {
                    if step.node.max_relev
                        <= contexts.peek_min().expect("contexts can't be empty").relev
                    {
                        complete = true;
                        break;
                    }
                }

                let is_single = step.prev_state.is_none() && step.node.children.is_empty();

                let subquery = step
                    .node
                    .phrasematch
                    .as_ref()
                    .expect("phrasematch must be set on non-root tree nodes");

                for key_group in subquery.match_keys.iter() {
                    if is_single || !data_cache.contains_key(&key_group.id) {
                        let match_opts = if key_group.nearby_only || key_group.bounds.is_some() {
                            step.match_opts.augment_bbox(key_group.nearby_only, key_group.bounds)
                        } else {
                            step.match_opts.clone()
                        };

                        let is_range = match key_group.key.match_phrase {
                            MatchPhrase::Exact(_) => false,
                            MatchPhrase::Range { start, end } => end - start > 1,
                        };

                        if is_range && subquery.mask.count_ones() == 1 {
                            if subquery.store.borrow().might_be_slow()
                                && step.node.is_leaf()
                                && step.possible_relev
                                    <= 0.75
                                        * contexts
                                            .peek_max()
                                            .map_or(0.0, |coalesce_context| coalesce_context.relev)
                            {
                                continue;
                            } else if !unique_keys.contains(&(key_group.id, is_single)) {
                                if key_group.phrase_length == 1 {
                                    if one_letter_range_count < ONE_LETTER_RANGE_QUOTA {
                                        one_letter_range_count += 1;
                                    } else {
                                        continue;
                                    }
                                }

                                if subquery.store.borrow().might_be_slow() && !key_group.nearby_only
                                {
                                    if one_word_high_zoom_range_count
                                        < ONE_WORD_HIGH_ZOOM_RANGE_QUOTA
                                    {
                                        one_word_high_zoom_range_count += 1;
                                    } else {
                                        continue;
                                    }
                                }

                                if one_word_range_count < ONE_WORD_RANGE_QUOTA {
                                    one_word_range_count += 1;
                                } else {
                                    continue;
                                }
                            }
                        }

                        if subquery.store.borrow().might_be_slow() && !key_group.nearby_only {
                            if is_range {
                                if all_high_zoom_range_count < ALL_HIGH_ZOOM_RANGE_QUOTA {
                                    all_high_zoom_range_count += 1;
                                } else {
                                    continue;
                                }
                            }

                            if all_high_zoom_count < ALL_HIGH_ZOOM_QUOTA {
                                all_high_zoom_count += 1;
                            } else {
                                continue;
                            }
                        }

                        if unique_keys.insert((key_group.id, is_single)) {
                            enqueued_work_in_this_iter = true;
                            keys.push(KeyFetchStep {
                                key_id: key_group.id,
                                subquery: (*subquery).clone(),
                                key: key_group.key.clone(),
                                match_opts,
                                is_single,
                            });
                        }
                    }
                }

                if !is_single {
                    enqueued_work_in_this_iter = true;
                    step_chunk.push(step);
                }

                if enqueued_work_in_this_iter {
                    added_in_this_chunk += 1;
                }
            } else {
                break;
            }
        }

        for key_step in keys {
            let result = if key_step.is_single {
                let bigger_max = 2 * MAX_CONTEXTS;
                let mut step_contexts: ConstrainedPriorityQueue<CoalesceContext> =
                    ConstrainedPriorityQueue::new(MAX_CONTEXTS);

                let grids = key_step.subquery.store.borrow().get_matching(
                    &key_step.key,
                    &key_step.match_opts,
                    bigger_max,
                )?;

                let coalesced = tree_coalesce_single(
                    &key_step.subquery,
                    &key_step.match_opts,
                    grids,
                    key_step.key_id,
                )?;

                for entry in coalesced {
                    step_contexts.push(entry);
                }

                KeyFetchResult::Single(step_contexts)
            } else {
                let mut unique_ids = fxhash::FxHashSet::default();
                let data: Vec<_> = key_step
                    .subquery
                    .store
                    .borrow()
                    .get_matching(
                        &key_step.key,
                        &key_step.match_opts,
                        MAX_GRIDS_PER_PHRASE,
                    )?
                    .take(MAX_GRIDS_PER_PHRASE)
                    .filter(|grid| {
                        unique_ids.insert((
                            grid.grid_entry.x,
                            grid.grid_entry.y,
                            grid.grid_entry.id,
                        ))
                    })
                    .collect();
                KeyFetchResult::Multi((key_step.key_id, data))
            };

            match result {
                KeyFetchResult::Single(phrasematch_contexts) => {
                    for context in phrasematch_contexts {
                        contexts.push(context);
                    }
                }
                KeyFetchResult::Multi((key_id, data)) => {
                    data_cache.insert(key_id, data);
                }
            }
        }

        for step in step_chunk {
            let mut relev_so_far = 0.0;
            let subquery = step
                .node
                .phrasematch
                .as_ref()
                .expect("phrasematch must be set on non-root tree nodes");

            let mut phrasematch_contexts: Vec<CoalesceContext> = Vec::new();

            let scale_factor: u16 = 1 << (subquery.store.borrow().zoom - step.prev_zoom);

            let mut state_contexts: Vec<CoalesceContext> = Vec::new();

            for key_group in subquery.match_keys.iter() {
                let grids = match data_cache.get(&key_group.id) {
                    Some(data) => data,
                    None => continue,
                };

                let mut step_contexts: ConstrainedPriorityQueue<CoalesceContext> =
                    ConstrainedPriorityQueue::new(MAX_CONTEXTS);

                if let Some(prev_state) = &step.prev_state {
                    for grid in grids.iter() {
                        let prev_zoom_xy = (
                            grid.grid_entry.x / scale_factor,
                            grid.grid_entry.y / scale_factor,
                        );

                        let entry = grid_to_coalesce_entry(
                            grid,
                            subquery,
                            &step.match_opts,
                            key_group.id,
                        );

                        let already_coalesced =
                            prev_state.bush.exact_as_vec(prev_zoom_xy.0, prev_zoom_xy.1);
                        for parent_id in already_coalesced {
                            let parent_context = &prev_state.contexts[parent_id];
                            let mut new_context = parent_context.clone();
                            new_context.entries.insert(0, entry.clone());

                            new_context.mask |= subquery.mask;
                            new_context.relev += entry.grid_entry.relev;
                            if new_context.relev > relev_so_far {
                                relev_so_far = new_context.relev;
                            }

                            let mut out_context = new_context.clone();
                            penalize_multi_context(&mut out_context);
                            step_contexts.push(out_context);

                            if !step.node.children.is_empty() {
                                state_contexts.push(new_context);
                            }
                        }
                    }
                } else {
                    for grid in grids.iter() {
                        let entry = grid_to_coalesce_entry(
                            grid,
                            subquery,
                            &step.match_opts,
                            key_group.id,
                        );
                        let context = CoalesceContext {
                            mask: subquery.mask,
                            relev: entry.grid_entry.relev,
                            entries: vec![entry],
                        };

                        if context.relev > relev_so_far {
                            relev_so_far = context.relev;
                        }

                        let mut out_context = context.clone();
                        penalize_multi_context(&mut out_context);
                        step_contexts.push(out_context);

                        state_contexts.push(context);
                    }
                }
                phrasematch_contexts.extend(step_contexts.into_iter());
            }

            if !state_contexts.is_empty() {
                let state = std::sync::Arc::new(TreeCoalesceState::new(state_contexts));
                let current_zoom = subquery.store.borrow().zoom;
                for child_idx in step.node.children.iter() {
                    if let Some(child) = stack_tree.arena.get(*child_idx) {
                        let child_store = child.phrasematch.unwrap().store.borrow();
                        let child_zoom = child_store.zoom;

                        let zoomed_bboxes: Vec<_>;
                        let child_bboxes = if child_zoom == current_zoom {
                            &child_store.bboxes
                        } else {
                            zoomed_bboxes = child_store
                                .bboxes
                                .iter()
                                .map(|bbox| {
                                    adjust_bbox_zoom(*bbox, child_zoom, current_zoom)
                                })
                                .collect();
                            &zoomed_bboxes
                        };

                        let overlaps = child_bboxes.iter().any(|bbox| {
                            state
                                .bush
                                .search_range(bbox[0], bbox[1], bbox[2], bbox[3])
                                .next()
                                .is_some()
                        });

                        if !overlaps {
                            continue;
                        }

                        steps.push(CoalesceStep::new(
                            child,
                            Some(state.clone()),
                            current_zoom,
                            match_opts,
                            relev_so_far
                                + child.phrasematch.expect("phrasematch required").weight,
                        ));
                    }
                }
            }

            for context in phrasematch_contexts {
                contexts.push(context);
            }
        }
    }

    Ok(contexts.into_vec_desc())
}

fn tree_coalesce_single<T: Borrow<GridStore> + Clone>(
    subquery: &PhrasematchSubquery<T>,
    match_opts: &MatchOpts,
    grids: impl Iterator<Item = MatchEntry>,
    phrasematch_id: u32,
) -> Result<Vec<CoalesceContext>> {
    let bigger_max = 2 * MAX_CONTEXTS;

    let mut max_relevance: f64 = 0.;
    let mut previous_id: u32 = 0;
    let mut previous_relevance: f64 = 0.;
    let mut previous_scoredist: f64 = 0.;
    let mut min_scoredist = std::f64::MAX;
    let mut feature_count: usize = 0;

    let mut coalesced: HashMap<u32, CoalesceEntry> = HashMap::new();

    for grid in grids {
        let coalesce_entry = grid_to_coalesce_entry(&grid, subquery, match_opts, phrasematch_id);

        if previous_id == coalesce_entry.grid_entry.id
            && coalesce_entry.scoredist <= previous_scoredist
        {
            continue;
        }

        if feature_count > bigger_max {
            if coalesce_entry.scoredist < min_scoredist {
                continue;
            } else if coalesce_entry.grid_entry.relev < previous_relevance {
                break;
            }
        }

        if coalesce_entry.grid_entry.relev > max_relevance {
            max_relevance = coalesce_entry.grid_entry.relev;
        }

        if coalesce_entry.scoredist < min_scoredist {
            min_scoredist = coalesce_entry.scoredist;
        }

        previous_id = coalesce_entry.grid_entry.id;
        previous_relevance = coalesce_entry.grid_entry.relev;
        previous_scoredist = coalesce_entry.scoredist;
        feature_count += 1;

        coalesced.insert(coalesce_entry.grid_entry.id, coalesce_entry);
    }

    Ok(coalesced.into_iter().map(|(_, entry)| CoalesceContext {
        mask: subquery.mask,
        relev: entry.grid_entry.relev,
        entries: vec![entry],
    }).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{
        GridStoreBuilder, GridKey, GridEntry, global_bbox_for_zoom,
        MatchKey, MatchPhrase, MatchKeyWithId, PhrasematchSubquery,
        MatchOpts, MAX_INDEXES,
    };
    use fixedbitset::FixedBitSet;

    fn create_test_store(
        entries: Vec<(GridKey, Vec<GridEntry>)>,
        zoom: u16,
        type_id: u16,
    ) -> GridStore {
        let directory = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(directory.path()).unwrap();
        
        for (key, grid_entries) in entries {
            builder.insert(&key, grid_entries).unwrap();
        }
        builder.finish().unwrap();
        
        GridStore::new_with_options(
            directory.path(),
            zoom,
            type_id,
            40.0,
            global_bbox_for_zoom(zoom),
            1.0,
        ).unwrap()
    }

    #[test]
    fn test_coalesce_multi_basic() {
        // Create two stores with overlapping entries
        let store1 = create_test_store(
            vec![(
                GridKey { phrase_id: 1, lang_set: 1 },
                vec![
                    GridEntry { id: 1, x: 1, y: 1, relev: 1.0, score: 1, source_phrase_hash: 0 },
                    GridEntry { id: 2, x: 2, y: 2, relev: 1.0, score: 1, source_phrase_hash: 0 },
                ],
            )],
            6,
            1,
        );

        let store2 = create_test_store(
            vec![(
                GridKey { phrase_id: 2, lang_set: 1 },
                vec![
                    GridEntry { id: 1, x: 1, y: 1, relev: 1.0, score: 3, source_phrase_hash: 0 },
                    GridEntry { id: 2, x: 2, y: 2, relev: 1.0, score: 3, source_phrase_hash: 0 },
                    GridEntry { id: 3, x: 3, y: 3, relev: 1.0, score: 1, source_phrase_hash: 0 },
                ],
            )],
            6,
            2,
        );

        let stack = vec![
            PhrasematchSubquery {
                store: &store1,
                idx: 1,
                non_overlapping_indexes: FixedBitSet::with_capacity(MAX_INDEXES),
                weight: 0.5,
                match_keys: vec![MatchKeyWithId {
                    id: 0,
                    key: MatchKey {
                        match_phrase: MatchPhrase::Range { start: 1, end: 3 },
                        lang_set: 1,
                    },
                    ..MatchKeyWithId::default()
                }],
                mask: 1 << 1,
            },
            PhrasematchSubquery {
                store: &store2,
                idx: 2,
                non_overlapping_indexes: FixedBitSet::with_capacity(MAX_INDEXES),
                weight: 0.5,
                match_keys: vec![MatchKeyWithId {
                    id: 1,
                    key: MatchKey {
                        match_phrase: MatchPhrase::Range { start: 1, end: 3 },
                        lang_set: 1,
                    },
                    ..MatchKeyWithId::default()
                }],
                mask: 1 << 0,
            },
        ];

        let match_opts = MatchOpts { zoom: 6, ..MatchOpts::default() };
        
        // Test tree_coalesce
        let tree = crate::storage::stackable(&stack);
        let result = tree_coalesce(&tree, &match_opts).unwrap();
        
        assert!(!result.is_empty(), "Should have coalesced results");
        assert_eq!(result[0].entries.len(), 2, "First result should have 2 entries (stacked)");
        assert_eq!(result[0].mask, 3, "First result should have combined mask");
        
        // Check that entries are from both stores
        let has_store1 = result[0].entries.iter().any(|e| e.idx == 1);
        let has_store2 = result[0].entries.iter().any(|e| e.idx == 2);
        assert!(has_store1 && has_store2, "Result should combine entries from both stores");
    }

    #[test]
    fn test_coalesce_single_basic() {
        let store = create_test_store(
            vec![(
                GridKey { phrase_id: 1, lang_set: 1 },
                vec![
                    GridEntry { id: 1, x: 1, y: 1, relev: 1.0, score: 7, source_phrase_hash: 0 },
                    GridEntry { id: 2, x: 2, y: 2, relev: 0.8, score: 3, source_phrase_hash: 0 },
                ],
            )],
            14,
            0,
        );

        let subquery = PhrasematchSubquery {
            store: &store,
            idx: 0,
            non_overlapping_indexes: FixedBitSet::with_capacity(MAX_INDEXES),
            weight: 1.0,
            match_keys: vec![MatchKeyWithId {
                id: 0,
                key: MatchKey {
                    match_phrase: MatchPhrase::Range { start: 1, end: 3 },
                    lang_set: 1,
                },
                ..MatchKeyWithId::default()
            }],
            mask: 1,
        };

        let match_opts = MatchOpts { zoom: 14, ..MatchOpts::default() };
        let result = coalesce_single(&subquery, &match_opts).unwrap();
        
        assert!(!result.is_empty(), "Should have results");
        assert_eq!(result[0].entries.len(), 1, "Single coalesce should have 1 entry per context");
    }

    #[test]
    fn test_tree_coalesce_with_proximity() {
        let store1 = create_test_store(
            vec![(
                GridKey { phrase_id: 1, lang_set: 1 },
                vec![
                    GridEntry { id: 1, x: 100, y: 100, relev: 1.0, score: 1, source_phrase_hash: 0 },
                    GridEntry { id: 2, x: 200, y: 200, relev: 1.0, score: 1, source_phrase_hash: 0 },
                ],
            )],
            14,
            1,
        );

        let store2 = create_test_store(
            vec![(
                GridKey { phrase_id: 2, lang_set: 1 },
                vec![
                    GridEntry { id: 1, x: 100, y: 100, relev: 1.0, score: 3, source_phrase_hash: 0 },
                    GridEntry { id: 2, x: 200, y: 200, relev: 1.0, score: 3, source_phrase_hash: 0 },
                ],
            )],
            14,
            2,
        );

        let stack = vec![
            PhrasematchSubquery {
                store: &store1,
                idx: 1,
                non_overlapping_indexes: FixedBitSet::with_capacity(MAX_INDEXES),
                weight: 0.5,
                match_keys: vec![MatchKeyWithId {
                    id: 0,
                    key: MatchKey {
                        match_phrase: MatchPhrase::Range { start: 1, end: 3 },
                        lang_set: 1,
                    },
                    ..MatchKeyWithId::default()
                }],
                mask: 1 << 1,
            },
            PhrasematchSubquery {
                store: &store2,
                idx: 2,
                non_overlapping_indexes: FixedBitSet::with_capacity(MAX_INDEXES),
                weight: 0.5,
                match_keys: vec![MatchKeyWithId {
                    id: 1,
                    key: MatchKey {
                        match_phrase: MatchPhrase::Range { start: 1, end: 3 },
                        lang_set: 1,
                    },
                    ..MatchKeyWithId::default()
                }],
                mask: 1 << 0,
            },
        ];

        // Test with proximity near first entry
        let match_opts = MatchOpts {
            zoom: 14,
            proximity: Some([105, 105]),
            ..MatchOpts::default()
        };
        
        let tree = crate::storage::stackable(&stack);
        let result = tree_coalesce(&tree, &match_opts).unwrap();
        
        assert!(!result.is_empty(), "Should have results with proximity");
        // First result should be closer to proximity point
        assert_eq!(result[0].entries[0].grid_entry.id, 1, "Closest entry should be first");
    }
}
