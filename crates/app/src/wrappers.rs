//! The wrapper-chain editor.
//!
//! Wrappers are processes placed in front of the game, outermost first:
//! `gamescope … -- gamemoderun mangohud <game>`. The editor edits a
//! [`WrapperDraft`] (the string form the GUI can type into) that the detail
//! page converts and validates on save. The live preview comes from
//! `platform::envc`, the same function the shim's chain is compiled with, so
//! what the user sees is what will run.
//!
//! No shell is ever composed: custom rows are `program + args` vectors, and the
//! preview's quoted text is for copying, not for execution.

use dioxus::prelude::*;

use crate::runtime::{WrapperDraft, WrapperDraftStep};

#[derive(Props, Clone, PartialEq)]
pub struct WrapperEditorProps {
    /// The draft shared with the detail page. Edits apply on save.
    pub draft: Signal<WrapperDraft>,
}

/// Toggle chips, gamescope options, custom rows and the resolved preview.
#[allow(non_snake_case)]
pub fn WrapperEditor(props: WrapperEditorProps) -> Element {
    let mut draft = props.draft;
    let mut program = use_signal(String::new);
    let mut args = use_signal(String::new);
    let mut step_error = use_signal(String::new);

    let current = draft.read().clone();
    let preview = current.preview();
    let program_text = program.read().clone();
    let args_text = args.read().clone();
    let step_error_text = step_error.read().clone();
    let row_count = current.custom.len();

    let width = current.width.clone();
    let height = current.height.clone();
    let refresh_hz = current.refresh_hz.clone();
    let extra_args = current.extra_args.clone();

    let preview_body = match preview {
        Ok(command) => rsx! {
            code {
                style: "display:block;padding:10px 12px;border-radius:8px;background:#14141b;border:1px solid #26262f;color:#b8c8e8;font-size:11px;overflow-wrap:anywhere;white-space:pre-wrap;",
                "{command}"
            }
        },
        Err(error) => rsx! {
            span {
                style: "padding:8px 12px;border-radius:8px;background:#3a2a1e;color:#e0c090;font-size:12px;",
                "{error}"
            }
        },
    };

    rsx! {
        div {
            style: "display:flex;flex-direction:column;gap:10px;padding:12px 14px;border-radius:10px;background:#1a1a22;border:1px solid #26262f;",

            div {
                style: "display:flex;align-items:baseline;gap:8px;flex-wrap:wrap;",
                span {
                    style: "font-size:12px;font-weight:600;color:#e8e8ee;",
                    "Wrappers"
                }
                span {
                    style: "font-size:11px;color:#78788a;",
                    "run outermost first, outside the game"
                }
            }

            div {
                style: "display:flex;gap:8px;flex-wrap:wrap;",
                button {
                    onclick: move |_| {
                        let mut draft = draft.write();
                        draft.gamemode = !draft.gamemode;
                    },
                    style: toggle_style(current.gamemode),
                    "gamemoderun"
                }
                button {
                    onclick: move |_| {
                        let mut draft = draft.write();
                        draft.mangohud = !draft.mangohud;
                    },
                    style: toggle_style(current.mangohud),
                    "mangohud"
                }
                button {
                    onclick: move |_| {
                        let mut draft = draft.write();
                        draft.gamescope = !draft.gamescope;
                    },
                    style: toggle_style(current.gamescope),
                    "gamescope"
                }
            }

            if current.gamescope {
                div {
                    style: "display:flex;flex-direction:column;gap:8px;padding:10px 12px;border-radius:8px;background:#1e1e28;border:1px solid #2c2c38;",
                    div {
                        style: "display:flex;gap:8px;flex-wrap:wrap;align-items:flex-end;",
                        label {
                            style: field_style(),
                            span { style: label_style(), "Width" }
                            input {
                                placeholder: "auto",
                                value: "{width}",
                                oninput: move |event| draft.write().width = event.value(),
                                style: input_style(),
                            }
                        }
                        label {
                            style: field_style(),
                            span { style: label_style(), "Height" }
                            input {
                                placeholder: "auto",
                                value: "{height}",
                                oninput: move |event| draft.write().height = event.value(),
                                style: input_style(),
                            }
                        }
                        label {
                            style: field_style(),
                            span { style: label_style(), "Refresh Hz" }
                            input {
                                placeholder: "auto",
                                value: "{refresh_hz}",
                                oninput: move |event| draft.write().refresh_hz = event.value(),
                                style: input_style(),
                            }
                        }
                        button {
                            onclick: move |_| {
                                let mut draft = draft.write();
                                draft.fullscreen = !draft.fullscreen;
                            },
                            style: toggle_style(current.fullscreen),
                            "fullscreen"
                        }
                    }
                    label {
                        style: field_style(),
                        span { style: label_style(), "Extra args" }
                        input {
                            placeholder: "--hdr-enabled",
                            value: "{extra_args}",
                            oninput: move |event| draft.write().extra_args = event.value(),
                            style: input_style(),
                        }
                    }
                    span {
                        style: "font-size:11px;color:#78788a;",
                        "The console session already owns a compositor; this step is skipped under PITAGAMES_SESSION=1."
                    }
                }
            }

            div {
                style: "display:flex;flex-direction:column;gap:6px;",
                span { style: label_style(), "Custom wrappers" }
                if current.custom.is_empty() {
                    span {
                        style: "font-size:11px;color:#78788a;",
                        "None. Add a program (and optional arguments) to put another process in front of the game."
                    }
                }
                for (index, step) in current.custom.iter().cloned().enumerate() {
                    {
                        let remove_index = index;
                        let up_index = index;
                        let down_index = index;
                        rsx! {
                            div {
                                key: "{index}",
                                style: "display:flex;align-items:center;gap:8px;padding:8px 10px;border-radius:8px;background:#1e1e28;border:1px solid #2c2c38;",
                                code {
                                    style: "flex:1;font-size:11px;color:#c8c8d4;overflow-wrap:anywhere;",
                                    "{step_label(&step)}"
                                }
                                button {
                                    disabled: up_index == 0,
                                    onclick: move |_| {
                                        let mut draft = draft.write();
                                        if up_index > 0 {
                                            draft.custom.swap(up_index, up_index - 1);
                                        }
                                    },
                                    style: small_style(up_index == 0),
                                    "Up"
                                }
                                button {
                                    disabled: down_index + 1 >= row_count,
                                    onclick: move |_| {
                                        let mut draft = draft.write();
                                        if down_index + 1 < draft.custom.len() {
                                            draft.custom.swap(down_index, down_index + 1);
                                        }
                                    },
                                    style: small_style(down_index + 1 >= row_count),
                                    "Down"
                                }
                                button {
                                    onclick: move |_| {
                                        let mut draft = draft.write();
                                        if remove_index < draft.custom.len() {
                                            draft.custom.remove(remove_index);
                                        }
                                    },
                                    style: "padding:5px 10px;border-radius:7px;border:1px solid #4a2c2c;background:#2a1e1e;color:#e0a0a0;font-size:11px;cursor:pointer;",
                                    "Remove"
                                }
                            }
                        }
                    }
                }

                div {
                    style: "display:flex;gap:8px;flex-wrap:wrap;align-items:center;",
                    input {
                        placeholder: "program",
                        value: "{program_text}",
                        oninput: move |event| program.set(event.value()),
                        style: input_style(),
                    }
                    input {
                        placeholder: "args (quoted as needed)",
                        value: "{args_text}",
                        oninput: move |event| args.set(event.value()),
                        style: "flex:1;min-width:180px;padding:7px 10px;border-radius:8px;border:1px solid #2c2c38;background:#1e1e28;color:#e8e8ee;font-size:12px;outline:none;",
                    }
                    button {
                        disabled: program_text.trim().is_empty(),
                        onclick: move |_| {
                            step_error.set(String::new());
                            let result = draft.write().add_step(&program(), &args());
                            match result {
                                Ok(()) => {
                                    program.set(String::new());
                                    args.set(String::new());
                                }
                                Err(message) => step_error.set(message),
                            }
                        },
                        style: small_style(program_text.trim().is_empty()),
                        "Add step"
                    }
                }

                if !step_error_text.is_empty() {
                    span {
                        style: "font-size:11px;color:#e8a0a0;",
                        "{step_error_text}"
                    }
                }
            }

            div {
                style: "display:flex;flex-direction:column;gap:4px;",
                span { style: label_style(), "Preview" }
                {preview_body}
            }
        }
    }
}

/// One row's display text: the same quoting the preview uses.
fn step_label(step: &WrapperDraftStep) -> String {
    let args = step.args.trim();
    if args.is_empty() {
        step.program.clone()
    } else {
        format!("{} {args}", step.program)
    }
}

fn toggle_style(active: bool) -> &'static str {
    if active {
        "padding:6px 14px;border-radius:999px;border:1px solid #3f5c8c;background:#243044;color:#ffffff;font-size:12px;cursor:pointer;"
    } else {
        "padding:6px 14px;border-radius:999px;border:1px solid #2c2c38;background:#1e1e28;color:#a8a8b8;font-size:12px;cursor:pointer;"
    }
}

fn small_style(disabled: bool) -> &'static str {
    if disabled {
        "padding:5px 10px;border-radius:7px;border:1px solid #26262f;background:#1a1a22;color:#5a5a68;font-size:11px;cursor:default;"
    } else {
        "padding:5px 10px;border-radius:7px;border:1px solid #3a3a4c;background:#2e3a56;color:#ffffff;font-size:11px;cursor:pointer;"
    }
}

fn field_style() -> &'static str {
    "display:flex;flex-direction:column;gap:3px;"
}

fn label_style() -> &'static str {
    "font-size:10px;text-transform:uppercase;letter-spacing:0.06em;color:#78788a;"
}

fn input_style() -> &'static str {
    "width:96px;padding:7px 10px;border-radius:8px;border:1px solid #2c2c38;background:#1e1e28;color:#e8e8ee;font-size:12px;outline:none;"
}
