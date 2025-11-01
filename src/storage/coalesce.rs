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

pub fn coalesce_single<T: Borrow<GridStore> + Clone>(
    subquery: &PhrasematchSubquery<T>,
    match_opts: &MatchOpts,
) -> Result<Vec<CoalesceContext>> {
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

fn coalesce_multi<T: Borrow<GridStore> + Clone>(
    mut stack: Vec<PhrasematchSubquery<T>>,
    match_opts: &MatchOpts,
) -> Result<Vec<CoalesceContext>> {
    stack.sort_by_key(|subquery| (subquery.store.borrow().zoom, subquery.idx));

    let mut coalesced: HashMap<(u16, u16, u16), Vec<CoalesceContext>> = HashMap::new();
    let mut contexts: Vec<CoalesceContext> = Vec::new();
    let mut max_relevance: f64 = 0.;
    let mut zoom_adjusted_match_options = match_opts.clone();

    for (i, subquery) in stack.iter().enumerate() {
        let mut to_add_to_coalesced: HashMap<(u16, u16, u16), Vec<CoalesceContext>> = HashMap::new();
        
        let compatible_zooms: Vec<u16> = stack
            .iter()
            .filter_map(|subquery_b| {
                if subquery.idx == subquery_b.idx
                    || subquery.store.borrow().zoom < subquery_b.store.borrow().zoom
                {
                    None
                } else {
                    Some(subquery_b.store.borrow().zoom)
                }
            })
            .dedup()
            .collect();

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

            for other_zoom in compatible_zooms.iter() {
                let scale_factor: u16 = 1 << (subquery.store.borrow().zoom - *other_zoom);
                let other_zxy = (
                    *other_zoom,
                    entries[0].grid_entry.x / scale_factor,
                    entries[0].grid_entry.y / scale_factor,
                );

                if let Some(already_coalesced) = coalesced.get(&other_zxy) {
                    let mut prev_mask = 0;
                    let mut prev_relev: f64 = 0.;
                    for parent_context in already_coalesced {
                        for parent_entry in &parent_context.entries {
                            if parent_entry.mask == prev_mask
                                && parent_entry.grid_entry.relev > prev_relev
                            {
                                entries.pop();
                                entries.push(parent_entry.clone());
                                context_relevance -= prev_relev;
                                context_relevance += parent_entry.grid_entry.relev;
                                prev_mask = parent_entry.mask;
                                prev_relev = parent_entry.grid_entry.relev;
                            } else if (context_mask & parent_entry.mask) == 0 {
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

            if i == (stack.len() - 1) {
                if entries.len() == 1 {
                    context_relevance -= 0.01;
                } else if entries[0].mask > entries[1].mask {
                    context_relevance -= 0.01
                }

                if max_relevance - context_relevance < MAX_RELEVANCE_GAP {
                    contexts.push(CoalesceContext {
                        entries,
                        mask: context_mask,
                        relev: context_relevance,
                    });
                }
            } else if i == 0 || entries.len() > 1 {
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
        
        for (to_add_zxy, to_add_context) in to_add_to_coalesced {
            if let Some(existing_vector) = coalesced.get_mut(&to_add_zxy) {
                existing_vector.extend(to_add_context);
            } else {
                coalesced.insert(to_add_zxy, to_add_context);
            }
        }
    }

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
