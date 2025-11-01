//! Stackable tree system for multi-phrase query combination.
//!
//! Builds a tree of valid phrase combinations that can be spatially stacked.
//! For example, "Main" + "Street" + "Seattle" creates a tree of all valid
//! combinations where phrases spatially overlap.
//!
//! This is a complete port of carmen-core's stackable.rs system.

#![allow(dead_code)]

use crate::storage::{GridStore, PhrasematchSubquery, MAX_INDEXES};
use fixedbitset::FixedBitSet;
use fxhash::FxHashMap as HashMap;
use generational_arena::{Arena, Index as ArenaIndex};
use ordered_float::OrderedFloat;
use std::borrow::Borrow;
use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::fmt::Debug;

/// Maximum number of leaf nodes to keep in the stackable tree.
///
/// Limits memory usage by pruning low-relevance combinations.
/// Carmen-core uses 2000 as a balance between coverage and performance.
pub const LEAF_SOFT_MAX: usize = 2000;

/// A node in the stackable tree representing a phrase combination.
///
/// Each node represents either:
/// - A leaf: Single phrase that can be stacked with others
/// - A branch: Combination of multiple phrases that spatially overlap
///
/// # Fields
/// - `phrasematch`: The phrase query this node represents (None for root)
/// - `children`: Child nodes that can stack with this one
/// - `bmask`: Bitmask of indexes that have been used in this branch
/// - `mask`: Combined mask of all phrases in this combination
/// - `idx`: Index of the phrase (0, 1, 2, ...)
/// - `max_relev`: Maximum possible relevance from this node down
/// - `zoom`: Zoom level of the phrase's index
#[derive(Debug, Clone)]
pub struct StackableNode<'a, T: Borrow<GridStore> + Clone + Debug> {
    pub phrasematch: Option<&'a PhrasematchSubquery<T>>,
    pub children: Vec<ArenaIndex>,
    pub bmask: FixedBitSet,
    pub mask: u32,
    pub idx: u16,
    pub max_relev: f64,
    pub zoom: u16,
}

impl<'a, T: Borrow<GridStore> + Clone + Debug> StackableNode<'a, T> {
    /// Returns true if this node has no children (is a leaf).
    pub fn is_leaf(&self) -> bool {
        self.children.is_empty()
    }
}

/// Breadth-first search traversal of stackable tree (used for testing).
///
/// Returns all nodes in BFS order starting from root.
pub fn bfs<T: Borrow<GridStore> + Clone + Debug>(tree: StackableTree<T>) -> Vec<StackableNode<T>> {
    let mut node_vec: Vec<StackableNode<T>> = vec![];
    let mut stack: Vec<_> = vec![];

    stack.push(tree.root.clone());

    while !stack.is_empty() {
        let node = stack.pop().unwrap();
        node_vec.push(node.clone());
        for maybe_child in node.children {
            if let Some(child) = tree.arena.get(maybe_child) {
                stack.push(child.clone());
            }
        }
    }
    node_vec
}

/// Arena-based memory manager for stackable tree nodes.
///
/// Manages node allocation and pruning based on relevance thresholds.
/// Keeps only the top LEAF_SOFT_MAX (2000) leaf nodes by relevance.
///
/// # Pruning Strategy
/// - Tracks leaf count per relevance level
/// - When full, culls the lowest relevance bin
/// - Maintains min_relev threshold for new insertions
#[derive(Debug, Clone)]
pub struct ArenaManager<'a, T: Borrow<GridStore> + Clone + Debug> {
    arena: Arena<StackableNode<'a, T>>,
    /// Map from max_relev to (leaf_count, list of arena indexes)
    relev_map: HashMap<OrderedFloat<f64>, (usize, Vec<ArenaIndex>)>,
    /// Minimum relevance currently in the arena
    min_relev: OrderedFloat<f64>,
    /// Total number of leaf nodes
    total_leaves: usize,
    /// Soft maximum for leaf nodes (2000)
    soft_max: usize,
}

impl<'a, T: Borrow<GridStore> + Clone + Debug> ArenaManager<'a, T> {
    fn new() -> Self {
        ArenaManager {
            arena: Arena::new(),
            relev_map: HashMap::default(),
            min_relev: OrderedFloat(std::f64::MAX),
            total_leaves: 0,
            soft_max: LEAF_SOFT_MAX,
        }
    }

    #[inline(always)]
    fn is_full(&self) -> bool {
        self.total_leaves >= self.soft_max
    }

    /// Adds a node to the arena, potentially pruning low-relevance nodes.
    ///
    /// Returns Some(ArenaIndex) if node was added, None if rejected.
    fn add(&mut self, node: StackableNode<'a, T>) -> Option<ArenaIndex> {
        let max_relev = OrderedFloat(node.max_relev);
        let is_leaf = node.children.is_empty();
        let old_total_leaves = self.total_leaves;

        if old_total_leaves >= self.soft_max && max_relev < self.min_relev {
            // Arena is full and this node is worse than our worst - reject it
            None
        } else {
            // Add the node to arena
            let arena_index = self.arena.insert(node);

            let relev_entry = self.relev_map.entry(max_relev).or_insert((0, Vec::new()));

            if is_leaf {
                relev_entry.0 += 1;
                self.total_leaves += 1;
            }
            relev_entry.1.push(arena_index);

            if old_total_leaves < self.soft_max {
                // Unconstrained add - just update min if necessary
                if max_relev < self.min_relev {
                    self.min_relev = max_relev;
                }
            } else {
                // Constrained add - may need to cull minimum bin
                if max_relev > self.min_relev {
                    // Added to better bin than worst - check if we can cull worst
                    let min_count = self
                        .relev_map
                        .get(&self.min_relev)
                        .expect("must contain min_relev")
                        .0;
                    let total_without_min = self.total_leaves - min_count;
                    if total_without_min >= self.soft_max {
                        // Can remove minimum bin and still have enough leaves
                        self.cull_min();
                    }
                }
            }
            Some(arena_index)
        }
    }

    /// Removes all nodes in the minimum relevance bin.
    fn cull_min(&mut self) {
        if let Some((min_leaf_count, min_nodes)) = self.relev_map.remove(&self.min_relev) {
            self.total_leaves -= min_leaf_count;
            for node_index in min_nodes {
                self.arena.remove(node_index);
            }
        }
        // Pick new minimum from remaining bins
        self.min_relev = self
            .relev_map
            .keys()
            .min()
            .copied()
            .unwrap_or(OrderedFloat(std::f64::MAX));
    }

    #[inline(always)]
    pub fn get(&self, index: ArenaIndex) -> Option<&StackableNode<'a, T>> {
        self.arena.get(index)
    }
}

/// Complete stackable tree with root node and arena.
#[derive(Debug, Clone)]
pub struct StackableTree<'a, T: Borrow<GridStore> + Clone + Debug> {
    pub root: StackableNode<'a, T>,
    pub arena: ArenaManager<'a, T>,
}

struct PhrasematchBin<'a, T: Borrow<GridStore> + Clone + Debug> {
    phrasematches: Vec<&'a PhrasematchSubquery<T>>,
    max_relev: OrderedFloat<f64>,
    max_relev_after_this: OrderedFloat<f64>,
}

/// Builds stackable tree from phrasematch results.
///
/// Groups phrasematches by type_id, then recursively builds tree of valid
/// combinations where phrases can spatially stack.
///
/// # Algorithm Overview
///
/// The stackable tree represents all valid combinations of phrases that can be
/// spatially stacked together. For example, given:
/// - "Main" (type_id=1, street)
/// - "Street" (type_id=1, street)  
/// - "Seattle" (type_id=2, city)
///
/// The tree structure would be:
/// ```text
/// Root
///  ├─ Main (type_id=1)
///  │   └─ Seattle (type_id=2)  ← Can stack: different type_id, compatible masks
///  ├─ Street (type_id=1)
///  │   └─ Seattle (type_id=2)  ← Can stack: different type_id, compatible masks
///  └─ Seattle (type_id=2)
///      ├─ Main (type_id=1)     ← Cannot stack: would create duplicate
///      └─ Street (type_id=1)   ← Cannot stack: would create duplicate
/// ```
///
/// # Type Hierarchy
///
/// Phrases are grouped by type_id to enforce stacking rules:
/// - type_id=0: Address/POI (most specific)
/// - type_id=1: Street
/// - type_id=2: Neighborhood/City
/// - type_id=3: Region/State
/// - type_id=4: Country (least specific)
///
/// Lower type_ids (more specific) can stack with higher type_ids (less specific).
///
/// # Pruning Strategy
///
/// To limit memory usage, only the top 2000 leaf nodes by relevance are kept.
/// The ArenaManager tracks relevance bins and culls the lowest bin when full.
pub fn stackable<'a, T: Borrow<GridStore> + Clone + Debug>(
    phrasematches: &'a Vec<PhrasematchSubquery<T>>,
) -> StackableTree<'a, T> {
    let mut arena: ArenaManager<'a, T> = ArenaManager::new();

    // Step 1: Group phrasematches by type_id
    // Example: type_id=1 (streets) → ["Main", "Street"]
    //          type_id=2 (cities) → ["Seattle"]
    let mut binned_phrasematches: BTreeMap<u16, PhrasematchBin<'a, T>> = BTreeMap::new();
    for phrasematch in phrasematches {
        let bin = binned_phrasematches
            .entry(phrasematch.store.borrow().type_id)
            .or_insert(PhrasematchBin {
                phrasematches: Vec::new(),
                max_relev: OrderedFloat(0.0),
                max_relev_after_this: OrderedFloat(0.0),
            });
        if phrasematch.weight > *bin.max_relev {
            bin.max_relev = OrderedFloat(phrasematch.weight);
        }
        bin.phrasematches.push(phrasematch);
    }

    // Step 2: Sort each bin by relevance (descending) then idx (ascending)
    // This ensures we process high-relevance phrases first for better pruning
    let mut binned_phrasematches: Vec<_> = binned_phrasematches
        .into_iter()
        .map(|(_k, mut v)| {
            v.phrasematches
                .sort_by_key(|pm| (Reverse(OrderedFloat(pm.weight)), pm.idx));
            v
        })
        .collect();

    // Step 3: Calculate max_relev_after_this for pruning optimization
    // This allows early termination when remaining bins can't improve results
    // Example: If bins have max_relev [0.8, 0.6, 0.4], then:
    //   bin[0].max_relev_after_this = 0.6 + 0.4 = 1.0
    //   bin[1].max_relev_after_this = 0.4
    //   bin[2].max_relev_after_this = 0.0
    let mut sum_so_far = 0.0;
    for bin in binned_phrasematches.iter_mut().rev() {
        bin.max_relev_after_this = OrderedFloat(sum_so_far);
        sum_so_far = sum_so_far + *bin.max_relev;
    }

    // Step 4: Build the tree recursively starting from an empty root
    let root = binned_stackable(
        &binned_phrasematches,
        None,                                    // No phrasematch for root
        FixedBitSet::with_capacity(MAX_INDEXES), // Empty bmask
        0,                                       // Empty mask
        (MAX_INDEXES as u16) + 1,                // Invalid idx for root
        0.0,                                     // Zero relevance so far
        0,                                       // Zero zoom
        0,                                       // Start from first type bin
        &mut arena,
    );
    StackableTree { root, arena }
}

/// Recursively builds stackable tree nodes with pruning.
///
/// # Algorithm
///
/// For each node, tries to stack phrases from subsequent type bins:
/// 1. Check mask compatibility (different query tokens)
/// 2. Check bmask compatibility (not in non_overlapping_indexes)
/// 3. Calculate potential relevance with remaining bins
/// 4. Prune if can't beat current worst result
/// 5. Recursively build child nodes
/// 6. Add child to arena if it passes pruning
///
/// # Example
///
/// Given current node "Main" (type_id=1, mask=0b01, relev=0.5):
/// ```text
/// Try stacking with type_id=2 phrases:
///   "Seattle" (mask=0b10):
///     ✓ mask check: 0b01 & 0b10 = 0 (different tokens)
///     ✓ bmask check: Seattle not in Main's non_overlapping_indexes
///     ✓ relev check: 0.5 + 0.6 + 0.4 (remaining) = 1.5 > min_relev
///     → Recurse to build "Main + Seattle" node
///   
///   "Tacoma" (mask=0b10):
///     ✓ mask check: 0b01 & 0b10 = 0
///     ✓ bmask check: passes
///     ✗ relev check: 0.5 + 0.3 + 0.4 = 1.2 < min_relev (1.3)
///     → Skip (can't beat worst result)
/// ```
///
/// # Parameters
///
/// - `binned_phrasematches`: All phrases grouped by type_id
/// - `current_phrasematch`: The phrase this node represents (None for root)
/// - `bmask`: Bitmask of non-overlapping indexes used so far
/// - `mask`: Combined mask of all phrases in this branch
/// - `idx`: Index of current phrase
/// - `relev_so_far`: Accumulated relevance from root to this node
/// - `zoom`: Zoom level of current phrase's index
/// - `start_type_idx`: Which type bin to start checking (skip earlier types)
/// - `arena`: Memory manager for pruning low-relevance nodes
fn binned_stackable<'b, 'a: 'b, T: Borrow<GridStore> + Clone + Debug>(
    binned_phrasematches: &'b Vec<PhrasematchBin<'a, T>>,
    current_phrasematch: Option<&'a PhrasematchSubquery<T>>,
    bmask: FixedBitSet,
    mask: u32,
    idx: u16,
    relev_so_far: f64,
    zoom: u16,
    start_type_idx: usize,
    arena: &mut ArenaManager<'a, T>,
) -> StackableNode<'a, T> {
    // Create node for current phrase
    let mut node = StackableNode {
        phrasematch: current_phrasematch,
        children: vec![],
        mask,
        bmask,
        idx,
        max_relev: relev_so_far,
        zoom,
    };

    // Try stacking with phrases from subsequent type bins
    // We skip earlier type bins to avoid creating duplicate combinations
    // Example: If we're at type_id=1, only try stacking with type_id=2, 3, 4...
    for (type_idx, phrasematch_group) in
        binned_phrasematches.iter().enumerate().skip(start_type_idx)
    {
        for phrasematch in phrasematch_group.phrasematches.iter() {
            // Check 1: Mask compatibility
            // Masks must not overlap (different query tokens)
            // Example: "Main" (mask=0b01) can stack with "Seattle" (mask=0b10)
            //          but not with "Street" (mask=0b01) - same token position
            if (node.mask & phrasematch.mask) == 0
                && !phrasematch
                    .non_overlapping_indexes
                    .contains(node.idx as usize)
            {
                // Check 2: Calculate potential relevance
                // If we're at capacity, check if this branch could beat our worst result
                let target_relev = relev_so_far + phrasematch.weight;
                let max_possible_relev = target_relev + *phrasematch_group.max_relev_after_this;

                // Pruning optimization: Skip if this branch can't beat worst result
                // Example: If min_relev=1.5 and max_possible=1.3, skip entire branch
                if arena.is_full() && max_possible_relev < *arena.min_relev {
                    continue;
                }

                // Check 3: Build combined masks for child
                let target_mask = phrasematch.mask | node.mask;
                let mut target_bmask: FixedBitSet = node.bmask.clone();
                target_bmask.union_with(&phrasematch.non_overlapping_indexes);

                // Recursively build child node
                // This explores all valid combinations from this point
                let child_node = binned_stackable(
                    binned_phrasematches,
                    Some(phrasematch),
                    target_bmask,
                    target_mask,
                    phrasematch.idx,
                    target_relev,
                    phrasematch.store.borrow().zoom,
                    type_idx + 1, // Start from next type bin to avoid duplicates
                    arena,
                );

                let max_relev = child_node.max_relev;

                // Try to add child to arena (may be rejected if relevance too low)
                if let Some(arena_index) = arena.add(child_node) {
                    node.children.push(arena_index);

                    // Update this node's max_relev if child has higher relevance
                    if max_relev > node.max_relev {
                        node.max_relev = max_relev;
                    }
                }
            }
        }
    }
    node
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::storage::{
        global_bbox_for_zoom, GridEntry, GridKey, GridStoreBuilder, MatchKey, MatchKeyWithId,
        MatchPhrase,
    };

    #[test]
    fn simple_stackable_test() {
        let directory: tempfile::TempDir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(directory.path()).unwrap();

        let key = GridKey {
            phrase_id: 1,
            lang_set: 1,
        };

        let entries = vec![
            GridEntry {
                id: 2,
                x: 2,
                y: 2,
                relev: 0.8,
                score: 3,
                source_phrase_hash: 0,
            },
            GridEntry {
                id: 3,
                x: 3,
                y: 3,
                relev: 1.,
                score: 1,
                source_phrase_hash: 1,
            },
            GridEntry {
                id: 1,
                x: 1,
                y: 1,
                relev: 1.,
                score: 7,
                source_phrase_hash: 2,
            },
        ];
        builder
            .insert(&key, entries)
            .expect("Unable to insert record");
        builder.finish().unwrap();
        let store1 = GridStore::new_with_options(
            directory.path(),
            14,
            1,
            200.,
            global_bbox_for_zoom(14),
            1.0,
        )
        .unwrap();
        let store2 = GridStore::new_with_options(
            directory.path(),
            14,
            2,
            200.,
            global_bbox_for_zoom(14),
            1.0,
        )
        .unwrap();

        let a1 = PhrasematchSubquery {
            store: &store1,
            idx: 1,
            non_overlapping_indexes: FixedBitSet::with_capacity(MAX_INDEXES),
            weight: 0.5,
            match_keys: vec![MatchKeyWithId {
                key: MatchKey {
                    match_phrase: MatchPhrase::Range { start: 0, end: 1 },
                    lang_set: 0,
                },
                id: 0,
                ..MatchKeyWithId::default()
            }],
            mask: 2,
        };

        let b1 = PhrasematchSubquery {
            store: &store2,
            idx: 2,
            non_overlapping_indexes: FixedBitSet::with_capacity(MAX_INDEXES),
            weight: 0.5,
            match_keys: vec![MatchKeyWithId {
                key: MatchKey {
                    match_phrase: MatchPhrase::Range { start: 0, end: 1 },
                    lang_set: 0,
                },
                id: 1,
                ..MatchKeyWithId::default()
            }],
            mask: 1,
        };

        let b2 = PhrasematchSubquery {
            store: &store2,
            idx: 2,
            non_overlapping_indexes: FixedBitSet::with_capacity(MAX_INDEXES),
            weight: 0.5,
            match_keys: vec![MatchKeyWithId {
                key: MatchKey {
                    match_phrase: MatchPhrase::Range { start: 0, end: 1 },
                    lang_set: 0,
                },
                id: 2,
                ..MatchKeyWithId::default()
            }],
            mask: 1,
        };

        let phrasematch_results = vec![a1, b1, b2];

        let tree = stackable(&phrasematch_results);
        let a1_children_ids: Vec<u32> = tree
            .arena
            .get(tree.root.children[0])
            .unwrap()
            .children
            .iter()
            .map(|node_idx| {
                tree.arena
                    .get(*node_idx)
                    .unwrap()
                    .phrasematch
                    .as_ref()
                    .map(|p| p.match_keys[0].id)
                    .unwrap()
            })
            .collect();
        assert_eq!(vec![1, 2], a1_children_ids, "a1 can stack with b1 and b2");
        let b1_children_ids: Vec<u32> = tree
            .arena
            .get(tree.root.children[1])
            .unwrap()
            .children
            .iter()
            .map(|node_idx| {
                tree.arena
                    .get(*node_idx)
                    .unwrap()
                    .phrasematch
                    .as_ref()
                    .map(|p| p.match_keys[0].id)
                    .unwrap()
            })
            .collect();
        assert_eq!(
            0,
            b1_children_ids.len(),
            "b1 cannot stack with b2, same nmask"
        );
        let b2_children_ids: Vec<u32> = tree
            .arena
            .get(tree.root.children[2])
            .unwrap()
            .children
            .iter()
            .map(|node_idx| {
                tree.arena
                    .get(*node_idx)
                    .unwrap()
                    .phrasematch
                    .as_ref()
                    .map(|p| p.match_keys[0].id)
                    .unwrap()
            })
            .collect();
        assert_eq!(
            0,
            b2_children_ids.len(),
            "b2 cannot stack with b1, same nmask"
        );
    }

    #[test]
    fn bmask_stackable_test() {
        let directory: tempfile::TempDir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(directory.path()).unwrap();

        let key = GridKey {
            phrase_id: 1,
            lang_set: 1,
        };

        let entries = vec![
            GridEntry {
                id: 2,
                x: 2,
                y: 2,
                relev: 0.8,
                score: 3,
                source_phrase_hash: 0,
            },
            GridEntry {
                id: 3,
                x: 3,
                y: 3,
                relev: 1.,
                score: 1,
                source_phrase_hash: 1,
            },
            GridEntry {
                id: 1,
                x: 1,
                y: 1,
                relev: 1.,
                score: 7,
                source_phrase_hash: 2,
            },
        ];
        builder
            .insert(&key, entries)
            .expect("Unable to insert record");
        builder.finish().unwrap();
        let store = GridStore::new_with_options(
            directory.path(),
            14,
            1,
            200.,
            global_bbox_for_zoom(14),
            1.0,
        )
        .unwrap();
        let mut a1_bmask: FixedBitSet = FixedBitSet::with_capacity(MAX_INDEXES);
        a1_bmask.insert(0);
        a1_bmask.insert(1);
        let mut b1_bmask: FixedBitSet = FixedBitSet::with_capacity(MAX_INDEXES);
        b1_bmask.insert(1);
        b1_bmask.insert(0);

        let a1 = PhrasematchSubquery {
            store: &store,
            idx: 1,
            non_overlapping_indexes: FixedBitSet::with_capacity(MAX_INDEXES),
            weight: 0.5,
            match_keys: vec![MatchKeyWithId {
                key: MatchKey {
                    match_phrase: MatchPhrase::Range { start: 0, end: 1 },
                    lang_set: 0,
                },
                id: 0,
                ..MatchKeyWithId::default()
            }],
            mask: 1,
        };

        let b1 = PhrasematchSubquery {
            store: &store,
            idx: 1,
            non_overlapping_indexes: FixedBitSet::with_capacity(MAX_INDEXES),
            weight: 0.5,
            match_keys: vec![MatchKeyWithId {
                key: MatchKey {
                    match_phrase: MatchPhrase::Range { start: 0, end: 1 },
                    lang_set: 0,
                },
                id: 1,
                ..MatchKeyWithId::default()
            }],
            mask: 1,
        };
        let phrasematch_results = vec![a1, b1];
        let tree = stackable(&phrasematch_results);

        let bmask_stacks: Vec<bool> = bfs(tree).iter().map(|node| node.is_leaf()).collect();
        assert_eq!(bmask_stacks[1], true, "a1 cannot stack with b1 since a1's bmask contains the idx of b1 - so they don't have any children");
        assert_eq!(bmask_stacks[2], true, "b1 cannot stack with a1 since b1's bmask contains the idx of a1 - so they don't have any children");
    }

    #[test]
    fn mask_stackable_test() {
        let directory: tempfile::TempDir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(directory.path()).unwrap();

        let key = GridKey {
            phrase_id: 1,
            lang_set: 1,
        };

        let entries = vec![
            GridEntry {
                id: 2,
                x: 2,
                y: 2,
                relev: 0.8,
                score: 3,
                source_phrase_hash: 0,
            },
            GridEntry {
                id: 3,
                x: 3,
                y: 3,
                relev: 1.,
                score: 1,
                source_phrase_hash: 1,
            },
            GridEntry {
                id: 1,
                x: 1,
                y: 1,
                relev: 1.,
                score: 7,
                source_phrase_hash: 2,
            },
        ];
        builder
            .insert(&key, entries)
            .expect("Unable to insert record");
        builder.finish().unwrap();
        let store = GridStore::new_with_options(
            directory.path(),
            14,
            1,
            200.,
            global_bbox_for_zoom(14),
            1.0,
        )
        .unwrap();

        let a1 = PhrasematchSubquery {
            store: &store,
            idx: 1,
            non_overlapping_indexes: FixedBitSet::with_capacity(MAX_INDEXES),
            weight: 0.5,
            match_keys: vec![MatchKeyWithId {
                key: MatchKey {
                    match_phrase: MatchPhrase::Range { start: 0, end: 1 },
                    lang_set: 0,
                },
                id: 0,
                ..MatchKeyWithId::default()
            }],
            mask: 1,
        };

        let b1 = PhrasematchSubquery {
            store: &store,
            idx: 1,
            non_overlapping_indexes: FixedBitSet::with_capacity(MAX_INDEXES),
            weight: 0.5,
            match_keys: vec![MatchKeyWithId {
                key: MatchKey {
                    match_phrase: MatchPhrase::Range { start: 0, end: 1 },
                    lang_set: 0,
                },
                id: 1,
                ..MatchKeyWithId::default()
            }],
            mask: 1,
        };
        let phrasematch_results = vec![a1, b1];
        let tree = stackable(&phrasematch_results);
        let mask_stacks: Vec<bool> = bfs(tree).iter().map(|node| node.is_leaf()).collect();
        assert_eq!(mask_stacks[1], true, "a1 and b1 cannot stack since they have the same mask - so they don't have any children");
        assert_eq!(mask_stacks[2], true, "a1 and b1 cannot stack since they have the same mask - so they don't have any children");
    }

    #[test]
    fn binned_stackable_test() {
        let directory: tempfile::TempDir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(directory.path()).unwrap();

        let key = GridKey {
            phrase_id: 1,
            lang_set: 1,
        };

        let entries = vec![
            GridEntry {
                id: 2,
                x: 2,
                y: 2,
                relev: 0.8,
                score: 3,
                source_phrase_hash: 0,
            },
            GridEntry {
                id: 3,
                x: 3,
                y: 3,
                relev: 1.,
                score: 1,
                source_phrase_hash: 1,
            },
            GridEntry {
                id: 1,
                x: 1,
                y: 1,
                relev: 1.,
                score: 7,
                source_phrase_hash: 2,
            },
        ];
        builder
            .insert(&key, entries)
            .expect("Unable to insert record");
        builder.finish().unwrap();
        let store = GridStore::new_with_options(
            directory.path(),
            14,
            1,
            200.,
            global_bbox_for_zoom(14),
            1.0,
        )
        .unwrap();

        let a1 = PhrasematchSubquery {
            store: &store,
            idx: 1,
            non_overlapping_indexes: FixedBitSet::with_capacity(MAX_INDEXES),
            weight: 0.5,
            match_keys: vec![MatchKeyWithId {
                key: MatchKey {
                    match_phrase: MatchPhrase::Range { start: 0, end: 1 },
                    lang_set: 0,
                },
                id: 0,
                ..MatchKeyWithId::default()
            }],
            mask: 1,
        };

        let b1 = PhrasematchSubquery {
            store: &store,
            idx: 1,
            non_overlapping_indexes: FixedBitSet::with_capacity(MAX_INDEXES),
            weight: 0.5,
            match_keys: vec![MatchKeyWithId {
                key: MatchKey {
                    match_phrase: MatchPhrase::Range { start: 0, end: 1 },
                    lang_set: 0,
                },
                id: 1,
                ..MatchKeyWithId::default()
            }],
            mask: 1,
        };
        let phrasematch_results = vec![a1, b1];
        let tree = stackable(&phrasematch_results);
        println!("{:?}", tree);
    }
}
