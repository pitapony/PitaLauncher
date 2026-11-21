//! The in-app grid on-screen keyboard.
//!
//! A controller-only shell cannot type, and a 10-foot UI must not ask anyone to
//! plug in a keyboard. The plan's answer is an on-screen keyboard drawn as a
//! bottom sheet and walked with the same [`FocusGraph`] every other screen uses.
//! This module is the whole model — the layout, the shift/layer state, the
//! caret, the text and the focus — so the screen can be a thin Dioxus seam and
//! every editing rule is tested without a display.
//!
//! The layout is a ragged grid: two character layers (letters/digits and
//! symbols) with the same row shape, plus one row of editing keys. Rows do not
//! all have the same number of keys, which is exactly what the focus graph's
//! diagonal fallback and per-axis wrap already handle (the console home's
//! shelves do the same).
//!
//! [`OnScreenKeyboard::handle`] takes an [`Action`] so the app's one input pump
//! drives the sheet without a second dispatch table: `Back` always means cancel
//! (a trapped sheet is worse than a lost edit), a direction moves the ring, and
//! `Confirm` presses the focused key.
//!
//! The text is a `String` with a *character* caret, not a byte one: a title can
//! contain non-ASCII and a byte caret would split a code point.

use crate::{Action, FocusGraph, FocusId};

/// The longest text an [`OnScreenKeyboard`] holds by default.
///
/// A search filter is short; the same cap keeps a held key from growing an
/// unbounded string on a couch-sized screen.
pub const MAX_TEXT: usize = 64;

/// Which character layer the keyboard shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OskLayer {
    /// Lower-case letters, digits and `, . -`.
    #[default]
    Letters,
    /// Symbol cluster (shift is still available, but symbols have no case).
    Symbols,
}

impl OskLayer {
    /// The other layer, for the layer key.
    pub const fn toggled(self) -> Self {
        match self {
            Self::Letters => Self::Symbols,
            Self::Symbols => Self::Letters,
        }
    }

    /// The four character rows of this layer.
    ///
    /// Both layers have the same row shape, so switching layers keeps the focus
    /// index meaningful instead of jumping to the first key.
    pub fn rows(self) -> Vec<Vec<OskKey>> {
        let source: [&str; 4] = match self {
            Self::Letters => ["1234567890", "qwertyuiop", "asdfghjkl", "zxcvbnm,.-"],
            Self::Symbols => ["!@#$%^&*()", "[]{}<>/\\|=", ";:'\"`~?!,", "_+-*&%$#@!"],
        };
        source
            .iter()
            .map(|row| row.chars().map(OskKey::char_key).collect())
            .collect()
    }
}

/// One key on the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OskKey {
    /// A character key; `shifted` is what typing it with shift produces.
    Char { normal: char, shifted: char },
    /// Toggle upper case (and the digit row's symbols).
    Shift,
    /// Switch between the letters and symbols layers.
    Layer,
    /// Insert a space.
    Space,
    /// Delete the character before the caret.
    Backspace,
    /// Move the caret one character left.
    CaretLeft,
    /// Move the caret one character right.
    CaretRight,
    /// Empty the text.
    Clear,
    /// Submit the text and close the sheet.
    Done,
}

impl OskKey {
    /// Build a character key, deriving its shifted character.
    fn char_key(normal: char) -> Self {
        let shifted = match normal {
            '1' => '!',
            '2' => '@',
            '3' => '#',
            '4' => '$',
            '5' => '%',
            '6' => '^',
            '7' => '&',
            '8' => '*',
            '9' => '(',
            '0' => ')',
            'a'..='z' => normal.to_ascii_uppercase(),
            other => other,
        };
        Self::Char { normal, shifted }
    }

    /// The text the view prints on the key.
    ///
    /// Shift and the current layer are arguments rather than state so a caption
    /// can never disagree with what pressing the key would do.
    pub fn caption(self, shift: bool, layer: OskLayer) -> String {
        match self {
            Self::Char { normal, shifted } => {
                let ch = if shift { shifted } else { normal };
                ch.to_string()
            }
            Self::Shift => "Shift".to_string(),
            Self::Layer => match layer {
                OskLayer::Letters => "123".to_string(),
                OskLayer::Symbols => "ABC".to_string(),
            },
            Self::Space => "Space".to_string(),
            Self::Backspace => "⌫".to_string(),
            Self::CaretLeft => "←".to_string(),
            Self::CaretRight => "→".to_string(),
            Self::Clear => "Clear".to_string(),
            Self::Done => "Done".to_string(),
        }
    }

    /// The character this key inserts, or `None` for a command key.
    pub fn inserts(self, shift: bool) -> Option<char> {
        match self {
            Self::Char { normal, shifted } => Some(if shift { shifted } else { normal }),
            Self::Space => Some(' '),
            _ => None,
        }
    }
}

/// What one press (or one action) did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OskPress {
    /// Nothing to do: an unbound action, `Confirm` with no focus, a refused
    /// insertion at the length cap, a caret already at the edge.
    Idle,
    /// The text, caret, shift or layer changed.
    Edited,
    /// The `Done` key was pressed; the caller commits [`OnScreenKeyboard::text`]
    /// and closes the sheet.
    Submit,
    /// `Back`: the caller closes the sheet and keeps the previous text.
    Cancel,
}

impl OskPress {
    /// Whether the sheet should close after this press.
    pub const fn is_finished(self) -> bool {
        matches!(self, Self::Submit | Self::Cancel)
    }
}

/// The editing row, shared by both layers.
fn edit_row() -> Vec<OskKey> {
    vec![
        OskKey::Shift,
        OskKey::CaretLeft,
        OskKey::CaretRight,
        OskKey::Space,
        OskKey::Backspace,
        OskKey::Layer,
        OskKey::Clear,
        OskKey::Done,
    ]
}

/// A grid keyboard with its text, caret and focus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnScreenKeyboard {
    text: String,
    /// Character index in `0..=text.chars().count()`.
    caret: usize,
    limit: usize,
    shift: bool,
    layer: OskLayer,
    /// Flat key index in layout order, or `None` until the first navigation.
    focus: Option<usize>,
}

impl Default for OnScreenKeyboard {
    fn default() -> Self {
        Self {
            text: String::new(),
            caret: 0,
            limit: MAX_TEXT,
            shift: false,
            layer: OskLayer::Letters,
            focus: None,
        }
    }
}

impl OnScreenKeyboard {
    /// An empty keyboard on the letters layer, unfocused.
    pub fn new() -> Self {
        Self::default()
    }

    /// A keyboard seeded with existing text (a search filter, a name).
    pub fn with_text(text: impl Into<String>) -> Self {
        let mut keyboard = Self::default();
        keyboard.set_text(text);
        keyboard
    }

    /// Replace the text; the caret moves to its end and the limit is applied.
    pub fn set_text(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.text = text.chars().take(self.limit).collect();
        self.caret = self.text.chars().count();
    }

    /// The committed text so far.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The caret position, as a character index.
    pub fn caret(&self) -> usize {
        self.caret
    }

    /// The text split at the caret, for drawing a caret between two spans.
    pub fn split_at_caret(&self) -> (&str, &str) {
        let byte = self.byte_index();
        self.text.split_at(byte)
    }

    /// Whether shift is held (the next character is upper case / shifted).
    pub fn shift(&self) -> bool {
        self.shift
    }

    /// The visible layer.
    pub fn layer(&self) -> OskLayer {
        self.layer
    }

    /// The length cap.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// The focused key's flat index, if any.
    pub fn focus_index(&self) -> Option<usize> {
        self.focus
    }

    /// Every row of the visible layer, including the editing row.
    pub fn rows(&self) -> Vec<Vec<OskKey>> {
        let mut rows = self.layer.rows();
        rows.push(edit_row());
        rows
    }

    /// How many keys the visible layer has.
    pub fn key_count(&self) -> usize {
        self.rows().iter().map(Vec::len).sum()
    }

    /// The focused key, when focus is still inside the layout.
    pub fn focused_key(&self) -> Option<OskKey> {
        let focus = self.focus?;
        self.key_at(focus)
    }

    /// The key at a flat index.
    pub fn key_at(&self, index: usize) -> Option<OskKey> {
        self.rows().into_iter().flatten().nth(index)
    }

    /// Focus a key directly (a pointer click continues from that key).
    ///
    /// An index past the layout clears the focus, like the library grid does
    /// when its filtered list shrinks.
    pub fn set_focus(&mut self, index: Option<usize>) {
        self.focus = index.filter(|index| *index < self.key_count());
    }

    /// The focus graph over the ragged key grid.
    ///
    /// Ids are the flat layout indices, so [`FocusGraph::focused_index`] doubles
    /// as the key index the view draws the ring on.
    pub fn graph(&self) -> FocusGraph {
        let mut graph = FocusGraph::new();
        let mut id = 0usize;
        for (row, keys) in self.rows().iter().enumerate() {
            for col in 0..keys.len() {
                graph.insert(FocusId::from(id), row, col);
                id += 1;
            }
        }
        if let Some(focus) = self.focus {
            graph.focus(FocusId::from(focus));
        }
        graph
    }

    /// Move the ring one cell; the first navigation enters the first key.
    pub fn move_focus(&mut self, action: Action) -> Option<usize> {
        if !action.is_navigation() {
            return self.focus;
        }
        let mut graph = self.graph();
        graph.move_focus(action);
        let next = graph.focused_index();
        if next.is_some() {
            self.focus = next;
        }
        next
    }

    /// Press the focused key.
    ///
    /// With nothing focused the first key is focused instead of pressed, so a
    /// stray `Confirm` cannot type a `1` into an empty field.
    pub fn press_focused(&mut self) -> OskPress {
        match self.focus {
            Some(index) => self.press_at(index),
            None => {
                self.focus = Some(0);
                OskPress::Idle
            }
        }
    }

    /// Focus and press one key (the mouse path).
    pub fn press_at(&mut self, index: usize) -> OskPress {
        let Some(key) = self.key_at(index) else {
            return OskPress::Idle;
        };
        self.focus = Some(index);
        match key {
            OskKey::Char { .. } | OskKey::Space => {
                let Some(ch) = key.inserts(self.shift) else {
                    return OskPress::Idle;
                };
                if !self.insert(ch) {
                    return OskPress::Idle;
                }
                // Shift is one character, like a phone keyboard: holding it
                // through a whole word is the rare case and a stuck shift is
                // the confusing one.
                self.shift = false;
                OskPress::Edited
            }
            OskKey::Shift => {
                self.shift = !self.shift;
                OskPress::Edited
            }
            OskKey::Layer => {
                self.layer = self.layer.toggled();
                OskPress::Edited
            }
            OskKey::CaretLeft => {
                if self.caret == 0 {
                    return OskPress::Idle;
                }
                self.caret -= 1;
                OskPress::Edited
            }
            OskKey::CaretRight => {
                if self.caret >= self.char_len() {
                    return OskPress::Idle;
                }
                self.caret += 1;
                OskPress::Edited
            }
            OskKey::Backspace => {
                if self.backspace() {
                    OskPress::Edited
                } else {
                    OskPress::Idle
                }
            }
            OskKey::Clear => {
                if self.text.is_empty() {
                    return OskPress::Idle;
                }
                self.text.clear();
                self.caret = 0;
                OskPress::Edited
            }
            OskKey::Done => OskPress::Submit,
        }
    }

    /// Apply one action from the app's input pump.
    ///
    /// `Back` cancels here rather than in the caller, so every screen that
    /// shows the sheet gets the same escape hatch.
    pub fn handle(&mut self, action: Action) -> OskPress {
        match action {
            Action::Back => OskPress::Cancel,
            Action::Confirm => self.press_focused(),
            action if action.is_navigation() => {
                self.move_focus(action);
                OskPress::Edited
            }
            _ => OskPress::Idle,
        }
    }

    /// Whether the layout is empty (never true; the editing row always exists).
    pub fn is_empty(&self) -> bool {
        self.key_count() == 0
    }

    fn char_len(&self) -> usize {
        self.text.chars().count()
    }

    fn byte_index(&self) -> usize {
        self.text
            .char_indices()
            .nth(self.caret)
            .map(|(index, _)| index)
            .unwrap_or(self.text.len())
    }

    /// Insert at the caret; returns whether it fitted under the limit.
    fn insert(&mut self, ch: char) -> bool {
        if self.char_len() >= self.limit {
            return false;
        }
        let byte = self.byte_index();
        self.text.insert(byte, ch);
        self.caret += 1;
        true
    }

    /// Delete the character before the caret; returns whether one was removed.
    fn backspace(&mut self) -> bool {
        if self.caret == 0 {
            return false;
        }
        let end = self.byte_index();
        let start = self.text[..end]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0);
        self.text.replace_range(start..end, "");
        self.caret -= 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashSet, VecDeque};

    /// Every key of the visible layout, in flat order: the four character rows
    /// then the editing row.
    fn flat(keyboard: &OnScreenKeyboard) -> Vec<OskKey> {
        keyboard.rows().into_iter().flatten().collect()
    }

    /// The flat index of a command key.
    fn index_of(keyboard: &OnScreenKeyboard, key: OskKey) -> usize {
        flat(keyboard)
            .iter()
            .position(|candidate| *candidate == key)
            .expect("key is in the layout")
    }

    /// The flat index of a character key, by its unshifted character.
    fn char_at(keyboard: &OnScreenKeyboard, target: char) -> usize {
        flat(keyboard)
            .iter()
            .position(|key| matches!(key, OskKey::Char { normal, .. } if *normal == target))
            .unwrap_or_else(|| panic!("{target:?} is in the layout"))
    }

    fn press(keyboard: &mut OnScreenKeyboard, key: OskKey) -> OskPress {
        let index = index_of(keyboard, key);
        keyboard.press_at(index)
    }

    #[test]
    fn both_layers_have_the_same_ragged_shape() {
        let letters = OskLayer::Letters.rows();
        let symbols = OskLayer::Symbols.rows();
        assert_eq!(letters.len(), 4);
        assert_eq!(
            letters.iter().map(Vec::len).collect::<Vec<_>>(),
            [10, 10, 9, 10]
        );
        assert_eq!(
            symbols.iter().map(Vec::len).collect::<Vec<_>>(),
            [10, 10, 9, 10]
        );
        let keyboard = OnScreenKeyboard::new();
        assert_eq!(keyboard.rows().len(), 5);
        assert_eq!(keyboard.key_count(), 47);
        assert_eq!(keyboard.rows().last().map(Vec::len), Some(8));
    }

    #[test]
    fn every_key_is_reachable_from_the_first_one() {
        // The plan's "no focus trap" rule, applied to the OSK: from the first
        // key every other key is reachable with navigation alone.
        for layer in [OskLayer::Letters, OskLayer::Symbols] {
            let mut keyboard = OnScreenKeyboard::new();
            keyboard.layer = layer;
            let total = keyboard.key_count();
            let mut seen = HashSet::new();
            let mut queue = VecDeque::from([0usize]);
            seen.insert(0usize);
            while let Some(index) = queue.pop_front() {
                keyboard.focus = Some(index);
                for action in [
                    Action::NavigateUp,
                    Action::NavigateDown,
                    Action::NavigateLeft,
                    Action::NavigateRight,
                ] {
                    let graph = keyboard.graph();
                    if let Some(next) = graph.resolve(action) {
                        let next = next.id.get() as usize;
                        if seen.insert(next) {
                            queue.push_back(next);
                        }
                    }
                }
            }
            assert_eq!(seen.len(), total, "{layer:?}");
        }
    }

    #[test]
    fn the_first_navigation_enters_the_first_key() {
        let mut keyboard = OnScreenKeyboard::new();
        assert_eq!(keyboard.focus_index(), None);
        assert_eq!(keyboard.move_focus(Action::NavigateDown), Some(0));
        assert_eq!(
            keyboard.focused_key(),
            Some(OskKey::Char {
                normal: '1',
                shifted: '!'
            })
        );
    }

    #[test]
    fn a_direction_moves_one_key_and_wraps_at_the_row_edge() {
        let mut keyboard = OnScreenKeyboard::new();
        keyboard.move_focus(Action::NavigateDown);
        assert_eq!(keyboard.move_focus(Action::NavigateRight), Some(1));
        // Right from the end of the first row wraps to its start.
        keyboard.focus = Some(9);
        assert_eq!(keyboard.move_focus(Action::NavigateRight), Some(0));
        // Down from the first key lands on `q`.
        keyboard.focus = Some(0);
        assert_eq!(keyboard.move_focus(Action::NavigateDown), Some(10));
    }

    #[test]
    fn pressing_a_letter_inserts_it_and_advances_the_caret() {
        let mut keyboard = OnScreenKeyboard::new();
        let q = char_at(&keyboard, 'q');
        assert_eq!(keyboard.press_at(q), OskPress::Edited);
        assert_eq!(keyboard.text(), "q");
        assert_eq!(keyboard.caret(), 1);
        assert_eq!(keyboard.split_at_caret(), ("q", ""));
    }

    #[test]
    fn shift_uppercases_one_character_then_resets() {
        let mut keyboard = OnScreenKeyboard::new();
        assert_eq!(press(&mut keyboard, OskKey::Shift), OskPress::Edited);
        assert!(keyboard.shift());
        let q = char_at(&keyboard, 'q');
        keyboard.press_at(q);
        assert_eq!(keyboard.text(), "Q");
        assert!(!keyboard.shift());
        keyboard.press_at(q);
        assert_eq!(keyboard.text(), "Qq");
    }

    #[test]
    fn shift_turns_the_digit_row_into_symbols() {
        let mut keyboard = OnScreenKeyboard::new();
        keyboard.shift = true;
        keyboard.press_at(char_at(&keyboard, '1'));
        assert_eq!(keyboard.text(), "!");
    }

    #[test]
    fn the_symbol_layer_swaps_captions_and_keeps_the_focus_index() {
        let mut keyboard = OnScreenKeyboard::new();
        let q = char_at(&keyboard, 'q');
        let layer = index_of(&keyboard, OskKey::Layer);
        assert_eq!(press(&mut keyboard, OskKey::Layer), OskPress::Edited);
        assert_eq!(keyboard.layer(), OskLayer::Symbols);
        assert_eq!(keyboard.focus_index(), Some(layer));
        // The layer kept the grid's shape, so the same index is a character.
        assert!(matches!(keyboard.key_at(q), Some(OskKey::Char { .. })));
        assert_ne!(keyboard.key_at(q), Some(OskKey::char_key('q')));
        assert_eq!(keyboard.rows()[0][0].caption(false, keyboard.layer()), "!");
        press(&mut keyboard, OskKey::Layer);
        assert_eq!(keyboard.layer(), OskLayer::Letters);
    }

    #[test]
    fn backspace_deletes_before_the_caret_and_stops_at_the_start() {
        let mut keyboard = OnScreenKeyboard::with_text("abc");
        assert_eq!(keyboard.caret(), 3);
        assert_eq!(press(&mut keyboard, OskKey::Backspace), OskPress::Edited);
        assert_eq!(keyboard.text(), "ab");
        assert_eq!(keyboard.caret(), 2);
        press(&mut keyboard, OskKey::Backspace);
        press(&mut keyboard, OskKey::Backspace);
        assert_eq!(keyboard.text(), "");
        assert_eq!(press(&mut keyboard, OskKey::Backspace), OskPress::Idle);
    }

    #[test]
    fn a_non_ascii_character_is_deleted_as_one_character() {
        let mut keyboard = OnScreenKeyboard::with_text("café");
        assert_eq!(keyboard.caret(), 4);
        press(&mut keyboard, OskKey::Backspace);
        assert_eq!(keyboard.text(), "caf");
        assert_eq!(keyboard.caret(), 3);
    }

    #[test]
    fn the_caret_keys_edit_in_the_middle() {
        let mut keyboard = OnScreenKeyboard::with_text("ac");
        // Caret starts at the end.
        assert_eq!(press(&mut keyboard, OskKey::CaretLeft), OskPress::Edited);
        assert_eq!(keyboard.caret(), 1);
        assert_eq!(press(&mut keyboard, OskKey::CaretRight), OskPress::Edited);
        assert_eq!(keyboard.caret(), 2);
        assert_eq!(press(&mut keyboard, OskKey::CaretRight), OskPress::Idle);
        press(&mut keyboard, OskKey::CaretLeft);
        // Insert `b` between `a` and `c`.
        keyboard.press_at(char_at(&keyboard, 'b'));
        assert_eq!(keyboard.text(), "abc");
        assert_eq!(keyboard.caret(), 2);
        press(&mut keyboard, OskKey::CaretLeft);
        press(&mut keyboard, OskKey::CaretLeft);
        assert_eq!(keyboard.caret(), 0);
        assert_eq!(press(&mut keyboard, OskKey::CaretLeft), OskPress::Idle);
    }

    #[test]
    fn space_clear_and_done() {
        let mut keyboard = OnScreenKeyboard::with_text("hello");
        assert_eq!(press(&mut keyboard, OskKey::Space), OskPress::Edited);
        assert_eq!(keyboard.text(), "hello ");
        assert_eq!(press(&mut keyboard, OskKey::Clear), OskPress::Edited);
        assert_eq!(keyboard.text(), "");
        assert_eq!(press(&mut keyboard, OskKey::Clear), OskPress::Idle);
        assert_eq!(press(&mut keyboard, OskKey::Done), OskPress::Submit);
    }

    #[test]
    fn the_limit_refuses_insertion_but_still_submits() {
        let mut keyboard = OnScreenKeyboard::new();
        keyboard.limit = 2;
        // `q` and `w` sit next to each other.
        let q = char_at(&keyboard, 'q');
        keyboard.press_at(q);
        keyboard.press_at(q + 1);
        assert_eq!(keyboard.text(), "qw");
        keyboard.press_at(q + 2);
        assert_eq!(keyboard.text(), "qw");
        assert_eq!(press(&mut keyboard, OskKey::Done), OskPress::Submit);
    }

    #[test]
    fn set_text_applies_the_limit_and_puts_the_caret_at_the_end() {
        let mut keyboard = OnScreenKeyboard::new();
        keyboard.limit = 3;
        keyboard.set_text("abcdef");
        assert_eq!(keyboard.text(), "abc");
        assert_eq!(keyboard.caret(), 3);
    }

    #[test]
    fn handle_maps_actions_to_navigation_press_and_cancel() {
        let mut keyboard = OnScreenKeyboard::new();
        assert_eq!(keyboard.handle(Action::NavigateDown), OskPress::Edited);
        assert_eq!(keyboard.focus_index(), Some(0));
        assert_eq!(keyboard.handle(Action::Confirm), OskPress::Edited);
        assert_eq!(keyboard.text(), "1");
        assert_eq!(keyboard.handle(Action::Back), OskPress::Cancel);
        assert!(OskPress::Cancel.is_finished());
        assert!(!OskPress::Edited.is_finished());
        assert_eq!(keyboard.handle(Action::Search), OskPress::Idle);
    }

    #[test]
    fn confirm_without_focus_focuses_the_first_key_instead_of_typing() {
        let mut keyboard = OnScreenKeyboard::new();
        assert_eq!(keyboard.handle(Action::Confirm), OskPress::Idle);
        assert_eq!(keyboard.focus_index(), Some(0));
        assert_eq!(keyboard.text(), "");
    }

    #[test]
    fn pressing_past_the_layout_is_idle() {
        let mut keyboard = OnScreenKeyboard::new();
        assert_eq!(keyboard.press_at(999), OskPress::Idle);
        assert_eq!(keyboard.focus_index(), None);
        keyboard.set_focus(Some(999));
        assert_eq!(keyboard.focus_index(), None);
    }

    #[test]
    fn captions_name_the_commands_and_follow_shift() {
        assert_eq!(OskKey::Shift.caption(false, OskLayer::Letters), "Shift");
        assert_eq!(OskKey::Layer.caption(false, OskLayer::Letters), "123");
        assert_eq!(OskKey::Layer.caption(false, OskLayer::Symbols), "ABC");
        assert_eq!(OskKey::Backspace.caption(false, OskLayer::Letters), "⌫");
        assert_eq!(OskKey::Done.caption(false, OskLayer::Letters), "Done");
        let q = OskKey::char_key('q');
        assert_eq!(q.caption(false, OskLayer::Letters), "q");
        assert_eq!(q.caption(true, OskLayer::Letters), "Q");
        let one = OskKey::char_key('1');
        assert_eq!(one.caption(false, OskLayer::Letters), "1");
        assert_eq!(one.caption(true, OskLayer::Letters), "!");
        // Symbols have no case, so shift leaves their caption alone.
        let bang = OskKey::char_key('!');
        assert_eq!(bang.caption(true, OskLayer::Symbols), "!");
    }

    #[test]
    fn the_graph_ids_are_the_flat_key_indices() {
        let keyboard = OnScreenKeyboard::new();
        let graph = keyboard.graph();
        assert_eq!(graph.len(), keyboard.key_count());
        for (index, node) in graph.nodes().iter().enumerate() {
            assert_eq!(node.id.get() as usize, index);
        }
    }
}
