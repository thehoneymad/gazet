//! Multi-phrase coalescing logic from carmen-core.
//!
//! Combines results from multiple phrase queries (e.g., "Main" + "Street" + "Seattle")
//! based on spatial overlap and type hierarchy rules.

use crate::storage::{
    CoalesceEntry, GridStore, MatchEntry, MatchOpts, PhrasematchSubquery,
    Result, StackableTree, MAX_CONTEXTS,
};
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

// Stub for tree_coalesce - will implement after testing coalesce_multi
pub fn tree_coalesce<T: Borrow<GridStore> + Clone + Debug>(
    _stack_tree: &StackableTree<T>,
    _match_opts: &MatchOpts,
) -> Result<Vec<CoalesceContext>> {
    unimplemented!("tree_coalesce not yet ported")
}
