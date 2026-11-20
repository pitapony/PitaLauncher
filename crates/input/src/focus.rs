//! The spatial focus graph shared by the desktop and console shells.
//!
//! A shell does not move a DOM focus and never runs JavaScript to navigate.
//! Instead every focusable component reports a cell — an id plus its `(row,
//! col)` position in the screen's logical layout — and the graph answers one
//! question: given the focused cell and a direction, which cell is next? That
//! keeps navigation unit-testable without a display (the plan's rule) and keeps
//! both shells walking the same rules.
//!
//! Two properties matter more than the scoring details:
//!
//! - **No traps.** A move must never leave the user with no way out. Same-
//!   row/column candidates are preferred, a diagonal fallback keeps a ragged
//!   grid reachable, and each axis wraps to its far end instead of dying at an
//!   edge.
//! - **Determinism.** Ties break on `(row, col, id)` and nodes are kept in
//!   layout order, so a key repeat walks a stable cycle and repeated presses on
//!   two identically built screens cannot diverge.
//!
//! Scroll-into-view and the focus ring are presentation, so they live in
//! `crates/app`; this crate only reports which cell is focused and which row it
//! occupies.

use std::fmt;

use crate::action::Action;

/// Stable identity of one focusable cell.
///
/// The value is chosen by the screen (the library grid uses the cell's index in
/// the filtered list) and only has to stay stable for the life of one layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FocusId(pub u64);

impl FocusId {
    /// Build an id from its numeric value.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The numeric value, for a screen that stores ids as indices.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for FocusId {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<usize> for FocusId {
    fn from(value: usize) -> Self {
        Self(value as u64)
    }
}

impl fmt::Display for FocusId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "focus-{}", self.0)
    }
}

/// One focusable cell in a screen's logical layout.
///
/// Rows run top to bottom and columns left to right; a shelf, a grid and a
/// dialog's button row are all the same shape to the resolver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FocusNode {
    pub id: FocusId,
    pub row: usize,
    pub col: usize,
}

impl FocusNode {
    /// A cell at `(row, col)`.
    pub const fn new(id: FocusId, row: usize, col: usize) -> Self {
        Self { id, row, col }
    }
}

/// A screen's focusable cells and the one that currently has focus.
///
/// Registering is cheap and idempotent, so a screen can rebuild the graph from
/// its current list on every render without losing which cell is focused (the
/// screen re-applies [`FocusGraph::focus`] afterwards, or uses
/// [`FocusGraph::grid`] with [`FocusGraph::focus_index`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FocusGraph {
    /// Always sorted by `(row, col, id)` so iteration and tie-breaks are stable.
    nodes: Vec<FocusNode>,
    current: Option<FocusId>,
}

impl FocusGraph {
    /// An empty graph with nothing focused.
    pub fn new() -> Self {
        Self::default()
    }

    /// A rectangular, row-major grid of `item_count` cells.
    ///
    /// The ids are the item indices, which is what the virtualized library grid
    /// needs: the focused index doubles as the list position to draw and scroll
    /// to. A zero column count is treated as one column, so a screen measured
    /// before its first resize still behaves.
    pub fn grid(item_count: usize, columns: usize) -> Self {
        let columns = columns.max(1);
        let nodes = (0..item_count)
            .map(|index| FocusNode {
                id: FocusId::from(index),
                row: index / columns,
                col: index % columns,
            })
            .collect();
        Self {
            nodes,
            current: None,
        }
    }

    /// Register one cell, replacing the coordinates of an existing id.
    pub fn insert(&mut self, id: impl Into<FocusId>, row: usize, col: usize) {
        let id = id.into();
        if let Some(node) = self.nodes.iter_mut().find(|node| node.id == id) {
            node.row = row;
            node.col = col;
        } else {
            self.nodes.push(FocusNode::new(id, row, col));
        }
        self.sort();
    }

    /// Drop every cell and the current focus.
    pub fn clear(&mut self) {
        self.nodes.clear();
        self.current = None;
    }

    /// How many cells the screen has.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the screen has nothing focusable.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The cells, in layout order.
    pub fn nodes(&self) -> &[FocusNode] {
        &self.nodes
    }

    /// The focused id, even if its cell has since disappeared from the layout.
    pub fn focused(&self) -> Option<FocusId> {
        self.current
    }

    /// The focused cell, when it is still registered.
    pub fn focused_node(&self) -> Option<FocusNode> {
        let id = self.current?;
        self.nodes.iter().copied().find(|node| node.id == id)
    }

    /// Focus `id`; returns `false` (leaving focus unchanged) when it is not a
    /// cell of this graph.
    pub fn focus(&mut self, id: impl Into<FocusId>) -> bool {
        let id = id.into();
        if self.nodes.iter().any(|node| node.id == id) {
            self.current = Some(id);
            true
        } else {
            false
        }
    }

    /// Focus a cell by its index in layout order.
    ///
    /// `None` (or an index past the end) clears the focus, which is what a
    /// screen does when its filtered list shrank under the focused cell.
    pub fn focus_index(&mut self, index: Option<usize>) -> bool {
        match index {
            Some(index) if index < self.nodes.len() => {
                self.current = Some(self.nodes[index].id);
                true
            }
            _ => {
                self.current = None;
                false
            }
        }
    }

    /// The focused cell's index in layout order, for a [`FocusGraph::grid`].
    ///
    /// A non-grid graph whose ids are arbitrary returns the id as an index;
    /// callers that need the cell should use [`FocusGraph::focused_node`].
    pub fn focused_index(&self) -> Option<usize> {
        let node = self.focused_node()?;
        usize::try_from(node.id.get()).ok()
    }

    /// Resolve a navigation action from the current cell without moving focus.
    ///
    /// Returns `None` for a non-navigation action, an empty graph, or nothing
    /// focused.
    pub fn resolve(&self, action: Action) -> Option<FocusNode> {
        let direction = Direction::from_action(action)?;
        let current = self.focused_node()?;
        self.resolve_from(current, direction)
    }

    /// Apply one action and return the focused id afterwards.
    ///
    /// The first navigation action on a screen with nothing focused focuses the
    /// first cell, so a controller or keyboard can enter the screen without a
    /// pointer. Non-navigation actions (`Confirm`, `Back`, …) leave focus alone
    /// and return the current id.
    pub fn move_focus(&mut self, action: Action) -> Option<FocusId> {
        if self.nodes.is_empty() {
            return None;
        }
        let Some(direction) = Direction::from_action(action) else {
            return self.current;
        };
        let Some(current) = self.focused_node() else {
            let first = self.nodes[0].id;
            self.current = Some(first);
            return Some(first);
        };
        if let Some(node) = self.resolve_from(current, direction) {
            self.current = Some(node.id);
        }
        self.current
    }

    /// Resolve one direction from `current`.
    ///
    /// Candidates in the direction are ranked by a stable key: a cell in the
    /// same row (or column) always beats a diagonal one, and among equals the
    /// primary distance dominates, then the perpendicular distance. Ties fall
    /// through to `(row, col, id)` so the answer never depends on insertion
    /// order.
    fn resolve_from(&self, current: FocusNode, direction: Direction) -> Option<FocusNode> {
        let mut best: Option<(SortKey, FocusNode)> = None;
        for node in &self.nodes {
            if node.id == current.id {
                continue;
            }
            let Some(key) = direction.key(current, *node) else {
                continue;
            };
            if best.is_none_or(|(best_key, _)| key < best_key) {
                best = Some((key, *node));
            }
        }
        if let Some((_, node)) = best {
            return Some(node);
        }
        self.wrap(current, direction)
    }

    /// Per-axis wrap: when there is no candidate in the direction, flip the
    /// moved axis to its far end and keep the other axis as close as possible.
    ///
    /// Moving right from the rightmost cell of a row lands on that row's
    /// leftmost cell; moving down from the bottom row lands on the top of the
    /// same column. A one-cell row or column has nowhere to go and returns
    /// `None` — that is a dead end, never a trap, because the other axis still
    /// moves.
    fn wrap(&self, current: FocusNode, direction: Direction) -> Option<FocusNode> {
        match direction {
            Direction::Right => self
                .nodes
                .iter()
                .filter(|node| node.row == current.row && node.col < current.col)
                .min_by_key(|node| (node.col, node.id))
                .copied(),
            Direction::Left => self
                .nodes
                .iter()
                .filter(|node| node.row == current.row && node.col > current.col)
                .max_by_key(|node| (node.col, node.id))
                .copied(),
            Direction::Down => self
                .nodes
                .iter()
                .filter(|node| node.col == current.col && node.row < current.row)
                .min_by_key(|node| (node.row, node.id))
                .copied(),
            Direction::Up => self
                .nodes
                .iter()
                .filter(|node| node.col == current.col && node.row > current.row)
                .max_by_key(|node| (node.row, node.id))
                .copied(),
        }
    }

    fn sort(&mut self) {
        self.nodes.sort_by_key(|node| (node.row, node.col, node.id));
    }
}

/// A ranked candidate: `(diagonal, score, perpendicular, primary, row, col,
/// id)`. Lower is better.
type SortKey = (u8, u64, u64, u64, u64, u64, u64);

/// A movement on one axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Up,
    Down,
    Left,
    Right,
}

impl Direction {
    fn from_action(action: Action) -> Option<Self> {
        match action {
            Action::NavigateUp => Some(Self::Up),
            Action::NavigateDown => Some(Self::Down),
            Action::NavigateLeft => Some(Self::Left),
            Action::NavigateRight => Some(Self::Right),
            _ => None,
        }
    }

    /// The ranking key for `node` seen from `from`, or `None` when `node` is
    /// not strictly in this direction.
    fn key(self, from: FocusNode, node: FocusNode) -> Option<SortKey> {
        let (primary, perpendicular) = match self {
            Self::Right => (
                node.col as i64 - from.col as i64,
                node.row as i64 - from.row as i64,
            ),
            Self::Left => (
                from.col as i64 - node.col as i64,
                node.row as i64 - from.row as i64,
            ),
            Self::Down => (
                node.row as i64 - from.row as i64,
                node.col as i64 - from.col as i64,
            ),
            Self::Up => (
                from.row as i64 - node.row as i64,
                node.col as i64 - from.col as i64,
            ),
        };
        if primary <= 0 {
            return None;
        }
        let straight = perpendicular == 0;
        let perpendicular = perpendicular.unsigned_abs();
        let primary = primary as u64;
        Some((
            u8::from(!straight),
            primary * 2 + perpendicular,
            perpendicular,
            primary,
            node.row as u64,
            node.col as u64,
            node.id.get(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn id(index: usize) -> FocusId {
        FocusId::from(index)
    }

    /// The ids one move from `id` in the four directions.
    fn moves_from(graph: &FocusGraph, from: FocusId) -> Vec<FocusId> {
        let mut out = Vec::new();
        for action in [
            Action::NavigateUp,
            Action::NavigateDown,
            Action::NavigateLeft,
            Action::NavigateRight,
        ] {
            let mut copy = graph.clone();
            assert!(copy.focus(from));
            if let Some(next) = copy.move_focus(action)
                && next != from
            {
                out.push(next);
            }
        }
        out
    }

    #[test]
    fn a_grid_lays_items_out_row_major() {
        let graph = FocusGraph::grid(7, 3);
        assert_eq!(graph.len(), 7);
        assert_eq!(
            graph.nodes(),
            &[
                FocusNode::new(id(0), 0, 0),
                FocusNode::new(id(1), 0, 1),
                FocusNode::new(id(2), 0, 2),
                FocusNode::new(id(3), 1, 0),
                FocusNode::new(id(4), 1, 1),
                FocusNode::new(id(5), 1, 2),
                FocusNode::new(id(6), 2, 0),
            ]
        );
    }

    #[test]
    fn a_zero_column_count_still_produces_a_single_column() {
        let graph = FocusGraph::grid(3, 0);
        assert_eq!(graph.nodes()[2], FocusNode::new(id(2), 2, 0));
    }

    #[test]
    fn movement_prefers_the_same_row_or_column() {
        let mut graph = FocusGraph::new();
        graph.insert(0usize, 0, 0); // origin
        graph.insert(1usize, 0, 1); // same row, one right
        graph.insert(2usize, 1, 0); // same column, one down
        graph.insert(3usize, 2, 1); // diagonal
        graph.focus(0usize);

        // Right: the straight cell wins even though the diagonal one is
        // reachable in the same half-plane.
        assert_eq!(graph.move_focus(Action::NavigateRight), Some(id(1)));

        // Down from the origin: the straight cell below wins.
        graph.focus(0usize);
        assert_eq!(graph.move_focus(Action::NavigateDown), Some(id(2)));

        // Up from the diagonal cell: the straight cell above wins over the
        // diagonal origin.
        graph.focus(3usize);
        assert_eq!(graph.move_focus(Action::NavigateUp), Some(id(1)));

        // Down from the straight cell: the only candidate below is diagonal.
        graph.focus(1usize);
        assert_eq!(graph.move_focus(Action::NavigateDown), Some(id(3)));
    }

    #[test]
    fn perpendicular_distance_breaks_a_diagonal_tie() {
        let mut graph = FocusGraph::new();
        graph.insert(0usize, 0, 0);
        graph.insert(1usize, 1, 1); // down one, right one
        graph.insert(2usize, 3, 2); // down three, right two
        graph.focus(0usize);
        // Both are diagonal; the nearer one wins on the penalized score even
        // though the far one is straighter.
        assert_eq!(graph.move_focus(Action::NavigateDown), Some(id(1)));
    }

    #[test]
    fn each_axis_wraps_to_its_far_end() {
        let mut graph = FocusGraph::grid(9, 3); // 3 x 3

        graph.focus(2usize); // top right
        assert_eq!(
            graph.move_focus(Action::NavigateRight),
            Some(id(0)),
            "right from the right edge wraps to that row's left cell"
        );
        graph.focus(0usize);
        assert_eq!(
            graph.move_focus(Action::NavigateLeft),
            Some(id(2)),
            "left from the left edge wraps to that row's right cell"
        );
        graph.focus(6usize); // bottom left
        assert_eq!(
            graph.move_focus(Action::NavigateDown),
            Some(id(0)),
            "down from the bottom wraps to the top of the same column"
        );
        graph.focus(0usize);
        assert_eq!(
            graph.move_focus(Action::NavigateUp),
            Some(id(6)),
            "up from the top wraps to the bottom of the same column"
        );
    }

    #[test]
    fn a_ragged_last_row_stays_reachable() {
        // 2 full rows of 3 plus one extra cell.
        let mut graph = FocusGraph::grid(7, 3);
        graph.focus(5usize); // row 1, col 2
        // No cell below col 2, so the diagonal fallback lands on the partial row.
        assert_eq!(graph.move_focus(Action::NavigateDown), Some(id(6)));
        // From there the only cell to the right is the one above it.
        assert_eq!(graph.move_focus(Action::NavigateRight), Some(id(4)));
        // Up still walks the column.
        graph.focus(6usize);
        assert_eq!(graph.move_focus(Action::NavigateUp), Some(id(3)));
    }

    #[test]
    fn every_cell_of_a_rectangular_grid_reaches_every_other() {
        for (count, columns) in [(12, 4), (20, 5), (9, 3), (6, 1), (5, 5)] {
            let graph = FocusGraph::grid(count, columns);
            for start in 0..count {
                let mut seen: BTreeSet<FocusId> = BTreeSet::new();
                let mut queue = vec![id(start)];
                seen.insert(id(start));
                while let Some(cell) = queue.pop() {
                    for next in moves_from(&graph, cell) {
                        if seen.insert(next) {
                            queue.push(next);
                        }
                    }
                }
                assert_eq!(
                    seen.len(),
                    count,
                    "from {start} in a {count}x{columns} grid only reached {} of {count}",
                    seen.len()
                );
            }
        }
    }

    #[test]
    fn repeat_is_a_stable_cycle() {
        let mut graph = FocusGraph::grid(10, 5);
        graph.focus(0usize);
        let mut walked = Vec::new();
        for _ in 0..10 {
            walked.push(graph.move_focus(Action::NavigateRight));
        }
        assert_eq!(
            walked,
            vec![
                Some(id(1)),
                Some(id(2)),
                Some(id(3)),
                Some(id(4)),
                Some(id(0)),
                Some(id(1)),
                Some(id(2)),
                Some(id(3)),
                Some(id(4)),
                Some(id(0)),
            ]
        );
    }

    #[test]
    fn insertion_order_does_not_change_the_answer() {
        let cells: [(usize, usize, usize); 5] =
            [(0, 0, 0), (1, 0, 1), (2, 1, 0), (3, 1, 1), (4, 2, 0)];

        let mut forward = FocusGraph::new();
        for (cell, row, col) in cells {
            forward.insert(cell, row, col);
        }

        let mut backward = FocusGraph::new();
        for (cell, row, col) in cells.into_iter().rev() {
            backward.insert(cell, row, col);
        }

        forward.focus(0usize);
        backward.focus(0usize);
        assert_eq!(
            forward.move_focus(Action::NavigateRight),
            backward.move_focus(Action::NavigateRight)
        );
    }

    #[test]
    fn the_first_navigation_focuses_the_first_cell() {
        let mut graph = FocusGraph::grid(4, 2);
        assert_eq!(graph.focused(), None);
        assert_eq!(graph.move_focus(Action::NavigateDown), Some(id(0)));
        assert_eq!(graph.focused(), Some(id(0)));
    }

    #[test]
    fn a_non_navigation_action_never_moves_focus() {
        let mut graph = FocusGraph::grid(4, 2);
        graph.focus(1usize);
        assert_eq!(graph.move_focus(Action::Confirm), Some(id(1)));
        assert_eq!(graph.move_focus(Action::Back), Some(id(1)));
        assert!(graph.focus(1usize));
    }

    #[test]
    fn an_empty_graph_has_nothing_to_focus() {
        let mut graph = FocusGraph::grid(0, 4);
        assert!(graph.is_empty());
        assert_eq!(graph.move_focus(Action::NavigateDown), None);
        assert!(!graph.focus(0usize));
        assert_eq!(graph.resolve(Action::NavigateUp), None);
    }

    #[test]
    fn focusing_an_unknown_id_is_refused() {
        let mut graph = FocusGraph::grid(3, 3);
        assert!(!graph.focus(9usize));
        assert_eq!(graph.focused(), None);
    }

    #[test]
    fn focus_index_clears_when_the_list_shrank() {
        let mut graph = FocusGraph::grid(5, 2);
        assert!(graph.focus_index(Some(3)));
        assert_eq!(graph.focused(), Some(id(3)));
        assert!(!graph.focus_index(Some(9)));
        assert_eq!(graph.focused(), None);
    }

    #[test]
    fn a_removed_focused_cell_falls_back_to_the_first() {
        let mut graph = FocusGraph::grid(4, 2);
        graph.focus(3usize);
        graph.clear();
        graph.insert(0usize, 0, 0);
        // The old id is gone; the next navigation re-enters at the first cell.
        assert_eq!(graph.focused_node(), None);
        assert_eq!(graph.move_focus(Action::NavigateRight), Some(id(0)));
    }

    #[test]
    fn resolve_reports_the_cell_without_moving_focus() {
        let mut graph = FocusGraph::grid(4, 2);
        graph.focus(0usize);
        assert_eq!(
            graph.resolve(Action::NavigateRight),
            Some(FocusNode::new(id(1), 0, 1))
        );
        assert_eq!(graph.focused(), Some(id(0)));
    }
}
