//! This module implements a key/value store based on a B-Tree
//! in stable memory.
//!
//! # V1 layout
//!
//! ```text
//! ---------------------------------------- <- Address 0
//! Magic "BTR"                 ↕ 3 bytes
//! ----------------------------------------
//! Layout version              ↕ 1 byte
//! ----------------------------------------
//! Max key size                ↕ 4 bytes
//! ----------------------------------------
//! Max value size              ↕ 4 bytes
//! ----------------------------------------
//! Root node address           ↕ 8 bytes
//! ----------------------------------------
//! Length (number of elements) ↕ 8 bytes
//! ---------------------------------------- <- Address 28 (PACKED_HEADER_SIZE)
//! Reserved space              ↕ 24 bytes
//! ---------------------------------------- <- Address 52 (ALLOCATOR_OFFSET)
//! Allocator
//! ----------------------------------------
//! ... free memory for nodes
//! ----------------------------------------
//! ```
mod allocator;
mod iter;
mod node;
use crate::{
    storable::max_size,
    types::{Address, NULL},
    Memory, Storable,
};
use allocator::Allocator;
pub use iter::Iter;
use iter::{Cursor, Index};
use node::{DerivedPageSize, Entry, Node, NodeType, Version};
use std::borrow::Cow;
use std::marker::PhantomData;
use std::ops::{Bound, RangeBounds};

#[cfg(test)]
mod proptests;

const MAGIC: &[u8; 3] = b"BTR";
const LAYOUT_VERSION: u8 = 1;
/// The sum of all the header fields, i.e. size of a packed header.
const PACKED_HEADER_SIZE: usize = 28;
/// The offset where the allocator begins.
const ALLOCATOR_OFFSET: usize = 52;

/// A "stable" map based on a B-tree.
///
/// The implementation is based on the algorithm outlined in "Introduction to Algorithms"
/// by Cormen et al.
pub struct BTreeMap<K, V, M>
where
    K: Storable + Ord + Clone,
    V: Storable,
    M: Memory,
{
    // The address of the root node. If a root node doesn't exist, the address
    // is set to NULL.
    root_addr: Address,

    // The maximum size a key can have.
    max_key_size: u32,

    // The maximum size a value can have.
    max_value_size: u32,

    // An allocator used for managing memory and allocating nodes.
    allocator: Allocator<M>,

    // The number of elements in the map.
    length: u64,

    // A marker to communicate to the Rust compiler that we own these types.
    _phantom: PhantomData<(K, V)>,
}

/// The packed header size must be <= ALLOCATOR_OFFSET.
struct BTreeHeaderV1 {
    magic: [u8; 3],
    version: u8,
    max_key_size: u32,
    max_value_size: u32,
    root_addr: Address,
    length: u64,
    // Reserved bytes for future extensions
}

impl<K, V, M> BTreeMap<K, V, M>
where
    K: Storable + Ord + Clone,
    V: Storable,
    M: Memory,
{
    /// Initializes a `BTreeMap`.
    ///
    /// If the memory provided already contains a `BTreeMap`, then that
    /// map is loaded. Otherwise, a new `BTreeMap` instance is created.
    pub fn init(memory: M) -> Self {
        if memory.size() == 0 {
            // Memory is empty. Create a new map.
            return BTreeMap::new(memory);
        }

        // Check if the magic in the memory corresponds to a BTreeMap.
        let mut dst = vec![0; 3];
        memory.read(0, &mut dst);
        if dst != MAGIC {
            // No BTreeMap found. Create a new instance.
            BTreeMap::new(memory)
        } else {
            // The memory already contains a BTreeMap. Load it.
            BTreeMap::load(memory)
        }
    }

    /// Creates a new instance a `BTreeMap`.
    ///
    /// The given `memory` is assumed to be exclusively reserved for this data
    /// structure and that it starts at address zero. Typically `memory` will
    /// be an instance of `RestrictedMemory`.
    ///
    /// When initialized, the data structure has the following memory layout:
    ///
    ///    |  BTreeHeader  |  Allocator | ... free memory for nodes |
    ///
    /// See `Allocator` for more details on its own memory layout.
    pub fn new(memory: M) -> Self {
        let max_key_size = max_size::<K>();
        let max_value_size = max_size::<V>();

        let btree = Self {
            root_addr: NULL,
            allocator: Allocator::new(
                memory,
                Address::from(ALLOCATOR_OFFSET as u64),
                Node::<K>::size(max_key_size, max_value_size),
            ),
            max_key_size,
            max_value_size,
            length: 0,
            _phantom: PhantomData,
        };

        btree.save();
        btree
    }

    /// Loads the map from memory.
    pub fn load(memory: M) -> Self {
        // Read the header from memory.
        let header = Self::read_header(&memory);
        assert_eq!(&header.magic, MAGIC, "Bad magic.");
        assert_eq!(header.version, LAYOUT_VERSION, "Unsupported version.");
        let expected_key_size = header.max_key_size;
        assert!(
            max_size::<K>() <= expected_key_size,
            "max_key_size must be <= {expected_key_size}"
        );
        let expected_value_size = header.max_value_size;
        assert!(
            max_size::<V>() <= expected_value_size,
            "max_value_size must be <= {expected_value_size}"
        );

        let allocator_addr = Address::from(ALLOCATOR_OFFSET as u64);
        Self {
            root_addr: header.root_addr,
            allocator: Allocator::load(memory, allocator_addr),
            max_key_size: header.max_key_size,
            max_value_size: header.max_value_size,
            length: header.length,
            _phantom: PhantomData,
        }
    }

    /// Reads the header from the specified memory.
    fn read_header(memory: &M) -> BTreeHeaderV1 {
        // Read the header
        let mut buf = [0; PACKED_HEADER_SIZE];
        memory.read(0, &mut buf);
        // Deserialize the fields
        BTreeHeaderV1 {
            magic: buf[0..3].try_into().unwrap(),
            version: buf[3],
            max_key_size: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
            max_value_size: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
            root_addr: Address::from(u64::from_le_bytes(buf[12..20].try_into().unwrap())),
            length: u64::from_le_bytes(buf[20..28].try_into().unwrap()),
        }
    }

    /// Inserts a key-value pair into the map.
    ///
    /// The previous value of the key, if present, is returned.
    ///
    /// PRECONDITION:
    ///   Key is bounded in size
    ///   Value is bounded in size
    ///   key.to_bytes().len() <= max_size(Key)
    ///   value.to_bytes().len() <= max_size(Value)
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        let key_bytes = key.to_bytes();
        let value_bytes = value.to_bytes();

        assert!(
            key_bytes.len() <= self.max_key_size as usize,
            "Key is too large. Expected <= {} bytes, found {} bytes",
            self.max_key_size,
            key_bytes.len()
        );

        assert!(
            value_bytes.len() <= self.max_value_size as usize,
            "Value is too large. Expected <= {} bytes, found {} bytes",
            self.max_value_size,
            value_bytes.len()
        );

        let value = value_bytes.to_vec();

        let root = if self.root_addr == NULL {
            // No root present. Allocate one.
            let node = self.allocate_node(NodeType::Leaf);
            self.root_addr = node.address();
            self.save();
            node
        } else {
            // Load the root from memory.
            let mut root = self.load_node(self.root_addr);

            // Check if the key already exists in the root.
            if let Ok(idx) = root.search(&key) {
                // The key exists. Overwrite it and return the previous value.
                let (_, previous_value) = root.swap_entry(idx, (key, value), self.memory());
                root.save(self.memory());
                return Some(V::from_bytes(Cow::Owned(previous_value)));
            }

            // If the root is full, we need to introduce a new node as the root.
            //
            // NOTE: In the case where we are overwriting an existing key, then introducing
            // a new root node isn't strictly necessary. However, that's a micro-optimization
            // that adds more complexity than it's worth.
            if root.is_full() {
                // The root is full. Allocate a new node that will be used as the new root.
                let mut new_root = self.allocate_node(NodeType::Internal);

                // The new root has the old root as its only child.
                new_root.push_child(self.root_addr);

                // Update the root address.
                self.root_addr = new_root.address();
                self.save();

                // Split the old (full) root.
                self.split_child(&mut new_root, 0);

                new_root
            } else {
                root
            }
        };

        self.insert_nonfull(root, key, value)
            .map(Cow::Owned)
            .map(V::from_bytes)
    }

    // Inserts an entry into a node that is *not full*.
    fn insert_nonfull(&mut self, mut node: Node<K>, key: K, value: Vec<u8>) -> Option<Vec<u8>> {
        // We're guaranteed by the caller that the provided node is not full.
        assert!(!node.is_full());

        // Look for the key in the node.
        match node.search(&key) {
            Ok(idx) => {
                // The key is already in the node.
                // Overwrite it and return the previous value.
                let (_, previous_value) = node.swap_entry(idx, (key, value), self.memory());

                node.save(self.memory());
                Some(previous_value)
            }
            Err(idx) => {
                // The key isn't in the node. `idx` is where that key should be inserted.

                match node.node_type() {
                    NodeType::Leaf => {
                        // The node is a non-full leaf.
                        // Insert the entry at the proper location.
                        node.insert_entry(idx, (key, value));
                        node.save(self.memory());

                        // Update the length.
                        self.length += 1;
                        self.save();

                        // No previous value to return.
                        None
                    }
                    NodeType::Internal => {
                        // The node is an internal node.
                        // Load the child that we should add the entry to.
                        let mut child = self.load_node(node.child(idx));

                        if child.is_full() {
                            // Check if the key already exists in the child.
                            if let Ok(idx) = child.search(&key) {
                                // The key exists. Overwrite it and return the previous value.
                                let (_, previous_value) =
                                    child.swap_entry(idx, (key, value), self.memory());
                                child.save(self.memory());
                                return Some(previous_value);
                            }

                            // The child is full. Split the child.
                            self.split_child(&mut node, idx);

                            // The children have now changed. Search again for
                            // the child where we need to store the entry in.
                            let idx = node.search(&key).unwrap_or_else(|idx| idx);
                            child = self.load_node(node.child(idx));
                        }

                        // The child should now be not full.
                        assert!(!child.is_full());

                        self.insert_nonfull(child, key, value)
                    }
                }
            }
        }
    }

    // Takes as input a nonfull internal `node` and index to its full child, then
    // splits this child into two, adding an additional child to `node`.
    //
    // Example:
    //
    //                          [ ... M   Y ... ]
    //                                  |
    //                 [ N  O  P  Q  R  S  T  U  V  W  X ]
    //
    //
    // After splitting becomes:
    //
    //                         [ ... M  S  Y ... ]
    //                                 / \
    //                [ N  O  P  Q  R ]   [ T  U  V  W  X ]
    //
    fn split_child(&mut self, node: &mut Node<K>, full_child_idx: usize) {
        // The node must not be full.
        assert!(!node.is_full());

        // The node's child must be full.
        let mut full_child = self.load_node(node.child(full_child_idx));
        assert!(full_child.is_full());

        // Create a sibling to this full child (which has to be the same type).
        let mut sibling = self.allocate_node(full_child.node_type());
        assert_eq!(sibling.node_type(), full_child.node_type());

        // Add sibling as a new child in the node.
        node.insert_child(full_child_idx + 1, sibling.address());

        let (median_key, median_value) = full_child.split(&mut sibling, self.memory());

        node.insert_entry(full_child_idx, (median_key, median_value));

        sibling.save(self.memory());
        full_child.save(self.memory());
        node.save(self.memory());
    }

    /// Returns the value associated with the given key if it exists.
    pub fn get(&self, key: &K) -> Option<V> {
        if self.root_addr == NULL {
            return None;
        }

        self.get_helper(self.root_addr, key)
            .map(Cow::Owned)
            .map(V::from_bytes)
    }

    fn get_helper(&self, node_addr: Address, key: &K) -> Option<Vec<u8>> {
        let node = self.load_node(node_addr);
        match node.search(key) {
            Ok(idx) => Some(node.value(idx, self.memory()).to_vec()),
            Err(idx) => {
                match node.node_type() {
                    NodeType::Leaf => None, // Key not found.
                    NodeType::Internal => {
                        // The key isn't in the node. Look for the key in the child.
                        self.get_helper(node.child(idx), key)
                    }
                }
            }
        }
    }

    /// Returns `true` if the key exists in the map, `false` otherwise.
    pub fn contains_key(&self, key: &K) -> bool {
        self.get(key).is_some()
    }

    /// Returns `true` if the map contains no elements.
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// Returns the number of elements in the map.
    pub fn len(&self) -> u64 {
        self.length
    }

    /// Returns the underlying memory.
    pub fn into_memory(self) -> M {
        self.allocator.into_memory()
    }

    /// Removes all elements from the map.
    pub fn clear(self) -> Self {
        let mem = self.allocator.into_memory();
        Self::new(mem)
    }

    /// Returns the first key-value pair in the map. The key in this
    /// pair is the minimum key in the map.
    pub fn first_key_value(&self) -> Option<(K, V)> {
        if self.root_addr == NULL {
            return None;
        }
        let root = self.load_node(self.root_addr);
        let (k, encoded_v) = root.get_min(self.memory());
        Some((k, V::from_bytes(Cow::Owned(encoded_v))))
    }

    /// Returns the last key-value pair in the map. The key in this
    /// pair is the maximum key in the map.
    pub fn last_key_value(&self) -> Option<(K, V)> {
        if self.root_addr == NULL {
            return None;
        }
        let root = self.load_node(self.root_addr);
        let (k, encoded_v) = root.get_max(self.memory());
        Some((k, V::from_bytes(Cow::Owned(encoded_v))))
    }

    fn memory(&self) -> &M {
        self.allocator.memory()
    }

    /// Removes a key from the map, returning the previous value at the key if it exists.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        if self.root_addr == NULL {
            return None;
        }

        let root_node = self.load_node(self.root_addr);
        self.remove_helper(root_node, key)
            .map(Cow::Owned)
            .map(V::from_bytes)
    }

    // A helper method for recursively removing a key from the B-tree.
    fn remove_helper(&mut self, mut node: Node<K>, key: &K) -> Option<Vec<u8>> {
        if node.address() != self.root_addr {
            // We're guaranteed that whenever this method is called an entry can be
            // removed from the node without it needing to be merged into a sibling.
            // This strengthened condition allows us to delete an entry in a single
            // pass most of the time without having to back up.
            assert!(node.can_remove_entry_without_merging());
        }

        match node.node_type() {
            NodeType::Leaf => {
                match node.search(key) {
                    Ok(idx) => {
                        // Case 1: The node is a leaf node and the key exists in it.
                        // This is the simplest case. The key is removed from the leaf.
                        let value = node.remove_entry(idx, self.memory()).1;
                        self.length -= 1;

                        if node.entries_len() == 0 {
                            assert_eq!(
                                node.address(), self.root_addr,
                                "Removal can only result in an empty leaf node if that node is the root"
                            );

                            // Deallocate the empty node.
                            self.allocator.deallocate(node.address());
                            self.root_addr = NULL;
                        } else {
                            node.save(self.memory());
                        }

                        self.save();
                        Some(value)
                    }
                    _ => None, // Key not found.
                }
            }
            NodeType::Internal => {
                match node.search(key) {
                    Ok(idx) => {
                        // Case 2: The node is an internal node and the key exists in it.

                        let left_child = self.load_node(node.child(idx));
                        if left_child.can_remove_entry_without_merging() {
                            // Case 2.a: A key can be removed from the left child without merging.
                            //
                            //                       parent
                            //                  [..., key, ...]
                            //                       /   \
                            //            [left child]   [...]
                            //           /            \
                            //        [...]         [..., key predecessor]
                            //
                            // In this case, we replace `key` with the key's predecessor from the
                            // left child's subtree, then we recursively delete the key's
                            // predecessor for the following end result:
                            //
                            //                       parent
                            //            [..., key predecessor, ...]
                            //                       /   \
                            //            [left child]   [...]
                            //           /            \
                            //        [...]          [...]

                            // Recursively delete the predecessor.
                            // TODO(EXC-1034): Do this in a single pass.
                            let predecessor = left_child.get_max(self.memory());
                            self.remove_helper(left_child, &predecessor.0)?;

                            // Replace the `key` with its predecessor.
                            let (_, old_value) = node.swap_entry(idx, predecessor, self.memory());

                            // Save the parent node.
                            node.save(self.memory());
                            return Some(old_value);
                        }

                        let right_child = self.load_node(node.child(idx + 1));
                        if right_child.can_remove_entry_without_merging() {
                            // Case 2.b: A key can be removed from the right child without merging.
                            //
                            //                       parent
                            //                  [..., key, ...]
                            //                       /   \
                            //                   [...]   [right child]
                            //                          /             \
                            //              [key successor, ...]     [...]
                            //
                            // In this case, we replace `key` with the key's successor from the
                            // right child's subtree, then we recursively delete the key's
                            // successor for the following end result:
                            //
                            //                       parent
                            //            [..., key successor, ...]
                            //                       /   \
                            //                  [...]   [right child]
                            //                           /            \
                            //                        [...]          [...]

                            // Recursively delete the successor.
                            // TODO(EXC-1034): Do this in a single pass.
                            let successor = right_child.get_min(self.memory());
                            self.remove_helper(right_child, &successor.0)?;

                            // Replace the `key` with its successor.
                            let (_, old_value) = node.swap_entry(idx, successor, self.memory());

                            // Save the parent node.
                            node.save(self.memory());
                            return Some(old_value);
                        }

                        // Case 2.c: Both the left and right child are at their minimum sizes.
                        //
                        //                       parent
                        //                  [..., key, ...]
                        //                       /   \
                        //            [left child]   [right child]
                        //
                        // In this case, we merge (left child, key, right child) into a single
                        // node. The result will look like this:
                        //
                        //                       parent
                        //                     [...  ...]
                        //                         |
                        //          [left child, `key`, right child] <= new child
                        //
                        // We then recurse on this new child to delete `key`.
                        //
                        // If `parent` becomes empty (which can only happen if it's the root),
                        // then `parent` is deleted and `new_child` becomes the new root.
                        assert!(left_child.at_minimum());
                        assert!(right_child.at_minimum());

                        // Merge the right child into the left child.
                        let new_child = self.merge(
                            right_child,
                            left_child,
                            node.remove_entry(idx, self.memory()),
                        );

                        // Remove the right child from the parent node.
                        node.remove_child(idx + 1);

                        if node.entries_len() == 0 {
                            // Can only happen if this node is root.
                            assert_eq!(node.address(), self.root_addr);
                            assert_eq!(node.child(0), new_child.address());
                            assert_eq!(node.children_len(), 1);

                            self.root_addr = new_child.address();

                            // Deallocate the root node.
                            self.allocator.deallocate(node.address());
                            self.save();
                        }

                        node.save(self.memory());
                        new_child.save(self.memory());

                        // Recursively delete the key.
                        self.remove_helper(new_child, key)
                    }
                    Err(idx) => {
                        // Case 3: The node is an internal node and the key does NOT exist in it.

                        // If the key does exist in the tree, it will exist in the subtree at index
                        // `idx`.
                        let mut child = self.load_node(node.child(idx));

                        if child.can_remove_entry_without_merging() {
                            // The child has enough nodes. Recurse to delete the `key` from the
                            // `child`.
                            return self.remove_helper(child, key);
                        }

                        // An entry can't be removed from the child without merging.
                        // See if it has a sibling where an entry can be removed without merging.
                        let mut left_sibling = if idx > 0 {
                            Some(self.load_node(node.child(idx - 1)))
                        } else {
                            None
                        };

                        let mut right_sibling = if idx + 1 < node.children_len() {
                            Some(self.load_node(node.child(idx + 1)))
                        } else {
                            None
                        };

                        if let Some(ref mut left_sibling) = left_sibling {
                            if left_sibling.can_remove_entry_without_merging() {
                                // Case 3.a (left):
                                // A key can be removed from the left child without merging.
                                //
                                //                            [d] (parent)
                                //                           /   \
                                //  (left sibling) [a, b, c]     [e, f] (child)
                                //                         \
                                //                         [c']
                                //
                                // In this case, we move a key down from the parent into the child
                                // and move a key from the left sibling up into the parent
                                // resulting in the following tree:
                                //
                                //                            [c] (parent)
                                //                           /   \
                                //       (left sibling) [a, b]   [d, e, f] (child)
                                //                              /
                                //                            [c']
                                //
                                // We then recurse to delete the key from the child.

                                // Remove the last entry from the left sibling.
                                let (left_sibling_key, left_sibling_value) =
                                    left_sibling.pop_entry(self.memory()).unwrap();

                                // Replace the parent's entry with the one from the left sibling.
                                let (parent_key, parent_value) = node.swap_entry(
                                    idx - 1,
                                    (left_sibling_key, left_sibling_value),
                                    self.memory(),
                                );

                                // Move the entry from the parent into the child.
                                child.insert_entry(0, (parent_key, parent_value));

                                // Move the last child from left sibling into child.
                                if let Some(last_child) = left_sibling.pop_child() {
                                    assert_eq!(left_sibling.node_type(), NodeType::Internal);
                                    assert_eq!(child.node_type(), NodeType::Internal);

                                    child.insert_child(0, last_child);
                                } else {
                                    assert_eq!(left_sibling.node_type(), NodeType::Leaf);
                                    assert_eq!(child.node_type(), NodeType::Leaf);
                                }

                                left_sibling.save(self.memory());
                                child.save(self.memory());
                                node.save(self.memory());
                                return self.remove_helper(child, key);
                            }
                        }

                        if let Some(right_sibling) = &mut right_sibling {
                            if right_sibling.can_remove_entry_without_merging() {
                                // Case 3.a (right):
                                // A key can be removed from the right child without merging.
                                //
                                //                            [c] (parent)
                                //                           /   \
                                //             (child) [a, b]     [d, e, f] (right sibling)
                                //                               /
                                //                            [d']
                                //
                                // In this case, we move a key down from the parent into the child
                                // and move a key from the right sibling up into the parent
                                // resulting in the following tree:
                                //
                                //                            [d] (parent)
                                //                           /   \
                                //          (child) [a, b, c]     [e, f] (right sibling)
                                //                          \
                                //                           [d']
                                //
                                // We then recurse to delete the key from the child.

                                // Remove the first entry from the right sibling.
                                let (right_sibling_key, right_sibling_value) =
                                    right_sibling.remove_entry(0, self.memory());

                                // Replace the parent's entry with the one from the right sibling.
                                let parent_entry = node.swap_entry(
                                    idx,
                                    (right_sibling_key, right_sibling_value),
                                    self.memory(),
                                );

                                // Move the entry from the parent into the child.
                                child.push_entry(parent_entry);

                                // Move the first child of right_sibling into `child`.
                                match right_sibling.node_type() {
                                    NodeType::Internal => {
                                        assert_eq!(child.node_type(), NodeType::Internal);
                                        child.push_child(right_sibling.remove_child(0));
                                    }
                                    NodeType::Leaf => {
                                        assert_eq!(child.node_type(), NodeType::Leaf);
                                    }
                                }

                                right_sibling.save(self.memory());
                                child.save(self.memory());
                                node.save(self.memory());
                                return self.remove_helper(child, key);
                            }
                        }

                        // Case 3.b: Both the left and right siblings are at their minimum sizes.

                        if let Some(left_sibling) = left_sibling {
                            // Merge child into left sibling if it exists.

                            assert!(left_sibling.at_minimum());
                            let left_sibling = self.merge(
                                child,
                                left_sibling,
                                node.remove_entry(idx - 1, self.memory()),
                            );
                            // Removing child from parent.
                            node.remove_child(idx);

                            if node.entries_len() == 0 {
                                self.allocator.deallocate(node.address());

                                if node.address() == self.root_addr {
                                    // Update the root.
                                    self.root_addr = left_sibling.address();
                                    self.save();
                                }
                            } else {
                                node.save(self.memory());
                            }

                            return self.remove_helper(left_sibling, key);
                        }

                        if let Some(right_sibling) = right_sibling {
                            // Merge child into right sibling.

                            assert!(right_sibling.at_minimum());
                            let right_sibling = self.merge(
                                child,
                                right_sibling,
                                node.remove_entry(idx, self.memory()),
                            );

                            // Removing child from parent.
                            node.remove_child(idx);

                            if node.entries_len() == 0 {
                                self.allocator.deallocate(node.address());

                                if node.address() == self.root_addr {
                                    // Update the root.
                                    self.root_addr = right_sibling.address();
                                    self.save();
                                }
                            } else {
                                node.save(self.memory());
                            }

                            return self.remove_helper(right_sibling, key);
                        }

                        unreachable!("At least one of the siblings must exist.");
                    }
                }
            }
        }
    }

    /// Returns an iterator over the entries of the map, sorted by key.
    pub fn iter(&self) -> Iter<K, V, M> {
        Iter::new(self)
    }

    /// Returns an iterator over the entries in the map where keys
    /// belong to the specified range.
    pub fn range(&self, key_range: impl RangeBounds<K>) -> Iter<K, V, M> {
        if self.root_addr == NULL {
            // Map is empty.
            return Iter::null(self);
        }

        let range = (
            key_range.start_bound().cloned(),
            key_range.end_bound().cloned(),
        );

        let mut cursors = vec![];

        match key_range.start_bound() {
            Bound::Unbounded => {
                cursors.push(Cursor::Address(self.root_addr));
                Iter::new_in_range(self, range, cursors)
            }
            Bound::Included(key) | Bound::Excluded(key) => {
                let mut node = self.load_node(self.root_addr);
                loop {
                    match node.search(key) {
                        Ok(idx) => {
                            if let Bound::Included(_) = key_range.start_bound() {
                                // We found the key exactly matching the left bound.
                                // Here is where we'll start the iteration.
                                cursors.push(Cursor::Node {
                                    node,
                                    next: Index::Entry(idx),
                                });
                                return Iter::new_in_range(self, range, cursors);
                            } else {
                                // We found the key that we must
                                // exclude.  We add its right neighbor
                                // to the stack and start iterating
                                // from its right child.
                                let right_child = match node.node_type() {
                                    NodeType::Internal => Some(node.child(idx + 1)),
                                    NodeType::Leaf => None,
                                };

                                if idx + 1 != node.entries_len()
                                    && key_range.contains(node.key(idx + 1))
                                {
                                    cursors.push(Cursor::Node {
                                        node,
                                        next: Index::Entry(idx + 1),
                                    });
                                }
                                if let Some(right_child) = right_child {
                                    cursors.push(Cursor::Address(right_child));
                                }
                                return Iter::new_in_range(self, range, cursors);
                            }
                        }
                        Err(idx) => {
                            // The `idx` variable points to the first
                            // key that is greater than the left
                            // bound.
                            //
                            // If the index points to a valid node, we
                            // will visit its left subtree and then
                            // return to this key.
                            //
                            // If the index points at the end of
                            // array, we'll continue with the right
                            // child of the last key.

                            // Load the left child of the node to visit if it exists.
                            // This is done first to avoid cloning the node.
                            let child = match node.node_type() {
                                NodeType::Internal => {
                                    // Note that loading a child node cannot fail since
                                    // len(children) = len(entries) + 1
                                    Some(self.load_node(node.child(idx)))
                                }
                                NodeType::Leaf => None,
                            };

                            if idx < node.entries_len() && key_range.contains(node.key(idx)) {
                                cursors.push(Cursor::Node {
                                    node,
                                    next: Index::Entry(idx),
                                });
                            }

                            match child {
                                None => {
                                    // Leaf node. Return an iterator with the found cursors.
                                    return Iter::new_in_range(self, range, cursors);
                                }
                                Some(child) => {
                                    // Iterate over the child node.
                                    node = child;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Returns an iterator pointing to the first element below the given bound.
    /// Returns an empty iterator if there are no keys below the given bound.
    pub fn iter_upper_bound(&self, bound: &K) -> Iter<K, V, M> {
        if self.root_addr == NULL {
            // Map is empty.
            return Iter::null(self);
        }

        let dummy_bounds = (Bound::Unbounded, Bound::Unbounded);
        // INVARIANT: all cursors point to keys greater than or equal to bound.
        let mut cursors = vec![];

        let mut node = self.load_node(self.root_addr);
        loop {
            match node.search(bound) {
                Ok(idx) | Err(idx) => {
                    match node.node_type() {
                        NodeType::Leaf => {
                            if idx == 0 {
                                // We descended into a leaf but didn't find a node less than
                                // the upper bound. Thus we unwind the cursor stack until we
                                // hit a cursor pointing to an element other than the first key,
                                // and we shift the position backward. If there is no such cursor,
                                // the bound must be <= min element, so we return an empty iterator.
                                while let Some(cursor) = cursors.pop() {
                                    match cursor {
                                        Cursor::Node {
                                            node,
                                            next: Index::Entry(n),
                                        } => {
                                            if n == 0 {
                                                debug_assert!(node.key(n) >= bound);
                                                continue;
                                            } else {
                                                debug_assert!(node.key(n - 1) < bound);
                                                cursors.push(Cursor::Node {
                                                    node,
                                                    next: Index::Entry(n - 1),
                                                });
                                                break;
                                            }
                                        }
                                        _ => panic!("BUG: unexpected cursor shape"),
                                    }
                                }
                                // If the cursors are empty, the iterator will be empty.
                                return Iter::new_in_range(self, dummy_bounds, cursors);
                            }
                            debug_assert!(node.key(idx - 1) < bound);

                            cursors.push(Cursor::Node {
                                node,
                                next: Index::Entry(idx - 1),
                            });
                            return Iter::new_in_range(self, dummy_bounds, cursors);
                        }
                        NodeType::Internal => {
                            let child = self.load_node(node.child(idx));
                            // We push the node even if idx == node.entries_len()
                            // If we find the position in the child, the iterator will skip this
                            // cursor. But if the all keys in the child are greater than or equal to
                            // the bound, we will be able to use this cursor as a fallback.
                            cursors.push(Cursor::Node {
                                node,
                                next: Index::Entry(idx),
                            });
                            node = child;
                        }
                    }
                }
            }
        }
    }

    // Merges one node (`source`) into another (`into`), along with a median entry.
    //
    // Example (values are not included for brevity):
    //
    // Input:
    //   Source: [1, 2, 3]
    //   Into: [5, 6, 7]
    //   Median: 4
    //
    // Output:
    //   [1, 2, 3, 4, 5, 6, 7] (stored in the `into` node)
    //   `source` is deallocated.
    fn merge(&mut self, source: Node<K>, mut into: Node<K>, median: Entry<K>) -> Node<K> {
        let source_address = source.address();
        into.merge(source, median, self.memory());
        into.save(self.memory());
        self.allocator.deallocate(source_address);
        into
    }

    fn allocate_node(&mut self, node_type: NodeType) -> Node<K> {
        Node::new(
            self.allocator.allocate(),
            node_type,
            self.max_key_size,
            self.max_value_size,
        )
    }

    fn load_node(&self, address: Address) -> Node<K> {
        Node::load(
            address,
            self.memory(),
            Version::V1(DerivedPageSize {
                max_key_size: self.max_key_size,
                max_value_size: self.max_value_size,
            }),
        )
    }

    // Saves the map to memory.
    fn save(&self) {
        let header = BTreeHeaderV1 {
            magic: *MAGIC,
            version: LAYOUT_VERSION,
            max_key_size: self.max_key_size,
            max_value_size: self.max_value_size,
            root_addr: self.root_addr,
            length: self.length,
        };

        Self::write_header(&header, self.memory());
    }

    /// Write the layout header to the memory.
    fn write_header(header: &BTreeHeaderV1, memory: &M) {
        // Serialize the header
        let mut buf = [0; PACKED_HEADER_SIZE];
        buf[0..3].copy_from_slice(&header.magic);
        buf[3] = header.version;
        buf[4..8].copy_from_slice(&header.max_key_size.to_le_bytes());
        buf[8..12].copy_from_slice(&header.max_value_size.to_le_bytes());
        buf[12..20].copy_from_slice(&header.root_addr.get().to_le_bytes());
        buf[20..28].copy_from_slice(&header.length.to_le_bytes());
        // Write the header
        crate::write(memory, 0, &buf);
    }
}

/// An error returned when inserting entries into the map.
#[derive(Debug, PartialEq, Eq)]
pub enum InsertError {
    KeyTooLarge { given: usize, max: usize },
    ValueTooLarge { given: usize, max: usize },
}

impl std::fmt::Display for InsertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::KeyTooLarge { given, max } => {
                write!(
                    f,
                    "InsertError::KeyTooLarge Expected key to be <= {max} bytes but received key with {given} bytes."
                )
            }
            Self::ValueTooLarge { given, max } => {
                write!(
                    f,
                    "InsertError::ValueTooLarge Expected value to be <= {max} bytes but received value with {given} bytes."
                )
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::storable::{Blob, Bound as StorableBound};
    use std::cell::RefCell;
    use std::rc::Rc;

    fn make_memory() -> Rc<RefCell<Vec<u8>>> {
        Rc::new(RefCell::new(Vec::new()))
    }

    // A helper method to succinctly create an entry.
    fn e(x: u8) -> (Blob<10>, Vec<u8>) {
        (b(&[x]), vec![])
    }

    pub(crate) fn b(x: &[u8]) -> Blob<10> {
        Blob::<10>::try_from(x).unwrap()
    }

    #[test]
    fn init_preserves_data() {
        let mem = make_memory();
        let mut btree = BTreeMap::init(mem.clone());
        assert_eq!(btree.insert(b(&[1, 2, 3]), b(&[4, 5, 6])), None);
        assert_eq!(btree.get(&b(&[1, 2, 3])), Some(b(&[4, 5, 6])));

        // Reload the btree
        let btree = BTreeMap::init(mem);

        // Data still exists.
        assert_eq!(btree.get(&b(&[1, 2, 3])), Some(b(&[4, 5, 6])));
    }

    #[test]
    fn insert_get() {
        let mem = make_memory();
        let mut btree = BTreeMap::new(mem);

        assert_eq!(btree.insert(b(&[1, 2, 3]), b(&[4, 5, 6])), None);
        assert_eq!(btree.get(&b(&[1, 2, 3])), Some(b(&[4, 5, 6])));
    }

    #[test]
    fn insert_overwrites_previous_value() {
        let mem = make_memory();
        let mut btree = BTreeMap::new(mem);

        assert_eq!(btree.insert(b(&[1, 2, 3]), b(&[4, 5, 6])), None);
        assert_eq!(
            btree.insert(b(&[1, 2, 3]), b(&[7, 8, 9])),
            Some(b(&[4, 5, 6]))
        );
        assert_eq!(btree.get(&b(&[1, 2, 3])), Some(b(&[7, 8, 9])));
    }

    #[test]
    fn insert_get_multiple() {
        let mem = make_memory();
        let mut btree = BTreeMap::new(mem);

        assert_eq!(btree.insert(b(&[1, 2, 3]), b(&[4, 5, 6])), None);
        assert_eq!(btree.insert(b(&[4, 5]), b(&[7, 8, 9, 10])), None);
        assert_eq!(btree.insert(b(&[]), b(&[11])), None);
        assert_eq!(btree.get(&b(&[1, 2, 3])), Some(b(&[4, 5, 6])));
        assert_eq!(btree.get(&b(&[4, 5])), Some(b(&[7, 8, 9, 10])));
        assert_eq!(btree.get(&b(&[])), Some(b(&[11])));
    }

    #[test]
    fn insert_overwrite_median_key_in_full_child_node() {
        let mem = make_memory();
        let mut btree = BTreeMap::new(mem);

        for i in 1..=17 {
            assert_eq!(btree.insert(b(&[i]), b(&[])), None);
        }

        // The result should look like this:
        //                [6]
        //               /   \
        // [1, 2, 3, 4, 5]   [7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17]

        let root = btree.load_node(btree.root_addr);
        assert_eq!(root.node_type(), NodeType::Internal);
        assert_eq!(root.entries(btree.memory()), vec![e(6)]);
        assert_eq!(root.children_len(), 2);

        // The right child should now be full, with the median key being "12"
        let right_child = btree.load_node(root.child(1));
        assert!(right_child.is_full());
        let median_index = right_child.entries_len() / 2;
        assert_eq!(right_child.key(median_index), &b(&[12]));

        // Overwrite the median key.
        assert_eq!(btree.insert(b(&[12]), b(&[1, 2, 3])), Some(b(&[])));

        // The key is overwritten successfully.
        assert_eq!(btree.get(&b(&[12])), Some(b(&[1, 2, 3])));

        // The child has not been split and is still full.
        let right_child = btree.load_node(root.child(1));
        assert_eq!(right_child.node_type(), NodeType::Leaf);
        assert!(right_child.is_full());
    }

    #[test]
    fn insert_overwrite_key_in_full_root_node() {
        let mem = make_memory();
        let mut btree = BTreeMap::new(mem);

        for i in 1..=11 {
            assert_eq!(btree.insert(b(&[i]), b(&[])), None);
        }

        // We now have a root that is full and looks like this:
        //
        // [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]
        let root = btree.load_node(btree.root_addr);
        assert!(root.is_full());

        // Overwrite an element in the root. It should NOT cause the node to be split.
        assert_eq!(btree.insert(b(&[6]), b(&[4, 5, 6])), Some(b(&[])));

        let root = btree.load_node(btree.root_addr);
        assert_eq!(root.node_type(), NodeType::Leaf);
        assert_eq!(btree.get(&b(&[6])), Some(b(&[4, 5, 6])));
        assert_eq!(root.entries_len(), 11);
    }

    #[test]
    fn allocations() {
        let mem = make_memory();
        let mut btree = BTreeMap::new(mem);

        // Insert entries until the root node is full.
        let mut i = 0;
        loop {
            assert_eq!(btree.insert(b(&[i]), b(&[])), None);
            let root = btree.load_node(btree.root_addr);
            if root.is_full() {
                break;
            }
            i += 1;
        }

        // Only need a single allocation to store up to `CAPACITY` elements.
        assert_eq!(btree.allocator.num_allocated_chunks(), 1);

        assert_eq!(btree.insert(b(&[255]), b(&[])), None);

        // The node had to be split into three nodes.
        assert_eq!(btree.allocator.num_allocated_chunks(), 3);
    }

    #[test]
    fn allocations_2() {
        let mem = make_memory();
        let mut btree = BTreeMap::new(mem);
        assert_eq!(btree.allocator.num_allocated_chunks(), 0);

        assert_eq!(btree.insert(b(&[]), b(&[])), None);
        assert_eq!(btree.allocator.num_allocated_chunks(), 1);

        assert_eq!(btree.remove(&b(&[])), Some(b(&[])));
        assert_eq!(btree.allocator.num_allocated_chunks(), 0);
    }

    #[test]
    fn insert_same_key_multiple() {
        let mem = make_memory();
        let mut btree = BTreeMap::new(mem);

        assert_eq!(btree.insert(b(&[1]), b(&[2])), None);

        for i in 2..10 {
            assert_eq!(btree.insert(b(&[1]), b(&[i + 1])), Some(b(&[i])));
        }
    }

    #[test]
    fn insert_split_node() {
        let mem = make_memory();
        let mut btree = BTreeMap::new(mem);

        for i in 1..=11 {
            assert_eq!(btree.insert(b(&[i]), b(&[])), None);
        }

        // Should now split a node.
        assert_eq!(btree.insert(b(&[12]), b(&[])), None);

        // The result should look like this:
        //                [6]
        //               /   \
        // [1, 2, 3, 4, 5]   [7, 8, 9, 10, 11, 12]

        for i in 1..=12 {
            assert_eq!(btree.get(&b(&[i])), Some(b(&[])));
        }
    }

    #[test]
    fn insert_split_multiple_nodes() {
        let mem = make_memory();
        let mut btree = BTreeMap::new(mem);

        for i in 1..=11 {
            assert_eq!(btree.insert(b(&[i]), b(&[])), None);
        }
        // Should now split a node.
        assert_eq!(btree.insert(b(&[12]), b(&[])), None);

        // The result should look like this:
        //                [6]
        //               /   \
        // [1, 2, 3, 4, 5]   [7, 8, 9, 10, 11, 12]

        let root = btree.load_node(btree.root_addr);
        assert_eq!(root.node_type(), NodeType::Internal);
        assert_eq!(root.entries(btree.memory()), vec![e(6)]);
        assert_eq!(root.children_len(), 2);

        let child_0 = btree.load_node(root.child(0));
        assert_eq!(child_0.node_type(), NodeType::Leaf);
        assert_eq!(
            child_0.entries(btree.memory()),
            vec![e(1), e(2), e(3), e(4), e(5)]
        );

        let child_1 = btree.load_node(root.child(1));
        assert_eq!(child_1.node_type(), NodeType::Leaf);
        assert_eq!(
            child_1.entries(btree.memory()),
            vec![e(7), e(8), e(9), e(10), e(11), e(12)]
        );

        for i in 1..=12 {
            assert_eq!(btree.get(&b(&[i])), Some(b(&[])));
        }

        // Insert more to cause more splitting.
        assert_eq!(btree.insert(b(&[13]), b(&[])), None);
        assert_eq!(btree.insert(b(&[14]), b(&[])), None);
        assert_eq!(btree.insert(b(&[15]), b(&[])), None);
        assert_eq!(btree.insert(b(&[16]), b(&[])), None);
        assert_eq!(btree.insert(b(&[17]), b(&[])), None);
        // Should cause another split
        assert_eq!(btree.insert(b(&[18]), b(&[])), None);

        for i in 1..=18 {
            assert_eq!(btree.get(&b(&[i])), Some(b(&[])));
        }

        let root = btree.load_node(btree.root_addr);
        assert_eq!(root.node_type(), NodeType::Internal);
        assert_eq!(root.entries(btree.memory()), vec![e(6), e(12)],);
        assert_eq!(root.children_len(), 3);

        let child_0 = btree.load_node(root.child(0));
        assert_eq!(child_0.node_type(), NodeType::Leaf);
        assert_eq!(
            child_0.entries(btree.memory()),
            vec![e(1), e(2), e(3), e(4), e(5)]
        );

        let child_1 = btree.load_node(root.child(1));
        assert_eq!(child_1.node_type(), NodeType::Leaf);
        assert_eq!(
            child_1.entries(btree.memory()),
            vec![e(7), e(8), e(9), e(10), e(11)]
        );

        let child_2 = btree.load_node(root.child(2));
        assert_eq!(child_2.node_type(), NodeType::Leaf);
        assert_eq!(
            child_2.entries(btree.memory()),
            vec![e(13), e(14), e(15), e(16), e(17), e(18)]
        );
    }

    #[test]
    fn remove_simple() {
        let mem = make_memory();
        let mut btree = BTreeMap::new(mem);

        assert_eq!(btree.insert(b(&[1, 2, 3]), b(&[4, 5, 6])), None);
        assert_eq!(btree.get(&b(&[1, 2, 3])), Some(b(&[4, 5, 6])));
        assert_eq!(btree.remove(&b(&[1, 2, 3])), Some(b(&[4, 5, 6])));
        assert_eq!(btree.get(&b(&[1, 2, 3])), None);
    }

    #[test]
    fn remove_case_2a_and_2c() {
        let mem = make_memory();
        let mut btree = BTreeMap::new(mem.clone());

        for i in 1..=11 {
            assert_eq!(btree.insert(b(&[i]), b(&[])), None);
        }
        // Should now split a node.
        assert_eq!(btree.insert(b(&[0]), b(&[])), None);

        // The result should look like this:
        //                    [6]
        //                   /   \
        // [0, 1, 2, 3, 4, 5]     [7, 8, 9, 10, 11]

        for i in 0..=11 {
            assert_eq!(btree.get(&b(&[i])), Some(b(&[])));
        }

        // Remove node 6. Triggers case 2.a
        assert_eq!(btree.remove(&b(&[6])), Some(b(&[])));

        // The result should look like this:
        //                [5]
        //               /   \
        // [0, 1, 2, 3, 4]   [7, 8, 9, 10, 11]
        let root = btree.load_node(btree.root_addr);
        assert_eq!(root.node_type(), NodeType::Internal);
        assert_eq!(root.entries(btree.memory()), vec![e(5)]);
        assert_eq!(root.children_len(), 2);

        let child_0 = btree.load_node(root.child(0));
        assert_eq!(child_0.node_type(), NodeType::Leaf);
        assert_eq!(
            child_0.entries(btree.memory()),
            vec![e(0), e(1), e(2), e(3), e(4)]
        );

        let child_1 = btree.load_node(root.child(1));
        assert_eq!(child_1.node_type(), NodeType::Leaf);
        assert_eq!(
            child_1.entries(btree.memory()),
            vec![e(7), e(8), e(9), e(10), e(11)]
        );

        // There are three allocated nodes.
        assert_eq!(btree.allocator.num_allocated_chunks(), 3);

        // Remove node 5. Triggers case 2c
        assert_eq!(btree.remove(&b(&[5])), Some(b(&[])));

        // Reload the btree to verify that we saved it correctly.
        let btree = BTreeMap::<Blob<10>, Blob<10>, _>::load(mem);

        // The result should look like this:
        // [0, 1, 2, 3, 4, 7, 8, 9, 10, 11]
        let root = btree.load_node(btree.root_addr);
        assert_eq!(
            root.entries(btree.memory()),
            vec![e(0), e(1), e(2), e(3), e(4), e(7), e(8), e(9), e(10), e(11)]
        );

        // There is only one node allocated.
        assert_eq!(btree.allocator.num_allocated_chunks(), 1);
    }

    #[test]
    fn remove_case_2b() {
        let mem = make_memory();
        let mut btree = BTreeMap::new(mem);

        for i in 1..=11 {
            assert_eq!(btree.insert(b(&[i]), b(&[])), None);
        }
        // Should now split a node.
        assert_eq!(btree.insert(b(&[12]), b(&[])), None);

        // The result should look like this:
        //                [6]
        //               /   \
        // [1, 2, 3, 4, 5]   [7, 8, 9, 10, 11, 12]

        for i in 1..=12 {
            assert_eq!(btree.get(&b(&[i])), Some(b(&[])));
        }

        // Remove node 6. Triggers case 2.b
        assert_eq!(btree.remove(&b(&[6])), Some(b(&[])));

        // The result should look like this:
        //                [7]
        //               /   \
        // [1, 2, 3, 4, 5]   [8, 9, 10, 11, 12]
        let root = btree.load_node(btree.root_addr);
        assert_eq!(root.node_type(), NodeType::Internal);
        assert_eq!(root.entries(btree.memory()), vec![e(7)]);
        assert_eq!(root.children_len(), 2);

        let child_0 = btree.load_node(root.child(0));
        assert_eq!(child_0.node_type(), NodeType::Leaf);
        assert_eq!(
            child_0.entries(btree.memory()),
            vec![e(1), e(2), e(3), e(4), e(5)]
        );

        let child_1 = btree.load_node(root.child(1));
        assert_eq!(child_1.node_type(), NodeType::Leaf);
        assert_eq!(
            child_1.entries(btree.memory()),
            vec![e(8), e(9), e(10), e(11), e(12)]
        );

        // Remove node 7. Triggers case 2.c
        assert_eq!(btree.remove(&b(&[7])), Some(b(&[])));
        // The result should look like this:
        //
        // [1, 2, 3, 4, 5, 8, 9, 10, 11, 12]
        let root = btree.load_node(btree.root_addr);
        assert_eq!(root.node_type(), NodeType::Leaf);
        assert_eq!(
            root.entries(btree.memory()),
            vec![
                e(1),
                e(2),
                e(3),
                e(4),
                e(5),
                e(8),
                e(9),
                e(10),
                e(11),
                e(12)
            ]
        );
    }

    #[test]
    fn remove_case_3a_right() {
        let mem = make_memory();
        let mut btree = BTreeMap::new(mem);

        for i in 1..=11 {
            assert_eq!(btree.insert(b(&[i]), b(&[])), None);
        }

        // Should now split a node.
        assert_eq!(btree.insert(b(&[12]), b(&[])), None);

        // The result should look like this:
        //                [6]
        //               /   \
        // [1, 2, 3, 4, 5]   [7, 8, 9, 10, 11, 12]

        // Remove node 3. Triggers case 3.a
        assert_eq!(btree.remove(&b(&[3])), Some(b(&[])));

        // The result should look like this:
        //                [7]
        //               /   \
        // [1, 2, 4, 5, 6]   [8, 9, 10, 11, 12]
        let root = btree.load_node(btree.root_addr);
        assert_eq!(root.node_type(), NodeType::Internal);
        assert_eq!(root.entries(btree.memory()), vec![e(7)]);
        assert_eq!(root.children_len(), 2);

        let child_0 = btree.load_node(root.child(0));
        assert_eq!(child_0.node_type(), NodeType::Leaf);
        assert_eq!(
            child_0.entries(btree.memory()),
            vec![e(1), e(2), e(4), e(5), e(6)]
        );

        let child_1 = btree.load_node(root.child(1));
        assert_eq!(child_1.node_type(), NodeType::Leaf);
        assert_eq!(
            child_1.entries(btree.memory()),
            vec![e(8), e(9), e(10), e(11), e(12)]
        );

        // There are three allocated nodes.
        assert_eq!(btree.allocator.num_allocated_chunks(), 3);
    }

    #[test]
    fn remove_case_3a_left() {
        let mem = make_memory();
        let mut btree = BTreeMap::new(mem);

        for i in 1..=11 {
            assert_eq!(btree.insert(b(&[i]), b(&[])), None);
        }
        // Should now split a node.
        assert_eq!(btree.insert(b(&[0]), b(&[])), None);

        // The result should look like this:
        //                   [6]
        //                  /   \
        // [0, 1, 2, 3, 4, 5]   [7, 8, 9, 10, 11]

        // Remove node 8. Triggers case 3.a left
        assert_eq!(btree.remove(&b(&[8])), Some(b(&[])));

        // The result should look like this:
        //                [5]
        //               /   \
        // [0, 1, 2, 3, 4]   [6, 7, 9, 10, 11]
        let root = btree.load_node(btree.root_addr);
        assert_eq!(root.node_type(), NodeType::Internal);
        assert_eq!(root.entries(btree.memory()), vec![e(5)]);
        assert_eq!(root.children_len(), 2);

        let child_0 = btree.load_node(root.child(0));
        assert_eq!(child_0.node_type(), NodeType::Leaf);
        assert_eq!(
            child_0.entries(btree.memory()),
            vec![e(0), e(1), e(2), e(3), e(4)]
        );

        let child_1 = btree.load_node(root.child(1));
        assert_eq!(child_1.node_type(), NodeType::Leaf);
        assert_eq!(
            child_1.entries(btree.memory()),
            vec![e(6), e(7), e(9), e(10), e(11)]
        );

        // There are three allocated nodes.
        assert_eq!(btree.allocator.num_allocated_chunks(), 3);
    }

    #[test]
    fn remove_case_3b_merge_into_right() {
        let mem = make_memory();
        let mut btree = BTreeMap::new(mem.clone());

        for i in 1..=11 {
            assert_eq!(btree.insert(b(&[i]), b(&[])), None);
        }
        // Should now split a node.
        assert_eq!(btree.insert(b(&[12]), b(&[])), None);

        // The result should look like this:
        //                [6]
        //               /   \
        // [1, 2, 3, 4, 5]   [7, 8, 9, 10, 11, 12]

        for i in 1..=12 {
            assert_eq!(btree.get(&b(&[i])), Some(b(&[])));
        }

        // Remove node 6. Triggers case 2.b
        assert_eq!(btree.remove(&b(&[6])), Some(b(&[])));
        // The result should look like this:
        //                [7]
        //               /   \
        // [1, 2, 3, 4, 5]   [8, 9, 10, 11, 12]
        let root = btree.load_node(btree.root_addr);
        assert_eq!(root.node_type(), NodeType::Internal);
        assert_eq!(root.entries(btree.memory()), vec![e(7)]);
        assert_eq!(root.children_len(), 2);

        let child_0 = btree.load_node(root.child(0));
        assert_eq!(child_0.node_type(), NodeType::Leaf);
        assert_eq!(
            child_0.entries(btree.memory()),
            vec![e(1), e(2), e(3), e(4), e(5)]
        );

        let child_1 = btree.load_node(root.child(1));
        assert_eq!(child_1.node_type(), NodeType::Leaf);
        assert_eq!(
            child_1.entries(btree.memory()),
            vec![e(8), e(9), e(10), e(11), e(12)]
        );

        // There are three allocated nodes.
        assert_eq!(btree.allocator.num_allocated_chunks(), 3);

        // Remove node 3. Triggers case 3.b
        assert_eq!(btree.remove(&b(&[3])), Some(b(&[])));

        // Reload the btree to verify that we saved it correctly.
        let btree = BTreeMap::<Blob<10>, Blob<10>, _>::load(mem);

        // The result should look like this:
        //
        // [1, 2, 4, 5, 7, 8, 9, 10, 11, 12]
        let root = btree.load_node(btree.root_addr);
        assert_eq!(root.node_type(), NodeType::Leaf);
        assert_eq!(
            root.entries(btree.memory()),
            vec![
                e(1),
                e(2),
                e(4),
                e(5),
                e(7),
                e(8),
                e(9),
                e(10),
                e(11),
                e(12)
            ]
        );

        // There is only one allocated node remaining.
        assert_eq!(btree.allocator.num_allocated_chunks(), 1);
    }

    #[test]
    fn remove_case_3b_merge_into_left() {
        let mem = make_memory();
        let mut btree = BTreeMap::new(mem.clone());

        for i in 1..=11 {
            assert_eq!(btree.insert(b(&[i]), b(&[])), None);
        }

        // Should now split a node.
        assert_eq!(btree.insert(b(&[12]), b(&[])), None);

        // The result should look like this:
        //                [6]
        //               /   \
        // [1, 2, 3, 4, 5]   [7, 8, 9, 10, 11, 12]

        for i in 1..=12 {
            assert_eq!(btree.get(&b(&[i])), Some(b(&[])));
        }

        // Remove node 6. Triggers case 2.b
        assert_eq!(btree.remove(&b(&[6])), Some(b(&[])));

        // The result should look like this:
        //                [7]
        //               /   \
        // [1, 2, 3, 4, 5]   [8, 9, 10, 11, 12]
        let root = btree.load_node(btree.root_addr);
        assert_eq!(root.node_type(), NodeType::Internal);
        assert_eq!(root.entries(btree.memory()), vec![e(7)]);
        assert_eq!(root.children_len(), 2);

        let child_0 = btree.load_node(root.child(0));
        assert_eq!(child_0.node_type(), NodeType::Leaf);
        assert_eq!(
            child_0.entries(btree.memory()),
            vec![e(1), e(2), e(3), e(4), e(5)]
        );

        let child_1 = btree.load_node(root.child(1));
        assert_eq!(child_1.node_type(), NodeType::Leaf);
        assert_eq!(
            child_1.entries(btree.memory()),
            vec![e(8), e(9), e(10), e(11), e(12)]
        );

        // There are three allocated nodes.
        assert_eq!(btree.allocator.num_allocated_chunks(), 3);

        // Remove node 10. Triggers case 3.b where we merge the right into the left.
        assert_eq!(btree.remove(&b(&[10])), Some(b(&[])));

        // Reload the btree to verify that we saved it correctly.
        let btree = BTreeMap::<Blob<10>, Blob<10>, _>::load(mem);

        // The result should look like this:
        //
        // [1, 2, 3, 4, 5, 7, 8, 9, 11, 12]
        let root = btree.load_node(btree.root_addr);
        assert_eq!(root.node_type(), NodeType::Leaf);
        assert_eq!(
            root.entries(btree.memory()),
            vec![e(1), e(2), e(3), e(4), e(5), e(7), e(8), e(9), e(11), e(12)]
        );

        // There is only one allocated node remaining.
        assert_eq!(btree.allocator.num_allocated_chunks(), 1);
    }

    #[test]
    fn many_insertions() {
        let mem = make_memory();
        let mut btree = BTreeMap::new(mem.clone());

        for j in 0..=10 {
            for i in 0..=255 {
                assert_eq!(btree.insert(b(&[i, j]), b(&[i, j])), None);
            }
        }

        for j in 0..=10 {
            for i in 0..=255 {
                assert_eq!(btree.get(&b(&[i, j])), Some(b(&[i, j])));
            }
        }

        let mut btree = BTreeMap::load(mem);

        for j in 0..=10 {
            for i in 0..=255 {
                assert_eq!(btree.remove(&b(&[i, j])), Some(b(&[i, j])));
            }
        }

        for j in 0..=10 {
            for i in 0..=255 {
                assert_eq!(btree.get(&b(&[i, j])), None);
            }
        }

        // We've deallocated everything.
        assert_eq!(btree.allocator.num_allocated_chunks(), 0);
    }

    #[test]
    fn many_insertions_2() {
        let mem = make_memory();
        let mut btree = BTreeMap::new(mem.clone());

        for j in (0..=10).rev() {
            for i in (0..=255).rev() {
                assert_eq!(btree.insert(b(&[i, j]), b(&[i, j])), None);
            }
        }

        for j in 0..=10 {
            for i in 0..=255 {
                assert_eq!(btree.get(&b(&[i, j])), Some(b(&[i, j])));
            }
        }

        let mut btree = BTreeMap::load(mem);

        for j in (0..=10).rev() {
            for i in (0..=255).rev() {
                assert_eq!(btree.remove(&b(&[i, j])), Some(b(&[i, j])));
            }
        }

        for j in 0..=10 {
            for i in 0..=255 {
                assert_eq!(btree.get(&b(&[i, j])), None);
            }
        }

        // We've deallocated everything.
        assert_eq!(btree.allocator.num_allocated_chunks(), 0);
    }

    #[test]
    fn reloading() {
        let mem = make_memory();
        let mut btree = BTreeMap::new(mem.clone());

        // The btree is initially empty.
        assert_eq!(btree.len(), 0);
        assert!(btree.is_empty());

        // Add an entry into the btree.
        assert_eq!(btree.insert(b(&[1, 2, 3]), b(&[4, 5, 6])), None);
        assert_eq!(btree.len(), 1);
        assert!(!btree.is_empty());

        // Reload the btree. The element should still be there, and `len()`
        // should still be `1`.
        let btree = BTreeMap::load(mem.clone());
        assert_eq!(btree.get(&b(&[1, 2, 3])), Some(b(&[4, 5, 6])));
        assert_eq!(btree.len(), 1);
        assert!(!btree.is_empty());

        // Remove an element. Length should be zero.
        let mut btree = BTreeMap::load(mem.clone());
        assert_eq!(btree.remove(&b(&[1, 2, 3])), Some(b(&[4, 5, 6])));
        assert_eq!(btree.len(), 0);
        assert!(btree.is_empty());

        // Reload. Btree should still be empty.
        let btree = BTreeMap::<Blob<10>, Blob<10>, _>::load(mem);
        assert_eq!(btree.get(&b(&[1, 2, 3])), None);
        assert_eq!(btree.len(), 0);
        assert!(btree.is_empty());
    }

    #[test]
    fn len() {
        let mem = make_memory();
        let mut btree = BTreeMap::new(mem);

        for i in 0..1000u32 {
            assert_eq!(btree.insert(b(i.to_le_bytes().as_slice()), b(&[])), None);
        }

        assert_eq!(btree.len(), 1000);
        assert!(!btree.is_empty());

        for i in 0..1000u32 {
            assert_eq!(btree.remove(&b(i.to_le_bytes().as_slice())), Some(b(&[])));
        }

        assert_eq!(btree.len(), 0);
        assert!(btree.is_empty());
    }

    #[test]
    fn contains_key() {
        let mem = make_memory();
        let mut btree = BTreeMap::new(mem);

        // Insert even numbers from 0 to 1000.
        for i in (0..1000u32).step_by(2) {
            assert_eq!(btree.insert(b(i.to_le_bytes().as_slice()), b(&[])), None);
        }

        // Contains key should return true on all the even numbers and false on all the odd
        // numbers.
        for i in 0..1000u32 {
            assert_eq!(
                btree.contains_key(&b(i.to_le_bytes().as_slice())),
                i % 2 == 0
            );
        }
    }

    #[test]
    fn range_empty() {
        let mem = make_memory();
        let btree = BTreeMap::<Blob<10>, Blob<10>, _>::new(mem);

        // Test prefixes that don't exist in the map.
        assert_eq!(btree.range(b(&[0])..).collect::<Vec<_>>(), vec![]);
        assert_eq!(btree.range(b(&[1, 2, 3, 4])..).collect::<Vec<_>>(), vec![]);
    }

    // Tests the case where the prefix is larger than all the entries in a leaf node.
    #[test]
    fn range_leaf_prefix_greater_than_all_entries() {
        let mem = make_memory();
        let mut btree = BTreeMap::new(mem);

        btree.insert(b(&[0]), b(&[]));

        // Test a prefix that's larger than the value in the leaf node. Should be empty.
        assert_eq!(btree.range(b(&[1])..).collect::<Vec<_>>(), vec![]);
    }

    // Tests the case where the prefix is larger than all the entries in an internal node.
    #[test]
    fn range_internal_prefix_greater_than_all_entries() {
        let mem = make_memory();
        let mut btree = BTreeMap::new(mem);

        for i in 1..=12 {
            assert_eq!(btree.insert(b(&[i]), b(&[])), None);
        }

        // The result should look like this:
        //                [6]
        //               /   \
        // [1, 2, 3, 4, 5]   [7, 8, 9, 10, 11, 12]

        // Test a prefix that's larger than the value in the internal node.
        assert_eq!(
            btree.range(b(&[7])..b(&[8])).collect::<Vec<_>>(),
            vec![(b(&[7]), b(&[]))]
        );
    }

    #[test]
    fn range_various_prefixes() {
        let mem = make_memory();
        let mut btree = BTreeMap::new(mem);

        btree.insert(b(&[0, 1]), b(&[]));
        btree.insert(b(&[0, 2]), b(&[]));
        btree.insert(b(&[0, 3]), b(&[]));
        btree.insert(b(&[0, 4]), b(&[]));
        btree.insert(b(&[1, 1]), b(&[]));
        btree.insert(b(&[1, 2]), b(&[]));
        btree.insert(b(&[1, 3]), b(&[]));
        btree.insert(b(&[1, 4]), b(&[]));
        btree.insert(b(&[2, 1]), b(&[]));
        btree.insert(b(&[2, 2]), b(&[]));
        btree.insert(b(&[2, 3]), b(&[]));
        btree.insert(b(&[2, 4]), b(&[]));

        // The result should look like this:
        //                                         [(1, 2)]
        //                                         /     \
        // [(0, 1), (0, 2), (0, 3), (0, 4), (1, 1)]       [(1, 3), (1, 4), (2, 1), (2, 2), (2, 3), (2, 4)]

        let root = btree.load_node(btree.root_addr);
        assert_eq!(root.node_type(), NodeType::Internal);
        assert_eq!(root.entries(btree.memory()), vec![(b(&[1, 2]), vec![])]);
        assert_eq!(root.children_len(), 2);

        // Tests a prefix that's smaller than the value in the internal node.
        assert_eq!(
            btree.range(b(&[0])..b(&[1])).collect::<Vec<_>>(),
            vec![
                (b(&[0, 1]), b(&[])),
                (b(&[0, 2]), b(&[])),
                (b(&[0, 3]), b(&[])),
                (b(&[0, 4]), b(&[])),
            ]
        );

        // Tests a prefix that crosses several nodes.
        assert_eq!(
            btree.range(b(&[1])..b(&[2])).collect::<Vec<_>>(),
            vec![
                (b(&[1, 1]), b(&[])),
                (b(&[1, 2]), b(&[])),
                (b(&[1, 3]), b(&[])),
                (b(&[1, 4]), b(&[])),
            ]
        );

        // Tests a prefix that's larger than the value in the internal node.
        assert_eq!(
            btree.range(b(&[2])..b(&[3])).collect::<Vec<_>>(),
            vec![
                (b(&[2, 1]), b(&[])),
                (b(&[2, 2]), b(&[])),
                (b(&[2, 3]), b(&[])),
                (b(&[2, 4]), b(&[])),
            ]
        );
    }

    #[test]
    fn range_various_prefixes_2() {
        let mem = make_memory();
        let mut btree = BTreeMap::new(mem);

        btree.insert(b(&[0, 1]), b(&[]));
        btree.insert(b(&[0, 2]), b(&[]));
        btree.insert(b(&[0, 3]), b(&[]));
        btree.insert(b(&[0, 4]), b(&[]));
        btree.insert(b(&[1, 2]), b(&[]));
        btree.insert(b(&[1, 4]), b(&[]));
        btree.insert(b(&[1, 6]), b(&[]));
        btree.insert(b(&[1, 8]), b(&[]));
        btree.insert(b(&[1, 10]), b(&[]));
        btree.insert(b(&[2, 1]), b(&[]));
        btree.insert(b(&[2, 2]), b(&[]));
        btree.insert(b(&[2, 3]), b(&[]));
        btree.insert(b(&[2, 4]), b(&[]));
        btree.insert(b(&[2, 5]), b(&[]));
        btree.insert(b(&[2, 6]), b(&[]));
        btree.insert(b(&[2, 7]), b(&[]));
        btree.insert(b(&[2, 8]), b(&[]));
        btree.insert(b(&[2, 9]), b(&[]));

        // The result should look like this:
        //                                         [(1, 4), (2, 3)]
        //                                         /      |       \
        // [(0, 1), (0, 2), (0, 3), (0, 4), (1, 2)]       |        [(2, 4), (2, 5), (2, 6), (2, 7), (2, 8), (2, 9)]
        //                                                |
        //                             [(1, 6), (1, 8), (1, 10), (2, 1), (2, 2)]
        let root = btree.load_node(btree.root_addr);
        assert_eq!(root.node_type(), NodeType::Internal);
        assert_eq!(
            root.entries(btree.memory()),
            vec![(b(&[1, 4]), vec![]), (b(&[2, 3]), vec![])]
        );
        assert_eq!(root.children_len(), 3);

        let child_0 = btree.load_node(root.child(0));
        assert_eq!(child_0.node_type(), NodeType::Leaf);
        assert_eq!(
            child_0.entries(btree.memory()),
            vec![
                (b(&[0, 1]), vec![]),
                (b(&[0, 2]), vec![]),
                (b(&[0, 3]), vec![]),
                (b(&[0, 4]), vec![]),
                (b(&[1, 2]), vec![]),
            ]
        );

        let child_1 = btree.load_node(root.child(1));
        assert_eq!(child_1.node_type(), NodeType::Leaf);
        assert_eq!(
            child_1.entries(btree.memory()),
            vec![
                (b(&[1, 6]), vec![]),
                (b(&[1, 8]), vec![]),
                (b(&[1, 10]), vec![]),
                (b(&[2, 1]), vec![]),
                (b(&[2, 2]), vec![]),
            ]
        );

        let child_2 = btree.load_node(root.child(2));
        assert_eq!(
            child_2.entries(btree.memory()),
            vec![
                (b(&[2, 4]), vec![]),
                (b(&[2, 5]), vec![]),
                (b(&[2, 6]), vec![]),
                (b(&[2, 7]), vec![]),
                (b(&[2, 8]), vec![]),
                (b(&[2, 9]), vec![]),
            ]
        );

        // Tests a prefix that doesn't exist, but is in the middle of the root node.
        assert_eq!(
            btree.range(b(&[1, 5])..b(&[1, 6])).collect::<Vec<_>>(),
            vec![]
        );

        // Tests a prefix that crosses several nodes.
        assert_eq!(
            btree.range(b(&[1])..b(&[2])).collect::<Vec<_>>(),
            vec![
                (b(&[1, 2]), b(&[])),
                (b(&[1, 4]), b(&[])),
                (b(&[1, 6]), b(&[])),
                (b(&[1, 8]), b(&[])),
                (b(&[1, 10]), b(&[])),
            ]
        );

        // Tests a prefix that starts from a leaf node, then iterates through the root and right
        // sibling.
        assert_eq!(
            btree.range(b(&[2])..).collect::<Vec<_>>(),
            vec![
                (b(&[2, 1]), b(&[])),
                (b(&[2, 2]), b(&[])),
                (b(&[2, 3]), b(&[])),
                (b(&[2, 4]), b(&[])),
                (b(&[2, 5]), b(&[])),
                (b(&[2, 6]), b(&[])),
                (b(&[2, 7]), b(&[])),
                (b(&[2, 8]), b(&[])),
                (b(&[2, 9]), b(&[])),
            ]
        );
    }

    #[test]
    fn range_large() {
        let mem = make_memory();
        let mut btree = BTreeMap::<Blob<10>, Blob<10>, _>::new(mem);

        // Insert 1000 elements with prefix 0 and another 1000 elements with prefix 1.
        for prefix in 0..=1 {
            for i in 0..1000u32 {
                assert_eq!(
                    btree.insert(
                        // The key is the prefix followed by the integer's encoding.
                        // The encoding is big-endian so that the byte representation of the
                        // integers are sorted.
                        b([vec![prefix], i.to_be_bytes().to_vec()]
                            .into_iter()
                            .flatten()
                            .collect::<Vec<_>>()
                            .as_slice()),
                        b(&[])
                    ),
                    None
                );
            }
        }

        // Getting the range with a prefix should return all 1000 elements with that prefix.
        for prefix in 0..=1 {
            let mut i: u32 = 0;
            for (key, _) in btree.range(b(&[prefix])..b(&[prefix + 1])) {
                assert_eq!(
                    key,
                    b(&[vec![prefix], i.to_be_bytes().to_vec()]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>())
                );
                i += 1;
            }
            assert_eq!(i, 1000);
        }
    }

    #[test]
    fn range_various_prefixes_with_offset() {
        let mem = make_memory();
        let mut btree = BTreeMap::new(mem);

        btree.insert(b(&[0, 1]), b(&[]));
        btree.insert(b(&[0, 2]), b(&[]));
        btree.insert(b(&[0, 3]), b(&[]));
        btree.insert(b(&[0, 4]), b(&[]));
        btree.insert(b(&[1, 1]), b(&[]));
        btree.insert(b(&[1, 2]), b(&[]));
        btree.insert(b(&[1, 3]), b(&[]));
        btree.insert(b(&[1, 4]), b(&[]));
        btree.insert(b(&[2, 1]), b(&[]));
        btree.insert(b(&[2, 2]), b(&[]));
        btree.insert(b(&[2, 3]), b(&[]));
        btree.insert(b(&[2, 4]), b(&[]));

        // The result should look like this:
        //                                         [(1, 2)]
        //                                         /     \
        // [(0, 1), (0, 2), (0, 3), (0, 4), (1, 1)]       [(1, 3), (1, 4), (2, 1), (2, 2), (2, 3), (2, 4)]

        let root = btree.load_node(btree.root_addr);
        assert_eq!(root.node_type(), NodeType::Internal);
        assert_eq!(root.entries(btree.memory()), vec![(b(&[1, 2]), vec![])]);
        assert_eq!(root.children_len(), 2);

        assert_eq!(
            btree.range(b(&[0])..b(&[1])).collect::<Vec<_>>(),
            vec![
                (b(&[0, 1]), b(&[])),
                (b(&[0, 2]), b(&[])),
                (b(&[0, 3]), b(&[])),
                (b(&[0, 4]), b(&[])),
            ]
        );

        // Tests a offset that has a value somewhere in the range of values of an internal node.
        assert_eq!(
            btree.range(b(&[1, 3])..b(&[2])).collect::<Vec<_>>(),
            vec![(b(&[1, 3]), b(&[])), (b(&[1, 4]), b(&[])),]
        );

        // Tests a offset that's larger than the value in the internal node.
        assert_eq!(btree.range(b(&[2, 5])..).collect::<Vec<_>>(), vec![]);
    }

    #[test]
    fn range_various_prefixes_with_offset_2() {
        let mem = make_memory();
        let mut btree = BTreeMap::new(mem);

        btree.insert(b(&[0, 1]), b(&[]));
        btree.insert(b(&[0, 2]), b(&[]));
        btree.insert(b(&[0, 3]), b(&[]));
        btree.insert(b(&[0, 4]), b(&[]));
        btree.insert(b(&[1, 2]), b(&[]));
        btree.insert(b(&[1, 4]), b(&[]));
        btree.insert(b(&[1, 6]), b(&[]));
        btree.insert(b(&[1, 8]), b(&[]));
        btree.insert(b(&[1, 10]), b(&[]));
        btree.insert(b(&[2, 1]), b(&[]));
        btree.insert(b(&[2, 2]), b(&[]));
        btree.insert(b(&[2, 3]), b(&[]));
        btree.insert(b(&[2, 4]), b(&[]));
        btree.insert(b(&[2, 5]), b(&[]));
        btree.insert(b(&[2, 6]), b(&[]));
        btree.insert(b(&[2, 7]), b(&[]));
        btree.insert(b(&[2, 8]), b(&[]));
        btree.insert(b(&[2, 9]), b(&[]));

        // The result should look like this:
        //                                         [(1, 4), (2, 3)]
        //                                         /      |       \
        // [(0, 1), (0, 2), (0, 3), (0, 4), (1, 2)]       |        [(2, 4), (2, 5), (2, 6), (2, 7), (2, 8), (2, 9)]
        //                                                |
        //                             [(1, 6), (1, 8), (1, 10), (2, 1), (2, 2)]
        let root = btree.load_node(btree.root_addr);
        assert_eq!(root.node_type(), NodeType::Internal);
        assert_eq!(
            root.entries(btree.memory()),
            vec![(b(&[1, 4]), vec![]), (b(&[2, 3]), vec![])]
        );
        assert_eq!(root.children_len(), 3);

        let child_0 = btree.load_node(root.child(0));
        assert_eq!(child_0.node_type(), NodeType::Leaf);
        assert_eq!(
            child_0.entries(btree.memory()),
            vec![
                (b(&[0, 1]), vec![]),
                (b(&[0, 2]), vec![]),
                (b(&[0, 3]), vec![]),
                (b(&[0, 4]), vec![]),
                (b(&[1, 2]), vec![]),
            ]
        );

        let child_1 = btree.load_node(root.child(1));
        assert_eq!(child_1.node_type(), NodeType::Leaf);
        assert_eq!(
            child_1.entries(btree.memory()),
            vec![
                (b(&[1, 6]), vec![]),
                (b(&[1, 8]), vec![]),
                (b(&[1, 10]), vec![]),
                (b(&[2, 1]), vec![]),
                (b(&[2, 2]), vec![]),
            ]
        );

        let child_2 = btree.load_node(root.child(2));
        assert_eq!(
            child_2.entries(btree.memory()),
            vec![
                (b(&[2, 4]), vec![]),
                (b(&[2, 5]), vec![]),
                (b(&[2, 6]), vec![]),
                (b(&[2, 7]), vec![]),
                (b(&[2, 8]), vec![]),
                (b(&[2, 9]), vec![]),
            ]
        );

        // Tests a offset that crosses several nodes.
        assert_eq!(
            btree.range(b(&[1, 4])..b(&[2])).collect::<Vec<_>>(),
            vec![
                (b(&[1, 4]), b(&[])),
                (b(&[1, 6]), b(&[])),
                (b(&[1, 8]), b(&[])),
                (b(&[1, 10]), b(&[])),
            ]
        );

        // Tests a offset that starts from a leaf node, then iterates through the root and right
        // sibling.
        assert_eq!(
            btree.range(b(&[2, 2])..b(&[3])).collect::<Vec<_>>(),
            vec![
                (b(&[2, 2]), b(&[])),
                (b(&[2, 3]), b(&[])),
                (b(&[2, 4]), b(&[])),
                (b(&[2, 5]), b(&[])),
                (b(&[2, 6]), b(&[])),
                (b(&[2, 7]), b(&[])),
                (b(&[2, 8]), b(&[])),
                (b(&[2, 9]), b(&[])),
            ]
        );
    }

    #[test]
    #[should_panic(expected = "max_key_size must be <= 4")]
    fn rejects_larger_key_sizes() {
        let mem = make_memory();
        let btree: BTreeMap<Blob<4>, Blob<3>, _> = BTreeMap::init(mem);
        let _btree: BTreeMap<Blob<5>, Blob<3>, _> = BTreeMap::init(btree.into_memory());
    }

    #[test]
    fn accepts_small_or_equal_key_sizes() {
        let mem = make_memory();
        let btree: BTreeMap<Blob<4>, Blob<3>, _> = BTreeMap::init(mem);
        // Smaller key size
        let btree: BTreeMap<Blob<3>, Blob<3>, _> = BTreeMap::init(btree.into_memory());
        // Equal key size
        let _btree: BTreeMap<Blob<4>, Blob<3>, _> = BTreeMap::init(btree.into_memory());
    }

    #[test]
    #[should_panic(expected = "max_value_size must be <= 3")]
    fn rejects_larger_value_sizes() {
        let mem = make_memory();
        let btree: BTreeMap<Blob<4>, Blob<3>, _> = BTreeMap::init(mem);
        let _btree: BTreeMap<Blob<4>, Blob<4>, _> = BTreeMap::init(btree.into_memory());
    }

    #[test]
    fn accepts_small_or_equal_value_sizes() {
        let mem = make_memory();
        let btree: BTreeMap<Blob<4>, Blob<3>, _> = BTreeMap::init(mem);
        // Smaller key size
        let btree: BTreeMap<Blob<4>, Blob<2>, _> = BTreeMap::init(btree.into_memory());
        // Equal key size
        let _btree: BTreeMap<Blob<4>, Blob<3>, _> = BTreeMap::init(btree.into_memory());
    }

    #[test]
    fn bruteforce_range_search() {
        use super::BTreeMap as StableBTreeMap;
        use std::collections::BTreeMap;

        const NKEYS: u64 = 60;

        let mut std_map = BTreeMap::new();
        let mut stable_map = StableBTreeMap::new(make_memory());

        for k in 0..NKEYS {
            std_map.insert(k, k);
            stable_map.insert(k, k);
        }

        assert_eq!(
            std_map.range(..).map(|(k, v)| (*k, *v)).collect::<Vec<_>>(),
            stable_map.range(..).collect::<Vec<_>>()
        );

        for l in 0..=NKEYS {
            assert_eq!(
                std_map
                    .range(l..)
                    .map(|(k, v)| (*k, *v))
                    .collect::<Vec<_>>(),
                stable_map.range(l..).collect::<Vec<_>>()
            );

            assert_eq!(
                std_map
                    .range(..l)
                    .map(|(k, v)| (*k, *v))
                    .collect::<Vec<_>>(),
                stable_map.range(..l).collect::<Vec<_>>()
            );

            assert_eq!(
                std_map
                    .range(..=l)
                    .map(|(k, v)| (*k, *v))
                    .collect::<Vec<_>>(),
                stable_map.range(..=l).collect::<Vec<_>>()
            );

            for r in l + 1..=NKEYS {
                for lbound in [Bound::Included(l), Bound::Excluded(l)] {
                    for rbound in [Bound::Included(r), Bound::Excluded(r)] {
                        let range = (lbound, rbound);
                        assert_eq!(
                            std_map
                                .range(range)
                                .map(|(k, v)| (*k, *v))
                                .collect::<Vec<_>>(),
                            stable_map.range(range).collect::<Vec<_>>(),
                            "range: {range:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn test_iter_upper_bound() {
        let mut stable_map = super::BTreeMap::new(make_memory());
        for k in 0..100u64 {
            stable_map.insert(k, ());
            for i in 0..=k {
                assert_eq!(
                    Some((i, ())),
                    stable_map.iter_upper_bound(&(i + 1)).next(),
                    "failed to get an upper bound for key {}",
                    i + 1
                );
            }
            assert_eq!(
                None,
                stable_map.iter_upper_bound(&0).next(),
                "key 0 must not have an upper bound"
            );
        }
    }

    #[test]
    #[should_panic(expected = "Key is too large. Expected <= 0 bytes, found 4 bytes")]
    fn panics_if_key_is_too_large() {
        #[derive(Clone, Ord, PartialOrd, Eq, PartialEq)]
        struct K;
        impl crate::Storable for K {
            fn to_bytes(&self) -> Cow<[u8]> {
                Cow::Borrowed(&[1, 2, 3, 4])
            }

            fn from_bytes(_: Cow<[u8]>) -> Self {
                unimplemented!();
            }

            // A buggy implementation where the max_size is smaller than what Storable::to_bytes()
            // returns.
            const BOUND: StorableBound = StorableBound::Bounded {
                max_size: 0,
                is_fixed_size: false,
            };
        }

        let mut btree: BTreeMap<K, (), _> = BTreeMap::init(make_memory());
        btree.insert(K, ());
    }

    #[test]
    #[should_panic(expected = "Value is too large. Expected <= 0 bytes, found 4 bytes")]
    fn panics_if_value_is_too_large() {
        #[derive(Clone, Ord, PartialOrd, Eq, PartialEq)]
        struct V;
        impl crate::Storable for V {
            fn to_bytes(&self) -> Cow<[u8]> {
                Cow::Borrowed(&[1, 2, 3, 4])
            }

            fn from_bytes(_: Cow<[u8]>) -> Self {
                unimplemented!();
            }

            // A buggy implementation where the max_size is smaller than what Storable::to_bytes()
            // returns.
            const BOUND: StorableBound = StorableBound::Bounded {
                max_size: 0,
                is_fixed_size: false,
            };
        }

        let mut btree: BTreeMap<(), V, _> = BTreeMap::init(make_memory());
        btree.insert((), V);
    }

    // To generate the memory dump file for the current version:
    //   cargo test create_btreemap_dump_file -- --include-ignored
    #[test]
    #[ignore]
    fn create_btreemap_dump_file() {
        let mem = make_memory();
        let mut btree = BTreeMap::init(mem.clone());
        assert_eq!(btree.insert(b(&[1, 2, 3]), b(&[4, 5, 6])), None);
        assert_eq!(btree.get(&b(&[1, 2, 3])), Some(b(&[4, 5, 6])));

        use std::io::prelude::*;
        let mut file =
            std::fs::File::create(format!("dumps/btreemap_v{LAYOUT_VERSION}.dump")).unwrap();
        file.write_all(&mem.borrow()).unwrap();
    }

    #[test]
    fn produces_layout_identical_to_layout_version_1_with_packed_headers() {
        let mem = make_memory();
        let mut btree = BTreeMap::init(mem.clone());
        assert_eq!(btree.insert(b(&[1, 2, 3]), b(&[4, 5, 6])), None);
        assert_eq!(btree.get(&b(&[1, 2, 3])), Some(b(&[4, 5, 6])));

        let btreemap_v1 = include_bytes!("../dumps/btreemap_v1_packed_headers.dump");
        assert_eq!(*mem.borrow(), btreemap_v1);
    }

    #[test]
    fn read_write_header_is_identical_to_read_write_struct() {
        #[repr(C, packed)]
        struct BTreePackedHeader {
            magic: [u8; 3],
            version: u8,
            max_key_size: u32,
            max_value_size: u32,
            root_addr: Address,
            length: u64,
            _buffer: [u8; 24],
        }
        let packed_header = BTreePackedHeader {
            magic: *MAGIC,
            version: LAYOUT_VERSION,
            root_addr: Address::from(0xDEADBEEF),
            max_key_size: 0x12345678,
            max_value_size: 0x87654321,
            length: 0xA1B2D3C4,
            _buffer: [0; 24],
        };

        let packed_mem = make_memory();
        crate::write_struct(&packed_header, Address::from(0), &packed_mem);

        let v1_header = BTreeHeaderV1 {
            magic: *MAGIC,
            version: LAYOUT_VERSION,
            max_key_size: 0x12345678,
            max_value_size: 0x87654321,
            root_addr: Address::from(0xDEADBEEF),
            length: 0xA1B2D3C4,
        };

        let v1_mem = make_memory();
        BTreeMap::<Vec<_>, Vec<_>, RefCell<Vec<_>>>::write_header(&v1_header, &v1_mem);

        assert_eq!(packed_mem, v1_mem);

        let packed_header: BTreePackedHeader = crate::read_struct(Address::from(0), &v1_mem);
        let v1_header = BTreeMap::<Vec<_>, Vec<_>, RefCell<Vec<_>>>::read_header(&v1_mem);
        assert!(packed_header.magic == v1_header.magic);
        assert!(packed_header.version == v1_header.version);
        assert!(packed_header.max_key_size == v1_header.max_key_size);
        assert!(packed_header.max_value_size == v1_header.max_value_size);
        assert!(packed_header.root_addr == v1_header.root_addr);
        assert!(packed_header.length == v1_header.length);
    }
}
