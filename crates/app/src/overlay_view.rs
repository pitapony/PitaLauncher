//! The console overlay's Dioxus seam.
//!
//! The rows and focus live in the framework-free [`crate::overlay`]; this module
//! only draws them. It is a fixed, full-window backdrop with a centred panel, so
//! it covers whatever screen the launcher is showing without becoming a route
//! (the shell beneath keeps its scroll position and focus).
//!
//! A click is wired the same way the d-pad is: a row calls `on_choose` with its
//! [`OverlayChoice`] (a disabled row calls nothing), and the backdrop closes the
//! menu. `App` owns both handlers, so a pointer and a controller take the same
//! path.

use dioxus::prelude::*;

use crate::focus;
use crate::overlay::OverlayChoice;

/// Props for [`OverlayView`].
#[derive(Props, Clone, PartialEq)]
pub struct OverlayViewProps {
    /// The open model: its rows and focus.
    pub entries: Vec<crate::overlay::OverlayEntry>,
    /// The focused row index.
    pub focused: Option<usize>,
    /// The controller + battery line.
    pub status: String,
    /// The wall clock, as `HH:MM`.
    pub clock: String,
    /// A row was chosen (a pointer click).
    pub on_choose: EventHandler<OverlayChoice>,
    /// The backdrop was clicked.
    pub on_close: EventHandler<()>,
}

#[allow(non_snake_case)]
pub fn OverlayView(props: OverlayViewProps) -> Element {
    let entries = props.entries;
    let focused = props.focused;
    let status = props.status;
    let clock = props.clock;
    let on_choose = props.on_choose;
    let on_close = props.on_close;

    rsx! {
        div {
            style: "position:fixed;inset:0;z-index:100;display:flex;align-items:center;justify-content:center;background:rgba(6,6,10,0.74);",
            onclick: move |_| on_close.call(()),
            div {
                style: "width:min(560px,90vw);max-height:82vh;overflow-y:auto;display:flex;flex-direction:column;gap:8px;padding:24px;border-radius:16px;background:#16161f;border:1px solid #2c2c38;box-shadow:0 18px 60px rgba(0,0,0,0.6);",
                onclick: move |event| event.stop_propagation(),

                div {
                    style: "display:flex;align-items:baseline;gap:12px;margin-bottom:6px;",
                    span {
                        style: "font-size:1.1rem;font-weight:700;color:#ffffff;",
                        "Menu"
                    }
                    span {
                        style: "margin-left:auto;font-size:1.05rem;color:#c8c8d4;",
                        "{clock}"
                    }
                }

                for (index, entry) in entries.iter().enumerate() {
                    {
                        let enabled = entry.is_enabled();
                        let ring = focus::ring_style(focused == Some(index));
                        let label = entry.label.clone();
                        let detail = entry.detail.clone();
                        let choice = entry.choice;
                        let background = if enabled { "#1e1e28" } else { "#15151c" };
                        let label_color = if enabled { "#ffffff" } else { "#6a6a7a" };
                        let detail_color = if enabled { "#8a8a9c" } else { "#55555f" };
                        let cursor = if enabled { "pointer" } else { "default" };
                        rsx! {
                            div {
                                key: "overlay-{index}",
                                onclick: move |_| {
                                    if let Some(choice) = choice {
                                        on_choose.call(choice);
                                    }
                                },
                                style: "display:flex;flex-direction:column;gap:2px;padding:12px 16px;border-radius:10px;background:{background};border:1px solid #2c2c38;cursor:{cursor};{ring}",
                                span {
                                    style: "font-size:1rem;font-weight:600;color:{label_color};",
                                    "{label}"
                                }
                                span {
                                    style: "font-size:0.8rem;color:{detail_color};",
                                    "{detail}"
                                }
                            }
                        }
                    }
                }

                div {
                    style: "display:flex;align-items:center;gap:10px;margin-top:8px;padding-top:10px;border-top:1px solid #26262f;font-size:0.8rem;color:#9a9aac;",
                    span { "Controller: {status}" }
                    span {
                        style: "margin-left:auto;",
                        "M or Start closes"
                    }
                }
            }
        }
    }
}
