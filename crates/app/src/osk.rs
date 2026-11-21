//! The in-app on-screen keyboard's bottom sheet.
//!
//! The rules live in [`pitagames_input::OnScreenKeyboard`] — the layout, shift,
//! the symbol layer, the character caret and the text. This module is only the
//! Dioxus seam: it draws the sheet, rings the focused key and turns a click into
//! `press_at`, so the pointer and the d-pad walk the same model.
//!
//! The sheet is an overlay, not a route. A screen opens it when the pad's
//! `Search` button arrives (or a pointer clicks the keyboard button) and keeps
//! draining its own [`ActionChannel`](crate::focus::ActionChannel); while it is
//! open the screen hands each action to [`OnScreenKeyboard::handle`], so `Back`
//! cancels the typing without leaving the screen behind it.
//!
//! `Done` does not write anything itself: the screen learns the text through
//! `on_done` and decides what it means (a filter, a profile name). A cancel
//! discards the edit and leaves the screen's previous text untouched, which is
//! why the model is seeded with a copy on open.

use dioxus::prelude::*;
use pitagames_input::{OnScreenKeyboard, OskKey, OskPress};

use crate::focus;

/// Props for the sheet.
#[derive(Props, Clone, PartialEq)]
pub struct OskSheetProps {
    /// What the text is for, printed above it ("Search your library").
    pub title: String,
    /// The keyboard model the screen owns.
    pub keyboard: Signal<OnScreenKeyboard>,
    /// `Done`: the committed text.
    pub on_done: EventHandler<String>,
    /// `Back`, or the Cancel button: the sheet closes and nothing is applied.
    pub on_cancel: EventHandler<()>,
}

/// One key of the grid, already resolved for the visible layer.
struct Cell {
    index: usize,
    key: OskKey,
    caption: String,
}

/// The bottom sheet.
#[allow(non_snake_case)]
pub fn OskSheet(props: OskSheetProps) -> Element {
    let mut keyboard = props.keyboard;
    let on_done = props.on_done;
    let on_cancel = props.on_cancel;
    let title = props.title.clone();

    // Resolve the visible layout before the RSX so the handlers below do not
    // hold a read guard while they write.
    let rows: Vec<Vec<Cell>> = {
        let snapshot = keyboard.read();
        let shift = snapshot.shift();
        let layer = snapshot.layer();
        let mut index = 0usize;
        snapshot
            .rows()
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|key| {
                        let cell = Cell {
                            index,
                            key,
                            caption: key.caption(shift, layer),
                        };
                        index += 1;
                        cell
                    })
                    .collect()
            })
            .collect()
    };
    let focus = keyboard.read().focus_index();
    let (before, after) = {
        let snapshot = keyboard.read();
        let (before, after) = snapshot.split_at_caret();
        (before.to_string(), after.to_string())
    };

    rsx! {
        div {
            style: "position:fixed;inset:0;z-index:60;display:flex;align-items:flex-end;justify-content:center;background:rgba(8,8,12,0.62);",

            div {
                style: "width:100%;max-width:1000px;margin:0 20px 28px;padding:18px 20px 16px;box-sizing:border-box;border-radius:18px;background:#14141c;border:1px solid #34344a;box-shadow:0 18px 48px rgba(0,0,0,0.55);display:flex;flex-direction:column;gap:12px;",

                div {
                    style: "display:flex;align-items:baseline;gap:12px;",
                    span {
                        style: "font-size:12px;text-transform:uppercase;letter-spacing:0.08em;color:#7a9ad6;",
                        "{title}"
                    }
                    span {
                        style: "margin-left:auto;font-size:11px;color:#78788a;",
                        "A types · B cancels"
                    }
                }

                div {
                    style: "min-height:30px;padding:9px 12px;border-radius:10px;background:#1e1e28;border:1px solid #2c2c38;color:#e8e8ee;font-size:20px;font-family:monospace;white-space:pre-wrap;word-break:break-word;",
                    span { "{before}" }
                    span {
                        style: "color:#7a9ad6;",
                        "▏"
                    }
                    span { "{after}" }
                }

                for (row_index, row) in rows.into_iter().enumerate() {
                    div {
                        key: "osk-row-{row_index}",
                        style: "display:flex;gap:8px;",
                        for cell in row {
                            {
                                let index = cell.index;
                                let key = cell.key;
                                let caption = cell.caption.clone();
                                let focused = focus == Some(index);
                                let ring = focus::ring_style(focused);
                                // Space and Done get room to breathe; a long
                                // caption (Clear, Shift) keeps its width.
                                let grow = match key {
                                    OskKey::Space => "flex:2.6 1 0;",
                                    OskKey::Done => "flex:1.8 1 0;",
                                    OskKey::Clear | OskKey::Shift => "flex:1.4 1 0;",
                                    _ => "flex:1 1 0;",
                                };
                                let accent = match key {
                                    OskKey::Done => "background:#2e3a56;color:#ffffff;",
                                    OskKey::Shift if keyboard.read().shift() => {
                                        "background:#2e3a56;color:#ffffff;"
                                    }
                                    _ => "background:#1e1e28;color:#e8e8ee;",
                                };
                                rsx! {
                                    button {
                                        key: "osk-key-{index}",
                                        onclick: move |_| {
                                            let press = keyboard.write().press_at(index);
                                            match press {
                                                OskPress::Submit => {
                                                    let text = keyboard.read().text().to_string();
                                                    on_done.call(text);
                                                }
                                                OskPress::Cancel => on_cancel.call(()),
                                                OskPress::Idle | OskPress::Edited => {}
                                            }
                                        },
                                        style: "{grow}min-width:40px;height:46px;border-radius:10px;border:1px solid #2c2c38;font-size:16px;font-weight:600;cursor:pointer;{accent}{ring}",
                                        "{caption}"
                                    }
                                }
                            }
                        }
                    }
                }

                div {
                    style: "display:flex;gap:10px;align-items:center;",
                    span {
                        style: "font-size:11px;color:#78788a;",
                        "D-pad moves · A types · B cancels"
                    }
                    button {
                        onclick: move |_| on_cancel.call(()),
                        style: "margin-left:auto;padding:8px 16px;border-radius:10px;border:1px solid #2c2c38;background:#1e1e28;color:#c8c8d4;font-size:13px;cursor:pointer;",
                        "Cancel"
                    }
                }
            }
        }
    }
}
