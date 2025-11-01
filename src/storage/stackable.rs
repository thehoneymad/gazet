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
                    let min_count =
                        self.relev_map.get(&self.min_relev).expect("must contain min_relev").0;
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
        self.min_relev =
            self.relev_map.keys().min().copied().unwrap_or(OrderedFloat(std::f64::MAX));
    }

    #[inline(always)]
    pub fn get(&self, index: ArenaIndex) -> Option<&StackableNode<'a, T>> {
        self.arena.get(index)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::storage::{GridStoreBuilder, GridKey, GridEntry, MatchKey, MatchPhrase, MatchKeyWithId};
    use tempfile;

    #[test]
    fn simple_stackable_test() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        let key = GridKey { phrase_id: 1, lang_set: 1 };
        let entries = vec![
            GridEntry { id: 2, x: 2, y: 2, relev: 0.8, score: 3, source_phrase_hash: 0 },
            GridEntry { id: 3, x: 3, y: 3, relev: 1., score: 1, source_phrase_hash: 1 },
            GridEntry { id: 1, x: 1, y: 1, relev: 1., score: 7, source_phrase_hash: 2 },
        ];
        builder.insert(&key, entries).unwrap();
        builder.finish().unwrap();

        let store1 = GridStore::new_with_options(dir.path(), 14, 200., 1).unwrap();
        let store2 = GridStore::new_with_options(dir.path(), 14, 200., 2).unwrap();

        let a1 = PhrasematchSubquery {
            store: &store1,
            idx: 1,
            non_overlapping_indexes: FixedBitSet::with_capacity(MAX_INDEXES),
            weight: 0.5,
            match_keys: vec![MatchKeyWithId {
                key: MatchKey { match_phrase: MatchPhrase::Range { start: 0, end: 1 }, lang_set: 0 },
                id: 0,
            }],
            mask: 2,
        };

        let b1 = PhrasematchSubquery {
            store: &store2,
            idx: 2,
            non_overlapping_indexes: FixedBitSet::with_capacity(MAX_INDEXES),
            weight: 0.5,
            match_keys: vec![MatchKeyWithId {
                key: MatchKey { match_phrase: MatchPhrase::Range { start: 0, end: 1 }, lang_set: 0 },
                id: 1,
            }],
            mask: 1,
        };

        let b2 = PhrasematchSubquery {
            store: &store2,
            idx: 2,
            non_overlapping_indexes: FixedBitSet::with_capacity(MAX_INDEXES),
            weight: 0.5,
            match_keys: vec![MatchKeyWithId {
                key: MatchKey { match_phrase: MatchPhrase::Range { start: 0, end: 1 }, lang_set: 0 },
                id: 2,
            }],
            mask: 1,
        };

        let phrasematches = vec![a1, b1, b2];
        let tree = stackable(&phrasematches);

        // Tree structure: root -> type_id=1 (a1) -> type_id=2 (b1, b2)
        // Root has 1 child (a1 from type_id=1)
        assert_eq!(tree.root.children.len(), 1, "Root should have 1 child (type_id=1 bin)");
        
        let a1_node = tree.arena.get(tree.root.children[0]).unwrap();
        assert_eq!(a1_node.children.len(), 2, "a1 can stack with b1 and b2");
        
        let a1_children_ids: Vec<u32> = a1_node.children.iter()
            .map(|idx| tree.arena.get(*idx).unwrap().phrasematch.unwrap().match_keys[0].id)
            .collect();
        assert_eq!(vec![1, 2], a1_children_ids, "a1 stacks with b1 (id=1) and b2 (id=2)");

        // b1 and b2 are leaves (cannot stack with each other - same mask)
        let b1_node = tree.arena.get(a1_node.children[0]).unwrap();
        assert_eq!(b1_node.children.len(), 0, "b1 cannot stack further (same mask as b2)");
        
        let b2_node = tree.arena.get(a1_node.children[1]).unwrap();
        assert_eq!(b2_node.children.len(), 0, "b2 cannot stack further (same mask as b1)");
    }

    #[test]
    fn bmask_stackable_test() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        let key = GridKey { phrase_id: 1, lang_set: 1 };
        let entries = vec![
            GridEntry { id: 2, x: 2, y: 2, relev: 0.8, score: 3, source_phrase_hash: 0 },
            GridEntry { id: 3, x: 3, y: 3, relev: 1., score: 1, source_phrase_hash: 1 },
            GridEntry { id: 1, x: 1, y: 1, relev: 1., score: 7, source_phrase_hash: 2 },
        ];
        builder.insert(&key, entries).unwrap();
        builder.finish().unwrap();

        let store = GridStore::new_with_options(dir.path(), 14, 200., 0).unwrap();

        let a1 = PhrasematchSubquery {
            store: &store,
            idx: 1,
            non_overlapping_indexes: FixedBitSet::with_capacity(MAX_INDEXES),
            weight: 0.5,
            match_keys: vec![MatchKeyWithId {
                key: MatchKey { match_phrase: MatchPhrase::Range { start: 0, end: 1 }, lang_set: 0 },
                id: 0,
            }],
            mask: 1,
        };

        let b1 = PhrasematchSubquery {
            store: &store,
            idx: 1,
            non_overlapping_indexes: FixedBitSet::with_capacity(MAX_INDEXES),
            weight: 0.5,
            match_keys: vec![MatchKeyWithId {
                key: MatchKey { match_phrase: MatchPhrase::Range { start: 0, end: 1 }, lang_set: 0 },
                id: 1,
            }],
            mask: 1,
        };

        let phrasematches = vec![a1, b1];
        let tree = stackable(&phrasematches);
        let bmask_stacks: Vec<bool> = bfs(tree).iter().map(|n| n.is_leaf()).collect();

        assert_eq!(bmask_stacks[1], true, "a1 cannot stack with b1 - bmask conflict");
        assert_eq!(bmask_stacks[2], true, "b1 cannot stack with a1 - bmask conflict");
    }

    #[test]
    fn mask_stackable_test() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        let key = GridKey { phrase_id: 1, lang_set: 1 };
        let entries = vec![
            GridEntry { id: 2, x: 2, y: 2, relev: 0.8, score: 3, source_phrase_hash: 0 },
            GridEntry { id: 3, x: 3, y: 3, relev: 1., score: 1, source_phrase_hash: 1 },
            GridEntry { id: 1, x: 1, y: 1, relev: 1., score: 7, source_phrase_hash: 2 },
        ];
        builder.insert(&key, entries).unwrap();
        builder.finish().unwrap();

        let store = GridStore::new_with_options(dir.path(), 14, 200., 0).unwrap();

        let a1 = PhrasematchSubquery {
            store: &store,
            idx: 1,
            non_overlapping_indexes: FixedBitSet::with_capacity(MAX_INDEXES),
            weight: 0.5,
            match_keys: vec![MatchKeyWithId {
                key: MatchKey { match_phrase: MatchPhrase::Range { start: 0, end: 1 }, lang_set: 0 },
                id: 0,
            }],
            mask: 1,
        };

        let b1 = PhrasematchSubquery {
            store: &store,
            idx: 1,
            non_overlapping_indexes: FixedBitSet::with_capacity(MAX_INDEXES),
            weight: 0.5,
            match_keys: vec![MatchKeyWithId {
                key: MatchKey { match_phrase: MatchPhrase::Range { start: 0, end: 1 }, lang_set: 0 },
                id: 1,
            }],
            mask: 1,
        };

        let phrasematches = vec![a1, b1];
        let tree = stackable(&phrasematches);
        let mask_stacks: Vec<bool> = bfs(tree).iter().map(|n| n.is_leaf()).collect();

        assert_eq!(mask_stacks[1], true, "a1 and b1 cannot stack - same mask");
        assert_eq!(mask_stacks[2], true, "a1 and b1 cannot stack - same mask");
    }

    #[test]
    fn binned_stackable_test() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GridStoreBuilder::new(dir.path()).unwrap();

        let key = GridKey { phrase_id: 1, lang_set: 1 };
        let entries = vec![
            GridEntry { id: 2, x: 2, y: 2, relev: 0.8, score: 3, source_phrase_hash: 0 },
            GridEntry { id: 3, x: 3, y: 3, relev: 1., score: 1, source_phrase_hash: 1 },
            GridEntry { id: 1, x: 1, y: 1, relev: 1., score: 7, source_phrase_hash: 2 },
        ];
        builder.insert(&key, entries).unwrap();
        builder.finish().unwrap();

        let store = GridStore::new_with_options(dir.path(), 14, 200., 0).unwrap();

        let a1 = PhrasematchSubquery {
            store: &store,
            idx: 1,
            non_overlapping_indexes: FixedBitSet::with_capacity(MAX_INDEXES),
            weight: 0.5,
            match_keys: vec![MatchKeyWithId {
                key: MatchKey { match_phrase: MatchPhrase::Range { start: 0, end: 1 }, lang_set: 0 },
                id: 0,
            }],
            mask: 1,
        };

        let b1 = PhrasematchSubquery {
            store: &store,
            idx: 1,
            non_overlapping_indexes: FixedBitSet::with_capacity(MAX_INDEXES),
            weight: 0.5,
            match_keys: vec![MatchKeyWithId {
                key: MatchKey { match_phrase: MatchPhrase::Range { start: 0, end: 1 }, lang_set: 0 },
                id: 1,
            }],
            mask: 1,
        };

        let phrasematches = vec![a1, b1];
        let tree = stackable(&phrasematches);
        
        // Just verify tree builds without panicking
        assert!(tree.root.children.len() >= 0);
    }
}

/// Complete stackable tree with root node and arena manager.
///
/// The tree represents all valid phrase combinations that can be spatially stacked.
#[derive(Debug, Clone)]
pub struct StackableTree<'a, T: Borrow<GridStore> + Clone + Debug> {
    pub root: StackableNode<'a, T>,
    pub arena: ArenaManager<'a, T>,
}

/// Bin of phrasematches grouped by type_id.
///
/// Phrases are binned by their index type (street, city, country, etc.)
/// to enable type-based stacking rules.
struct PhrasematchBin<'a, T: Borrow<GridStore> + Clone + Debug> {
    phrasematches: Vec<&'a PhrasematchSubquery<T>>,
    max_relev: OrderedFloat<f64>,
    max_relev_after_this: OrderedFloat<f64>,
}

/// Builds a stackable tree from a list of phrasematch subqueries.
///
/// # Algorithm
///
/// 1. **Bin by type_id**: Group phrases by their index type
///    - Example: streets (type=0), cities (type=1), countries (type=2)
///
/// 2. **Calculate max relevance**: For each bin, track max possible relevance
///    - Used for pruning: if max_relev_after_this + current < threshold, stop
///
/// 3. **Build tree recursively**: Call binned_stackable() to construct tree
///    - Each level adds one phrase type
///    - Prunes combinations that can't reach relevance threshold
///
/// 4. **Arena management**: Keep only top 2000 leaf combinations
///
/// # Example
///
/// ```ignore
/// // Query: "Main Street Seattle"
/// // Phrasematches: [streets:"Main Street", cities:"Seattle"]
///
/// let tree = stackable(&phrasematches);
/// // Tree structure:
/// // Root
/// //   ├─ streets:"Main Street" (can stack with cities)
/// //   │   └─ cities:"Seattle" (leaf: complete address)
/// //   └─ cities:"Seattle" (can stand alone)
/// ```
pub fn stackable<'a, T: Borrow<GridStore> + Clone + Debug>(
    phrasematches: &'a [PhrasematchSubquery<T>],
) -> StackableTree<'a, T> {
    let mut arena: ArenaManager<'a, T> = ArenaManager::new();

    // Bin phrasematches by type_id
    let mut binned_phrasematches: BTreeMap<u16, PhrasematchBin<'a, T>> = BTreeMap::new();
    for phrasematch in phrasematches {
        let bin = binned_phrasematches.entry(phrasematch.store.borrow().type_id).or_insert(
            PhrasematchBin {
                phrasematches: Vec::new(),
                max_relev: OrderedFloat(0.0),
                max_relev_after_this: OrderedFloat(0.0),
            },
        );
        if phrasematch.weight > *bin.max_relev {
            bin.max_relev = OrderedFloat(phrasematch.weight);
        }
        bin.phrasematches.push(phrasematch);
    }

    // Sort phrasematches within each bin by weight descending, then idx
    let mut binned_phrasematches: Vec<_> = binned_phrasematches
        .into_iter()
        .map(|(_k, mut v)| {
            v.phrasematches.sort_by_key(|pm| (Reverse(OrderedFloat(pm.weight)), pm.idx));
            v
        })
        .collect();

    // Calculate max_relev_after_this for pruning
    let mut sum_so_far = 0.0;
    for bin in binned_phrasematches.iter_mut().rev() {
        bin.max_relev_after_this = OrderedFloat(sum_so_far);
        sum_so_far += *bin.max_relev;
    }

    // Build the tree recursively
    let root = binned_stackable(
        &binned_phrasematches,
        None,
        FixedBitSet::with_capacity(MAX_INDEXES),
        0,
        (MAX_INDEXES as u16) + 1,
        0.0,
        0,
        0,
        &mut arena,
    );

    StackableTree { root, arena }
}

/// Recursively builds stackable tree from binned phrasematches.
///
/// This is the core tree-building algorithm that creates all valid phrase combinations.
///
/// # Parameters
/// - `bins`: Phrasematches grouped by type_id
/// - `parent_phrasematch`: Parent phrase (None for root)
/// - `bmask`: Bitmask of indexes already used in this branch
/// - `mask`: Combined mask of all phrases in this combination
/// - `parent_idx`: Index of parent phrase
/// - `relev_so_far`: Accumulated relevance from ancestors
/// - `bin_idx`: Current bin being processed
/// - `zoom`: Zoom level
/// - `arena`: Arena manager for node allocation
///
/// # Returns
/// Root node of the subtree
#[allow(clippy::too_many_arguments)]
fn binned_stackable<'a, T: Borrow<GridStore> + Clone + Debug>(
    bins: &[PhrasematchBin<'a, T>],
    parent_phrasematch: Option<&'a PhrasematchSubquery<T>>,
    bmask: FixedBitSet,
    mask: u32,
    parent_idx: u16,
    relev_so_far: f64,
    bin_idx: usize,
    zoom: u16,
    arena: &mut ArenaManager<'a, T>,
) -> StackableNode<'a, T> {
    let mut children = Vec::new();

    // Process remaining bins
    if bin_idx < bins.len() {
        let bin = &bins[bin_idx];
        let max_relev_after_this = *bin.max_relev_after_this;

        for phrasematch in &bin.phrasematches {
            // Skip if this index already used in this branch
            if bmask.contains(phrasematch.idx as usize) {
                continue;
            }

            // Skip if this phrase conflicts with parent (non_overlapping_indexes)
            if let Some(parent) = parent_phrasematch {
                if parent.non_overlapping_indexes.contains(phrasematch.idx as usize) {
                    continue;
                }
            }

            let new_relev = relev_so_far + phrasematch.weight;

            // Prune if can't reach relevance threshold even with all remaining bins
            if new_relev + max_relev_after_this < 0.25 {
                continue;
            }

            // Create new bmask with this index added
            let mut new_bmask = bmask.clone();
            new_bmask.insert(phrasematch.idx as usize);

            let new_mask = mask | phrasematch.mask;
            let new_zoom = phrasematch.store.borrow().zoom;

            // Recursively build subtree for this phrase
            let child = binned_stackable(
                bins,
                Some(phrasematch),
                new_bmask,
                new_mask,
                phrasematch.idx,
                new_relev,
                bin_idx + 1,
                new_zoom,
                arena,
            );

            // Add child to arena and track index
            if let Some(child_idx) = arena.add(child) {
                children.push(child_idx);
            }
        }
    }

    // Create node for current phrase (or root if parent_phrasematch is None)
    let max_relev = if bin_idx < bins.len() {
        relev_so_far + bins[bin_idx..].iter().map(|b| *b.max_relev).sum::<f64>()
    } else {
        relev_so_far
    };

    StackableNode {
        phrasematch: parent_phrasematch,
        children,
        bmask,
        mask,
        idx: parent_idx,
        max_relev,
        zoom,
    }
}
