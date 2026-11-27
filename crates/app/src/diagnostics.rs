//! The diagnostics toggles in the detail page's Runtime panel.
//!
//! The toggles are plain booleans on the profile; `platform::envc` turns them
//! into environment variables at compile time. The editor only sets them — it
//! runs nothing, and the resolved command below the panel shows exactly what a
//! launch would export.

use dioxus::prelude::*;

use pitagames_core::i18n::t;

use crate::runtime::{DiagnosticToggle, DiagnosticsDraft};

#[derive(Props, Clone, PartialEq)]
pub struct DiagnosticsEditorProps {
    /// The draft shared with the detail page. Edits apply on save.
    pub draft: Signal<DiagnosticsDraft>,
}

/// Toggle chips for the HUDs, logs and indicators.
#[allow(non_snake_case)]
pub fn DiagnosticsEditor(props: DiagnosticsEditorProps) -> Element {
    let mut draft = props.draft;
    let current = draft();

    let enabled = current.enabled_labels();
    let summary = if current.is_empty() {
        t("diagnostics.none").to_string()
    } else {
        t("diagnostics.enabled").replace("{list}", &enabled.join(" · "))
    };
    let title = t("diagnostics.title");
    let hint = t("diagnostics.hint");

    rsx! {
        div {
            style: "display:flex;flex-direction:column;gap:8px;padding:12px 14px;border-radius:10px;background:#1a1a22;border:1px solid #26262f;",

            div {
                style: "display:flex;align-items:baseline;gap:8px;flex-wrap:wrap;",
                span {
                    style: "font-size:var(--text-caption);font-weight:600;color:var(--text);",
                    "{title}"
                }
                span {
                    style: "font-size:var(--text-micro);color:var(--text-muted);",
                    "{hint}"
                }
            }

            div {
                style: "display:flex;gap:8px;flex-wrap:wrap;",
                for toggle in DiagnosticToggle::ALL {
                    button {
                        key: "{toggle.label()}",
                        onclick: move |_| draft.write().toggle(toggle),
                        style: toggle_style(current.is_on(toggle)),
                        div {
                            style: "display:flex;flex-direction:column;gap:2px;align-items:flex-start;text-align:left;",
                            span {
                                style: "font-size:var(--text-caption);color:var(--text);",
                                "{toggle.label()}"
                            }
                            code {
                                style: "font-size:var(--text-micro);color:#8a8a9c;",
                                "{toggle.env_hint()}"
                            }
                        }
                    }
                }
            }

            span {
                style: "font-size:var(--text-micro);color:var(--text-muted);",
                "{summary}"
            }
        }
    }
}

fn toggle_style(active: bool) -> &'static str {
    if active {
        "padding:6px 10px;border-radius:8px;border:1px solid #3f5c8c;background:#243044;cursor:pointer;min-height:var(--hit-min);min-width:var(--hit-min);"
    } else {
        "padding:6px 10px;border-radius:8px;border:1px solid #2c2c38;background:#1e1e28;cursor:pointer;min-height:var(--hit-min);min-width:var(--hit-min);"
    }
}
