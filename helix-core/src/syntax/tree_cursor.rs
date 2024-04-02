use std::{cmp::Reverse, collections::VecDeque, ops::Range};

use super::{LanguageLayer, LayerId};

use ropey::RopeSlice;
use slotmap::HopSlotMap;
use tree_sitter::Node;

/// The byte range of an injection layer.
///
/// Injection ranges may overlap, but all overlapping parts are subsets of their parent ranges.
/// This allows us to sort the ranges ahead of time in order to efficiently find a range that
/// contains a point with maximum depth.
#[derive(Debug)]
struct InjectionRange {
    start: usize,
    end: usize,
    layer_id: LayerId,
    depth: u32,
}

pub struct TreeCursor<'n> {
    layers: &'n HopSlotMap<LayerId, LanguageLayer>,
    root: LayerId,
    current: LayerId,
    injection_ranges: Vec<InjectionRange>,
    // TODO: Ideally this would be a `tree_sitter::TreeCursor<'a>` but
    // that returns very surprising results in testing.
    cursor: Node<'n>,
}

impl<'n> TreeCursor<'n> {
    pub(super) fn new(layers: &'n HopSlotMap<LayerId, LanguageLayer>, root: LayerId) -> Self {
        let mut injection_ranges = Vec::new();

        for (layer_id, layer) in layers.iter() {
            // Skip the root layer
            if layer.parent.is_none() {
                continue;
            }
            for byte_range in layer.ranges.iter() {
                let range = InjectionRange {
                    start: byte_range.start_byte,
                    end: byte_range.end_byte,
                    layer_id,
                    depth: layer.depth,
                };
                injection_ranges.push(range);
            }
        }

        injection_ranges.sort_unstable_by_key(|range| (range.end, Reverse(range.depth)));

        let cursor = layers[root].tree().root_node();

        Self {
            layers,
            root,
            current: root,
            injection_ranges,
            cursor,
        }
    }

    pub fn node(&self) -> Node<'n> {
        self.cursor
    }

    pub fn goto_parent(&mut self) -> bool {
        if let Some(parent) = self.node().parent() {
            self.cursor = parent;
            return true;
        }

        // If we are already on the root layer, we cannot ascend.
        if self.current == self.root {
            return false;
        }

        // Ascend to the parent layer.
        let range = self.node().byte_range();
        let parent_id = self.layers[self.current]
            .parent
            .expect("non-root layers have a parent");
        self.current = parent_id;
        let root = self.layers[self.current].tree().root_node();
        self.cursor = root
            .descendant_for_byte_range(range.start, range.end)
            .unwrap_or(root);

        true
    }

    /// Finds the injection layer that has exactly the same range as the given `range`.
    fn layer_id_of_byte_range(&self, search_range: Range<usize>) -> Option<LayerId> {
        let start_idx = self
            .injection_ranges
            .partition_point(|range| range.end < search_range.end);

        self.injection_ranges[start_idx..]
            .iter()
            .take_while(|range| range.end == search_range.end)
            .find_map(|range| (range.start == search_range.start).then_some(range.layer_id))
    }

    pub fn goto_first_child(&mut self) -> bool {
        // Check if the current node's range is an exact injection layer range.
        if let Some(layer_id) = self
            .layer_id_of_byte_range(self.node().byte_range())
            .filter(|&layer_id| layer_id != self.current)
        {
            // Switch to the child layer.
            self.current = layer_id;
            self.cursor = self.layers[self.current].tree().root_node();
            true
        } else if let Some(child) = self.cursor.child(0) {
            // Otherwise descend in the current tree.
            self.cursor = child;
            true
        } else {
            false
        }
    }

    pub fn goto_first_named_child(&mut self) -> bool {
        // Check if the current node's range is an exact injection layer range.
        if let Some(layer_id) = self
            .layer_id_of_byte_range(self.node().byte_range())
            .filter(|&layer_id| layer_id != self.current)
        {
            // Switch to the child layer.
            self.current = layer_id;
            self.cursor = self.layers[self.current].tree().root_node();
            true
        } else if let Some(child) = self.cursor.named_child(0) {
            // Otherwise descend in the current tree.
            self.cursor = child;
            true
        } else {
            false
        }
    }

    /// Finds the first child node that is contained "inside" the given input
    /// range, i.e. either start_new > start_old and end_new <= end old OR
    /// start_new >= start_old and end_new < end_old
    pub fn goto_first_contained_child(&'n mut self, range: &crate::Range, text: RopeSlice) -> bool {
        self.first_contained_child(range, text).is_some()
    }

    /// Finds the first child node that is contained "inside" the given input
    /// range, i.e. either start_new > start_old and end_new <= end old OR
    /// start_new >= start_old and end_new < end_old
    pub fn first_contained_child(
        &'n mut self,
        range: &crate::Range,
        text: RopeSlice,
    ) -> Option<Node<'n>> {
        let from = text.char_to_byte(range.from());
        let to = text.char_to_byte(range.to());

        self.into_iter().find(|&node| {
            (node.start_byte() > from && node.end_byte() <= to)
                || (node.start_byte() >= from && node.end_byte() < to)
        })
    }

    pub fn goto_next_sibling(&mut self) -> bool {
        if let Some(sibling) = self.cursor.next_sibling() {
            self.cursor = sibling;
            true
        } else {
            false
        }
    }

    pub fn goto_next_named_sibling(&mut self) -> bool {
        if let Some(sibling) = self.cursor.next_named_sibling() {
            self.cursor = sibling;
            true
        } else {
            false
        }
    }

    pub fn goto_prev_sibling(&mut self) -> bool {
        if let Some(sibling) = self.cursor.prev_sibling() {
            self.cursor = sibling;
            true
        } else {
            false
        }
    }

    pub fn goto_prev_named_sibling(&mut self) -> bool {
        if let Some(sibling) = self.cursor.prev_named_sibling() {
            self.cursor = sibling;
            true
        } else {
            false
        }
    }

    /// Finds the injection layer that contains the given start-end range.
    fn layer_id_containing_byte_range(&self, start: usize, end: usize) -> LayerId {
        let start_idx = self
            .injection_ranges
            .partition_point(|range| range.end < end);

        self.injection_ranges[start_idx..]
            .iter()
            .take_while(|range| range.start < end)
            .find_map(|range| (range.start <= start).then_some(range.layer_id))
            .unwrap_or(self.root)
    }

    pub fn reset_to_byte_range(&mut self, start: usize, end: usize) {
        self.current = self.layer_id_containing_byte_range(start, end);
        let root = self.layers[self.current].tree().root_node();
        self.cursor = root.descendant_for_byte_range(start, end).unwrap_or(root);
    }
}

impl<'n> IntoIterator for &'n mut TreeCursor<'n> {
    type Item = Node<'n>;
    type IntoIter = TreeRecursiveWalker<'n>;

    fn into_iter(self) -> Self::IntoIter {
        let mut queue = VecDeque::new();
        let root = self.node();
        queue.push_back(root);

        TreeRecursiveWalker {
            cursor: self,
            queue,
            root,
        }
    }
}

pub struct TreeRecursiveWalker<'n> {
    cursor: &'n mut TreeCursor<'n>,
    queue: VecDeque<Node<'n>>,
    root: Node<'n>,
}

impl<'n> Iterator for TreeRecursiveWalker<'n> {
    type Item = Node<'n>;

    fn next(&mut self) -> Option<Self::Item> {
        let current = self.cursor.node();
        log::debug!("recursive walk -- current: {current:?}");

        if current != self.root && self.cursor.goto_next_named_sibling() {
            self.queue.push_back(current);
            log::debug!("recursive walk -- sibling: {:?}", self.cursor.node());
            return Some(self.cursor.node());
        }

        while let Some(queued) = self.queue.pop_front() {
            self.cursor.cursor = queued;

            if !self.cursor.goto_first_named_child() {
                continue;
            }

            log::debug!("recursive walk -- child: {:?}", self.cursor.node());
            return Some(self.cursor.node());
        }

        None
    }
}
