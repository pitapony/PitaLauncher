//! The Controls screen: the remap UI, and the one writer of `input.toml`.
//!
//! Two levels, both driven by the shared focus graph so a controller can do
//! everything a pointer can:
//!
//! - the **list** picks a device (the keyboard, or a connected pad by its GUID),
//!   shows every action with its current bindings, and offers Reset all / Save /
//!   Discard;
//! - the **detail** edits one action: rebind a binding slot, remove it, add one,
//!   or reset the action to its built-in layout.
//!
//! A rebind does not prompt with a button list. The session goes into *capture*
//! mode and the app's own input pumps hand it the next control pressed — the pad
//! button or the key itself — which is the only way to bind a control the map
//! does not already recognise, and the reason the session lives in the app and
//! not in this component. The pump swallows the action a capture consumes, so a
//! pressed button can never navigate and bind at once (`docs/m2.5-remap.md`).
//!
//! Saving writes `<XDG_CONFIG_HOME>/pitagames/input.toml` through
//! [`InputMaps::save_default`], applies the maps to the hint row, and pushes them
//! into the running gamepad worker so a remap takes effect without a restart.

use dioxus::prelude::*;
use pitagames_input::{
    Action, Binding, CaptureSlot, Device, InputMaps, RemapSession, RemapTarget, action_label,
    binding_label,
};

use crate::View;
use crate::focus::{self, ActionChannel};
use crate::gamepad::MapReload;

/// Props for the Controls screen.
#[derive(Props, Clone, PartialEq)]
pub struct ControlsViewProps {
    /// The app's current view; Back leaves the Controls screen.
    pub view: Signal<View>,
    /// The app's one input pump posts this screen's actions here.
    pub actions: ActionChannel,
    /// The persisted maps; Save replaces them and the hint row follows.
    pub maps: Signal<InputMaps>,
    /// The one remap session, owned by the app because the input pumps read its
    /// capture state.
    pub remap: Signal<Option<RemapSession>>,
    /// The pads the gamepad worker has named, for the target list.
    pub pads: Signal<Vec<Device>>,
    /// A handle to push saved maps into the running gamepad worker.
    pub reload: Signal<MapReload>,
    /// True while a number field owns the keyboard.
    pub text_focused: Signal<bool>,
}

/// One focusable cell of the list level, in reading order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListCell {
    /// Select the `n`th device (keyboard first, then each pad).
    Target(usize),
    /// Open an action's detail.
    Action(Action),
    /// Restore every built-in binding.
    ResetAll,
    /// Write the file and apply it.
    Save,
    /// Throw away unsaved edits and reload the stored map.
    Discard,
}

/// One focusable cell of an action's detail level, in reading order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetailCell {
    /// Capture a new control for one slot.
    Rebind { action: Action, index: usize },
    /// Remove one slot.
    Remove { action: Action, index: usize },
    /// Add a binding to the action.
    Add(Action),
    /// Reset the action to its built-in bindings.
    Reset(Action),
    /// Return to the list.
    Back,
}

/// The device targets: the keyboard, then every connected gamepad.
fn targets(pads: &[Device]) -> Vec<RemapTarget> {
    let mut list = vec![RemapTarget::keyboard()];
    for device in pads.iter().filter(|device| device.is_gamepad) {
        list.push(RemapTarget::pad(&device.guid, &device.name));
    }
    list
}

/// The list level's cells.
fn list_cells(pads: &[Device]) -> Vec<ListCell> {
    let mut cells: Vec<ListCell> = (0..targets(pads).len()).map(ListCell::Target).collect();
    cells.extend(Action::ALL.into_iter().map(ListCell::Action));
    cells.push(ListCell::ResetAll);
    cells.push(ListCell::Save);
    cells.push(ListCell::Discard);
    cells
}

/// One action's detail cells, in reading order.
fn detail_cells(session: &RemapSession, action: Action) -> Vec<DetailCell> {
    let mut cells = Vec::new();
    for index in 0..session.bindings(action).len() {
        cells.push(DetailCell::Rebind { action, index });
        cells.push(DetailCell::Remove { action, index });
    }
    cells.push(DetailCell::Add(action));
    cells.push(DetailCell::Reset(action));
    cells.push(DetailCell::Back);
    cells
}

/// One binding's short label, or a placeholder for a shape with none.
fn binding_text(binding: &Binding) -> String {
    binding_label(binding).unwrap_or_else(|| "?".to_string())
}

/// An action's bindings as one line, `unbound` when it has none.
fn binding_summary(session: &RemapSession, action: Action) -> String {
    let labels: Vec<String> = session.bindings(action).iter().map(binding_text).collect();
    if labels.is_empty() {
        "unbound".to_string()
    } else {
        labels.join("  ·  ")
    }
}

/// Switch the session to another device.
fn select_target(
    index: usize,
    pads: Signal<Vec<Device>>,
    maps: Signal<InputMaps>,
    mut remap: Signal<Option<RemapSession>>,
    focus: focus::ScreenFocus,
) {
    let list = targets(&pads.read());
    let Some(target) = list.into_iter().nth(index) else {
        return;
    };
    remap.set(Some(RemapSession::new(target, &maps.read())));
    focus.set(None);
}

/// Throw away unsaved edits and reload the device's stored map.
fn discard(maps: Signal<InputMaps>, mut remap: Signal<Option<RemapSession>>) {
    let Some(target) = remap
        .read()
        .as_ref()
        .map(|session| session.target().clone())
    else {
        return;
    };
    remap.set(Some(RemapSession::new(target, &maps.read())));
}

/// Put a session into capture mode for a slot.
fn begin_capture(mut remap: Signal<Option<RemapSession>>, slot: CaptureSlot) {
    if let Some(session) = remap.write().as_mut() {
        session.begin_capture(slot);
    }
}

/// Write the session into `input.toml`, apply it to the hint row, and push it
/// into the running gamepad worker.
fn save(
    mut maps: Signal<InputMaps>,
    remap: Signal<Option<RemapSession>>,
    reload: MapReload,
    mut status: Signal<String>,
    mut status_error: Signal<bool>,
) {
    let Some(session) = remap.read().clone() else {
        return;
    };
    let mut next = maps.read().clone();
    session.apply(&mut next);
    match next.save_default() {
        Ok(()) => {
            maps.set(next.clone());
            reload.send(next);
            status.set("Saved. The new bindings apply at once.".to_string());
            status_error.set(false);
        }
        Err(error) => {
            status.set(format!("Could not save input.toml: {error}"));
            status_error.set(true);
        }
    }
}

/// The Controls screen.
#[allow(non_snake_case)]
pub fn ControlsView(props: ControlsViewProps) -> Element {
    let view = props.view;
    let actions = props.actions;
    let maps = props.maps;
    let mut remap = props.remap;
    let pads = props.pads;
    let reload = props.reload;
    let text_focused = props.text_focused;

    let screen_focus = focus::use_focus();
    // `None` is the list; `Some(action)` is that action's detail.
    let mut editing = use_signal(|| None::<Action>);
    let mut status = use_signal(String::new);
    let status_error = use_signal(|| false);

    // The action drain. The app's pump posts here once a capture (if any) has
    // been resolved, so this only navigates and confirms.
    use_future(move || {
        let channel = actions.clone();
        async move {
            while let Some(action) = channel.next().await {
                if view() != View::Controls {
                    continue;
                }
                handle_action(
                    action,
                    screen_focus,
                    editing,
                    remap,
                    maps,
                    pads,
                    reload(),
                    status,
                    status_error,
                    view,
                );
            }
        }
    });

    let pad_list = pads.read().clone();
    let target_list = targets(&pad_list);
    let session = remap.read().clone();
    let list = list_cells(&pad_list);
    let focused = screen_focus.index();
    let detail = *editing.read();
    let target_count = target_list.len();
    let message = session
        .as_ref()
        .and_then(|session| session.message().map(str::to_string));
    let message_error = session.as_ref().is_some_and(|session| session.has_error());
    let capture = session.as_ref().is_some_and(RemapSession::is_capturing);
    let customized = session
        .as_ref()
        .is_some_and(|session| session.is_customized());
    let save_status = status();
    let save_error = status_error();
    let maps_path = pitagames_core::paths::input_maps_file()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "input.toml".to_string());
    // The detail level's last cell index, for the Add/Reset/Back buttons. Only
    // meaningful when an action is open, but computed here because RSX children
    // cannot be `let` statements.
    let back_cell = match (session.as_ref(), detail) {
        (Some(session), Some(action)) => detail_cells(session, action).len() - 1,
        _ => 0,
    };

    rsx! {
        div {
            style: "display:flex;flex-direction:column;gap:14px;padding:0 32px 40px;max-width:860px;",

            div {
                style: "display:flex;flex-direction:column;gap:6px;",
                span {
                    style: "font-size:16px;font-weight:600;color:#ffffff;",
                    "Controls"
                }
                span {
                    style: "font-size:12px;color:#78788a;line-height:1.5;",
                    "Rebind the keyboard or a connected controller. A rebind asks you to press the control you want; the launcher writes it to input.toml, so a remap survives a restart and a replugged controller."
                }
            }

            if let Some(session) = session.as_ref() {
                // Target chips: the keyboard, then each connected pad.
                div {
                    style: "display:flex;align-items:center;gap:8px;flex-wrap:wrap;",
                    span {
                        style: "font-size:12px;color:#9a9aac;",
                        "Device:"
                    }
                    for (index, target) in target_list.iter().enumerate() {
                        button {
                            key: "{target.device()}",
                            onclick: move |_| select_target(index, pads, maps, remap, screen_focus),
                            style: "{chip_style(session.target() == target)}{focus::ring_style(focused == Some(index))}",
                            "{target.label()}"
                        }
                    }
                }

                if capture {
                    div {
                        style: "padding:10px 14px;border-radius:8px;background:#2a2a1e;color:#e8d8a0;font-size:13px;font-weight:600;",
                        "Press a control now…"
                    }
                }

                if let Some(message) = message.clone() {
                    div {
                        style: if message_error { "padding:8px 12px;border-radius:8px;background:#3a1e1e;color:#e8a0a0;font-size:12px;" } else { "padding:8px 12px;border-radius:8px;background:#1e2430;color:#9ab0d8;font-size:12px;" },
                        "{message}"
                    }
                }

                if detail.is_none() {
                    for (row, action) in Action::ALL.into_iter().enumerate() {
                        button {
                            key: "{action.as_str()}",
                            onclick: move |_| {
                                editing.set(Some(action));
                                screen_focus.set(None);
                                status.set(String::new());
                            },
                            style: "{row_style()}{focus::ring_style(focused == Some(target_count + row))}",
                            span {
                                style: "min-width:150px;text-align:left;font-weight:600;",
                                "{action_label(action)}"
                            }
                            span {
                                style: "color:#a8a8b8;",
                                "{binding_summary(session, action)}"
                            }
                        }
                    }

                    div {
                        style: "display:flex;align-items:center;gap:8px;flex-wrap:wrap;",
                        button {
                            onclick: move |_| {
                                if let Some(session) = remap.write().as_mut() {
                                    session.reset_all();
                                }
                            },
                            style: "{action_style()}{focus::ring_style(focused == Some(list.len() - 3))}",
                            "Reset all"
                        }
                        button {
                            onclick: move |_| save(maps, remap, reload(), status, status_error),
                            style: "{primary_style()}{focus::ring_style(focused == Some(list.len() - 2))}",
                            "Save"
                        }
                        button {
                            disabled: !customized,
                            onclick: move |_| {
                                discard(maps, remap);
                                status.set(String::new());
                            },
                            style: "{action_style()}{focus::ring_style(focused == Some(list.len() - 1))}",
                            "Discard changes"
                        }
                    }
                } else if let Some(action) = detail {
                    // The action's detail: one rebind/remove pair per binding.
                    div {
                        style: "display:flex;align-items:center;gap:10px;",
                        button {
                            onclick: move |_| {
                                editing.set(None);
                                screen_focus.set(None);
                            },
                            style: "{action_style()}{focus::ring_style(focused == Some(back_cell))}",
                            "Back"
                        }
                        span {
                            style: "font-size:14px;font-weight:600;color:#ffffff;",
                            "{action_label(action)}"
                        }
                    }

                    for (index, binding) in session.bindings(action).iter().enumerate() {
                        div {
                            key: "{index}",
                            style: "display:flex;align-items:center;gap:8px;",
                            button {
                                onclick: move |_| begin_capture(remap, CaptureSlot::Replace { action, index }),
                                style: "{row_style()}{focus::ring_style(focused == Some(index * 2))}",
                                span {
                                    style: "min-width:120px;text-align:left;font-weight:600;",
                                    "{binding_text(binding)}"
                                }
                                span {
                                    style: "color:#a8a8b8;font-size:12px;",
                                    "press a control to rebind"
                                }
                            }
                            button {
                                onclick: move |_| {
                                    if let Some(session) = remap.write().as_mut() {
                                        session.remove_binding(action, index);
                                    }
                                },
                                style: "{action_style()}{focus::ring_style(focused == Some(index * 2 + 1))}",
                                "Remove"
                            }
                        }
                    }

                    div {
                        style: "display:flex;align-items:center;gap:8px;flex-wrap:wrap;",
                        button {
                            onclick: move |_| begin_capture(remap, CaptureSlot::Add { action }),
                            style: "{action_style()}{focus::ring_style(focused == Some(back_cell - 2))}",
                            "Add binding"
                        }
                        button {
                            onclick: move |_| {
                                if let Some(session) = remap.write().as_mut() {
                                    session.reset_action(action);
                                }
                            },
                            style: "{action_style()}{focus::ring_style(focused == Some(back_cell - 1))}",
                            "Reset action"
                        }
                    }
                }

                // The feel numbers. Pointer-only for now: they are free text.
                div {
                    style: "display:flex;align-items:center;gap:12px;flex-wrap:wrap;padding-top:8px;border-top:1px solid #26262f;",
                    TuningField {
                        label: "Deadzone",
                        field: Tuning::Deadzone,
                        value: session.map().deadzone.to_string(),
                        remap,
                        text_focused,
                    }
                    TuningField {
                        label: "Repeat delay (ms)",
                        field: Tuning::RepeatDelay,
                        value: session.map().repeat_delay_ms.to_string(),
                        remap,
                        text_focused,
                    }
                    TuningField {
                        label: "Repeat every (ms)",
                        field: Tuning::RepeatInterval,
                        value: session.map().repeat_interval_ms.to_string(),
                        remap,
                        text_focused,
                    }
                }

                span {
                    style: "font-size:11px;color:#78788a;line-height:1.5;",
                    "Save writes {maps_path}. A device with no changes keeps no entry, so resetting everything empties the file."
                }
            } else {
                div {
                    style: "padding:40px 0;color:#78788a;font-size:13px;",
                    "Preparing the remap session…"
                }
            }

            if !save_status.is_empty() {
                div {
                    style: if save_error { "padding:8px 12px;border-radius:8px;background:#3a1e1e;color:#e8a0a0;font-size:12px;" } else { "padding:8px 12px;border-radius:8px;background:#1e2a24;color:#b8dcc4;font-size:12px;" },
                    "{save_status}"
                }
            }
        }
    }
}

/// Which feel number a [`TuningField`] edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tuning {
    Deadzone,
    RepeatDelay,
    RepeatInterval,
}

/// Props for one feel number.
#[derive(Props, Clone, PartialEq)]
struct TuningFieldProps {
    label: &'static str,
    field: Tuning,
    value: String,
    remap: Signal<Option<RemapSession>>,
    text_focused: Signal<bool>,
}

/// One number field for the feel tuning.
///
/// Not a focus-graph cell: a pad cannot type a number, so the tuning stays a
/// pointer affordance until the on-screen keyboard slice lands.
#[allow(non_snake_case)]
fn TuningField(props: TuningFieldProps) -> Element {
    let label = props.label;
    let field = props.field;
    let value = props.value;
    let mut remap = props.remap;
    let mut text_focused = props.text_focused;
    rsx! {
        label {
            style: "display:flex;align-items:center;gap:6px;font-size:12px;color:#9a9aac;",
            "{label}"
            input {
                r#type: "number",
                value: "{value}",
                onfocus: move |_| text_focused.set(true),
                onblur: move |_| text_focused.set(false),
                oninput: move |event| {
                    if let Ok(parsed) = event.value().parse::<f32>() {
                        let mut guard = remap.write();
                        if let Some(session) = guard.as_mut() {
                            match field {
                                Tuning::Deadzone => session.set_deadzone(parsed),
                                Tuning::RepeatDelay => {
                                    session.set_repeat_delay_ms(parsed.max(0.0) as u32)
                                }
                                Tuning::RepeatInterval => {
                                    session.set_repeat_interval_ms(parsed.max(0.0) as u32)
                                }
                            }
                        }
                    }
                },
                style: "width:110px;padding:5px 8px;border-radius:7px;border:1px solid #2c2c38;background:#1e1e28;color:#e8e8ee;font-size:12px;outline:none;",
            }
        }
    }
}

/// Apply one action to the Controls screen's own graph.
#[allow(clippy::too_many_arguments)]
fn handle_action(
    action: Action,
    screen_focus: focus::ScreenFocus,
    mut editing: Signal<Option<Action>>,
    mut remap: Signal<Option<RemapSession>>,
    maps: Signal<InputMaps>,
    pads: Signal<Vec<Device>>,
    reload: MapReload,
    mut status: Signal<String>,
    status_error: Signal<bool>,
    mut view: Signal<View>,
) {
    if action == Action::Back {
        if editing.read().is_some() {
            editing.set(None);
            screen_focus.set(None);
        } else {
            view.set(View::Library);
        }
        return;
    }

    let Some(session) = remap.read().clone() else {
        return;
    };
    let detail = *editing.read();

    if action.is_navigation() {
        let count = match detail {
            Some(action) => detail_cells(&session, action).len(),
            None => list_cells(&pads.read()).len(),
        };
        screen_focus.move_focus(action, count, 1);
        return;
    }

    if action != Action::Confirm {
        return;
    }
    let Some(index) = screen_focus.index() else {
        return;
    };

    match detail {
        None => {
            let Some(cell) = list_cells(&pads.read()).get(index).copied() else {
                return;
            };
            match cell {
                ListCell::Target(_) => select_target(index, pads, maps, remap, screen_focus),
                ListCell::Action(action) => {
                    editing.set(Some(action));
                    screen_focus.set(None);
                    status.set(String::new());
                }
                ListCell::ResetAll => {
                    if let Some(session) = remap.write().as_mut() {
                        session.reset_all();
                    }
                }
                ListCell::Save => save(maps, remap, reload, status, status_error),
                ListCell::Discard => {
                    discard(maps, remap);
                    if let Some(session) = remap.write().as_mut() {
                        session.cancel_capture();
                    }
                }
            }
        }
        Some(action) => {
            let Some(cell) = detail_cells(&session, action).get(index).copied() else {
                return;
            };
            match cell {
                DetailCell::Rebind { action, index } => {
                    begin_capture(remap, CaptureSlot::Replace { action, index })
                }
                DetailCell::Remove { action, index } => {
                    if let Some(session) = remap.write().as_mut() {
                        session.remove_binding(action, index);
                    }
                    // The list shrank by two cells; the old index may point past
                    // the end, so return the ring to the first cell.
                    screen_focus.set(Some(0));
                }
                DetailCell::Add(action) => begin_capture(remap, CaptureSlot::Add { action }),
                DetailCell::Reset(action) => {
                    if let Some(session) = remap.write().as_mut() {
                        session.reset_action(action);
                    }
                }
                DetailCell::Back => {
                    editing.set(None);
                    screen_focus.set(None);
                }
            }
        }
    }
}

fn chip_style(active: bool) -> &'static str {
    if active {
        "padding:6px 12px;border-radius:999px;border:1px solid #4a6a9a;background:#2e3a56;color:#ffffff;font-size:12px;cursor:pointer;"
    } else {
        "padding:6px 12px;border-radius:999px;border:1px solid #2c2c38;background:#1e1e28;color:#c8c8d4;font-size:12px;cursor:pointer;"
    }
}

fn row_style() -> &'static str {
    "display:flex;align-items:center;gap:12px;width:100%;padding:9px 12px;border-radius:8px;border:1px solid #2c2c38;background:#1e1e28;color:#e8e8ee;font-size:13px;text-align:left;cursor:pointer;"
}

fn action_style() -> &'static str {
    "padding:6px 12px;border-radius:7px;border:1px solid #3a3a4c;background:#242432;color:#e8e8ee;font-size:12px;cursor:pointer;"
}

fn primary_style() -> &'static str {
    "padding:6px 14px;border-radius:7px;border:1px solid #3a5a48;background:#243a2e;color:#d8efe0;font-size:12px;cursor:pointer;font-weight:600;"
}

#[cfg(test)]
mod tests {
    use super::*;
    use pitagames_input::DeviceId;

    fn pad(id: u64) -> Device {
        Device::new(DeviceId(id), format!("Pad {id}"), format!("guid-{id}"))
    }

    #[test]
    fn the_target_list_is_the_keyboard_then_every_gamepad() {
        let pads = vec![pad(1), pad(2)];
        let list = targets(&pads);
        assert_eq!(list.len(), 3);
        assert!(list[0].is_keyboard());
        assert_eq!(list[1].device(), "guid-1");
        assert_eq!(list[2].label(), "Pad 2");
    }

    #[test]
    fn the_list_cells_are_the_targets_the_actions_and_the_globals() {
        let pads = vec![pad(1)];
        let cells = list_cells(&pads);
        // keyboard + one pad + 12 actions + reset/save/discard.
        assert_eq!(cells.len(), 2 + Action::ALL.len() + 3);
        assert_eq!(cells[0], ListCell::Target(0));
        assert_eq!(cells[1], ListCell::Target(1));
        assert_eq!(cells[2], ListCell::Action(Action::NavigateUp));
        assert_eq!(cells[cells.len() - 3], ListCell::ResetAll);
        assert_eq!(cells[cells.len() - 1], ListCell::Discard);
    }

    #[test]
    fn a_detail_lists_a_rebind_and_remove_per_binding_then_the_globals() {
        let maps = InputMaps::new();
        let session = RemapSession::new(RemapTarget::keyboard(), &maps);
        // Confirm is Enter and Space on the keyboard.
        let cells = detail_cells(&session, Action::Confirm);
        assert_eq!(
            cells,
            [
                DetailCell::Rebind {
                    action: Action::Confirm,
                    index: 0
                },
                DetailCell::Remove {
                    action: Action::Confirm,
                    index: 0
                },
                DetailCell::Rebind {
                    action: Action::Confirm,
                    index: 1
                },
                DetailCell::Remove {
                    action: Action::Confirm,
                    index: 1
                },
                DetailCell::Add(Action::Confirm),
                DetailCell::Reset(Action::Confirm),
                DetailCell::Back,
            ]
        );

        // An action with no bindings still offers add/reset/back.
        let mut session = RemapSession::new(RemapTarget::keyboard(), &maps);
        session.remove_binding(Action::Confirm, 0);
        session.remove_binding(Action::Confirm, 0);
        let cells = detail_cells(&session, Action::Confirm);
        assert_eq!(cells.len(), 3);
        assert_eq!(cells[0], DetailCell::Add(Action::Confirm));
        assert_eq!(cells[2], DetailCell::Back);
    }

    #[test]
    fn a_row_summarises_an_unbound_action_honestly() {
        let maps = InputMaps::new();
        let mut session = RemapSession::new(RemapTarget::keyboard(), &maps);
        session.remove_binding(Action::Confirm, 0);
        session.remove_binding(Action::Confirm, 0);
        assert_eq!(binding_summary(&session, Action::Confirm), "unbound");
    }
}
