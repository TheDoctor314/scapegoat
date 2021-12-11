use core::borrow::Borrow;
use core::cmp::Ordering;
use core::fmt::{self, Debug};
use core::hash::{Hash, Hasher};
use core::iter::FromIterator;
use core::mem;
use core::ops::{Index, Sub};

use super::arena::Arena;
use super::error::SGErr;
use super::iter::{IntoIter, Iter, IterMut};
use super::node::{Node, NodeGetHelper, NodeRebuildHelper};
use super::node_dispatch::SmallNode;

use crate::{ALPHA_DENOM, ALPHA_NUM};

#[allow(unused_imports)] // micromath only used if `no_std`
use micromath::F32Ext;
use smallnum::SmallUnsigned;
use smallvec::{smallvec, SmallVec};

// TODO: document rationale in updated README for v2.0
pub(crate) type Idx = u16;

/// A memory-efficient, self-balancing binary search tree.
#[allow(clippy::upper_case_acronyms)] // TODO: Removal == breaking change, e.g. v2.0
#[derive(Clone)]
pub struct SGTree<K: Default, V: Default, const N: usize> {
    // Storage
    pub(crate) arena: Arena<K, V, Idx, N>,
    pub(crate) root_idx: Option<usize>, // TODO: rename to opt_root_idx

    // Query cache
    max_idx: usize,
    min_idx: usize,
    curr_size: usize,

    // Balance control
    alpha_num: f32,
    alpha_denom: f32,
    max_size: usize,
    rebal_cnt: usize,
}

impl<K: Ord + Default, V: Default, const N: usize> SGTree<K, V, N> {
    // Public API ------------------------------------------------------------------------------------------------------

    /// Makes a new, empty `SGTree`.
    pub fn new() -> Self {
        if N > (Idx::MAX as usize) {
            panic!("Max stack item capacity (0xffff) exceeded!");
        }

        SGTree {
            arena: Arena::<K, V, Idx, N>::new(),
            root_idx: None,
            max_idx: 0,
            min_idx: 0,
            curr_size: 0,
            alpha_num: ALPHA_NUM,
            alpha_denom: ALPHA_DENOM,
            max_size: 0,
            rebal_cnt: 0,
        }
    }

    /// The [original scapegoat tree paper's](https://people.csail.mit.edu/rivest/pubs/GR93.pdf) alpha, `a`, can be chosen in the range `0.5 <= a < 1.0`.
    /// `a` tunes how "aggressively" the data structure self-balances.
    /// It controls the trade-off between total rebuild time and maximum height guarantees.
    ///
    /// * As `a` approaches `0.5`, the tree will rebalance more often. Ths means slower insertions, but faster lookups and deletions.
    ///     * An `a` equal to `0.5` means a tree that always maintains a perfect balance (e.g."complete" binary tree, at all times).
    ///
    /// * As `a` approaches `1.0`, the tree will rebalance less often. This means quicker insertions, but slower lookups and deletions.
    ///     * If `a` reached `1.0`, it'd mean a tree that never rebalances.
    ///
    /// Returns `Err` if `0.5 <= alpha_num / alpha_denom < 1.0` isn't `true` (invalid `a`, out of range).
    ///
    /// # Examples
    ///
    /// ```
    /// use scapegoat::SGTree;
    ///
    /// let mut sgt: SGTree<isize, isize> = SGTree::new();
    ///
    /// // Set 2/3, e.g. `a = 0.666...` (it's default value).
    /// assert!(sgt.set_rebal_param(2.0, 3.0).is_ok());
    /// ```
    pub fn set_rebal_param(&mut self, alpha_num: f32, alpha_denom: f32) -> Result<(), SGErr> {
        let a = alpha_num / alpha_denom;
        match (0.5..1.0).contains(&a) {
            true => {
                self.alpha_num = alpha_num;
                self.alpha_denom = alpha_denom;
                Ok(())
            }
            false => Err(SGErr::RebalanceFactorOutOfRange),
        }
    }

    /// Get the current rebalance parameter, alpha, as a tuple of `(alpha_numerator, alpha_denominator)`.
    /// See [the corresponding setter method][SGTree::set_rebal_param] for more details.
    ///
    /// # Examples
    ///
    /// ```
    /// use scapegoat::SGTree;
    ///
    /// let mut sgt: SGTree<isize, isize> = SGTree::new();
    ///
    /// // Set 2/3, e.g. `a = 0.666...` (it's default value).
    /// assert!(sgt.set_rebal_param(2.0, 3.0).is_ok());
    ///
    /// // Get the currently set value
    /// assert_eq!(sgt.rebal_param(), (2.0, 3.0));
    /// ```
    pub fn rebal_param(&self) -> (f32, f32) {
        (self.alpha_num, self.alpha_denom)
    }

    /// `#![no_std]`: total capacity, e.g. maximum number of tree pairs.
    /// Attempting to insert pairs beyond capacity will panic, unless the `high_assurance` feature is enabled.
    ///
    /// If using `std`: fast capacity, e.g. number of tree pairs stored on the stack.
    /// Pairs inserted beyond capacity will be stored on the heap.
    pub fn capacity(&self) -> usize {
        self.arena.capacity()
    }

    /// Get the size of an individual node in this tree, in bytes.
    pub fn node_size(&self) -> usize {
        self.arena.node_size()
    }

    /// Moves all elements from `other` into `self`, leaving `other` empty.
    #[cfg(not(feature = "high_assurance"))]
    pub fn append(&mut self, other: &mut SGTree<K, V, N>)
    where
        K: Ord,
    {
        // Nothing to append!
        if other.is_empty() {
            return;
        }

        // Nothing to append to!
        if self.is_empty() {
            mem::swap(self, other);
            return;
        }

        // Rip elements directly out of other's arena and clear it
        for arena_idx in 0..other.arena.len() {
            if let Some(mut node) = other.arena.remove(arena_idx) {
                self.insert(node.take_key(), node.take_val());
            }
        }
        other.clear();
    }

    /// Attempts to move all elements from `other` into `self`, leaving `other` empty.
    #[cfg(feature = "high_assurance")]
    pub fn append(&mut self, other: &mut SGTree<K, V, N>) -> Result<(), SGErr> {
        // Nothing to append!
        if other.is_empty() {
            return Ok(());
        }

        // Nothing to append to!
        if self.is_empty() {
            mem::swap(self, other);
            return Ok(());
        }

        // Rip elements directly out of other's arena and clear it
        if (self.len() + other.len()) <= self.capacity() {
            for arena_idx in 0..other.arena.len() {
                if let Some(mut node) = other.arena.remove(arena_idx) {
                    self.insert(node.take_key(), node.take_val());
                }
            }
            other.clear();
        } else {
            // Preemptive - we haven't mutated `self` or `other`!
            // Caller can assume unchanged state.
            return Err(SGErr::StackCapacityExceeded);
        }

        Ok(())
    }

    /// Insert a key-value pair into the tree.
    /// If the tree did not have this key present, `None` is returned.
    /// If the tree did have this key present, the value is updated, the old value is returned,
    /// and the key is updated. This accommodates types that can be `==` without being identical.
    #[cfg(not(feature = "high_assurance"))]
    pub fn insert(&mut self, key: K, val: V) -> Option<V>
    where
        K: Ord,
    {
        self.priv_balancing_insert::<Idx>(key, val)
    }

    /// Insert a key-value pair into the tree.
    /// Returns `Err` if tree's stack capacity is full, else the `Ok` contains:
    /// * `None` if the tree did not have this key present.
    /// * The old value if the tree did have this key present (both the value and key are updated,
    /// this accommodates types that can be `==` without being identical).
    #[cfg(feature = "high_assurance")]
    pub fn insert(&mut self, key: K, val: V) -> Result<Option<V>, SGErr>
    where
        K: Ord,
    {
        match self.capacity() > self.len() {
            true => Ok(self.priv_balancing_insert::<Idx>(key, val)),
            false => Err(SGErr::StackCapacityExceeded),
        }
    }

    /// Gets an iterator over the entries of the tree, sorted by key.
    ///
    /// # Examples
    ///
    /// Basic usage:
    ///
    /// ```
    /// use scapegoat::SGTree;
    ///
    /// let mut tree = SGTree::new();
    /// tree.insert(3, "c");
    /// tree.insert(2, "b");
    /// tree.insert(1, "a");
    ///
    /// for (key, value) in tree.iter() {
    ///     println!("{}: {}", key, value);
    /// }
    ///
    /// let (first_key, first_value) = tree.iter().next().unwrap();
    /// assert_eq!((*first_key, *first_value), (1, "a"));
    /// ```
    pub fn iter(&self) -> Iter<'_, K, V, N> {
        Iter::new(self)
    }

    /// Gets a mutable iterator over the entries of the tree, sorted by key.
    ///
    /// # Examples
    ///
    /// Basic usage:
    ///
    /// ```
    /// use scapegoat::SGTree;
    ///
    /// let mut tree = SGTree::new();
    /// tree.insert("a", 1);
    /// tree.insert("b", 2);
    /// tree.insert("c", 3);
    ///
    /// // Add 10 to the value if the key isn't "a"
    /// for (key, value) in tree.iter_mut() {
    ///     if key != &"a" {
    ///         *value += 10;
    ///     }
    /// }
    ///
    /// let (second_key, second_value) = tree.iter().skip(1).next().unwrap();
    /// assert_eq!((*second_key, *second_value), ("b", 12));
    /// ```
    pub fn iter_mut(&mut self) -> IterMut<'_, K, V, N> {
        IterMut::new(self)
    }

    /// Removes a key from the tree, returning the stored key and value if the key was previously in the tree.
    ///
    /// The key may be any borrowed form of the map’s key type, but the ordering
    /// on the borrowed form must match the ordering on the key type.
    pub fn remove_entry<Q>(&mut self, key: &Q) -> Option<(K, V)>
    where
        K: Borrow<Q> + Ord,
        Q: Ord + ?Sized,
    {
        match self.priv_remove_by_key(key) {
            Some((key, val)) => {
                if self.max_size > (2 * self.curr_size) {
                    if let Some(root_idx) = self.root_idx {
                        self.rebuild::<Idx>(root_idx);
                        self.max_size = self.curr_size;
                    }
                }
                Some((key, val))
            }
            None => None,
        }
    }

    /// Removes a key from the tree, returning the value at the key if the key was previously in the tree.
    ///
    /// The key may be any borrowed form of the map’s key type, but the ordering
    /// on the borrowed form must match the ordering on the key type.
    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q> + Ord,
        Q: Ord + ?Sized,
    {
        self.remove_entry(key).map(|(_, v)| v)
    }

    /// Retains only the elements specified by the predicate.
    pub fn retain<F>(&mut self, mut f: F)
    where
        F: FnMut(&K, &mut V) -> bool,
        K: Ord,
    {
        self.priv_drain_filter(|k, v| !f(k, v));
    }

    /// Splits the collection into two at the given key. Returns everything after the given key, including the key.
    pub fn split_off<Q>(&mut self, key: &Q) -> Self
    where
        K: Borrow<Q> + Ord,
        Q: Ord + ?Sized,
    {
        self.priv_drain_filter(|k, _| k >= key)
    }

    /// Returns the key-value pair corresponding to the given key.
    ///
    /// The supplied key may be any borrowed form of the map’s key type,
    /// but the ordering on the borrowed form must match the ordering on the key type.
    pub fn get_key_value<Q>(&self, key: &Q) -> Option<(&K, &V)>
    where
        K: Borrow<Q> + Ord,
        Q: Ord + ?Sized,
    {
        let ngh: NodeGetHelper<Idx> = self.priv_get(None, key);
        match ngh.node_idx() {
            Some(idx) => {
                let node = &self.arena[idx];
                Some((node.key(), node.val()))
            }
            None => None,
        }
    }

    /// Returns a reference to the value corresponding to the given key.
    ///
    /// The key may be any borrowed form of the map’s key type, but the ordering
    /// on the borrowed form must match the ordering on the key type.
    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q> + Ord,
        Q: Ord + ?Sized,
    {
        self.get_key_value(key).map(|(_, v)| v)
    }

    /// Get mutable reference corresponding to key.
    ///
    /// The key may be any borrowed form of the map’s key type,
    /// but the ordering on the borrowed form must match the ordering on the key type.
    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q> + Ord,
        Q: Ord + ?Sized,
    {
        let ngh: NodeGetHelper<Idx> = self.priv_get(None, key);
        match ngh.node_idx() {
            Some(idx) => {
                let (_, val) = self.arena[idx].get_mut();
                Some(val)
            }
            None => None,
        }
    }

    /// Clears the tree, removing all elements.
    pub fn clear(&mut self) {
        if !self.is_empty() {
            let rebal_cnt = self.rebal_cnt;
            *self = SGTree::new();
            self.rebal_cnt = rebal_cnt;
        }
    }

    /// Returns `true` if the tree contains a value for the given key.
    ///
    /// The key may be any borrowed form of the map’s key type, but the
    /// ordering on the borrowed form must match the ordering on the key type.
    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q> + Ord,
        Q: Ord + ?Sized,
    {
        self.get(key).is_some()
    }

    /// Returns `true` if the tree contains no elements.
    pub fn is_empty(&self) -> bool {
        self.root_idx.is_none()
    }

    /// Returns a reference to the first key-value pair in the tree.
    /// The key in this pair is the minimum key in the tree.
    pub fn first_key_value(&self) -> Option<(&K, &V)>
    where
        K: Ord,
    {
        if self.len() > 0 {
            let node = &self.arena[self.min_idx];
            Some((node.key(), node.val()))
        } else {
            None
        }
    }

    /// Returns a reference to the first/minium key in the tree, if any.
    pub fn first_key(&self) -> Option<&K>
    where
        K: Ord,
    {
        self.first_key_value().map(|(k, _)| k)
    }

    /// Removes and returns the first element in the tree.
    /// The key of this element is the minimum key that was in the tree.
    pub fn pop_first(&mut self) -> Option<(K, V)>
    where
        K: Ord,
    {
        self.priv_remove_by_idx(self.min_idx)
    }

    /// Returns a reference to the last key-value pair in the tree.
    /// The key in this pair is the maximum key in the tree.
    pub fn last_key_value(&self) -> Option<(&K, &V)>
    where
        K: Ord,
    {
        if self.len() > 0 {
            let node = &self.arena[self.max_idx];
            Some((node.key(), node.val()))
        } else {
            None
        }
    }

    /// Returns a reference to the last/maximum key in the tree, if any.
    pub fn last_key(&self) -> Option<&K>
    where
        K: Ord,
    {
        self.last_key_value().map(|(k, _)| k)
    }

    /// Removes and returns the last element in the tree.
    /// The key of this element is the maximum key that was in the tree.
    pub fn pop_last(&mut self) -> Option<(K, V)>
    where
        K: Ord,
    {
        self.priv_remove_by_idx(self.max_idx)
    }

    /// Returns the number of elements in the tree.
    pub fn len(&self) -> usize {
        self.curr_size
    }

    /// Get the number of times this tree rebalanced itself (for testing and/or performance engineering).
    /// This count will wrap if `usize::MAX` is exceeded.
    pub fn rebal_cnt(&self) -> usize {
        self.rebal_cnt
    }

    // Crate-internal API ----------------------------------------------------------------------------------------------

    // Remove a node by index.
    // A wrapper for by-key removal, traversal is still required to determine node parent.
    #[cfg(not(feature = "fast_rebalance"))]
    pub(crate) fn priv_remove_by_idx(&mut self, idx: usize) -> Option<(K, V)> {
        if (0..self.arena.len()).contains(&idx) {
            let node = &self.arena[idx];
            let ngh: NodeGetHelper<Idx> = self.priv_get(None, node.key());
            debug_assert!(
                ngh.node_idx().unwrap() == idx,
                "By-key retrieval index doesn't match arena storage index!"
            );
            self.priv_remove(None, ngh)
        } else {
            None
        }
    }

    // Remove a node by index.
    // A wrapper for by-key removal, traversal is still required to determine node parent.
    #[cfg(feature = "fast_rebalance")]
    pub(crate) fn priv_remove_by_idx(&mut self, idx: usize) -> Option<(K, V)> {
        if (0..self.arena.len()).contains(&idx) {
            let node = &self.arena[idx];
            let mut path = Arena::new_idx_vec();
            let ngh = self.priv_get(Some(&mut path), node.key());
            debug_assert!(
                ngh.node_idx().unwrap() == idx,
                "By-key retrieval index doesn't match arena storage index!"
            );
            self.priv_remove(Some(&path), ngh)
        } else {
            None
        }
    }

    // Flatten subtree into array of node indexes sorted by node key
    pub(crate) fn flatten_subtree_to_sorted_idxs<U: SmallUnsigned + Copy>(
        &self,
        idx: usize,
    ) -> SmallVec<[U; N]> {
        let mut subtree_node_idx_pairs: SmallVec<[(&Node<K, V, Idx>, U); N]> =
            smallvec![(&self.arena[idx], U::checked_from(idx))];
        let mut subtree_worklist: SmallVec<[&Node<K, V, Idx>; N]> = smallvec![&self.arena[idx]];

        while let Some(node) = subtree_worklist.pop() {
            if let Some(left_idx) = node.left_idx() {
                let left_child_node = &self.arena[left_idx];
                subtree_node_idx_pairs.push((left_child_node, U::checked_from(left_idx)));
                subtree_worklist.push(left_child_node);
            }

            if let Some(right_idx) = node.right_idx() {
                let right_child_node = &self.arena[right_idx];
                subtree_node_idx_pairs.push((right_child_node, U::checked_from(right_idx)));
                subtree_worklist.push(right_child_node);
            }
        }

        // Sort by SmallNode key
        // Faster than sort_by() but may not preserve order of equal elements - OK b/c tree won't have equal nodes
        subtree_node_idx_pairs.sort_unstable_by(|a, b| a.0.key().cmp(&b.0.key()));

        subtree_node_idx_pairs.iter().map(|(_, idx)| *idx).collect()
    }

    /// Sort the internal arena such that logically contiguous nodes are in-order (by key).
    pub(crate) fn sort_arena(&mut self) {
        if let Some(root_idx) = self.root_idx {
            let mut sort_metadata = self
                .arena
                .iter()
                .filter(|n| n.is_some())
                .map(|n| n.as_ref().unwrap())
                .map(|n| self.priv_get(None, &n.key()))
                .collect::<SmallVec<[NodeGetHelper<usize>; N]>>();

            sort_metadata.sort_by_key(|ngh| self.arena[ngh.node_idx().unwrap()].key());
            let sorted_root_idx = self.arena.sort(root_idx, sort_metadata);

            self.root_idx = Some(sorted_root_idx);
            self.update_max_idx();
            self.update_min_idx();
        }
    }

    // Private API -----------------------------------------------------------------------------------------------------

    // Iterative search. If key found, returns node idx, parent idx, and a bool indicating if node is right child
    // `opt_path` is only populated if `Some` and key is found.
    fn priv_get<Q, U: SmallUnsigned + Copy>(
        &self,
        mut opt_path: Option<&mut SmallVec<[U; N]>>,
        key: &Q,
    ) -> NodeGetHelper<U>
    where
        K: Borrow<Q> + Ord,
        Q: Ord + ?Sized,
    {
        match self.root_idx {
            Some(root_idx) => {
                let mut opt_parent_idx = None;
                let mut curr_idx = root_idx;
                let mut is_right_child = false;
                loop {
                    let node = &self.arena[curr_idx];

                    if let Some(ref mut path) = opt_path {
                        path.push(U::checked_from(curr_idx));
                    }

                    match key.cmp(node.key().borrow()) {
                        Ordering::Less => match node.left_idx() {
                            Some(lt_idx) => {
                                opt_parent_idx = Some(curr_idx);
                                curr_idx = lt_idx;
                                is_right_child = false;
                            }
                            None => {
                                if let Some(path) = opt_path {
                                    path.clear(); // Find failed, clear path
                                }

                                return NodeGetHelper::new(None, None, false);
                            }
                        },
                        Ordering::Equal => {
                            if let Some(path) = opt_path {
                                path.pop(); // Only parents in path
                            }

                            return NodeGetHelper::new(
                                Some(curr_idx),
                                opt_parent_idx,
                                is_right_child,
                            );
                        }
                        Ordering::Greater => match node.right_idx() {
                            Some(gt_idx) => {
                                opt_parent_idx = Some(curr_idx);
                                curr_idx = gt_idx;
                                is_right_child = true;
                            }
                            None => {
                                if let Some(path) = opt_path {
                                    path.clear(); // Find failed, clear path
                                }

                                return NodeGetHelper::new(None, None, false);
                            }
                        },
                    }
                }
            }
            None => NodeGetHelper::new(None, None, false),
        }
    }

    // Sorted insert of node into the tree (outer).
    // Re-balances the tree if necessary.
    fn priv_balancing_insert<U: Default + Copy + Ord + Sub + SmallUnsigned>(
        &mut self,
        key: K,
        val: V,
    ) -> Option<V> {
        let mut path: SmallVec<[U; N]> = Arena::<K, V, U, N>::new_idx_vec();
        let opt_val = self.priv_insert(&mut path, key, val);

        #[cfg(feature = "fast_rebalance")]
        {
            // Update subtree sizes
            for parent_idx in &path {
                let parent_node = self.arena[*parent_idx];
                parent_node.subtree_size += 1;
            }
        }

        // Potential rebalance
        if path.len() > self.alpha_balance_depth(self.max_size) {
            if let Some(scapegoat_idx) = self.find_scapegoat(&path) {
                self.rebuild::<U>(scapegoat_idx);
            }
        }

        opt_val
    }

    // Sorted insert of node into the tree (inner).
    // Maintains a traversal path to avoid nodes needing to maintain a parent index.
    // If a node with the same key existed, overwrites both that nodes key and value with the new one's and returns the old value.
    fn priv_insert<U: SmallUnsigned + Copy>(
        &mut self,
        path: &mut SmallVec<[U; N]>,
        key: K,
        val: V,
    ) -> Option<V> {
        match self.root_idx {
            // Sorted insert
            Some(idx) => {
                // Iterative traversal
                let mut curr_idx = idx;
                let mut opt_val = None;
                let ngh: NodeGetHelper<U>;
                loop {
                    let curr_node = &mut self.arena[curr_idx];
                    path.push(U::checked_from(curr_idx));

                    match key.cmp(&curr_node.key()) {
                        Ordering::Less => {
                            match curr_node.left_idx() {
                                Some(left_idx) => curr_idx = left_idx,
                                None => {
                                    // New min check
                                    let mut new_min_found = false;
                                    let min_node = &self.arena[self.min_idx];
                                    if &key < min_node.key() {
                                        new_min_found = true;
                                    }

                                    // Left insert
                                    let new_node_idx = self.arena.add(key, val);

                                    // New min update
                                    if new_min_found {
                                        self.min_idx = new_node_idx;
                                    }

                                    ngh = NodeGetHelper::new(
                                        Some(new_node_idx),
                                        Some(curr_idx),
                                        false,
                                    );
                                    break;
                                }
                            }
                        }
                        Ordering::Equal => {
                            // Replacing key necessary b/c custom Eq impl may not consider all K's fields
                            curr_node.set_key(key);

                            // Replacing val necessary b/c it may be different
                            opt_val = Some(curr_node.take_val());
                            curr_node.set_val(val);

                            // Key/val updated "in-place": no need to update `curr_node`'s parent or children
                            ngh = NodeGetHelper::new(None, None, false);
                            break;
                        }
                        Ordering::Greater => {
                            match curr_node.right_idx() {
                                Some(right_idx) => curr_idx = right_idx,
                                None => {
                                    // New max check
                                    let mut new_max_found = false;
                                    let max_node = &self.arena[self.max_idx];
                                    if &key > max_node.key() {
                                        new_max_found = true;
                                    }

                                    // Right insert
                                    let new_node_idx = self.arena.add(key, val);

                                    // New max update
                                    if new_max_found {
                                        self.max_idx = new_node_idx;
                                    }

                                    ngh = NodeGetHelper::new(
                                        Some(new_node_idx),
                                        Some(curr_idx),
                                        true,
                                    );
                                    break;
                                }
                            }
                        }
                    }
                }

                // Link to parent
                if let Some(parent_idx) = ngh.parent_idx() {
                    self.curr_size += 1;
                    self.max_size += 1;

                    let parent_node = &mut self.arena[parent_idx];
                    if ngh.is_right_child() {
                        parent_node.set_right_idx(ngh.node_idx());
                    } else {
                        parent_node.set_left_idx(ngh.node_idx());
                    }
                }

                // Return old value if overwritten
                opt_val
            }

            // Empty tree
            None => {
                debug_assert_eq!(self.curr_size, 0);
                self.curr_size += 1;
                self.max_size += 1;

                let root_idx = self.arena.add(key, val);
                self.root_idx = Some(root_idx);
                self.max_idx = root_idx;
                self.min_idx = root_idx;

                None
            }
        }
    }

    // Remove a node by key.
    #[cfg(not(feature = "fast_rebalance"))]
    fn priv_remove_by_key<Q>(&mut self, key: &Q) -> Option<(K, V)>
    where
        K: Borrow<Q> + Ord,
        Q: Ord + ?Sized,
    {
        let ngh: NodeGetHelper<Idx> = self.priv_get(None, key);
        self.priv_remove(None, ngh)
    }

    // Remove a node by key.
    #[cfg(feature = "fast_rebalance")]
    fn priv_remove_by_key<Q>(&mut self, key: &Q) -> Option<(K, V)>
    where
        K: Borrow<Q> + Ord,
        Q: Ord + ?Sized,
    {
        let mut path = Arena::new_idx_vec();
        let ngh = self.priv_get(Some(&mut path), key);
        self.priv_remove(Some(&path), ngh)
    }

    // Remove a node from the tree, re-linking remaining nodes as necessary.
    #[allow(unused_variables)] // `opt_path` only used when feature `fast_rebalance` is enabled
    fn priv_remove<U: SmallUnsigned + Copy>(
        &mut self,
        opt_path: Option<&SmallVec<[U; N]>>,
        ngh: NodeGetHelper<U>,
    ) -> Option<(K, V)> {
        match ngh.node_idx() {
            Some(node_idx) => {
                let node_to_remove = &self.arena[node_idx];

                // Copy out child indexes to reduce scope of above immutable borrow
                let node_to_remove_left_idx = node_to_remove.left_idx();
                let mut node_to_remove_right_idx = node_to_remove.right_idx();

                let new_child = match (node_to_remove_left_idx, node_to_remove_right_idx) {
                    // No children
                    (None, None) => None,
                    // Left child only
                    (Some(left_idx), None) => Some(left_idx),
                    // Right child only
                    (None, Some(right_idx)) => Some(right_idx),
                    // Zero-copy algorithm for removal of node with two children:
                    // 1. Iterative search for min node in right subtree
                    // 2. Unlink min node from it's parent (has either no children or a right child)
                    // 3. Re-link min node to removed node's children
                    (Some(_), Some(right_idx)) => {
                        let mut min_idx = right_idx;
                        let mut min_parent_idx = node_idx;

                        #[cfg(feature = "fast_rebalance")]
                        let min_node_subtree_size = node_to_remove.subtree_size - 1;

                        loop {
                            let min_node = &self.arena[min_idx];
                            match min_node.left_idx() {
                                // Continue search for min node
                                Some(lt_idx) => {
                                    min_parent_idx = min_idx;
                                    min_idx = lt_idx;
                                }
                                // Min node found, unlink it
                                None => match min_node.right_idx() {
                                    Some(_) => {
                                        let unlink_new_child = min_node.right_idx();
                                        if min_parent_idx == node_idx {
                                            node_to_remove_right_idx = unlink_new_child;
                                        } else {
                                            let min_parent_node = &mut self.arena[min_parent_idx];
                                            min_parent_node.set_left_idx(unlink_new_child);

                                            #[cfg(feature = "fast_rebalance")]
                                            {
                                                min_parent_node.subtree_size -= 1;
                                            }
                                        }
                                        break;
                                    }
                                    None => {
                                        if min_parent_idx == node_idx {
                                            node_to_remove_right_idx = None;
                                        } else {
                                            let min_parent_node = &mut self.arena[min_parent_idx];
                                            min_parent_node.set_left_idx(None);

                                            #[cfg(feature = "fast_rebalance")]
                                            {
                                                min_parent_node.subtree_size -= 1;
                                            }
                                        }
                                        break;
                                    }
                                },
                            }
                        }

                        // Re-link min node to removed node's children
                        let min_node = &mut self.arena[min_idx];
                        min_node.set_right_idx(node_to_remove_right_idx);
                        min_node.set_left_idx(node_to_remove_left_idx);

                        #[cfg(feature = "fast_rebalance")]
                        {
                            min_node.subtree_size = min_node_subtree_size;
                        }

                        // Return as new child
                        Some(min_idx)
                    }
                };

                // Update parent or root
                match ngh.parent_idx() {
                    Some(parent_idx) => {
                        let parent_node = &mut self.arena[parent_idx];
                        if ngh.is_right_child() {
                            parent_node.set_right_idx(new_child);
                        } else {
                            parent_node.set_left_idx(new_child);
                        }
                    }
                    None => {
                        self.root_idx = new_child;
                    }
                }

                // Perform removal
                let mut removed_node = self.arena.hard_remove(node_idx);
                self.curr_size -= 1;

                // Update min/max
                if node_idx == self.min_idx {
                    self.update_min_idx();
                } else if node_idx == self.max_idx {
                    self.update_max_idx();
                }

                // Update subtree sizes
                #[cfg(feature = "fast_rebalance")]
                {
                    debug_assert!(opt_path.is_some());
                    if let Some(path) = opt_path {
                        for parent_idx in path {
                            let parent_node = self.arena[*parent_idx];
                            debug_assert!(parent_node.subtree_size > 1);
                            parent_node.subtree_size -= 1;
                        }
                    }
                }

                Some((removed_node.take_key(), removed_node.take_val()))
            }
            None => None,
        }
    }

    /// Temporary internal drain_filter() implementation. To be replaced/supplemented with a public implementation.
    fn priv_drain_filter<Q, F>(&mut self, mut pred: F) -> Self
    where
        K: Borrow<Q> + Ord,
        Q: Ord + ?Sized,
        F: FnMut(&Q, &mut V) -> bool,
    {
        /*
        // TODO: make public version with this signature
        pub fn drain_filter<F>(&mut self, pred: F) -> DrainFilter<'_, K, V, F>
        where
            K: Ord,
            F: FnMut(&K, &mut V) -> bool,
        {
        */

        // TODO: this implementation is rather inefficient!

        // Note: this uses `usize` as a `U` stand-in to encapsulate `U` away for public APIs

        let mut key_idxs = Arena::<K, V, usize, N>::new_idx_vec();
        let mut remove_idxs = Arena::<K, V, usize, N>::new_idx_vec();

        // Below iter_mut() will want to sort, require want consistent indexes, so do work up front
        self.sort_arena();

        // Safely treat mutable ref as immutable, init list of node's arena indexes
        for (k, _) in &(*self) {
            let ngh: NodeGetHelper<Idx> = self.priv_get(None, k.borrow());
            debug_assert!(ngh.node_idx().is_some());
            key_idxs.push(ngh.node_idx().unwrap());
        }

        // Filter arena index list to those not matching predicate
        for (i, (k, v)) in self.iter_mut().enumerate() {
            if pred(k.borrow(), v) {
                remove_idxs.push(key_idxs[i]);
            }
        }

        // Drain non-matches
        let mut drained_sgt = Self::new();
        for i in remove_idxs {
            if let Some((key, val)) = self.priv_remove_by_idx(i) {
                #[cfg(not(feature = "high_assurance"))]
                {
                    drained_sgt.insert(key, val);
                }
                #[cfg(feature = "high_assurance")]
                {
                    assert!(drained_sgt.insert(node.key(), node.val()).is_ok());
                }
            }
        }

        drained_sgt
    }

    /// Minimum update without recursion
    fn update_min_idx(&mut self) {
        match self.root_idx {
            Some(root_idx) => {
                let mut curr_idx = root_idx;
                loop {
                    let node = &self.arena[curr_idx];
                    match node.left_idx() {
                        Some(lt_idx) => curr_idx = lt_idx,
                        None => {
                            self.min_idx = curr_idx;
                            return;
                        }
                    }
                }
            }
            None => self.min_idx = 0,
        }
    }

    /// Maximum update without recursion
    fn update_max_idx(&mut self) {
        match self.root_idx {
            Some(root_idx) => {
                let mut curr_idx = root_idx;
                loop {
                    let node = &self.arena[curr_idx];
                    match node.right_idx() {
                        Some(gt_idx) => curr_idx = gt_idx,
                        None => {
                            self.max_idx = curr_idx;
                            return;
                        }
                    }
                }
            }
            None => self.max_idx = 0,
        }
    }

    // Traverse upward, using path information, to find first unbalanced parent.
    // Uses the algorithm proposed in the original paper (Galperin and Rivest, 1993).
    #[cfg(not(feature = "alt_impl"))]
    fn find_scapegoat<U: SmallUnsigned>(&self, path: &[U]) -> Option<usize> {
        if path.len() <= 1 {
            return None;
        }

        let mut node_subtree_size = 1; // Newly inserted
        let mut parent_path_idx = path.len() - 1; // Parent of newly inserted
        let mut parent_subtree_size = self.get_subtree_size::<U>(path[parent_path_idx].usize());

        while (parent_path_idx > 0)
            && (self.alpha_denom * node_subtree_size as f32)
                <= (self.alpha_num * parent_subtree_size as f32)
        {
            node_subtree_size = parent_subtree_size;
            parent_path_idx -= 1;
            parent_subtree_size = self.get_subtree_size_differential::<U>(
                path[parent_path_idx].usize(),     // Parent index
                path[parent_path_idx + 1].usize(), // Child index
                node_subtree_size,                 // Child subtree size
            );

            debug_assert!(parent_subtree_size > node_subtree_size);
        }

        Some(path[parent_path_idx].usize())
    }

    // Traverse upward, using path information, to find first unbalanced parent.
    // Uses an alternate algorithm proposed in Galperin's PhD thesis (1996).
    #[cfg(feature = "alt_impl")]
    fn find_scapegoat<U: SmallUnsigned>(&self, path: &[U]) -> Option<usize> {
        if path.len() <= 1 {
            return None;
        }

        let mut i = 0;
        let mut node_subtree_size = 1; // Newly inserted
        let mut parent_path_idx = path.len() - 1; // Parent of newly inserted
        let mut parent_subtree_size = self.get_subtree_size::<U>(path[parent_path_idx].usize());

        while (parent_path_idx > 0) && (i <= self.alpha_balance_depth(node_subtree_size)) {
            node_subtree_size = parent_subtree_size;
            parent_path_idx -= 1;
            i += 1;
            parent_subtree_size = self.get_subtree_size_differential::<U>(
                path[parent_path_idx].usize(),     // Parent index
                path[parent_path_idx + 1].usize(), // Child index
                node_subtree_size,                 // Child subtree size
            );

            debug_assert!(parent_subtree_size > node_subtree_size);
        }

        Some(path[parent_path_idx].usize())
    }

    // Iterative subtree size computation
    #[cfg(not(feature = "fast_rebalance"))]
    fn get_subtree_size<U: SmallUnsigned>(&self, idx: usize) -> usize {
        let mut subtree_worklist: SmallVec<[&Node<K, V, Idx>; N]> = smallvec![&self.arena[idx]];
        let mut subtree_size = 0;

        while let Some(node) = subtree_worklist.pop() {
            subtree_size += 1;

            if let Some(left_idx) = node.left_idx() {
                subtree_worklist.push(&self.arena[left_idx]);
            }

            if let Some(right_idx) = node.right_idx() {
                subtree_worklist.push(&self.arena[right_idx]);
            }
        }

        subtree_size
    }

    // Retrieve cached subtree size
    #[cfg(feature = "fast_rebalance")]
    fn get_subtree_size<U: SmallUnsigned>(&self, idx: usize) -> usize {
        self.arena[idx].subtree_size
    }

    // Differential subtree size helper
    #[cfg(not(feature = "fast_rebalance"))]
    fn get_subtree_size_differential<U: SmallUnsigned>(
        &self,
        parent_idx: usize,
        child_idx: usize,
        child_subtree_size: usize,
    ) -> usize {
        let parent = &self.arena[parent_idx];

        debug_assert!(
            (parent.right_idx() == Some(child_idx)) || (parent.left_idx() == Some(child_idx))
        );

        let mut is_right_child = false;
        if let Some(right_child_idx) = parent.right_idx() {
            if right_child_idx == child_idx {
                is_right_child = true;
            }
        }

        let other_child_subtree_size = if is_right_child {
            match parent.left_idx() {
                Some(idx) => self.get_subtree_size::<U>(idx),
                None => 0,
            }
        } else {
            match parent.right_idx() {
                Some(idx) => self.get_subtree_size::<U>(idx),
                None => 0,
            }
        };

        let computed_subtree_size = child_subtree_size + other_child_subtree_size + 1;

        debug_assert_eq!(
            computed_subtree_size,
            self.get_subtree_size::<U>(parent_idx)
        );

        computed_subtree_size
    }

    // Subtree size helper
    // Size already cached if `fast_rebalance` is enabled, no need for differential logic
    #[cfg(feature = "fast_rebalance")]
    fn get_subtree_size_differential(
        &self,
        parent_idx: usize,
        _child_idx: usize,
        _child_subtree_size: usize,
    ) -> usize {
        self.get_subtree_size(parent_idx)
    }

    // Iterative in-place rebuild for balanced subtree
    fn rebuild<U: Copy + Ord + Sub + SmallUnsigned>(&mut self, idx: usize) {
        let sorted_sub = self.flatten_subtree_to_sorted_idxs(idx);
        self.rebalance_subtree_from_sorted_idxs::<U>(idx, &sorted_sub);
        self.rebal_cnt = self.rebal_cnt.wrapping_add(1);
    }

    // Height re-balance of subtree (e.g. depth of the two subtrees of every node never differs by more than one).
    // Adapted from public interview question: https://afteracademy.com/blog/sorted-array-to-balanced-bst
    fn rebalance_subtree_from_sorted_idxs<U: Copy + Ord + Sub + SmallUnsigned>(
        &mut self,
        old_subtree_root_idx: usize,
        sorted_arena_idxs: &[usize],
    ) {
        if sorted_arena_idxs.len() <= 1 {
            return;
        }

        debug_assert!(
            self.root_idx.is_some(),
            "Internal invariant failed: rebalance of multi-node tree without root!"
        );

        let sorted_last_idx = sorted_arena_idxs.len() - 1;
        let subtree_root_sorted_idx = sorted_last_idx / 2;
        let subtree_root_arena_idx = sorted_arena_idxs[subtree_root_sorted_idx];
        let mut subtree_worklist = SmallVec::<[(U, NodeRebuildHelper<U>); N]>::new();

        // Init worklist with middle node (balanced subtree root)
        subtree_worklist.push((
            U::checked_from(subtree_root_sorted_idx),
            NodeRebuildHelper::new(0, sorted_last_idx),
        ));

        // Update tree root or subtree parent
        if let Some(root_idx) = self.root_idx {
            if sorted_arena_idxs.contains(&root_idx) {
                self.root_idx = Some(subtree_root_arena_idx);
            } else {
                let old_subtree_root = &self.arena[old_subtree_root_idx];
                let ngh: NodeGetHelper<U> = self.priv_get(None, &old_subtree_root.key());
                debug_assert!(
                    ngh.parent_idx().is_some(),
                    "Internal invariant failed: rebalance of non-root parent-less node!"
                );
                if let Some(parent_idx) = ngh.parent_idx() {
                    let parent_node = &mut self.arena[parent_idx];
                    if ngh.is_right_child() {
                        parent_node.set_right_idx(Some(subtree_root_arena_idx));
                    } else {
                        parent_node.set_left_idx(Some(subtree_root_arena_idx));
                    }
                }
            }
        }

        // Iteratively re-assign all children
        while let Some((sorted_idx, parent_nrh)) = subtree_worklist.pop() {
            let parent_node = &mut self.arena[sorted_arena_idxs[sorted_idx.usize()]];

            parent_node.set_left_idx(None);
            parent_node.set_right_idx(None);

            // Set left child
            if parent_nrh.low_idx < parent_nrh.mid_idx {
                let child_nrh: NodeRebuildHelper<U> = NodeRebuildHelper::new(
                    parent_nrh.low_idx.usize(),
                    parent_nrh.mid_idx.usize() - 1,
                );
                parent_node.set_left_idx(Some(sorted_arena_idxs[child_nrh.mid_idx.usize()]));
                subtree_worklist.push((child_nrh.mid_idx, child_nrh));
            }

            // Set right child
            if parent_nrh.mid_idx < parent_nrh.high_idx {
                let child_nrh: NodeRebuildHelper<U> = NodeRebuildHelper::new(
                    parent_nrh.mid_idx.usize() + 1,
                    parent_nrh.high_idx.usize(),
                );
                parent_node.set_right_idx(Some(sorted_arena_idxs[child_nrh.mid_idx.usize()]));
                subtree_worklist.push((child_nrh.mid_idx, child_nrh));
            }

            // Set subtree size
            #[cfg(feature = "fast_rebalance")]
            {
                parent_node.subtree_size = parent_nrh.high_idx - parent_nrh.low_idx + 1;
                debug_assert!(parent_node.subtree_size >= 1);
            }
        }

        debug_assert!(
            self.get_subtree_size::<U>(subtree_root_arena_idx) == (sorted_arena_idxs.len()),
            "Internal invariant failed: rebalance changed node count! {} -> {}",
            self.get_subtree_size::<U>(subtree_root_arena_idx),
            sorted_arena_idxs.len()
        );
    }

    // Alpha weight balance computation helper.
    fn alpha_balance_depth(&self, val: usize) -> usize {
        // log base (1/alpha), hence (denom/num)
        (val as f32).log(self.alpha_denom / self.alpha_num).floor() as usize
    }
}

// Convenience Traits --------------------------------------------------------------------------------------------------

// Debug
impl<K, V, const N: usize> Debug for SGTree<K, V, N>
where
    K: Ord + Debug + Default,
    V: Debug + Default,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

// Default
impl<K, V, const N: usize> Default for SGTree<K, V, N>
where
    K: Ord + Default,
    V: Default,
{
    fn default() -> Self {
        Self::new()
    }
}

// From array
impl<K, V, const N: usize> From<[(K, V); N]> for SGTree<K, V, N>
where
    K: Ord + Default,
    V: Default,
{
    /// ```
    /// use scapegoat::SGTree;
    ///
    /// let tree1 = SGTree::from([(1, 2), (3, 4)]);
    /// let tree2: SGTree<_, _> = [(1, 2), (3, 4)].into();
    /// assert_eq!(tree1, tree2);
    /// ```
    fn from(arr: [(K, V); N]) -> Self {
        core::array::IntoIter::new(arr).collect()
    }
}

// Indexing
impl<K, V, Q, const N: usize> Index<&Q> for SGTree<K, V, N>
where
    K: Borrow<Q> + Ord + Default,
    Q: Ord + ?Sized,
    V: Default,
{
    type Output = V;

    /// Returns a reference to the value corresponding to the supplied key.
    ///
    /// # Panics
    ///
    /// Panics if the key is not present in the `SGTree`.
    fn index(&self, key: &Q) -> &Self::Output {
        self.get(key).expect("No value found for key")
    }
}

// Extension from iterator.
impl<K, V, const N: usize> Extend<(K, V)> for SGTree<K, V, N>
where
    K: Ord + Default,
    V: Default,
{
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, iter: I) {
        iter.into_iter().for_each(move |(k, v)| {
            #[cfg(not(feature = "high_assurance"))]
            self.insert(k, v);

            #[cfg(feature = "high_assurance")]
            self.insert(k, v).expect("Stack-storage capacity exceeded!");
        });
    }
}

// Extension from reference iterator.
impl<'a, K, V, const N: usize> Extend<(&'a K, &'a V)> for SGTree<K, V, N>
where
    K: Ord + Copy + Default,
    V: Copy + Default,
{
    fn extend<I: IntoIterator<Item = (&'a K, &'a V)>>(&mut self, iter: I) {
        self.extend(iter.into_iter().map(|(&key, &value)| (key, value)));
    }
}

// PartialEq
impl<K, V, const N: usize> PartialEq for SGTree<K, V, N>
where
    K: Ord + PartialEq + Default,
    V: PartialEq + Default,
{
    fn eq(&self, other: &SGTree<K, V, N>) -> bool {
        self.len() == other.len() && self.iter().zip(other).all(|(a, b)| a == b)
    }
}

// Eq
impl<K, V, const N: usize> Eq for SGTree<K, V, N>
where
    K: Ord + Eq + Default,
    V: Eq + Default,
{
}

// PartialOrd
impl<K, V, const N: usize> PartialOrd for SGTree<K, V, N>
where
    K: Ord + PartialOrd + Default,
    V: PartialOrd + Default,
{
    fn partial_cmp(&self, other: &SGTree<K, V, N>) -> Option<Ordering> {
        self.iter().partial_cmp(other.iter())
    }
}

// Ord
impl<K, V, const N: usize> Ord for SGTree<K, V, N>
where
    K: Ord + Default,
    V: Ord + Default,
{
    fn cmp(&self, other: &SGTree<K, V, N>) -> Ordering {
        self.iter().cmp(other.iter())
    }
}

// Hash
impl<K, V, const N: usize> Hash for SGTree<K, V, N>
where
    K: Ord + Hash + Default,
    V: Hash + Default,
{
    fn hash<H: Hasher>(&self, state: &mut H) {
        for i in self {
            i.hash(state);
        }
    }
}

// Iterators -----------------------------------------------------------------------------------------------------------

// Construct from iterator.
impl<K, V, const N: usize> FromIterator<(K, V)> for SGTree<K, V, N>
where
    K: Ord + Default,
    V: Default,
{
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        let mut sgt = SGTree::new();

        for (k, v) in iter {
            #[cfg(not(feature = "high_assurance"))]
            sgt.insert(k, v);

            #[cfg(feature = "high_assurance")]
            sgt.insert(k, v).expect("Stack-storage capacity exceeded!");
        }

        sgt
    }
}

// Reference iterator, mutable
impl<'a, K, V, const N: usize> IntoIterator for &'a mut SGTree<K, V, N>
where
    K: Ord + Default,
    V: Default,
{
    type Item = (&'a K, &'a mut V);
    type IntoIter = IterMut<'a, K, V, N>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

// Reference iterator, immutable
impl<'a, K, V, const N: usize> IntoIterator for &'a SGTree<K, V, N>
where
    K: Ord + Default,
    V: Default,
{
    type Item = (&'a K, &'a V);
    type IntoIter = Iter<'a, K, V, N>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

// Consuming iterator
impl<K, V, const N: usize> IntoIterator for SGTree<K, V, N>
where
    K: Ord + Default,
    V: Default,
{
    type Item = (K, V);
    type IntoIter = IntoIter<K, V, N>;

    fn into_iter(self) -> Self::IntoIter {
        IntoIter::new(self)
    }
}
